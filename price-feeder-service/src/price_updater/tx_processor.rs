use alloy::{
	primitives::{Address, B256},
	providers::{Provider, ProviderBuilder},
	rpc::types::TransactionReceipt,
	rpc::types::TransactionRequest,
	transports::Transport,
};
use log::{error, info, warn};
use reqwest::Url;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::price_updater::alerts;
use crate::price_updater::chain::{PriorityFeeMultiplier, ResyncKind};

/// Hard safety valve for the tracked-tx list. Entries are pruned as soon as
/// their nonce is consumed on-chain, so the list normally holds only the
/// in-flight txs (around ten at typical update intervals). Reaching this
/// limit means the chain has been unable to mine our txs for a very long
/// time; we drop the oldest entries and log loudly rather than grow without
/// bound.
const MAX_TRACKED_TXS: usize = 5_000;
const RESYNC_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);
const CONFIRM_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Consecutive receipt-poll RPC failures after which a single Slack alert is
/// sent for that tx. The poller keeps retrying either way — a transient RPC
/// error must not permanently blind us to a confirmation.
const RPC_ERROR_ALERT_THRESHOLD: u32 = 5;
/// How long the "on-chain nonce below our lowest tracked tx" condition must
/// persist before the watchdog trusts it and forces a resync. A single such
/// read can be a lagging / load-balanced RPC replica rather than a real
/// untracked blocker; replica lag is transient while a genuine gap or
/// leftover persists indefinitely.
const GAP_CONFIRMATION_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateTxKind {
	DarkOracle,
	Pyth,
	DisableAsset,
	EnableAsset,
}

impl fmt::Display for UpdateTxKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			UpdateTxKind::DarkOracle => write!(f, "DarkOracle"),
			UpdateTxKind::Pyth => write!(f, "Pyth"),
			UpdateTxKind::DisableAsset => write!(f, "DisableAsset"),
			UpdateTxKind::EnableAsset => write!(f, "EnableAsset"),
		}
	}
}

/// `true` for kinds whose payload may be swapped for a newer same-kind
/// payload when replaced (a price update is superseded by fresher prices).
/// `EnableAsset`/`DisableAsset` are load-bearing state-change txs: the
/// watchdog still fee-bumps them when they get stuck, but always with their
/// *own* payload — the data must land exactly as submitted.
pub fn is_replaceable(kind: UpdateTxKind) -> bool {
	matches!(kind, UpdateTxKind::DarkOracle | UpdateTxKind::Pyth)
}

#[derive(Debug)]
pub enum ConfirmOutcome {
	Confirmed,
	Reverted,
	RpcError,
}

/// Final on-chain outcome of a tracked nonce, delivered to callers that
/// registered a resolution waiter (e.g. the feed loop blocking on a disable
/// tx). Resolution is per-*nonce*, not per-hash: if the original tx is
/// replaced by a fee-bumped resend, the waiter fires when the replacement
/// lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxResolution {
	Confirmed,
	Reverted,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxState {
	Unconfirmed,
	/// A replacement for this entry's nonce has been *tracked* (it reached
	/// the node and came back through the update channel). The entry is kept
	/// until the nonce resolves — its original hash may still win the race —
	/// but it is no longer a watchdog candidate.
	Superseded,
}

#[derive(Debug, Clone)]
pub struct ReplaceRequest {
	pub kind: UpdateTxKind,
	pub targets: Vec<ReplacementTarget>,
}

#[derive(Debug, Clone)]
pub struct ReplacementTarget {
	pub nonce: u64,
	pub old_tx_hash: B256,
	pub old_max_priority_fee_per_gas: u128,
	pub old_max_fee_per_gas: u128,
	pub payload: Arc<TransactionRequest>,
}

/// Subset of `TrackedTx` returned by the watchdog scan, so the watcher loop
/// can act without holding the `tracked` lock across an await.
#[derive(Debug, Clone)]
pub struct WatchdogHit {
	pub tx_hash: B256,
	pub kind: UpdateTxKind,
	pub nonce: u64,
	pub replaceable: bool,
	pub replacement_count: u8,
}

#[derive(Debug)]
struct TrackedTx {
	tx_hash: B256,
	nonce: u64,
	timestamp: std::time::Instant,
	state: TxState,
	kind: UpdateTxKind,
	/// This is `is_replaceable(kind)`.
	replaceable: bool,
	/// Fees used on submit, captured by `send_tx_with_retry`. Needed so the
	/// replacement builder can floor its bump over these.
	max_priority_fee_per_gas: u128,
	max_fee_per_gas: u128,
	replacement_count: u8,
	/// Set when the watchdog queues a replacement for this entry. Re-arms the
	/// timeout so the watchdog fires again after another full timeout if the
	/// replacement never comes back through the tracking channel (submit
	/// failed, channel dropped, ...) — the nonce can never be orphaned.
	last_replacement_attempt_at: Option<std::time::Instant>,
	replacement_payload: Arc<TransactionRequest>,
}

struct TrackedState {
	txs: Vec<TrackedTx>,
	/// Per-nonce resolution waiters (see `TxResolution`).
	waiters: BTreeMap<u64, oneshot::Sender<TxResolution>>,
	/// Lowest nonce not yet known to be consumed on-chain. Unconfirmed
	/// entries below it are not actionable.
	consumed_floor: u64,
}

impl TrackedState {
	fn new() -> Self {
		Self { txs: Vec::new(), waiters: BTreeMap::new(), consumed_floor: 0 }
	}
}

/// Debounces the "untracked blocker" signal (on-chain latest nonce below our
/// lowest tracked unresolved tx).
struct GapDetector {
	window: std::time::Duration,
	suspected: Option<(u64, std::time::Instant)>,
}

impl GapDetector {
	fn new(window: std::time::Duration) -> Self {
		Self { window, suspected: None }
	}

	/// Records an observation of `latest < hit_nonce`. Returns `true` once
	/// the condition has persisted for the full window against the same hit
	/// nonce — the caller should then resync. A change of hit nonce restarts
	/// the window (the previous suspicion resolved itself).
	fn observe(&mut self, hit_nonce: u64, now: std::time::Instant) -> bool {
		match self.suspected {
			Some((nonce, first_seen)) if nonce == hit_nonce => {
				if now.duration_since(first_seen) >= self.window {
					self.suspected = None;
					true
				} else {
					false
				}
			},
			_ => {
				self.suspected = Some((hit_nonce, now));
				false
			},
		}
	}

	fn clear(&mut self) {
		self.suspected = None;
	}
}

/// Marks every live (Unconfirmed) entry at `nonce` as Superseded.
fn supersede_at_nonce(txs: &mut [TrackedTx], nonce: u64) -> usize {
	let mut count = 0;
	for tx in txs.iter_mut().filter(|t| t.nonce == nonce && t.state == TxState::Unconfirmed) {
		tx.state = TxState::Superseded;
		count += 1;
	}
	count
}

/// Resolves every tracked nonce `<= nonce`: fires the exact waiter at `nonce`
/// with `resolution`, sweeps lower waiters as Confirmed (EVM nonce order —
/// a mined nonce implies all lower nonces were consumed), prunes the entries,
/// and advances the floor. Returns the number of pruned entries.
fn resolve_up_to(
	state: &mut TrackedState,
	nonce: u64,
	resolution: TxResolution,
	context: &str,
) -> usize {
	let waiter_nonces: Vec<u64> = state.waiters.range(..=nonce).map(|(k, _)| *k).collect();
	for waiter_nonce in waiter_nonces {
		if let Some(waiter) = state.waiters.remove(&waiter_nonce) {
			let res = if waiter_nonce == nonce {
				resolution
			} else {
				warn!(
					"[Watchdog] waiter at nonce {} swept as Confirmed by resolution of nonce {} ({})",
					waiter_nonce, nonce, context
				);
				TxResolution::Confirmed
			};
			// The receiver may have been dropped (caller gave up) — ignore.
			let _ = waiter.send(res);
		}
	}

	let before = state.txs.len();
	state.txs.retain(|t| t.nonce > nonce);
	state.consumed_floor = state.consumed_floor.max(nonce.saturating_add(1));
	before - state.txs.len()
}

/// Returns `Some(WatchdogHit)` when the lowest unresolved nonce has exceeded
/// `nonce_tx_timeout` since its last action (submission or last replacement
/// attempt). EVM accounts execute strictly in nonce order, so a higher
/// timed-out nonce is not actionable while a lower nonce is still pending.
/// Superseded entries are skipped — their replacement is the live candidate.
fn check_watchdog_timeout(
	tracked: &[TrackedTx],
	consumed_floor: u64,
	nonce_tx_timeout: std::time::Duration,
) -> Option<WatchdogHit> {
	let now = std::time::Instant::now();

	let candidate = tracked
		.iter()
		.filter(|t| t.state == TxState::Unconfirmed && t.nonce >= consumed_floor)
		.min_by_key(|t| (t.nonce, t.timestamp))?;

	let last_action = candidate.last_replacement_attempt_at.unwrap_or(candidate.timestamp);
	if now.duration_since(last_action) < nonce_tx_timeout {
		return None;
	}

	Some(WatchdogHit {
		tx_hash: candidate.tx_hash,
		kind: candidate.kind,
		nonce: candidate.nonce,
		replaceable: candidate.replaceable,
		replacement_count: candidate.replacement_count,
	})
}

fn build_replace_request(tracked: &[TrackedTx], hit: &WatchdogHit) -> Option<ReplaceRequest> {
	if hit.replaceable {
		// Price kinds: latest-wins. Replace every live same-kind nonce from
		// the blocker upward with the newest same-kind payload.
		let source = tracked
			.iter()
			.filter(|t| t.state == TxState::Unconfirmed && t.kind == hit.kind)
			.fold(None, |best: Option<&TrackedTx>, tx| match best {
				Some(best) if best.nonce > tx.nonce => Some(best),
				Some(best) if best.nonce == tx.nonce && best.timestamp >= tx.timestamp =>
					Some(best),
				_ => Some(tx),
			})?;
		let payload = source.replacement_payload.clone();

		let mut targets_by_nonce = BTreeMap::new();
		for tx in tracked.iter().filter(|t| {
			t.state == TxState::Unconfirmed && t.kind == hit.kind && t.nonce >= hit.nonce
		}) {
			targets_by_nonce.insert(
				tx.nonce,
				ReplacementTarget {
					nonce: tx.nonce,
					old_tx_hash: tx.tx_hash,
					old_max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
					old_max_fee_per_gas: tx.max_fee_per_gas,
					payload: payload.clone(),
				},
			);
		}

		let targets = targets_by_nonce.into_values().collect::<Vec<_>>();
		if targets.is_empty() {
			return None;
		}
		Some(ReplaceRequest { kind: hit.kind, targets })
	} else {
		// Critical kinds (Enable/Disable): re-send the stuck tx's *own*
		// payload with bumped fees. Single target — a higher critical tx gets
		// its turn once it becomes the blocker.
		let entry = tracked
			.iter()
			.find(|t| t.tx_hash == hit.tx_hash && t.state == TxState::Unconfirmed)?;
		Some(ReplaceRequest {
			kind: hit.kind,
			targets: vec![ReplacementTarget {
				nonce: entry.nonce,
				old_tx_hash: entry.tx_hash,
				old_max_priority_fee_per_gas: entry.max_priority_fee_per_gas,
				old_max_fee_per_gas: entry.max_fee_per_gas,
				payload: entry.replacement_payload.clone(),
			}],
		})
	}
}

pub struct UpdateTx {
	pub kind: UpdateTxKind,
	pub tx_hash: B256,
	pub nonce: u64,
	pub max_priority_fee_per_gas: u128,
	pub max_fee_per_gas: u128,
	pub replacement_payload: Arc<TransactionRequest>,
	/// Optional per-nonce resolution waiter (see `TxResolution`). Used by
	/// callers that must block until the tx (or a fee-bumped replacement of
	/// it) lands — e.g. the disable-asset path.
	pub resolution_tx: Option<oneshot::Sender<TxResolution>>,
}

/// Receives `UpdateTx` messages, handles confirmation, and tracks tx state
/// for watchdog-style timeout detection. When the lowest unresolved nonce
/// times out, the watchdog first probes the on-chain (latest) account nonce:
///   * on-chain nonce **above** the stuck one → the txs were consumed and the
///     receipts just haven't been observed yet; resolve and move on;
///   * on-chain nonce **below** the stuck one → an *untracked* tx (gap from a
///     failed submit, or a leftover from a previous run) blocks the queue; a
///     nonce resync (to the latest nonce) is forced so the next submissions
///     attack that slot directly;
///   * on-chain nonce **equal** → genuine blocker: a same-nonce, fee-bumped
///     replacement is queued via `replace_tx`. Price kinds are replaced with
///     the newest same-kind payload (batched over the stuck tail);
///     Enable/Disable are re-sent with their own payload. There is no
///     replacement cap — fees keep escalating until the nonce lands.
pub async fn run_tx_processor(
	rx: mpsc::Receiver<UpdateTx>,
	resync_tx: mpsc::Sender<ResyncKind>,
	replace_tx: mpsc::Sender<ReplaceRequest>,
	nonce_tx_timeout: std::time::Duration,
	priority_multiplier: Arc<PriorityFeeMultiplier>,
	wallet_address: Address,
) {
	let rpc_url = std::env::var("RPC_URL").expect("RPC_URL not set");
	let provider = Arc::new(
		ProviderBuilder::new()
			.on_http(Url::parse(&rpc_url).expect("Invalid RPC_URL"))
			.boxed(),
	);

	let tracked: Arc<Mutex<TrackedState>> = Arc::new(Mutex::new(TrackedState::new()));
	let mut rx = rx;
	let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(1));
	let mut last_resync_at: Option<std::time::Instant> = None;
	let mut gap_detector = GapDetector::new(GAP_CONFIRMATION_WINDOW);

	// Anchor the consumed floor at the boot-time on-chain nonce
	for attempt in 1..=3 {
		match provider.get_transaction_count(wallet_address).await {
			Ok(latest) => {
				tracked.lock().await.consumed_floor = latest;
				info!("[Watchdog] consumed-nonce floor anchored at boot-time on-chain nonce {}", latest);
				break;
			},
			Err(e) => {
				warn!(
					"[Watchdog] failed to fetch boot-time on-chain nonce (attempt {}/3): {} — stale-read guard weakened until the first receipt is observed",
					attempt, e
				);
				if attempt < 3 {
					tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				}
			},
		}
	}

	loop {
		tokio::select! {
			Some(tx) = rx.recv() => {
				let kind = tx.kind;
				let replaceable = is_replaceable(kind);
				let mut state = tracked.lock().await;
				let replacement_count = state.txs
					.iter()
					.rev()
					.find(|t| t.nonce == tx.nonce)
					.map(|t| t.replacement_count)
					.unwrap_or(0);
				// A new tx at an already-tracked nonce is a replacement (or a
				// post-resync re-submission). Only NOW that it is tracked do we
				// retire the previous entry — marking earlier (at watchdog-queue
				// time) could orphan the nonce if the replacement submit failed
				// or its tracking message was dropped.
				let superseded = supersede_at_nonce(&mut state.txs, tx.nonce);
				if superseded > 0 {
					info!("[{}] Superseded {} tracked tx(s) at nonce {}", kind, superseded, tx.nonce);
				}

				if let Some(waiter) = tx.resolution_tx {
					if state.waiters.insert(tx.nonce, waiter).is_some() {
						warn!("[{}] Replaced an existing resolution waiter at nonce {}", kind, tx.nonce);
					}
				}

				let tracked_entry = TrackedTx {
					tx_hash: tx.tx_hash,
					nonce: tx.nonce,
					timestamp: std::time::Instant::now(),
					state: TxState::Unconfirmed,
					kind,
					replaceable,
					max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
					max_fee_per_gas: tx.max_fee_per_gas,
					replacement_count,
					last_replacement_attempt_at: None,
					replacement_payload: tx.replacement_payload.clone(),
				};
				state.txs.push(tracked_entry);

				if state.txs.len() > MAX_TRACKED_TXS {
					let excess = state.txs.len() - MAX_TRACKED_TXS;
					error!(
						"[Watchdog] tracked tx list exceeded {} entries — chain appears stuck for a very long time; dropping {} oldest entries",
						MAX_TRACKED_TXS, excess
					);
					state.txs.drain(0..excess);
				}

				info!(
					"[{}] Tracking new tx: {:?} nonce={} (replacement_count={}, total tracked: {})",
					kind, tx.tx_hash, tx.nonce, replacement_count, state.txs.len()
				);
				drop(state);

				let provider = Arc::clone(&provider);
				let tracked = Arc::clone(&tracked);
				tokio::spawn(run_confirm_poller(provider, tracked, kind, tx.tx_hash, tx.nonce));
			},
			_ = check_interval.tick() => {
				let (hit, floor) = {
					let state = tracked.lock().await;
					(
						check_watchdog_timeout(&state.txs, state.consumed_floor, nonce_tx_timeout),
						state.consumed_floor,
					)
				};
				let Some(hit) = hit else { continue };

				let can_send = last_resync_at
					.map(|t| t.elapsed() >= RESYNC_BACKOFF)
					.unwrap_or(true);
				if !can_send {
					continue;
				}

				let proceed_to_replace = match provider.get_transaction_count(wallet_address).await {
					Ok(latest_nonce) if latest_nonce > hit.nonce => {
						gap_detector.clear();
						warn!(
							"[Watchdog] nonces below {} were consumed on-chain but no receipt observed yet for {:?}; resolving tracked entries up to nonce {}",
							latest_nonce, hit.tx_hash, latest_nonce - 1
						);
						let pruned = {
							let mut state = tracked.lock().await;
							resolve_up_to(
								&mut state,
								latest_nonce - 1,
								TxResolution::Confirmed,
								"consumed on-chain without observed receipt",
							)
						};
						if pruned > 0 {
							alerts::send_slack_alert(format!(
								"[Watchdog] resolved {} tracked tx(s) below on-chain nonce {} without receipts — treated as confirmed; please verify on-chain state",
								pruned, latest_nonce
							)).await;
						}
						false
					},
					Ok(latest_nonce) if latest_nonce < hit.nonce => {
						if latest_nonce < floor {
							// Provably stale read: nonces below the floor were
							// already seen consumed (boot anchor or observed
							// receipts). Ignore the probe and attack the hit as
							// usual — if the node really is behind and the tx
							// already mined, the replacement bounces off as
							// "nonce too low", which is treated as success.
							warn!(
								"[Watchdog] on-chain nonce probe returned {} which is below the known-consumed floor {} — stale RPC read, ignoring",
								latest_nonce, floor
							);
							true
						} else if gap_detector.observe(hit.nonce, std::time::Instant::now()) {
							warn!(
								"[Watchdog] on-chain nonce {} persistently below the lowest tracked tx (nonce {}): an untracked tx (gap from a failed submit, or leftover from a previous run) blocks the queue. Rewinding the nonce so the next submissions attack that slot.",
								latest_nonce, hit.nonce
							);
							send_resync(
								&resync_tx,
								ResyncKind::Rewind,
								&priority_multiplier,
								&mut last_resync_at,
							);
							false
						} else {
							info!(
								"[Watchdog] on-chain nonce {} below lowest tracked tx (nonce {}) — suspected untracked blocker, waiting {:?} for confirmation before resyncing",
								latest_nonce, hit.nonce, GAP_CONFIRMATION_WINDOW
							);
							false
						}
					},
					Ok(_) => {
						// latest == hit.nonce: hit is the genuine blocker.
						gap_detector.clear();
						true
					},
					Err(e) => {
						warn!(
							"[Watchdog] failed to probe on-chain nonce ({}); proceeding with replacement — a wrong guess is harmless ('nonce too low' is treated as success downstream)",
							e
						);
						true
					},
				};
				if !proceed_to_replace {
					continue;
				}

				let req = {
					let state = tracked.lock().await;
					build_replace_request(&state.txs, &hit)
				};
				let Some(req) = req else {
					// No eligible replacement target (typically the hit
					// resolved between the scan and the build). Nothing suggests
					// an untracked blocker — advance-only re-anchor, don't
					// disturb the live queue.
					send_resync(
						&resync_tx,
						ResyncKind::Reanchor,
						&priority_multiplier,
						&mut last_resync_at,
					);
					continue;
				};
				let targets = req.targets.iter().map(|t| (t.nonce, t.old_tx_hash)).collect::<Vec<_>>();
				priority_multiplier.bump_up();
				match replace_tx.try_send(req) {
					Ok(()) => {
						warn!(
							"[Watchdog] {} tx at nonce {} timed out; queued {} same-kind replacement(s) (attempt #{} for this nonce): {:?}",
							hit.kind, hit.nonce, targets.len(), hit.replacement_count + 1, hit.tx_hash,
						);
						let now = std::time::Instant::now();
						let mut state = tracked.lock().await;
						for (nonce, hash) in &targets {
							if let Some(entry) = state.txs.iter_mut().find(|t| t.tx_hash == *hash && t.nonce == *nonce) {
								if entry.state == TxState::Unconfirmed {
									// Deliberately NOT Superseded here: the entry
									// stays live until its replacement is tracked
									// (recv arm). If the replacement never makes
									// it, this timer re-arms and the watchdog
									// fires again instead of orphaning the nonce.
									entry.last_replacement_attempt_at = Some(now);
									entry.replacement_count = entry.replacement_count.saturating_add(1);
								}
							}
						}
					},
					Err(e) => {
						let reason = match &e {
							mpsc::error::TrySendError::Full(_) => "replace channel full",
							mpsc::error::TrySendError::Closed(_) => "replace channel closed",
						};
						warn!(
							"[Watchdog] {} — falling back to resync for {:?}",
							reason, hit.tx_hash
						);
						send_resync_without_bump(
							&resync_tx,
							ResyncKind::Reanchor,
							&mut last_resync_at,
						);
					},
				}
			},
		}
	}
}

/// Sends a resync signal of the given kind and bumps the priority multiplier.
/// The stuck entry is intentionally left live so the watchdog re-arms against
/// it.
fn send_resync(
	resync_tx: &mpsc::Sender<ResyncKind>,
	kind: ResyncKind,
	priority_multiplier: &Arc<PriorityFeeMultiplier>,
	last_resync_at: &mut Option<std::time::Instant>,
) {
	priority_multiplier.bump_up();
	send_resync_without_bump(resync_tx, kind, last_resync_at);
}

fn send_resync_without_bump(
	resync_tx: &mpsc::Sender<ResyncKind>,
	kind: ResyncKind,
	last_resync_at: &mut Option<std::time::Instant>,
) {
	if let Err(e) = resync_tx.try_send(kind) {
		match e {
			mpsc::error::TrySendError::Full(_) => {
				warn!("[Watchdog] Resync channel full — signal dropped");
			},
			mpsc::error::TrySendError::Closed(_) => {
				error!("[Watchdog] Resync channel closed");
			},
		}
	}
	*last_resync_at = Some(std::time::Instant::now());
}

/// Per-tx receipt poller. One task per tracked hash — simple to reason about,
/// and bounded: it exits as soon as its receipt arrives, its nonce is
/// resolved under another hash (it lost a replacement race), or its entry was
/// pruned. Transient RPC errors are retried, not fatal.
async fn run_confirm_poller<P, T>(
	provider: Arc<P>,
	tracked: Arc<Mutex<TrackedState>>,
	kind: UpdateTxKind,
	tx_hash: B256,
	nonce: u64,
) where
	P: Provider<T> + ?Sized,
	T: Transport + Clone,
{
	let mut rpc_error_streak: u32 = 0;
	let mut rpc_alert_sent = false;
	loop {
		match provider.get_transaction_receipt(tx_hash).await {
			Ok(Some(receipt)) => {
				let reverted = !receipt.status();
				{
					let mut state = tracked.lock().await;
					resolve_up_to(
						&mut state,
						nonce,
						if reverted { TxResolution::Reverted } else { TxResolution::Confirmed },
						"receipt observed",
					);
				}
				if reverted {
					on_tx_reverted(kind, &receipt).await;
				} else {
					info!(
						"[{}] transaction confirmed: tx_hash={:?}, block={:?}",
						kind, receipt.transaction_hash, receipt.block_number,
					);
				}
				return;
			},
			Ok(None) => {
				rpc_error_streak = 0;
				if entry_gone(&tracked, tx_hash).await {
					// Nonce resolved under another hash, or entry pruned — this
					// hash will never get a receipt. Stop polling.
					return;
				}
				tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
			},
			Err(e) => {
				rpc_error_streak += 1;
				error!(
					"[Watchdog] RPC error confirming tx {:?} (streak {}): {:?} — retrying",
					tx_hash, rpc_error_streak, e
				);
				if rpc_error_streak >= RPC_ERROR_ALERT_THRESHOLD && !rpc_alert_sent {
					rpc_alert_sent = true;
					on_tx_error(kind, Box::new(e)).await;
				}
				if entry_gone(&tracked, tx_hash).await {
					return;
				}
				tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
			},
		}
	}
}

async fn entry_gone(tracked: &Arc<Mutex<TrackedState>>, tx_hash: B256) -> bool {
	!tracked.lock().await.txs.iter().any(|t| t.tx_hash == tx_hash)
}

/// Polls for a transaction receipt until it is mined, reverts, or the RPC errors.
/// Slack alerts are emitted for revert / RPC failure cases.
///
/// Used by callers outside the tracking loop (fallback confirmation when the
/// tracker channel is unavailable). Inside the tracker, `run_confirm_poller`
/// is used instead — it additionally exits when the nonce resolves under a
/// different hash.
pub async fn confirm_tx<P, T>(provider: Arc<P>, kind: UpdateTxKind, tx_hash: B256) -> ConfirmOutcome
where
	P: Provider<T> + ?Sized,
	T: Transport + Clone,
{
	loop {
		match provider.get_transaction_receipt(tx_hash).await {
			Ok(Some(receipt)) => {
				if receipt.status() {
					info!(
						"[{}] transaction confirmed: tx_hash={:?}, block={:?}",
						kind, receipt.transaction_hash, receipt.block_number,
					);
					return ConfirmOutcome::Confirmed;
				} else {
					on_tx_reverted(kind, &receipt).await;
					return ConfirmOutcome::Reverted;
				}
			},
			Ok(None) => {
				tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
			},
			Err(e) => {
				on_tx_error(kind, Box::new(e)).await;
				return ConfirmOutcome::RpcError;
			},
		}
	}
}

async fn on_tx_reverted(kind: UpdateTxKind, receipt: &TransactionReceipt) {
	let message = format!(
		"[{}] transaction REVERTED on-chain: tx_hash={:?}, block={:?}",
		kind, receipt.transaction_hash, receipt.block_number,
	);
	error!("{}", message);

	alerts::send_slack_alert(message).await;
}

async fn on_tx_error(kind: UpdateTxKind, err: Box<dyn Error + Send + Sync + 'static>) {
	let message = format!("[{}] failed to confirm transaction: {:?}", kind, err);
	error!("{}", message);

	alerts::send_slack_alert(message).await;
}

#[cfg(test)]
mod tests {
	use super::*;

	const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

	fn make_tx(state: TxState, age: std::time::Duration) -> TrackedTx {
		TrackedTx {
			tx_hash: B256::repeat_byte(0x01),
			nonce: 0,
			timestamp: std::time::Instant::now() - age,
			state,
			kind: UpdateTxKind::DarkOracle,
			replaceable: true,
			max_priority_fee_per_gas: 1_000,
			max_fee_per_gas: 10_000,
			replacement_count: 0,
			last_replacement_attempt_at: None,
			replacement_payload: Arc::new(TransactionRequest::default()),
		}
	}

	fn make_tx_with_nonce(nonce: u64, state: TxState, age: std::time::Duration) -> TrackedTx {
		let mut tx = make_tx(state, age);
		tx.nonce = nonce;
		tx
	}

	#[test]
	fn no_signal_when_unconfirmed_not_timed_out() {
		let tracked =
			vec![make_tx_with_nonce(1, TxState::Unconfirmed, std::time::Duration::from_secs(2))];
		assert!(check_watchdog_timeout(&tracked, 1, TIMEOUT).is_none());
	}

	#[test]
	fn signal_when_lowest_unresolved_nonce_timed_out() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(20)),
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(15)),
		];
		let hit = check_watchdog_timeout(&tracked, 5, TIMEOUT).expect("hit");
		assert_eq!(hit.nonce, 5);
	}

	#[test]
	fn no_signal_for_unconfirmed_nonce_below_consumed_floor() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(20)),
		];
		assert!(check_watchdog_timeout(&tracked, 8, TIMEOUT).is_none());
	}

	#[test]
	fn no_signal_for_higher_nonce_while_lower_replacement_is_fresh() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::Superseded, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(2)),
		];
		assert!(check_watchdog_timeout(&tracked, 0, TIMEOUT).is_none());
	}

	#[test]
	fn signal_for_lower_replacement_before_higher_nonce() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::Superseded, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
		];
		let hit = check_watchdog_timeout(&tracked, 0, TIMEOUT).expect("hit");
		assert_eq!(hit.nonce, 5);
	}

	#[test]
	fn recent_replacement_attempt_rearms_the_timer() {
		// The entry itself is old, but a replacement was queued recently — the
		// watchdog must wait a full timeout for that replacement to be tracked
		// before firing again (prevents replacement spam and gives the
		// recv-arm supersede a chance to happen).
		let mut entry =
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(60));
		entry.last_replacement_attempt_at =
			Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
		let tracked = vec![entry];
		assert!(check_watchdog_timeout(&tracked, 0, TIMEOUT).is_none());

		// Once the attempt is also older than the timeout, the watchdog fires
		// again — a lost replacement can never orphan the nonce.
		let mut entry =
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(60));
		entry.last_replacement_attempt_at =
			Some(std::time::Instant::now() - std::time::Duration::from_secs(30));
		let tracked = vec![entry];
		assert!(check_watchdog_timeout(&tracked, 0, TIMEOUT).is_some());
	}

	#[test]
	fn watchdog_hit_carries_kind_and_nonce() {
		let mut entry = make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(30));
		entry.nonce = 42;
		entry.kind = UpdateTxKind::Pyth;
		entry.replaceable = true;
		let tracked = vec![entry];
		let hit = check_watchdog_timeout(&tracked, 0, TIMEOUT).expect("hit");
		assert_eq!(hit.nonce, 42);
		assert_eq!(hit.kind, UpdateTxKind::Pyth);
		assert!(hit.replaceable);
		assert_eq!(hit.replacement_count, 0);
	}

	#[test]
	fn is_replaceable_only_true_for_dark_oracle_and_pyth() {
		assert!(is_replaceable(UpdateTxKind::DarkOracle));
		assert!(is_replaceable(UpdateTxKind::Pyth));
		assert!(!is_replaceable(UpdateTxKind::EnableAsset));
		assert!(!is_replaceable(UpdateTxKind::DisableAsset));
	}

	#[test]
	fn supersede_marks_only_live_entries_at_nonce() {
		let mut txs = vec![
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(10)),
			make_tx_with_nonce(8, TxState::Unconfirmed, std::time::Duration::from_secs(10)),
			make_tx_with_nonce(6, TxState::Superseded, std::time::Duration::from_secs(40)),
		];
		let marked = supersede_at_nonce(&mut txs, 7);
		assert_eq!(marked, 2);
		assert!(txs.iter().filter(|t| t.nonce == 7).all(|t| t.state == TxState::Superseded));
		assert_eq!(txs.iter().find(|t| t.nonce == 8).unwrap().state, TxState::Unconfirmed);
	}

	#[test]
	fn replacement_count_inherits_from_prior_entry_at_same_nonce() {
		// The lookup runs BEFORE the new entry is pushed, so the tracked
		// vec here represents the pre-insert state.
		let mut prior = make_tx(TxState::Superseded, std::time::Duration::from_secs(60));
		prior.nonce = 7;
		prior.replacement_count = 3;
		let tracked = vec![prior];
		let inherited = tracked
			.iter()
			.rev()
			.find(|t| t.nonce == 7)
			.map(|t| t.replacement_count)
			.unwrap_or(0);
		assert_eq!(inherited, 3);
	}

	#[test]
	fn build_replace_request_batches_latest_unconfirmed_target_per_nonce() {
		let source_payload = Arc::new(TransactionRequest::default());
		let mut stale =
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30));
		stale.tx_hash = B256::repeat_byte(0x05);
		stale.max_priority_fee_per_gas = 5;
		stale.max_fee_per_gas = 50;

		let mut superseded_same_nonce =
			make_tx_with_nonce(6, TxState::Superseded, std::time::Duration::from_secs(20));
		superseded_same_nonce.tx_hash = B256::repeat_byte(0x61);

		let mut latest_same_nonce =
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(10));
		latest_same_nonce.tx_hash = B256::repeat_byte(0x62);
		latest_same_nonce.max_priority_fee_per_gas = 6;
		latest_same_nonce.max_fee_per_gas = 60;

		let mut other_kind =
			make_tx_with_nonce(8, TxState::Unconfirmed, std::time::Duration::from_secs(10));
		other_kind.tx_hash = B256::repeat_byte(0x08);
		other_kind.kind = UpdateTxKind::Pyth;

		let mut source =
			make_tx_with_nonce(9, TxState::Unconfirmed, std::time::Duration::from_secs(1));
		source.tx_hash = B256::repeat_byte(0x09);
		source.replacement_payload = source_payload.clone();

		let tracked = vec![stale, superseded_same_nonce, latest_same_nonce, other_kind, source];
		let hit = check_watchdog_timeout(&tracked, 0, TIMEOUT).expect("hit");
		let req = build_replace_request(&tracked, &hit).expect("replace request");

		assert_eq!(req.kind, UpdateTxKind::DarkOracle);
		assert_eq!(req.targets.len(), 3);
		assert_eq!(req.targets[0].nonce, 5);
		assert_eq!(req.targets[0].old_tx_hash, B256::repeat_byte(0x05));
		assert_eq!(req.targets[1].nonce, 6);
		assert_eq!(req.targets[1].old_tx_hash, B256::repeat_byte(0x62));
		assert_eq!(req.targets[1].old_max_priority_fee_per_gas, 6);
		assert_eq!(req.targets[1].old_max_fee_per_gas, 60);
		assert_eq!(req.targets[2].nonce, 9);
		assert!(req.targets.iter().all(|t| Arc::ptr_eq(&t.payload, &source_payload)));
	}

	#[test]
	fn build_replace_request_uses_highest_nonce_payload_for_same_kind() {
		let low_payload = Arc::new(TransactionRequest::default());
		let high_payload = Arc::new(TransactionRequest::default());
		let other_kind_payload = Arc::new(TransactionRequest::default());

		let mut stale =
			make_tx_with_nonce(3, TxState::Unconfirmed, std::time::Duration::from_secs(30));
		stale.replacement_payload = low_payload;

		let mut high =
			make_tx_with_nonce(8, TxState::Unconfirmed, std::time::Duration::from_secs(1));
		high.replacement_payload = high_payload.clone();

		let mut higher_other_kind =
			make_tx_with_nonce(9, TxState::Unconfirmed, std::time::Duration::from_secs(1));
		higher_other_kind.kind = UpdateTxKind::Pyth;
		higher_other_kind.replacement_payload = other_kind_payload;

		let tracked = vec![stale, high, higher_other_kind];
		let hit = check_watchdog_timeout(&tracked, 0, TIMEOUT).expect("hit");
		let req = build_replace_request(&tracked, &hit).expect("replace request");

		assert!(req.targets.iter().all(|t| Arc::ptr_eq(&t.payload, &high_payload)));
		assert!(req.targets.iter().all(|target| target.nonce != 9));
	}

	#[test]
	fn build_replace_request_critical_kind_resends_own_payload_single_target() {
		let own_payload = Arc::new(TransactionRequest::default());
		let newer_payload = Arc::new(TransactionRequest::default());

		let mut disable =
			make_tx_with_nonce(42, TxState::Unconfirmed, std::time::Duration::from_secs(30));
		disable.kind = UpdateTxKind::DisableAsset;
		disable.replaceable = false;
		disable.tx_hash = B256::repeat_byte(0x42);
		disable.replacement_payload = own_payload.clone();

		// A newer price tx above must NOT be batched into a critical replacement.
		let mut price =
			make_tx_with_nonce(43, TxState::Unconfirmed, std::time::Duration::from_secs(1));
		price.replacement_payload = newer_payload;

		let tracked = vec![disable, price];
		let hit = check_watchdog_timeout(&tracked, 0, TIMEOUT).expect("hit");
		assert!(!hit.replaceable);
		let req = build_replace_request(&tracked, &hit).expect("replace request");

		assert_eq!(req.kind, UpdateTxKind::DisableAsset);
		assert_eq!(req.targets.len(), 1);
		assert_eq!(req.targets[0].nonce, 42);
		assert_eq!(req.targets[0].old_tx_hash, B256::repeat_byte(0x42));
		assert!(Arc::ptr_eq(&req.targets[0].payload, &own_payload));
	}

	#[test]
	fn resolve_up_to_fires_waiters_prunes_and_advances_floor() {
		let mut state = TrackedState::new();
		state.txs = vec![
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(6, TxState::Superseded, std::time::Duration::from_secs(20)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(10)),
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(5)),
		];
		let (tx5, mut rx5) = oneshot::channel();
		let (tx6, mut rx6) = oneshot::channel();
		let (tx7, mut rx7) = oneshot::channel();
		state.waiters.insert(5, tx5);
		state.waiters.insert(6, tx6);
		state.waiters.insert(7, tx7);

		let pruned = resolve_up_to(&mut state, 6, TxResolution::Reverted, "test");

		// Nonces 5 and 6 (both entries) pruned; 7 remains.
		assert_eq!(pruned, 3);
		assert_eq!(state.txs.len(), 1);
		assert_eq!(state.txs[0].nonce, 7);
		assert_eq!(state.consumed_floor, 7);

		// Exact nonce gets the real resolution; lower nonce is swept as
		// Confirmed (consumed by nonce order); higher waiter is untouched.
		assert_eq!(rx5.try_recv().unwrap(), TxResolution::Confirmed);
		assert_eq!(rx6.try_recv().unwrap(), TxResolution::Reverted);
		assert!(rx7.try_recv().is_err()); // still pending
		assert!(state.waiters.contains_key(&7));
	}

	#[test]
	fn resolve_up_to_is_monotonic_on_floor() {
		let mut state = TrackedState::new();
		state.consumed_floor = 10;
		let pruned = resolve_up_to(&mut state, 4, TxResolution::Confirmed, "test");
		assert_eq!(pruned, 0);
		assert_eq!(state.consumed_floor, 10);
	}

	#[test]
	fn gap_detector_only_fires_after_persistence_window() {
		let window = std::time::Duration::from_secs(5);
		let mut detector = GapDetector::new(window);
		let t0 = std::time::Instant::now();

		// First observation only records the suspicion.
		assert!(!detector.observe(100, t0));
		// Still inside the window — a lagging replica read must not resync.
		assert!(!detector.observe(100, t0 + std::time::Duration::from_secs(2)));
		// Persisted for the full window against the same nonce → act.
		assert!(detector.observe(100, t0 + std::time::Duration::from_secs(5)));
		// Firing resets the detector — the next observation starts over.
		assert!(!detector.observe(100, t0 + std::time::Duration::from_secs(6)));
	}

	#[test]
	fn gap_detector_resets_when_hit_nonce_changes() {
		let window = std::time::Duration::from_secs(5);
		let mut detector = GapDetector::new(window);
		let t0 = std::time::Instant::now();

		assert!(!detector.observe(100, t0));
		// The blocked nonce changed (previous suspicion resolved itself) —
		// the window restarts even though plenty of time has passed.
		assert!(!detector.observe(101, t0 + std::time::Duration::from_secs(10)));
		assert!(detector.observe(101, t0 + std::time::Duration::from_secs(15)));
	}

	#[test]
	fn gap_detector_clear_discards_suspicion() {
		let window = std::time::Duration::from_secs(5);
		let mut detector = GapDetector::new(window);
		let t0 = std::time::Instant::now();

		assert!(!detector.observe(100, t0));
		// A probe at/past the hit nonce clears the suspicion (transient lag).
		detector.clear();
		assert!(!detector.observe(100, t0 + std::time::Duration::from_secs(10)));
	}
}

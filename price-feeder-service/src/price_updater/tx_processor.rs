use alloy::{
	primitives::B256,
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
use tokio::sync::{mpsc, Mutex};

use crate::price_updater::alerts;
use crate::price_updater::chain::PriorityFeeMultiplier;

const MAX_TRACKED_TXS: usize = 100;
const RESYNC_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// Maximum number of same-nonce replacement attempts the watchdog will issue
/// for a single stuck nonce before falling back to nonce-resync
/// behaviour.
const REPLACEMENT_CAP_PER_NONCE: u8 = 2;

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

/// `true` for kinds that the watchdog is allowed to replace. Currently
/// DarkOracle and Pyth price updates. `EnableAsset` and `DisableAsset` are
/// load-bearing state-change txs that bypass the update_tx channel entirely
/// (see `mod.rs` startup and disable paths) — they MUST never reach the
/// replacement handler.
pub fn is_replaceable(kind: UpdateTxKind) -> bool {
	matches!(kind, UpdateTxKind::DarkOracle | UpdateTxKind::Pyth)
}

#[derive(Debug)]
pub enum ConfirmOutcome {
	Confirmed,
	Reverted,
	RpcError,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxState {
	Confirmed,
	Unconfirmed,
	Reverted,
	/// Watchdog already fired a resync (or a replacement) for this tx and is
	/// now skipping it so it doesn't keep re-arming against the same stuck
	/// hash.
	TimedOut,
}

#[derive(Debug, Clone)]
pub struct ReplaceRequest {
	pub kind: UpdateTxKind,
	pub replacement_payload: Arc<TransactionRequest>,
	pub targets: Vec<ReplacementTarget>,
}

#[derive(Debug, Clone)]
pub struct ReplacementTarget {
	pub nonce: u64,
	pub old_tx_hash: B256,
	pub old_max_priority_fee_per_gas: u128,
	pub old_max_fee_per_gas: u128,
}

/// Subset of `TrackedTx` returned by the watchdog scan, so the watcher loop
/// can branch on kind/replaceability without holding the `tracked` lock
/// across an await.
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
	/// replacement builder can floor its bump at 1.125× these.
	max_priority_fee_per_gas: u128,
	max_fee_per_gas: u128,
	/// Number of same-nonce replacements already issued for this tracked tx.
	/// Bumped each time the watchdog queues a replacement. Once it reaches
	/// `REPLACEMENT_CAP_PER_NONCE` the watchdog stops replacing.
	replacement_count: u8,
	replacement_payload: Arc<TransactionRequest>,
}

/// Returns `Some(WatchdogHit)` when the lowest unresolved nonce has exceeded
/// `nonce_tx_timeout`. EVM accounts execute strictly in nonce order, so a
/// higher timed-out nonce is not actionable while a lower nonce is still
/// pending. Tx hashes already marked `TimedOut` are skipped so the watchdog
/// can't re-arm against the same stuck hash indefinitely.
fn check_watchdog_timeout(
	tracked: &[TrackedTx],
	nonce_tx_timeout: std::time::Duration,
) -> Option<WatchdogHit> {
	let now = std::time::Instant::now();
	let consumed_nonce_floor = tracked
		.iter()
		.filter(|t| t.state == TxState::Confirmed || t.state == TxState::Reverted)
		.map(|t| t.nonce.saturating_add(1))
		.max()
		.unwrap_or(0);

	let candidate = tracked
		.iter()
		.filter(|t| t.state == TxState::Unconfirmed && t.nonce >= consumed_nonce_floor)
		.min_by_key(|t| (t.nonce, t.timestamp));

	let candidate = candidate?;
	if now.duration_since(candidate.timestamp) < nonce_tx_timeout {
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
	let source = tracked
		.iter()
		.filter(|t| t.state == TxState::Unconfirmed && t.kind == hit.kind)
		.fold(None, |best: Option<&TrackedTx>, tx| match best {
			Some(best) if best.nonce > tx.nonce => Some(best),
			Some(best) if best.nonce == tx.nonce && best.timestamp >= tx.timestamp => Some(best),
			_ => Some(tx),
		})?;

	let mut targets_by_nonce = BTreeMap::new();
	for tx in tracked.iter().filter(|t| {
		t.state == TxState::Unconfirmed
			&& t.kind == hit.kind
			&& t.replaceable
			&& t.nonce >= hit.nonce
			&& t.replacement_count < REPLACEMENT_CAP_PER_NONCE
	}) {
		targets_by_nonce.insert(
			tx.nonce,
			ReplacementTarget {
				nonce: tx.nonce,
				old_tx_hash: tx.tx_hash,
				old_max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
				old_max_fee_per_gas: tx.max_fee_per_gas,
			},
		);
	}

	let targets = targets_by_nonce.into_values().collect::<Vec<_>>();
	if targets.is_empty() {
		return None;
	}

	Some(ReplaceRequest {
		kind: hit.kind,
		replacement_payload: source.replacement_payload.clone(),
		targets,
	})
}

pub struct UpdateTx {
	pub kind: UpdateTxKind,
	pub tx_hash: B256,
	pub nonce: u64,
	pub max_priority_fee_per_gas: u128,
	pub max_fee_per_gas: u128,
	pub replacement_payload: Arc<TransactionRequest>,
}

/// Receives `UpdateTx` messages, handles confirmation, and tracks tx state
/// for watchdog-style timeout detection. When a timeout is detected for an
/// unconfirmed tx in the contiguous tail (after the newest Confirmed/Reverted):
///   * for **replaceable** price-update kinds (DarkOracle, Pyth) with a
///     remaining replacement budget, a `ReplaceRequest` is sent to the feed
///     loop via `replace_tx` and the stuck entry is marked `TimedOut` *without*
///     triggering a nonce resync (the replacement reuses the existing nonce);
///   * for **non-replaceable** kinds (or once the per-nonce replacement cap is
///     hit), a resync signal is sent via `resync_tx` and the priority-fee
///     multiplier is bumped.
pub async fn run_tx_processor(
	rx: mpsc::Receiver<UpdateTx>,
	resync_tx: mpsc::Sender<()>,
	replace_tx: mpsc::Sender<ReplaceRequest>,
	nonce_tx_timeout: std::time::Duration,
	priority_multiplier: Arc<PriorityFeeMultiplier>,
) {
	let rpc_url = std::env::var("RPC_URL").expect("RPC_URL not set");
	let provider = Arc::new(
		ProviderBuilder::new()
			.on_http(Url::parse(&rpc_url).expect("Invalid RPC_URL"))
			.boxed(),
	);

	let tracked: Arc<Mutex<Vec<TrackedTx>>> = Arc::new(Mutex::new(Vec::new()));
	let mut rx = rx;
	let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(1));
	let mut last_resync_at: Option<std::time::Instant> = None;

	loop {
		tokio::select! {
			Some(tx) = rx.recv() => {
				let kind = tx.kind;
				let replaceable = is_replaceable(kind);
				let mut tracked_guard = tracked.lock().await;
				// For replaceable kinds, inherit `replacement_count` from the
				// most recent tracked entry at the same nonce (typically the
				// prior, now-TimedOut tx at this nonce). This caps the
				// *total* replacement attempts per nonce across the whole
				// process lifetime at `REPLACEMENT_CAP_PER_NONCE`, not just
				// per tracked hash. Without this, each new replacement would
				// start at 0 and a stuck nonce could churn forever.
				let replacement_count = if replaceable {
					tracked_guard
						.iter()
						.rev()
						.find(|t| t.nonce == tx.nonce)
						.map(|t| t.replacement_count)
						.unwrap_or(0)
				} else {
					0
				};
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
					replacement_payload: tx.replacement_payload.clone(),
				};
				tracked_guard.push(tracked_entry);

				if tracked_guard.len() > MAX_TRACKED_TXS {
					let excess = tracked_guard.len() - MAX_TRACKED_TXS;
					tracked_guard.drain(0..excess);
				}

				info!(
					"[{}] Tracking new tx: {:?} nonce={} (replacement_count={}, total tracked: {})",
					kind, tx.tx_hash, tx.nonce, replacement_count, tracked_guard.len()
				);
				drop(tracked_guard);

				let provider = Arc::clone(&provider);
				let tracked = Arc::clone(&tracked);
				let tx_hash = tx.tx_hash;
				tokio::spawn(async move {
					let outcome = confirm_tx(provider, kind, tx_hash).await;
					let mut tracked = tracked.lock().await;
					if let Some(entry) = tracked.iter_mut().find(|t| t.tx_hash == tx_hash) {
						match outcome {
							ConfirmOutcome::Confirmed => {
								entry.state = TxState::Confirmed;
								info!("[Watchdog] tx confirmed: {:?}", tx_hash);
							},
							ConfirmOutcome::Reverted => {
								entry.state = TxState::Reverted;
								warn!("[Watchdog] tx reverted: {:?}", tx_hash);
							},
							ConfirmOutcome::RpcError => {
								error!("[Watchdog] RPC error confirming tx: {:?}", tx_hash);
							},
						}
					}
				});
			},
			_ = check_interval.tick() => {
				let hit = {
					let tracked = tracked.lock().await;
					check_watchdog_timeout(&tracked, nonce_tx_timeout)
				};
				if let Some(hit) = hit {
					let can_send = last_resync_at
						.map(|t| t.elapsed() >= RESYNC_BACKOFF)
						.unwrap_or(true);

					if !can_send {
						continue;
					}

					// Branch: replaceable kind with budget remaining →
					// queue a same-nonce replacement and DO NOT send the
					// resync signal. Otherwise, fall back to the original
					// resync+bump behaviour (unchanged).
					if hit.replaceable && hit.replacement_count < REPLACEMENT_CAP_PER_NONCE {
						let req = {
							let tracked = tracked.lock().await;
							build_replace_request(&tracked, &hit)
						};
						let Some(req) = req else {
							let mut tracked_guard = tracked.lock().await;
							send_resync(
								&resync_tx,
								&priority_multiplier,
								&mut last_resync_at,
								hit.tx_hash,
								&mut tracked_guard,
							);
							continue;
						};
						let targets = req.targets.clone();
						priority_multiplier.bump_up();
						let send_result = replace_tx.try_send(req);
						let mut tracked_guard = tracked.lock().await;
						match send_result {
							Ok(()) => {
								warn!(
									"[Watchdog] {} tx at nonce {} timed out; queued {} same-kind replacements (attempt {}/{}): {:?}",
									hit.kind, hit.nonce, targets.len(), hit.replacement_count + 1, REPLACEMENT_CAP_PER_NONCE, hit.tx_hash,
								);
								for target in &targets {
									if let Some(entry) = tracked_guard.iter_mut().find(|t| t.tx_hash == target.old_tx_hash) {
										if entry.state == TxState::Unconfirmed {
											entry.state = TxState::TimedOut;
											entry.replacement_count =
												entry.replacement_count.saturating_add(1);
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
									&mut last_resync_at,
									hit.tx_hash,
									&mut tracked_guard,
								);
							},
						}
					} else {
						let reason = if !hit.replaceable {
							"non-replaceable"
						} else {
							"replacement cap exhausted"
						};
						warn!(
							"[Watchdog] {} tx timeout ({}, kind={}): {:?}. Triggering nonce resync.",
							reason, hit.replaceable, hit.kind, hit.tx_hash
						);
						let mut tracked_guard = tracked.lock().await;
						send_resync(
							&resync_tx,
							&priority_multiplier,
							&mut last_resync_at,
							hit.tx_hash,
							&mut tracked_guard,
						);
					}
				}
			},
		}
	}
}

/// Sends a resync signal, bumps the priority multiplier, and marks the stuck
/// tx as `TimedOut`. Helper extracted so the replace-fallback paths can reuse
/// it without duplicating the resync logic.
fn send_resync(
	resync_tx: &mpsc::Sender<()>,
	priority_multiplier: &Arc<PriorityFeeMultiplier>,
	last_resync_at: &mut Option<std::time::Instant>,
	tx_hash: B256,
	tracked: &mut Vec<TrackedTx>,
) {
	priority_multiplier.bump_up();
	send_resync_without_bump(resync_tx, last_resync_at, tx_hash, tracked);
}

fn send_resync_without_bump(
	resync_tx: &mpsc::Sender<()>,
	last_resync_at: &mut Option<std::time::Instant>,
	tx_hash: B256,
	tracked: &mut Vec<TrackedTx>,
) {
	if let Err(e) = resync_tx.try_send(()) {
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

	if let Some(entry) = tracked.iter_mut().find(|t| t.tx_hash == tx_hash) {
		if entry.state == TxState::Unconfirmed {
			entry.state = TxState::TimedOut;
		}
	}
}

/// Polls for a transaction receipt until it is mined, reverts, or the RPC errors.
/// Slack alerts are emitted for revert / RPC failure cases.
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
				tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
		let tracked = vec![
			make_tx_with_nonce(0, TxState::Confirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(1, TxState::Unconfirmed, std::time::Duration::from_secs(2)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_none());
	}

	#[test]
	fn signal_when_lowest_unresolved_nonce_timed_out() {
		let tracked = vec![
			make_tx_with_nonce(0, TxState::Confirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(1, TxState::Confirmed, std::time::Duration::from_secs(55)),
			make_tx_with_nonce(2, TxState::Confirmed, std::time::Duration::from_secs(50)),
			make_tx_with_nonce(3, TxState::Confirmed, std::time::Duration::from_secs(45)),
			make_tx_with_nonce(4, TxState::Confirmed, std::time::Duration::from_secs(40)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(20)),
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(15)),
		];
		let hit = check_watchdog_timeout(&tracked, TIMEOUT).expect("hit");
		assert_eq!(hit.nonce, 5);
	}

	#[test]
	fn no_signal_for_unconfirmed_nonce_below_confirmed_floor() {
		let tracked = vec![
			make_tx_with_nonce(0, TxState::Confirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(1, TxState::Confirmed, std::time::Duration::from_secs(55)),
			make_tx_with_nonce(2, TxState::Confirmed, std::time::Duration::from_secs(50)),
			make_tx_with_nonce(3, TxState::Confirmed, std::time::Duration::from_secs(45)),
			make_tx_with_nonce(4, TxState::Confirmed, std::time::Duration::from_secs(40)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(20)),
			make_tx_with_nonce(7, TxState::Confirmed, std::time::Duration::from_secs(5)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_none());
	}

	#[test]
	fn no_signal_for_higher_nonce_while_lower_replacement_is_fresh() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::TimedOut, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(2)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_none());
	}

	#[test]
	fn signal_for_lower_replacement_before_higher_nonce() {
		let tracked = vec![
			make_tx_with_nonce(5, TxState::TimedOut, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(60)),
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30)),
		];
		let hit = check_watchdog_timeout(&tracked, TIMEOUT).expect("hit");
		assert_eq!(hit.nonce, 5);
	}

	#[test]
	fn watchdog_hit_carries_kind_and_nonce() {
		let mut entry = make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(30));
		entry.nonce = 42;
		entry.kind = UpdateTxKind::Pyth;
		entry.replaceable = true;
		let tracked = vec![entry];
		let hit = check_watchdog_timeout(&tracked, TIMEOUT).expect("hit");
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
	fn replacement_count_inherits_from_prior_entry_at_same_nonce() {
		// When a replacement tx is added at nonce N, its replacement_count
		// should be inherited from the most recent tracked entry at nonce N
		// (typically the now-TimedOut prior tx at that nonce). This caps
		// the total replacement attempts per nonce, not per hash.
		//
		// The lookup runs BEFORE the new entry is pushed, so the tracked
		// vec here represents the pre-insert state.
		let mut prior = make_tx(TxState::TimedOut, std::time::Duration::from_secs(60));
		prior.nonce = 7;
		prior.replacement_count = 1; // 1 replacement already issued
		let tracked = vec![prior];
		let inherited = tracked
			.iter()
			.rev()
			.find(|t| t.nonce == 7)
			.map(|t| t.replacement_count)
			.unwrap_or(0);
		assert_eq!(inherited, 1);
	}

	#[test]
	fn replacement_count_zero_for_fresh_nonce() {
		// No prior entry at the nonce → count starts at 0 (normal submit).
		let entry = make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(0));
		let tracked = vec![entry];
		let inherited = tracked
			.iter()
			.rev()
			.find(|t| t.nonce == 99)
			.map(|t| t.replacement_count)
			.unwrap_or(0);
		assert_eq!(inherited, 0);
	}

	#[test]
	fn build_replace_request_batches_latest_unconfirmed_target_per_nonce() {
		let source_payload = Arc::new(TransactionRequest::default());
		let mut stale =
			make_tx_with_nonce(5, TxState::Unconfirmed, std::time::Duration::from_secs(30));
		stale.tx_hash = B256::repeat_byte(0x05);
		stale.max_priority_fee_per_gas = 5;
		stale.max_fee_per_gas = 50;

		let mut older_same_nonce =
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(20));
		older_same_nonce.tx_hash = B256::repeat_byte(0x61);

		let mut latest_same_nonce =
			make_tx_with_nonce(6, TxState::Unconfirmed, std::time::Duration::from_secs(10));
		latest_same_nonce.tx_hash = B256::repeat_byte(0x62);
		latest_same_nonce.max_priority_fee_per_gas = 6;
		latest_same_nonce.max_fee_per_gas = 60;

		let mut capped =
			make_tx_with_nonce(7, TxState::Unconfirmed, std::time::Duration::from_secs(10));
		capped.tx_hash = B256::repeat_byte(0x07);
		capped.replacement_count = REPLACEMENT_CAP_PER_NONCE;

		let mut other_kind =
			make_tx_with_nonce(8, TxState::Unconfirmed, std::time::Duration::from_secs(10));
		other_kind.tx_hash = B256::repeat_byte(0x08);
		other_kind.kind = UpdateTxKind::Pyth;

		let mut source =
			make_tx_with_nonce(9, TxState::Unconfirmed, std::time::Duration::from_secs(1));
		source.tx_hash = B256::repeat_byte(0x09);
		source.replacement_payload = source_payload.clone();

		let tracked = vec![stale, older_same_nonce, latest_same_nonce, capped, other_kind, source];
		let hit = check_watchdog_timeout(&tracked, TIMEOUT).expect("hit");
		let req = build_replace_request(&tracked, &hit).expect("replace request");

		assert_eq!(req.kind, UpdateTxKind::DarkOracle);
		assert!(Arc::ptr_eq(&req.replacement_payload, &source_payload));
		assert_eq!(req.targets.len(), 3);
		assert_eq!(req.targets[0].nonce, 5);
		assert_eq!(req.targets[0].old_tx_hash, B256::repeat_byte(0x05));
		assert_eq!(req.targets[1].nonce, 6);
		assert_eq!(req.targets[1].old_tx_hash, B256::repeat_byte(0x62));
		assert_eq!(req.targets[1].old_max_priority_fee_per_gas, 6);
		assert_eq!(req.targets[1].old_max_fee_per_gas, 60);
		assert_eq!(req.targets[2].nonce, 9);
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
		let hit = check_watchdog_timeout(&tracked, TIMEOUT).expect("hit");
		let req = build_replace_request(&tracked, &hit).expect("replace request");

		assert!(Arc::ptr_eq(&req.replacement_payload, &high_payload));
		assert!(req.targets.iter().all(|target| target.nonce != 9));
	}
}

use alloy::{
	primitives::B256,
	providers::{Provider, ProviderBuilder},
	rpc::types::TransactionReceipt,
	transports::Transport,
};
use log::{error, info, warn};
use reqwest::Url;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::price_updater::alerts;
use crate::price_updater::chain::PriorityFeeMultiplier;

const MAX_TRACKED_TXS: usize = 100;
const RESYNC_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
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
	/// Watchdog already fired a resync for this tx and is now skipping it so
	/// it doesn't keep re-arming against the same stuck hash.
	TimedOut,
}

#[derive(Debug)]
struct TrackedTx {
	tx_hash: B256,
	timestamp: std::time::Instant,
	state: TxState,
}

/// Scans `tracked` from newest to oldest. Returns `Some(tx_hash)` when any
/// unconfirmed tx in the contiguous tail (after the newest Confirmed/Reverted)
/// has exceeded `nonce_tx_timeout`, or `None` otherwise. Tx hashes already
/// marked `TimedOut` are skipped so the watchdog can't re-arm against the
/// same stuck hash indefinitely.
fn check_watchdog_timeout(
	tracked: &[TrackedTx],
	nonce_tx_timeout: std::time::Duration,
) -> Option<B256> {
	let now = std::time::Instant::now();
	for i in (0..tracked.len()).rev() {
		let state = tracked[i].state;
		if state == TxState::Confirmed || state == TxState::Reverted {
			break;
		}
		if state == TxState::TimedOut {
			continue;
		}
		let elapsed = now.duration_since(tracked[i].timestamp);

		if elapsed >= nonce_tx_timeout {
			return Some(tracked[i].tx_hash);
		}
	}
	None
}

pub struct UpdateTx {
	pub kind: UpdateTxKind,
	pub tx_hash: B256,
}

/// Receives `UpdateTx` messages, handles confirmation, and tracks tx state
/// for watchdog-style timeout detection. When a timeout is detected for an
/// unconfirmed tx in the contiguous tail (after the newest Confirmed/Reverted),
/// a resync signal is sent via `resync_tx`.
pub async fn run_tx_processor(
	rx: mpsc::Receiver<UpdateTx>,
	resync_tx: mpsc::Sender<()>,
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
				let tx_hash = tx.tx_hash;
				let kind = tx.kind;
				{
					let mut tracked = tracked.lock().await;
					tracked.push(TrackedTx {
						tx_hash,
						timestamp: std::time::Instant::now(),
						state: TxState::Unconfirmed,
					});

					if tracked.len() > MAX_TRACKED_TXS {
						let excess = tracked.len() - MAX_TRACKED_TXS;
						tracked.drain(0..excess);
					}

					info!(
						"[{}] Tracking new tx: {:?} (total tracked: {})",
						kind, tx_hash, tracked.len()
					);
				}

				let provider = Arc::clone(&provider);
				let tracked = Arc::clone(&tracked);
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
				let timeout_tx = {
					let tracked = tracked.lock().await;
					check_watchdog_timeout(&tracked, nonce_tx_timeout)
				};
				if let Some(tx_hash) = timeout_tx {
					let can_send = last_resync_at
						.map(|t| t.elapsed() >= RESYNC_BACKOFF)
						.unwrap_or(true);

					if can_send {
						warn!(
							"[Watchdog] Transaction timeout detected: {:?}. Triggering nonce resync.",
							tx_hash
						);

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
						last_resync_at = Some(std::time::Instant::now());
						priority_multiplier.bump_up();

						// Mark this tx so the watchdog doesn't keep re-arming
						// against the same stuck hash every tick.
						let mut tracked = tracked.lock().await;
						if let Some(entry) =
							tracked.iter_mut().find(|t| t.tx_hash == tx_hash)
						{
							if entry.state == TxState::Unconfirmed {
								entry.state = TxState::TimedOut;
							}
						}
					}
				}
			},
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
			timestamp: std::time::Instant::now() - age,
			state,
		}
	}

	#[test]
	fn no_signal_when_unconfirmed_not_timed_out() {
		let tracked = vec![
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(30)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(2)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_none());
	}

	#[test]
	fn signal_when_newest_unconfirmed_timed_out() {
		let tracked = vec![
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(60)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(55)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(50)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(45)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(40)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(20)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(15)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_some());
	}

	#[test]
	fn no_signal_when_confirmed_blocks_scan() {
		let tracked = vec![
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(60)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(55)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(50)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(45)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(40)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(30)),
			make_tx(TxState::Unconfirmed, std::time::Duration::from_secs(20)),
			make_tx(TxState::Confirmed, std::time::Duration::from_secs(5)),
		];
		assert!(check_watchdog_timeout(&tracked, TIMEOUT).is_none());
	}
}

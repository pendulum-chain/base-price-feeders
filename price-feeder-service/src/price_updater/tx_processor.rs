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
use tokio::sync::mpsc;

use crate::price_updater::alerts;

const MAX_TRACKED_TXS: usize = 100;

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
}

struct TrackedTx {
	tx_hash: B256,
	timestamp: std::time::Instant,
	state: TxState,
}

enum ProcessorMsg {
	NewTx { kind: UpdateTxKind, tx_hash: B256 },
	TxOutcome { tx_hash: B256, outcome: ConfirmOutcome },
}

pub struct UpdateTx {
	pub kind: UpdateTxKind,
	pub tx_hash: B256,
}

/// Receives `UpdateTx` messages, handles confirmation, and tracks tx state
/// for watchdog-style timeout detection. When a timeout is detected on the
/// oldest unconfirmed tx (with no confirmed/reverted tx ahead of it),
/// a resync signal is sent via `resync_tx`.
pub async fn run_tx_processor(
	rx: mpsc::Receiver<UpdateTx>,
	resync_tx: mpsc::Sender<()>,
	nonce_tx_timeout: std::time::Duration,
) {
	let rpc_url = std::env::var("RPC_URL").expect("RPC_URL not set");
	let provider = Arc::new(
		ProviderBuilder::new()
			.on_http(Url::parse(&rpc_url).expect("Invalid RPC_URL"))
			.boxed(),
	);

	let (internal_tx, mut internal_rx) = mpsc::channel::<ProcessorMsg>(200);

	// Forward incoming UpdateTx into internal channel
	let mut rx = rx;
	let forwarder_tx = internal_tx.clone();
	tokio::spawn(async move {
		while let Some(tx) = rx.recv().await {
			if forwarder_tx.send(ProcessorMsg::NewTx { kind: tx.kind, tx_hash: tx.tx_hash }).await.is_err() {
				break;
			}
		}
	});

	let mut tracked: Vec<TrackedTx> = Vec::new();
	let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(1));

	loop {
		tokio::select! {
			Some(msg) = internal_rx.recv() => {
				match msg {
					ProcessorMsg::NewTx { kind, tx_hash } => {
						tracked.push(TrackedTx {
							tx_hash,
							timestamp: std::time::Instant::now(),
							state: TxState::Unconfirmed,
						});

						if tracked.len() > MAX_TRACKED_TXS {
							tracked.drain(0..tracked.len() - MAX_TRACKED_TXS);
						}

						info!(
							"[{}] Tracking new tx: {:?} (total tracked: {})",
							kind, tx_hash, tracked.len()
						);

						let provider = Arc::clone(&provider);
						let outcome_tx = internal_tx.clone();
						tokio::spawn(async move {
							let outcome = confirm_tx(provider, kind, tx_hash).await;
							let _ = outcome_tx.send(ProcessorMsg::TxOutcome { tx_hash, outcome }).await;
						});
					},
					ProcessorMsg::TxOutcome { tx_hash, outcome } => {
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
									// Leave as Unconfirmed so watchdog can detect timeout
									error!("[Watchdog] RPC error confirming tx: {:?}", tx_hash);
								},
							}
						}
					},
				}
			},
			_ = check_interval.tick() => {
				let now = std::time::Instant::now();

				// Scan from newest (highest index) backwards.
				// Stop as soon as we hit a settled tx (no older tx can
				// trigger resync) or the newest unconfirmed tx.
				for i in (0..tracked.len()).rev() {
					if tracked[i].state == TxState::Confirmed || tracked[i].state == TxState::Reverted {
						break;
					}
					// tracked[i] is Unconfirmed
					let elapsed = now.duration_since(tracked[i].timestamp);
					if elapsed >= nonce_tx_timeout {
						warn!(
							"[Watchdog] Transaction timeout detected: {:?} (elapsed: {:?}). Triggering nonce resync.",
							tracked[i].tx_hash, elapsed
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
					}
					break;
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

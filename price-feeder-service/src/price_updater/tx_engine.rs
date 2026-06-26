use alloy::{
	primitives::{Address, B256},
	rpc::types::TransactionRequest,
};
use log::{error, info, warn};
use std::error::Error;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::price_updater::alerts;
use crate::price_updater::chain::{ChainProvider, PriorityFeeMultiplier};
use crate::price_updater::tx_processor::UpdateTxKind;

const CONFIRMATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_SUBMIT_RETRIES: u32 = 5;
const SUBMIT_RETRY_DELAY_MS: u64 = 250;

struct ActiveTx {
	nonce: u64,
	hash: B256,
	kind: UpdateTxKind,
	submitted_at: std::time::Instant,
	priority_fee: u128,
	max_fee: u128,
	replacement_count: u32,
}

impl ActiveTx {
	fn is_replaceable(&self) -> bool {
		matches!(self.kind, UpdateTxKind::DarkOracle | UpdateTxKind::Pyth)
	}
}

struct TxEngineInner {
	next_nonce: u64,
	active: Option<ActiveTx>,
}

pub struct TxEngine {
	provider: Arc<ChainProvider>,
	address: Address,
	inner: Arc<Mutex<TxEngineInner>>,
	priority_multiplier: Arc<PriorityFeeMultiplier>,
}

impl TxEngine {
	pub async fn new(
		provider: Arc<ChainProvider>,
		address: Address,
		priority_multiplier: Arc<PriorityFeeMultiplier>,
	) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let initial_nonce =
			alloy::providers::Provider::get_transaction_count(&*provider, address).await?;
		info!("[TxEngine] Initial nonce: {}", initial_nonce);
		Ok(Self {
			provider,
			address,
			inner: Arc::new(Mutex::new(TxEngineInner { next_nonce: initial_nonce, active: None })),
			priority_multiplier,
		})
	}

	pub fn spawn_confirmation_loop(self: &Arc<Self>) {
		let engine = Arc::clone(self);
		tokio::spawn(async move {
			info!("[TxEngine] Starting confirmation loop");
			loop {
				tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
				engine.check_active_tx().await;
			}
		});
	}

	pub async fn submit(
		&self,
		mut tx_req: TransactionRequest,
		kind: UpdateTxKind,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		let mut inner = self.inner.lock().await;

		for attempt in 0..MAX_SUBMIT_RETRIES {
			let (nonce, is_replacement, old_priority, old_max_fee, old_replacement_count) = {
				match &inner.active {
					// TODO is the right move to essentially not track anymore non-replaceable txs, should they be delayed in confirmation?
					Some(active) if active.is_replaceable() => (
						active.nonce,
						true,
						active.priority_fee,
						active.max_fee,
						active.replacement_count,
					),
					_ => (inner.next_nonce, false, 0u128, 0u128, 0u32),
				}
			};

			let (priority_fee, max_fee, replacement_count) = if is_replacement {
				let (pf, mf) = self.compute_replacement_fees(old_priority, old_max_fee).await?;
				(pf, mf, old_replacement_count + 1)
			} else {
				let (pf, mf) = self.estimate_fees().await?;
				(pf, mf, 0)
			};

			tx_req.nonce = Some(nonce);
			tx_req.max_priority_fee_per_gas = Some(priority_fee);
			tx_req.max_fee_per_gas = Some(max_fee);

			if is_replacement {
				let old_hash = inner.active.as_ref().map(|a| a.hash);
				warn!(
					"[TxEngine] Replacing active tx: nonce={} old_hash={:?} kind={} replacement={} priority_fee={} wei",
					nonce, old_hash, kind, replacement_count, priority_fee
				);
			}

			match alloy::providers::Provider::send_transaction(&*self.provider, tx_req.clone())
				.await
			{
				Ok(pending_tx) => {
					let hash = *pending_tx.tx_hash();
					let now = std::time::Instant::now();

					if is_replacement {
						info!(
							"[TxEngine] Replacement submitted: nonce={} hash={:?} kind={} priority_fee={} wei",
							nonce, hash, kind, priority_fee
						);
					} else {
						info!(
							"[TxEngine] New tx submitted: nonce={} hash={:?} kind={} priority_fee={} wei",
							nonce, hash, kind, priority_fee
						);
						inner.next_nonce = nonce + 1;
					}

					inner.active = Some(ActiveTx {
						nonce,
						hash,
						kind,
						submitted_at: now,
						priority_fee,
						max_fee,
						replacement_count,
					});

					return Ok(hash);
				},
				Err(e) => {
					let err_msg = e.to_string();
					if err_msg.contains("nonce too low") && attempt < MAX_SUBMIT_RETRIES - 1 {
						warn!("[TxEngine] nonce too low (attempt {}). Syncing...", attempt + 1);
						let chain_nonce = alloy::providers::Provider::get_transaction_count(
							&*self.provider,
							self.address,
						)
						.pending()
						.await?;

						if let Some(active) = &inner.active {
							if active.nonce < chain_nonce {
								info!(
									"[TxEngine] Active tx nonce {} already consumed on-chain. Clearing.",
									active.nonce
								);
								inner.active = None;
							}
						}

						if chain_nonce > inner.next_nonce {
							inner.next_nonce = chain_nonce;
						}
						continue;
					} else if attempt < MAX_SUBMIT_RETRIES - 1 {
						warn!(
							"[TxEngine] Tx error: {}. Retrying {}/{}...",
							err_msg,
							attempt + 1,
							MAX_SUBMIT_RETRIES
						);
						tokio::time::sleep(std::time::Duration::from_millis(SUBMIT_RETRY_DELAY_MS))
							.await;
						continue;
					}
					return Err(e.into());
				},
			}
		}

		Err("max submit retries exceeded".into())
	}

	async fn estimate_fees(&self) -> Result<(u128, u128), Box<dyn Error + Send + Sync + 'static>> {
		self.priority_multiplier.try_bump_down();
		self.estimate_fees_raw().await
	}

	async fn estimate_fees_raw(
		&self,
	) -> Result<(u128, u128), Box<dyn Error + Send + Sync + 'static>> {
		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let multiplier = self.priority_multiplier.get() as f64;
		let priority_fee = (fees.max_priority_fee_per_gas as f64 * multiplier) as u128;
		let max_fee = (fees.max_fee_per_gas as f64 * multiplier) as u128;
		Ok((priority_fee, max_fee))
	}

	async fn compute_replacement_fees(
		&self,
		old_priority: u128,
		old_max_fee: u128,
	) -> Result<(u128, u128), Box<dyn Error + Send + Sync + 'static>> {
		self.priority_multiplier.bump_up();
		let (est_priority, est_max_fee) = self.estimate_fees_raw().await?;

		let min_priority = old_priority + old_priority / 8;
		let min_max_fee = old_max_fee + old_max_fee / 8;

		Ok((est_priority.max(min_priority), est_max_fee.max(min_max_fee)))
	}

	async fn check_active_tx(&self) {
		let (hash, nonce, kind) = {
			let inner = self.inner.lock().await;
			match &inner.active {
				Some(active) => (active.hash, active.nonce, active.kind),
				None => return,
			}
		};

		match alloy::providers::Provider::get_transaction_receipt(&*self.provider, hash).await {
			Ok(Some(receipt)) => {
				let confirmed = receipt.status();
				let block_number = receipt.block_number;

				{
					let mut inner = self.inner.lock().await;
					if let Some(active) = &inner.active {
						if active.nonce != nonce {
							return;
						}
					} else {
						return;
					}

					inner.active = None;
					if inner.next_nonce <= nonce {
						inner.next_nonce = nonce + 1;
					}
				}

				if confirmed {
					info!(
						"[TxEngine] Confirmed: nonce={} hash={:?} block={:?}",
						nonce, hash, block_number
					);
				} else {
					error!(
						"[TxEngine] Reverted: nonce={} hash={:?} block={:?}",
						nonce, hash, block_number
					);
					let message = format!(
						"[{}] transaction REVERTED on-chain: tx_hash={:?}, block={:?}",
						kind, hash, block_number
					);
					alerts::send_slack_alert(message).await;
				}
			},
			Ok(None) => {
				let chain_nonce = match alloy::providers::Provider::get_transaction_count(
					&*self.provider,
					self.address,
				)
				.latest()
				.await
				{
					Ok(n) => n,
					Err(e) => {
						warn!("[TxEngine] RPC error checking on-chain nonce: {:?}", e);
						return;
					},
				};

				let mut inner = self.inner.lock().await;
				if let Some(active) = &inner.active {
					if active.nonce == nonce && active.nonce < chain_nonce {
						info!(
							"[TxEngine] Nonce {} consumed on-chain (hash {:?} replaced). Advancing to {}.",
							nonce, hash, chain_nonce
						);
						inner.active = None;
						if inner.next_nonce < chain_nonce {
							inner.next_nonce = chain_nonce;
						}
					}
				}
			},
			Err(e) => {
				warn!(
					"[TxEngine] RPC error checking receipt for nonce={} hash={:?}: {:?}",
					nonce, hash, e
				);
			},
		}
	}
}

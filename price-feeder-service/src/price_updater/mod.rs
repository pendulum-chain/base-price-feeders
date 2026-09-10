// Retained for a possible future reference-price alert, but not started by the service.
#[allow(dead_code)]
pub mod alerts;
pub mod chain;
pub mod configs;
pub mod dark_oracle;
pub mod helpers;
// Retained for compatibility and tests, but deliberately not wired into either runtime loop.
#[allow(dead_code)]
pub mod pyth;
pub mod tx_processor;

pub use chain::ChainClient;
pub use dark_oracle::DarkOracleUpdater;
pub use tx_processor::UpdateTx;

use crate::api::PriceApi;
use crate::storage::{CoinInfoStorage, TimeframeStatus};
use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use alloy::primitives::B256;
use configs::HierarchyEntry;
use futures::stream::{FuturesUnordered, StreamExt};
use helpers::convert_to_coin_info;
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tx_processor::{ConfirmOutcome, UpdateTx as Tx, UpdateTxKind as TxKind};

pub use configs::ProviderHierarchy;

// Number of consecutive hierarchy-exhausted occurrences before we send the
// disable tx on-chain. Anything below this threshold is considered transient.
const DISABLE_FAILURE_THRESHOLD: u8 = 3;

// Backoff between disable-tx resubmissions while we wait for an on-chain
const DISABLE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

// The longer this value, the "oldest" the price feed at the time of feeding it. But if a fetch cycle
// ends up taking longer, we missed the cycle and the feed process uses prices from previous iteration.
pub const FETCH_LEAD_TIME: std::time::Duration = std::time::Duration::from_millis(700);

// Maximum number of retries for startup enable tx before giving up
const STARTUP_ENABLE_MAX_RETRIES: u8 = 3;

// Backoff between startup enable-tx resubmissions
const STARTUP_ENABLE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

// Tracks the state of problematic assets.
// Note: There is no "Enabled" state because once an asset is successfully
// enabled on-chain, it is removed from the tracking map entirely.
#[derive(Debug, Clone, PartialEq)]
enum AssetStatus {
	// Hierarchy exhausted but we haven't yet hit DISABLE_FAILURE_THRESHOLD.
	// The counter holds the number of consecutive failures so far.
	Failing(u8),
	Disabled(dark_oracle::AssetMetadata),
	Enabling(dark_oracle::AssetMetadata),
}

fn should_recover_asset_on_startup(asset_is_registered: bool) -> bool {
	!asset_is_registered
}

async fn reconcile_asset_registration_on_startup(
	asset_symbol: &str,
	disabled_assets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, AssetStatus>>>,
	startup_reconciled_assets: Arc<tokio::sync::Mutex<HashSet<String>>>,
	dark_oracle_updater: &DarkOracleUpdater,
	hierarchy: &ProviderHierarchy,
) {
	if !hierarchy.disable_on_exhaustion.get(asset_symbol).cloned().unwrap_or(false) {
		return;
	}

	{
		let reconciled_guard = startup_reconciled_assets.lock().await;
		if reconciled_guard.contains(asset_symbol) {
			return;
		}
	}

	let meta = match configs::get_configured_registration_metadata(asset_symbol) {
		Some(m) => m,
		None => {
			warn!(
				"Skipping startup registration reconciliation for {} because ASSET_REGISTRATION_METADATA has no entry",
				asset_symbol
			);
			startup_reconciled_assets.lock().await.insert(asset_symbol.to_string());
			return;
		},
	};

	{
		let disabled_guard = disabled_assets.lock().await;
		if disabled_guard.contains_key(asset_symbol) {
			return;
		}
	}

	let is_registered = match dark_oracle_updater.is_asset_registered(asset_symbol).await {
		Ok(is_registered) => is_registered,
		Err(e) => {
			error!("Failed to check registration status for {}: {:?}", asset_symbol, e);
			return;
		},
	};

	if !should_recover_asset_on_startup(is_registered) {
		startup_reconciled_assets.lock().await.insert(asset_symbol.to_string());
		return;
	}

	info!(
		"Recovered feed for untracked disabled asset {}, sending startup enable tx",
		asset_symbol
	);

	for attempt in 1..=STARTUP_ENABLE_MAX_RETRIES {
		let tx_hash = match dark_oracle_updater.enable_asset(asset_symbol, &meta).await {
			Ok(tx_hash) => tx_hash,
			Err(e) => {
				error!(
					"Failed to enable {} during startup reconciliation (attempt {}/{}): {:?}",
					asset_symbol, attempt, STARTUP_ENABLE_MAX_RETRIES, e
				);
				if attempt < STARTUP_ENABLE_MAX_RETRIES {
					tokio::time::sleep(STARTUP_ENABLE_RETRY_BACKOFF).await;
					continue;
				}
				error!(
					"Giving up startup enable for {} after {} attempts",
					asset_symbol, STARTUP_ENABLE_MAX_RETRIES
				);
				startup_reconciled_assets.lock().await.insert(asset_symbol.to_string());
				return;
			},
		};

		let provider = dark_oracle_updater.provider();
		match tx_processor::confirm_tx(provider, TxKind::EnableAsset, tx_hash).await {
			ConfirmOutcome::Confirmed => {
				startup_reconciled_assets.lock().await.insert(asset_symbol.to_string());
				info!("Successfully enabled {} during startup reconciliation", asset_symbol);
				return;
			},
			ConfirmOutcome::Reverted => {
				warn!(
					"Startup enable tx for {} reverted (attempt {}/{})",
					asset_symbol, attempt, STARTUP_ENABLE_MAX_RETRIES
				);
			},
			ConfirmOutcome::RpcError => {
				warn!(
					"RPC error confirming startup enable tx for {} (attempt {}/{})",
					asset_symbol, attempt, STARTUP_ENABLE_MAX_RETRIES
				);
			},
		}

		if attempt < STARTUP_ENABLE_MAX_RETRIES {
			tokio::time::sleep(STARTUP_ENABLE_RETRY_BACKOFF).await;
		}
	}

	error!(
		"Giving up startup enable for {} after {} attempts, marking as reconciled to avoid spam",
		asset_symbol, STARTUP_ENABLE_MAX_RETRIES
	);
	startup_reconciled_assets.lock().await.insert(asset_symbol.to_string());
}

async fn handle_asset_recovery(
	asset_symbol: &str,
	disabled_assets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, AssetStatus>>>,
	dark_oracle_updater: DarkOracleUpdater,
) {
	let mut disabled_guard = disabled_assets.lock().await;
	match disabled_guard.get(asset_symbol) {
		Some(AssetStatus::Failing(_)) => {
			// Recover from transient failure.
			info!("Feed for {} recovered before disable threshold, clearing counter", asset_symbol);
			disabled_guard.remove(asset_symbol);
		},
		Some(AssetStatus::Disabled(meta)) => {
			info!("Recovered feed for {}, sending enable tx", asset_symbol);
			let meta_clone = meta.clone();
			disabled_guard
				.insert(asset_symbol.to_string(), AssetStatus::Enabling(meta_clone.clone()));

			let symbol_clone = asset_symbol.to_string();
			let updater_clone = dark_oracle_updater.clone();
			let disabled_assets_clone = disabled_assets.clone();

			tokio::spawn(async move {
				if let Err(e) = updater_clone.enable_asset(&symbol_clone, &meta_clone).await {
					error!("Failed to enable {}: {:?}", symbol_clone, e);
				} else {
					let mut guard = disabled_assets_clone.lock().await;
					// Only remove if the status hasn't been flipped back to Disabled in the meantime
					if let Some(AssetStatus::Enabling(_)) = guard.get(&symbol_clone) {
						guard.remove(&symbol_clone);
						info!("Successfully enabled {}", symbol_clone);
					}
				}
			});
		},
		_ => {},
	}
}

async fn handle_asset_exhausted(
	asset: &AssetSpecifier,
	asset_hierarchy: Vec<&HierarchyEntry>,
	storage: &CoinInfoStorage,
	disabled_assets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, AssetStatus>>>,
	dark_oracle_updater: &DarkOracleUpdater,
	currencies_to_feed: &mut Vec<CoinInfo>,
	missing_data: &mut bool,
	hierarchy: &ProviderHierarchy,
) {
	let asset_symbol = asset.symbol.as_str();
	// Look up the most recent (but stale) price across the hierarchy.
	let last_price = asset_hierarchy.into_iter().find_map(|entry| {
		storage.get_timeframe_any(asset_symbol, &asset.blockchain, entry.aggregator.clone())
	});

	let should_send_disable = {
		if !hierarchy.disable_on_exhaustion.get(asset_symbol).cloned().unwrap_or(false) {
			false
		} else {
			let mut disabled_guard = disabled_assets.lock().await;
			match disabled_guard.get(asset_symbol) {
				Some(AssetStatus::Disabled(_)) => false,
				// Mid-flight enable: the feed has dropped again, so we need to
				// send a fresh disable tx
				Some(AssetStatus::Enabling(_)) => true,
				Some(AssetStatus::Failing(count)) => {
					let next = count.saturating_add(1);
					if next >= DISABLE_FAILURE_THRESHOLD {
						true
					} else {
						info!(
							"Hierarchy exhausted for {} ({}/{}), deferring disable",
							asset_symbol, next, DISABLE_FAILURE_THRESHOLD
						);
						disabled_guard.insert(asset_symbol.to_string(), AssetStatus::Failing(next));
						false
					}
				},
				// First failure for this asset.
				None => {
					if DISABLE_FAILURE_THRESHOLD <= 1 {
						true
					} else {
						info!(
							"Hierarchy exhausted for {} (1/{}), deferring disable",
							asset_symbol, DISABLE_FAILURE_THRESHOLD
						);
						disabled_guard.insert(asset_symbol.to_string(), AssetStatus::Failing(1));
						false
					}
				},
			}
		}
	};

	if should_send_disable {
		info!("Hierarchy exhausted for {}, sending disable tx", asset_symbol);

		// Block the feed loop until the disable tx is confirmed on-chain!!
		let provider = dark_oracle_updater.provider();
		let meta = loop {
			let (tx_hash, meta) = match dark_oracle_updater.disable_asset(asset_symbol).await {
				Ok(ok) => ok,
				Err(e) => {
					error!("Failed to submit disable tx for {}: {:?}, retrying", asset_symbol, e);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
					continue;
				},
			};

			match tx_processor::confirm_tx(provider.clone(), TxKind::DisableAsset, tx_hash).await {
				ConfirmOutcome::Confirmed => break meta,
				ConfirmOutcome::Reverted => {
					warn!("Disable tx for {} reverted on-chain, resubmitting", asset_symbol);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
				},
				ConfirmOutcome::RpcError => {
					warn!("RPC error confirming disable tx for {}, resubmitting", asset_symbol);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
				},
			}
		};

		let mut disabled_guard = disabled_assets.lock().await;
		disabled_guard.insert(asset_symbol.to_string(), AssetStatus::Disabled(meta));
		if let Some(last_tf) = last_price {
			currencies_to_feed.push(last_tf);
		} else {
			error!("No last price available for token: {}", asset_symbol);
			*missing_data = true;
		}
	} else {
		// Either already disabled, or still under the failure threshold:
		// reuse the last known price if we have one.
		if let Some(last_tf) = last_price {
			currencies_to_feed.push(last_tf);
		} else {
			error!("No last price available for token: {}", asset_symbol);
			*missing_data = true;
		}
	}
}

// ── Public entry point ─────────────────────────────────────────────────────────

/// The fetch loop runs **on demand**: it sleeps until either the feed loop signals
//       via `fetch_trigger` (the normal case, scheduled
///      to fire `FETCH_LEAD_TIME` before each feed tick), or
///
/// Each trigger starts at most one request per provider required by the active
/// hierarchy. Provider failures are retried on the next scheduled trigger, so
/// one failing provider cannot amplify traffic to healthy providers.
pub async fn run_fetch_loop<T>(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	update_interval: std::time::Duration,
	fetch_trigger: Arc<Notify>,
	api: T,
	hierarchy: ProviderHierarchy,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
	T: PriceApi + Send + Sync + 'static,
{
	let mut had_error =
		run_single_fetch(&storage, &supported_currencies, update_interval, &api, &hierarchy).await;
	if had_error {
		warn!("Fetch loop encountered a provider error; waiting for the next scheduled fetch");
	}

	loop {
		fetch_trigger.notified().await;
		had_error =
			run_single_fetch(&storage, &supported_currencies, update_interval, &api, &hierarchy)
				.await;
		if had_error {
			warn!("Fetch loop encountered a provider error; waiting for the next scheduled fetch");
		}
	}
}

async fn run_single_fetch<T>(
	storage: &Arc<CoinInfoStorage>,
	supported_currencies: &HashSet<AssetSpecifier>,
	update_interval: std::time::Duration,
	api: &T,
	hierarchy: &ProviderHierarchy,
) -> bool
where
	T: PriceApi + Send + Sync + 'static,
{
	let start = tokio::time::Instant::now();

	let assets_by_provider =
		hierarchy.get_assets_by_provider(supported_currencies, chrono::Utc::now());

	let quotations_future = async {
		let mut futures = FuturesUnordered::new();
		for future in api.get_quotation_futures(assets_by_provider) {
			futures.push(future);
		}

		let mut had_error = false;
		while let Some(outcome) = futures.next().await {
			for q in outcome.quotations {
				match convert_to_coin_info(q.clone()) {
					Ok(ci) => {
						storage.update_timeframe(ci);
					},
					Err(e) => error!("Error converting to CoinInfo: {:#?}", e),
				}
			}
			if outcome.had_error {
				had_error = true;
			}
			debug!("{} quotations fetch completed", outcome.provider);
		}

		had_error
	};

	let had_error = match tokio::time::timeout(update_interval, quotations_future).await {
		Ok(had_error) => had_error,
		Err(_) => {
			error!(
				"Timed out waiting for all API provider fetches after {:?}; completed provider results were already stored",
				update_interval
			);
			true
		},
	};
	debug!("run_single_fetch completed in {:?} (had_error: {})", start.elapsed(), had_error);

	had_error
}

pub async fn run_feed_loop(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	dark_oracle_updater: DarkOracleUpdater,
	update_tx: mpsc::Sender<UpdateTx>,
	fetch_trigger: Arc<Notify>,
	hierarchy: ProviderHierarchy,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
	info!("Starting feed loop");

	let disabled_assets =
		Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<String, AssetStatus>::new()));
	let startup_reconciled_assets = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
	let update_interval = dark_oracle_updater.get_update_interval();

	loop {
		let feed_start = tokio::time::Instant::now();

		let next_tick = feed_start + update_interval;

		// Schedule the fetch trigger to fire just before the *next* feed
		// tick.
		schedule_fetch_trigger(fetch_trigger.clone(), next_tick, FETCH_LEAD_TIME);

		let mut currencies_to_feed = vec![];
		let mut missing_data = false;

		let now = chrono::Utc::now();

		for asset in &supported_currencies {
			let asset_hierarchy = hierarchy.get_hierarchy(&asset.symbol, now);

			let mut selected_tf = None;
			for entry in &asset_hierarchy {
				let aggregator = &entry.aggregator;
				match storage.get_timeframe_status(
					&asset.symbol,
					&asset.blockchain,
					aggregator.clone(),
				) {
					TimeframeStatus::Fresh(tf) => {
						selected_tf = Some(tf);
						break;
					},
					TimeframeStatus::Stale { age_ms, max_age_ms, .. } => {
						warn!(
							"{} entry for {} is stale (age {}ms > max {}ms). Trying next provider.",
							aggregator, asset.symbol, age_ms, max_age_ms
						);
					},
					TimeframeStatus::Missing => {
						warn!(
							"{} has no entry for {}. Trying next provider.",
							aggregator, asset.symbol
						);
					},
				}
			}

			if let Some(tf) = selected_tf {
				reconcile_asset_registration_on_startup(
					&asset.symbol,
					disabled_assets.clone(),
					startup_reconciled_assets.clone(),
					&dark_oracle_updater,
					&hierarchy,
				)
				.await;

				// We found a price. Check if it was previously disabled.
				handle_asset_recovery(
					&asset.symbol,
					disabled_assets.clone(),
					dark_oracle_updater.clone(),
				)
				.await;

				currencies_to_feed.push(tf);
			} else {
				// No price found anywhere in the hierarchy
				handle_asset_exhausted(
					asset,
					asset_hierarchy,
					&storage,
					disabled_assets.clone(),
					&dark_oracle_updater,
					&mut currencies_to_feed,
					&mut missing_data,
					&hierarchy,
				)
				.await;
			}
		}

		let dark_oracle_future = async {
			if missing_data {
				error!("Rejecting feeding transaction because at least 1 token is missing data");
			} else {
				let provider_summary = currencies_to_feed
					.iter()
					.map(|c| format!("{}={}", c.symbol, c.provider))
					.collect::<Vec<_>>()
					.join(", ");
				info!("Pushing prices to DarkOracle on-chain from providers: {}", provider_summary);

				match dark_oracle_updater.update_prices(&currencies_to_feed).await {
					Ok((tx_hash, _price_data)) => {
						send_tx(&update_tx, TxKind::DarkOracle, tx_hash);
					},
					Err(e) => {
						error!("Failed to submit DarkOracle tx: {:?}", e);
					},
				}
			}
		};

		dark_oracle_future.await;
		let elapsed = feed_start.elapsed();
		debug!("Feed loop tick completed in {:?}", elapsed);
		if elapsed < update_interval {
			tokio::time::sleep(update_interval - elapsed).await;
		}
	}
}

/// Spawns a tiny task that fires `trigger.notify_one()` at
/// `next_tick - lead_time`. If `lead_time >= time_until_next_tick` (i.e. the
/// previous fetch ran long), the trigger fires immediately so the fetch
/// loop has a chance to refresh before the next feed tick.
fn schedule_fetch_trigger(
	trigger: Arc<Notify>,
	next_tick: tokio::time::Instant,
	lead_time: std::time::Duration,
) {
	tokio::spawn(async move {
		let wake_at = next_tick.checked_sub(lead_time).unwrap_or_else(tokio::time::Instant::now);
		tokio::time::sleep_until(wake_at).await;
		trigger.notify_one();
	});
}

/// Forwards a transaction hash to the tx_processor channel via `try_send`.
fn send_tx(tx: &mpsc::Sender<Tx>, kind: TxKind, tx_hash: B256) {
	if let Err(e) = tx.try_send(Tx { kind, tx_hash }) {
		match e {
			mpsc::error::TrySendError::Full(_) => {
				warn!("[{kind}] tx_processor channel full — tx_hash dropped");
			},
			mpsc::error::TrySendError::Closed(_) => {
				error!("[{kind}] tx_processor channel closed");
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::api::{AssetsByProvider, QuotationsFuture, QuotationsOutcome};
	use std::sync::atomic::{AtomicUsize, Ordering};

	#[test]
	fn startup_recovery_only_registers_when_asset_is_unregistered() {
		assert!(should_recover_asset_on_startup(false));
		assert!(!should_recover_asset_on_startup(true));
	}

	struct AlwaysFailPriceApi {
		calls: Arc<AtomicUsize>,
	}

	impl PriceApi for AlwaysFailPriceApi {
		fn get_quotation_futures<'a>(
			&'a self,
			assets_by_provider: AssetsByProvider<'a>,
		) -> Vec<QuotationsFuture<'a>> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			let provider = assets_by_provider.keys().next().cloned().unwrap_or(Aggregator::Unknown);
			vec![Box::pin(async move {
				QuotationsOutcome { provider, quotations: Vec::new(), had_error: true }
			})]
		}
	}

	fn eurc_currencies() -> HashSet<AssetSpecifier> {
		vec![AssetSpecifier { blockchain: "Base".into(), symbol: "EURC".into() }]
			.into_iter()
			.collect()
	}

	#[tokio::test]
	async fn fetch_loop_waits_for_next_trigger_after_provider_error() {
		let calls = Arc::new(AtomicUsize::new(0));
		let api = AlwaysFailPriceApi { calls: calls.clone() };
		let interval = std::time::Duration::from_secs(1);
		let result = tokio::time::timeout(
			std::time::Duration::from_millis(20),
			run_fetch_loop(
				Arc::new(CoinInfoStorage::new(interval)),
				eurc_currencies(),
				interval,
				Arc::new(Notify::new()),
				api,
				ProviderHierarchy::default(),
			),
		)
		.await;

		assert!(result.is_err(), "the loop should be waiting for its next trigger");
		assert_eq!(calls.load(Ordering::SeqCst), 1, "a provider error must not hot-retry");
	}
}

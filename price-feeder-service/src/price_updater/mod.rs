pub mod alerts;
pub mod chain;
pub mod configs;
pub mod dark_oracle;
pub mod helpers;
pub mod pyth;
pub mod tx_processor;

pub use alerts::PriceDivergenceAlert;
pub use chain::ChainClient;
pub use dark_oracle::DarkOracleUpdater;
pub use pyth::PythPriceUpdater;
pub use tx_processor::UpdateTx;

use crate::api::PriceApi;
use crate::storage::CoinInfoStorage;
use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use alloy::primitives::B256;
use configs::HierarchyEntry;
use helpers::{convert_to_coin_info, BIPS_DIVISOR};
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
		}
		Some(AssetStatus::Disabled(meta)) => {
			info!("Recovered feed for {}, sending enable tx", asset_symbol);
			let meta_clone = meta.clone();
			disabled_guard.insert(asset_symbol.to_string(), AssetStatus::Enabling(meta_clone.clone()));

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
		}
		_ => {}
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
		let aggregator = &entry.aggregator;
		let blockchain = if *aggregator == Aggregator::Pyth {
			"unknown"
		} else {
			asset.blockchain.as_str()
		};
		storage.get_timeframe_any(asset_symbol, blockchain, aggregator.clone())
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
				}
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
				}
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
					error!(
						"Failed to submit disable tx for {}: {:?}, retrying",
						asset_symbol, e
					);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
					continue;
				}
			};

			match tx_processor::confirm_tx(provider.clone(), TxKind::DisableAsset, tx_hash).await {
				ConfirmOutcome::Confirmed => break meta,
				ConfirmOutcome::Reverted => {
					warn!(
						"Disable tx for {} reverted on-chain, resubmitting",
						asset_symbol
					);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
				}
				ConfirmOutcome::RpcError => {
					warn!(
						"RPC error confirming disable tx for {}, resubmitting",
						asset_symbol
					);
					tokio::time::sleep(DISABLE_RETRY_BACKOFF).await;
				}
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

pub const FETCH_LEAD_TIME: std::time::Duration = std::time::Duration::from_millis(1_500);// TODO "play" with this value\
// in theory setting it to 0, and using a 1 multiplier in the storage should error all the feeds.
// Should probably be the average.

pub const FETCH_WATCHDOG_MIN: std::time::Duration = std::time::Duration::from_secs(2);

/// The fetch loop runs **on demand**: it sleeps until either
///   1. the feed loop signals via `fetch_trigger` (the normal case, scheduled
///      to fire `FETCH_LEAD_TIME` before each feed tick), or
///   2. the watchdog timeout elapses (defensive — should not happen in
///      steady state).
///
/// On a successful fetch (no provider errored) it goes back to sleep. On
/// **any** provider error it retries immediately without waiting for the
/// next trigger, until either the fetch succeeds or a new trigger arrives.
pub async fn run_fetch_loop<T>(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	update_interval: std::time::Duration,
	fetch_trigger: Arc<Notify>,
	api: T,
	_update_tx: mpsc::Sender<UpdateTx>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
	T: PriceApi + Send + Sync + 'static,
{
	info!("Starting fetch loop (trigger-gated, watchdog={:?})", update_interval * 2);

	let _ = run_single_fetch(&storage, &supported_currencies, &api).await;

	let watchdog = std::cmp::max(FETCH_WATCHDOG_MIN, update_interval * 2);

	loop {
		// Wait for either the feed loop's trigger or the watchdog.
		tokio::select! {
			_ = fetch_trigger.notified() => {
				info!("Fetch loop woken by feed trigger");
			}
			_ = tokio::time::sleep(watchdog) => {
				warn!("Fetch loop watchdog fired ({:?}); no trigger received", watchdog);
			}
		}

		// Run fetches; on any provider error, retry immediately without
		// waiting for the next trigger.
		loop {
			let fetch_start = tokio::time::Instant::now();
			let had_error = run_single_fetch(&storage, &supported_currencies, &api).await;
			info!("Fetch completed in {:?}", fetch_start.elapsed());
			if !had_error {
				break;
			}
			warn!("Fetch loop encountered a provider error; retrying immediately");
			//  avoid pegging a CPU on a hard-down provider.
			tokio::task::yield_now().await;
		}
	}
}

async fn run_single_fetch<T>(
	storage: &Arc<CoinInfoStorage>,
	supported_currencies: &HashSet<AssetSpecifier>,
	api: &T,
) -> bool
where
	T: PriceApi + Send + Sync + 'static,
{
	let start = tokio::time::Instant::now();

	let assets_refs: Vec<&AssetSpecifier> = supported_currencies.iter().collect();
	let storage_pyth = storage.clone();

	let quotations_future = async {
		let outcome = api.get_quotations(assets_refs).await;
		for q in outcome.quotations {
			match convert_to_coin_info(q.clone()) {
				Ok(ci) => {
					storage.update_timeframe(ci);
				},
				Err(e) => error!("Error converting to CoinInfo: {:#?}", e),
			}
		}
		outcome.had_error
	};

	// Fetch Pyth prices. Purely for storage update as coinbase/coingecko final backups.
	let pyth_future = async {
		match pyth::fetch_pyth_prices(supported_currencies).await {
			Ok((_data, price_data)) => {
				let time = chrono::Utc::now().timestamp().unsigned_abs();

				let update_pyth = |symbol: &str, price: f64| {
					let scale = 1_000_000_000_000_000_000f64; // 10^18 for price
					storage_pyth.update_timeframe(CoinInfo {
						symbol: symbol.into(),
						name: symbol.into(),
						blockchain: "unknown".into(),
						supply: 0,
						last_update_timestamp: time,
						price: (price * scale) as u128,
						provider: Aggregator::Pyth,
					});
				};

				for (symbol, price) in &price_data.prices {
					update_pyth(symbol, *price);
				}
				false
			},
			Err(e) => {
				error!("Failed to fetch Pyth prices in fetch loop: {:?}", e);
				true
			},
		}
	};

	let (quotations_err, pyth_err) = tokio::join!(quotations_future, pyth_future);

	debug!(
		"Storage state after fetch ({:?}, quotations_err={}, pyth_err={}):",
		start.elapsed(),
		quotations_err,
		pyth_err,
	);
	for (key, tf) in storage.timeframes.read().unwrap().iter() {
		debug!(
			"{}: {} (provider: {:?}) (timestamp: {})",
			key, tf.price, tf.provider, tf.last_update_timestamp
		);
	}

	quotations_err || pyth_err
}

pub async fn run_feed_loop(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	divergence_threshold_bp: u64,
	dark_oracle_updater: DarkOracleUpdater,
	divergence_tx: mpsc::Sender<PriceDivergenceAlert>,
	update_tx: mpsc::Sender<UpdateTx>,
	mut pyth_updater: PythPriceUpdater,
	pyth_client: Arc<ChainClient>,
	fetch_trigger: Arc<Notify>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
	info!("Starting feed loop");

	let disabled_assets = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<String, AssetStatus>::new()));
	let hierarchy = ProviderHierarchy::default();
	let update_interval = dark_oracle_updater.get_update_interval();

	loop {
		let feed_start = tokio::time::Instant::now();
		info!("Feed loop tick started");
		storage.log_average_feed_age();
		let next_tick = feed_start + update_interval;

		// Schedule the fetch trigger to fire just before the *next* feed
		// tick.
		schedule_fetch_trigger(fetch_trigger.clone(), next_tick, FETCH_LEAD_TIME);

		let pyth_future = async {
			match pyth_updater.run_update(pyth_client.clone(), &supported_currencies).await {
				Ok((tx_hash_opt, price_data)) => {
					info!("Pyth prices fetched in feed loop: {:?}", price_data.prices);
					if let Some(tx_hash) = tx_hash_opt {
						send_tx(&update_tx, TxKind::Pyth, tx_hash);
					}
				},
				Err(e) => {
					error!("Failed to fetch/submit Pyth prices: {:?}", e);
				},
			}
		};

		let mut currencies_to_feed = vec![];
		let mut missing_data = false;

		let now = chrono::Utc::now();

		for asset in &supported_currencies {
			let asset_hierarchy = hierarchy
				.get_hierarchy(&asset.symbol, now);

			let mut selected_tf = None;
			for entry in &asset_hierarchy {
				let aggregator = &entry.aggregator;
				let blockchain = if *aggregator == Aggregator::Pyth {
					"unknown"
				} else {
					asset.blockchain.as_str()
				};
				selected_tf = storage.get_timeframe(&asset.symbol, blockchain, aggregator.clone());
				if selected_tf.is_some() {
					break;
				} else {
					warn!("{} failed for {}. Trying next provider.", aggregator, asset.symbol);
				}
			}

			if let Some(tf) = selected_tf {
				// We found a price. Check if it was previously disabled.
				handle_asset_recovery(
					&asset.symbol,
					disabled_assets.clone(),
					dark_oracle_updater.clone(),
				).await;

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
				).await;
			}
		}

		let dark_oracle_future = async {
			if missing_data {
				error!("Rejecting feeding transaction because at least 1 token is missing data");
			} else {
				match dark_oracle_updater
					.update_prices(&currencies_to_feed)
					.await
				{
					Ok((tx_hash, price_data)) => {
						send_tx(&update_tx, TxKind::DarkOracle, tx_hash);

						// Price divergence validation for EURC
						let pyth_eurc_tf =
							storage.get_timeframe("EURC", "unknown", Aggregator::Pyth);
						if let Some(pyth_tf) = pyth_eurc_tf {
							if let Some(&price) = price_data.prices.get("EURC") {
								let scale = 1_000_000_000_000_000_000f64; // 10^18 for price
								let fallback = (pyth_tf.price as f64) / scale;
								let abs_div = if fallback > price {
									fallback - price
								} else {
									price - fallback
								};
								let bp_div = (abs_div * BIPS_DIVISOR as f64) / fallback;
								debug!(
									"EURC divergence: {:.2} bp (DarkOracle: {}, Pyth: {})",
									bp_div, price, fallback
								);

								if bp_div > divergence_threshold_bp as f64 {
									let alert = PriceDivergenceAlert {
										asset: "EURC".to_string(),
										bp_divergence: bp_div,
										threshold_bp: divergence_threshold_bp,
										dark_oracle_price: price,
										pyth_price: fallback,
									};
									if let Err(e) = divergence_tx.try_send(alert) {
										match e {
											mpsc::error::TrySendError::Full(_) => {
												warn!("Divergence alert channel full — alert dropped");
											},
											mpsc::error::TrySendError::Closed(_) => {
												error!("Divergence alert channel closed");
											},
										}
									}
								}
							}
						}
					},
					Err(e) => {
						error!("Failed to submit DarkOracle tx: {:?}", e);
					},
				}
			}
		};

		tokio::join!(pyth_future, dark_oracle_future);
		let elapsed = feed_start.elapsed();
		info!("Feed loop tick completed in {:?}", elapsed);
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
		info!("Fetch trigger notify fired at {:?}", tokio::time::Instant::now());
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

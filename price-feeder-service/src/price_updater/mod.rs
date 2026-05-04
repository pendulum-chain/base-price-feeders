pub mod alerts;
pub mod chain;
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
use helpers::{convert_to_coin_info, BIPS_DIVISOR};
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::mpsc;
use tx_processor::{UpdateTx as Tx, UpdateTxKind as TxKind};

#[derive(Debug, Clone)]
pub struct ProviderHierarchy {
	pub default: Vec<Aggregator>,
	pub per_asset: std::collections::HashMap<String, Vec<Aggregator>>,
}

// Default hierarchy: Binance > Coinbase > Pyth for BRL/BRLA,
// Coinbase > Coingecko > Pyth for everything else.
impl Default for ProviderHierarchy {
	fn default() -> Self {
		let mut per_asset = std::collections::HashMap::new();
		per_asset.insert(
			"BRLA".to_string(),
			vec![Aggregator::Binance],
		);
		per_asset.insert(
			"BRL".to_string(),
			vec![Aggregator::Binance],
		);

		Self {
			default: vec![Aggregator::Coinbase, Aggregator::Coingecko, Aggregator::Pyth],
			per_asset,
		}
	}
}

// Tracks the state of problematic assets.
// Note: There is no "Enabled" state because once an asset is successfully
// enabled on-chain, it is removed from the tracking map entirely.
#[derive(Debug, Clone, PartialEq)]
enum AssetStatus {
	Disabled(dark_oracle::AssetMetadata),
	Enabling(dark_oracle::AssetMetadata),
}

async fn handle_asset_recovery(
	asset_symbol: &str,
	disabled_assets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, AssetStatus>>>,
	dark_oracle_updater: DarkOracleUpdater,
) {
	let mut disabled_guard = disabled_assets.lock().await;
	if let Some(status) = disabled_guard.get(asset_symbol) {
		if let AssetStatus::Disabled(meta) = status {
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
	}
}

async fn handle_asset_exhausted(
	asset_symbol: &str,
	disabled_assets: Arc<tokio::sync::Mutex<std::collections::HashMap<String, AssetStatus>>>,
	last_prices: &std::collections::HashMap<String, CoinInfo>,
	dark_oracle_updater: &DarkOracleUpdater,
	currencies_to_feed: &mut Vec<CoinInfo>,
	missing_data: &mut bool,
) {
	let mut disabled_guard = disabled_assets.lock().await;
	// Only skip the disable tx if the asset is already strictly Disabled.
	// If it's Enabling, we still want to send a disable tx because the feed was lost again.
	let is_disabled = matches!(disabled_guard.get(asset_symbol), Some(AssetStatus::Disabled(_)));

	if !is_disabled {
		info!("Hierarchy exhausted for {}, sending disable tx", asset_symbol);
		match dark_oracle_updater.disable_asset(asset_symbol).await {
			Ok((_, meta)) => {
				disabled_guard.insert(asset_symbol.to_string(), AssetStatus::Disabled(meta));
				if let Some(last_tf) = last_prices.get(asset_symbol) {
					currencies_to_feed.push(last_tf.clone());
				} else {
					error!("No last price available for token: {}", asset_symbol);
					*missing_data = true;
				}
			}
			Err(e) => {
				error!("Failed to disable asset {}: {:?}", asset_symbol, e);
				*missing_data = true;
			}
		}
	} else {
		// Already disabled, just use last known price
		if let Some(last_tf) = last_prices.get(asset_symbol) {
			currencies_to_feed.push(last_tf.clone());
		} else {
			error!("No last price available for token: {}", asset_symbol);
			*missing_data = true;
		}
	}
}

// ── Public entry point ─────────────────────────────────────────────────────────

pub async fn run_fetch_loop<T>(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	update_interval: std::time::Duration,
	api: T,
	_update_tx: mpsc::Sender<UpdateTx>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
	T: PriceApi + Send + Sync + 'static,
{
	info!("Starting fetch loop");

	loop {
		let start = tokio::time::Instant::now();

		let assets_refs: Vec<&AssetSpecifier> = supported_currencies.iter().collect();
		let storage_pyth = storage.clone();

		let quotations_future = async {
			let quotations = api.get_quotations(assets_refs).await;
			for q in quotations {
				match convert_to_coin_info(q.clone()) {
					Ok(ci) => {
						storage.update_timeframe(ci);
					},
					Err(e) => error!("Error converting to CoinInfo: {:#?}", e),
				}
			}
		};

		// Fetch Pyth prices. Purely for storage update as coinbase/coingecko final backups.
		let pyth_future = async {
			match pyth::fetch_pyth_prices(&supported_currencies).await {
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
				},
				Err(e) => {
					error!("Failed to fetch Pyth prices in fetch loop: {:?}", e);
				},
			}
		};

		tokio::join!(quotations_future, pyth_future);

		debug!("Storage state after fetch loop iteration:");
		for (key, tf) in storage.timeframes.read().unwrap().iter() {
			debug!(
				"{}: {} (provider: {:?}) (timestamp: {})",
				key, tf.price, tf.provider, tf.last_update_timestamp
			);
		}

		let elapsed = start.elapsed();
		if elapsed < update_interval {
			tokio::time::sleep(update_interval - elapsed).await;
		}
	}
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
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
	info!("Starting feed loop");

	let disabled_assets = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<String, AssetStatus>::new()));
	let mut last_prices = std::collections::HashMap::<String, CoinInfo>::new();

	loop {
		let start = tokio::time::Instant::now();

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
		let hierarchy = ProviderHierarchy::default();

		for asset in &supported_currencies {
			let asset_hierarchy = hierarchy
				.per_asset
				.get(&asset.symbol)
				.unwrap_or(&hierarchy.default);

			let mut selected_tf = None;
			for aggregator in asset_hierarchy {
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

				last_prices.insert(asset.symbol.clone(), tf.clone());
				currencies_to_feed.push(tf);
			} else {
				// No price found anywhere in the hierarchy
				handle_asset_exhausted(
					&asset.symbol,
					disabled_assets.clone(),
					&last_prices,
					&dark_oracle_updater,
					&mut currencies_to_feed,
					&mut missing_data,
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
		let update_interval = dark_oracle_updater.get_update_interval();
		let elapsed = start.elapsed();
		if elapsed < update_interval {
			tokio::time::sleep(update_interval - elapsed).await;
		}
	}
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

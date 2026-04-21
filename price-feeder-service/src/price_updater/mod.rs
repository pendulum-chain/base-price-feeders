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
	update_interval: std::time::Duration,
	divergence_threshold_bp: u64,
	dark_oracle_updater: DarkOracleUpdater,
	dark_oracle_client: Arc<ChainClient>,
	divergence_tx: mpsc::Sender<PriceDivergenceAlert>,
	update_tx: mpsc::Sender<UpdateTx>,
	mut pyth_updater: PythPriceUpdater,
	pyth_client: Arc<ChainClient>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
	info!("Starting feed loop");

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

		for asset in &supported_currencies {
			// Hierarchy: coinbase -> coingecko -> pyth
			let mut selected_tf =
				storage.get_timeframe(&asset.symbol, &asset.blockchain, Aggregator::Coinbase);
			if selected_tf.is_none() {
				selected_tf =
					storage.get_timeframe(&asset.symbol, &asset.blockchain, Aggregator::Coingecko);
				warn!("Coinbase failed for {}. Checking price on CoinGecko", asset.symbol);
			}
			if selected_tf.is_none() {
				warn!("Coingecko failed for {}. Checking price on Pyth", asset.symbol);
				// Pyth uses "unknown" as blockchain in our updater
				selected_tf = storage.get_timeframe(&asset.symbol, "unknown", Aggregator::Pyth);
			}

			if let Some(tf) = selected_tf {
				currencies_to_feed.push(tf);
			} else {
				error!("No data available for token: {}", asset.symbol);
				missing_data = true;
			}
		}

		let dark_oracle_future = async {
			if missing_data {
				error!("Rejecting feeding transaction because at least 1 token is missing data");
			} else {
				match dark_oracle_updater
					.update_prices(&currencies_to_feed, dark_oracle_client.clone())
					.await
				{
					Ok((tx_hash, price_data)) => {
						info!("DarkOracle tx submitted: {:?}", price_data.prices);
						send_tx(&update_tx, TxKind::DarkOracle, tx_hash);

						// Price divergence validation logic here
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
									let _ = divergence_tx.try_send(PriceDivergenceAlert {
										asset: "EURC".to_string(),
										bp_divergence: bp_div,
										threshold_bp: divergence_threshold_bp,
										dark_oracle_price: price,
										pyth_price: fallback,
									});
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

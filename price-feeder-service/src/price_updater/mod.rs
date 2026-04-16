pub mod alerts;
pub mod chain;
pub mod dark_oracle;
pub mod helpers;
pub mod pyth;
pub mod tx_processor;

pub use alerts::PriceDivergenceAlert;
pub use chain::{PriceData, ChainClient};
pub use dark_oracle::DarkOracleUpdater;
pub use pyth::PythPriceUpdater;
pub use tx_processor::UpdateTx;

use crate::api::PriceApi;
use crate::storage::CoinInfoStorage;
use crate::AssetSpecifier;
use helpers::{convert_to_coin_info, BIPS_DIVISOR};
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::error::Error;
use alloy::primitives::B256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tx_processor::{UpdateTx as Tx, UpdateTxKind as TxKind};

// ── Public entry point ─────────────────────────────────────────────────────────

pub async fn run_update_prices_loop<T>(
	storage: Arc<CoinInfoStorage>,
	supported_currencies: HashSet<AssetSpecifier>,
	update_interval: std::time::Duration,
	pyth_update_interval: std::time::Duration,
	divergence_threshold_bp: u64,
	api: T,
	divergence_tx: mpsc::Sender<PriceDivergenceAlert>,
	update_tx: mpsc::Sender<UpdateTx>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
	T: PriceApi + Send + Sync + 'static,
{
	let mut pyth_updater = PythPriceUpdater::new(pyth_update_interval)?;
	let dark_oracle_updater = DarkOracleUpdater::new()?;
	
	let nonce_manager = ChainClient::create_nonce_manager().await?;
	let dark_oracle_client = Arc::new(ChainClient::new(nonce_manager.clone()).await?);
	let pyth_client = Arc::new(ChainClient::new(nonce_manager).await?);
	
	info!("Initialized chain clients and updaters");

	loop {
		let start = tokio::time::Instant::now();
		let coins = Arc::clone(&storage);
		update_prices(
			coins,
			&supported_currencies,
			&api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client.clone(),
			pyth_client.clone(),
			divergence_threshold_bp,
			&divergence_tx,
			&update_tx,
		)
		.await;
		let elapsed = start.elapsed();
		if elapsed < update_interval {
			tokio::time::sleep(update_interval - elapsed).await;
		}
	}
}

pub(crate) async fn update_prices<T>(
	coins: Arc<CoinInfoStorage>,
	supported_currencies: &HashSet<AssetSpecifier>,
	api: &T,
	pyth_updater: &mut PythPriceUpdater,
	dark_oracle_updater: &DarkOracleUpdater,
	dark_oracle_client: Arc<ChainClient>,
	pyth_client: Arc<ChainClient>,
	divergence_threshold_bp: u64,
	divergence_tx: &mpsc::Sender<PriceDivergenceAlert>,
	update_tx: &mpsc::Sender<Tx>,
) where
	T: PriceApi + Send + Sync + 'static,
{
	let mut currencies = vec![];
	api.get_quotations(supported_currencies.iter().collect())
		.await
		.into_iter()
		.for_each(|q| match convert_to_coin_info(q) {
			Ok(ci) => currencies.push(ci),
			Err(e) => error!("Error converting to CoinInfo: {:#?}", e),
		});

	coins.replace_currencies_by_symbols(currencies.clone());
	info!("Currencies updated");


	let (dark_oracle_result, pyth_result) = tokio::join!(
		dark_oracle_updater.update_prices(&currencies, dark_oracle_client.clone()),
		pyth_updater.run_update(pyth_client),
	);

	let dark_oracle_prices: Option<PriceData> = match dark_oracle_result {
		Ok((tx_hash, price_data)) => {
			info!("DarkOracle tx submitted: USDC={}, EURC={}, BRLA={}", price_data.usdc, price_data.eurc, price_data.brla);
			send_tx(update_tx, TxKind::DarkOracle, tx_hash);
			Some(price_data)
		}
		Err(e) => {
			error!("Failed to submit DarkOracle tx: {:?}", e);
			if let Ok((_, ref pyth_data)) = pyth_result {
				info!("Retrying DarkOracle update using Pyth prices as fallback");
				let fallback_currencies = pyth_price_data_to_currencies(pyth_data);
				match dark_oracle_updater.update_prices(&fallback_currencies, dark_oracle_client).await {
					Ok((tx_hash, price_data)) => {
						info!("DarkOracle tx submitted (fallback Pyth prices): USDC={}, EURC={}, BRLA={}", price_data.usdc, price_data.eurc, price_data.brla);
						send_tx(update_tx, TxKind::DarkOracle, tx_hash);
						Some(price_data)
					}
					Err(e2) => {
						error!("Failed to submit DarkOracle tx with fallback Pyth prices: {:?}", e2);
						None
					}
				}
			} else {
				None
			}
		}
	};

	let pyth_prices: Option<PriceData> = match pyth_result {
		Ok((tx_hash_opt, price_data)) => {
			info!("Pyth prices fetched: USDC={}, EURC={}, BRLA={}", price_data.usdc, price_data.eurc, price_data.brla);
			if let Some(tx_hash) = tx_hash_opt {
				send_tx(update_tx, TxKind::Pyth, tx_hash);
			}
			Some(price_data)
		}
		Err(e) => {
			error!("Failed to fetch/submit Pyth prices: {:?}", e);
			None
		}
	};

	// Price divergence validation
	// Mirrors `_validatePrice` in SafePriceProvider.sol (DarkOracle contract).
	if let (Some(dark_oracle), Some(pyth)) = (dark_oracle_prices, pyth_prices) {
		let fallback = pyth.eurc;
		let price = dark_oracle.eurc;
		let abs_div = if fallback > price { fallback - price } else { price - fallback };
		let bp_div = (abs_div * BIPS_DIVISOR as f64) / fallback;
		debug!("EURC divergence: {:.2} bp (DarkOracle: {}, Pyth: {})", bp_div, price, fallback);

		if bp_div > divergence_threshold_bp as f64 {
			if let Err(e) = divergence_tx.try_send(PriceDivergenceAlert {
				asset: "EURC".to_string(),
				bp_divergence: bp_div,
				threshold_bp: divergence_threshold_bp,
				dark_oracle_price: price,
				pyth_price: fallback,
			}) {
				match e {
					mpsc::error::TrySendError::Full(_) => {
						warn!("Divergence alert channel full — alert dropped");
					}
					mpsc::error::TrySendError::Closed(_) => {
						error!("Divergence alert channel closed");
					}
				}
			}
		}
	}
}

/// Forwards a transaction hash to the tx_processor channel via `try_send`.
fn send_tx(tx: &mpsc::Sender<Tx>, kind: TxKind, tx_hash: B256) {
	if let Err(e) = tx.try_send(Tx { kind, tx_hash }) {
		match e {
			mpsc::error::TrySendError::Full(_) => {
				warn!("[{kind}] tx_processor channel full — tx_hash dropped");
			}
			mpsc::error::TrySendError::Closed(_) => {
				error!("[{kind}] tx_processor channel closed");
			}
		}
	}
}

fn pyth_price_data_to_currencies(data: &PriceData) -> Vec<crate::types::CoinInfo> {
	let mut currencies = Vec::new();
	let scale = 1_000_000_000_000_000_000f64; // 10^18 for price
	
	let mut add_coin = |symbol: &str, price: f64| {
		currencies.push(crate::types::CoinInfo {
			symbol: symbol.into(),
			name: symbol.into(),
			blockchain: "unknown".into(),
			supply: 0,
			last_update_timestamp: 0,
			price: (price * scale) as u128,
		});
	};

	add_coin("USDC", data.usdc);
	add_coin("EURC", data.eurc);
	add_coin("BRL", data.brla); // mapped from BRLA to BRL for dark_oracle updater

	currencies
}

#[cfg(test)]
#[path = "../price_updater_tests.rs"]
mod tests;

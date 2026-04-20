use alloy::{
	primitives::{Address, B256},
	sol,
};
use log::{info, warn};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::sync::Arc;

use super::chain::{ChainClient, PriceData};
use crate::types::CoinInfo;

sol! {
	#[sol(rpc)]
	contract DarkOracle {
		function updatePriceFeeds(uint48[5] _prices, uint56 _timestamp) external returns (bool success_);
	}
}

pub struct DarkOracleUpdater {
	contract_address: Address,
}

impl DarkOracleUpdater {
	pub fn new() -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let contract_address =
			std::env::var("CONTRACT_ADDRESS").map_err(|_| "CONTRACT_ADDRESS not set")?;
		let addr = contract_address.parse::<Address>()?;
		Ok(Self { contract_address: addr })
	}

	pub async fn update_prices(
		&self,
		currencies: &Vec<CoinInfo>,
		client: Arc<ChainClient>,
	) -> Result<(B256, PriceData), Box<dyn Error + Send + Sync + 'static>> {
		info!("Starting DarkOracle contract price update...");

		let oracle = DarkOracle::new(self.contract_address, &*client.provider);

		let symbol_to_price: HashMap<&str, u128> =
			currencies.iter().map(|c| (c.symbol.as_str(), c.price)).collect();

		let mut prices: [u64; 5] = [0; 5];

		// ETH index 0
		if let Some(eth_price) = symbol_to_price.get("ETH") {
			prices[0] = u64::try_from(*eth_price / 10_000_000_000)?;
		}

		// BTC index 1
		if let Some(btc_price) = symbol_to_price.get("BTC") {
			prices[1] = u64::try_from(*btc_price / 10_000_000_000)?;
		}

		// USDC index 2
		if let Some(usdc_price) = symbol_to_price.get("USDC") {
			prices[2] = u64::try_from(*usdc_price / 10_000_000_000)?;
		}

		// BRL index 3
		if let Some(brl_price) = symbol_to_price.get("BRL") {
			prices[3] = u64::try_from(*brl_price / 10_000_000_000)?;
		}

		// EURC index 4
		if let Some(eurc_price) = symbol_to_price.get("EURC") {
			prices[4] = u64::try_from(*eurc_price / 10_000_000_000)?;
		}

		let timestamp = u64::try_from(
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis(),
		)?;

		info!("Updating DarkOracle contract prices: {:?}", prices);
		info!("Timestamp: {:?}", timestamp);

		let priority_fee = client.estimate_priority_fee().await?;
		info!("DarkOracle priority fee: {} wei", priority_fee);

		let nonce = client.nonce_manager.next_nonce();
		let call_builder = oracle
			.updatePriceFeeds(prices, timestamp)
			.gas(1_000_000)
			.max_priority_fee_per_gas(priority_fee * 7)
			.nonce(nonce);

		let pending_tx = call_builder.send().await?;
		let tx_hash = *pending_tx.tx_hash();
		info!("DarkOracle updatePriceFeeds tx hash: {:?}", tx_hash);

		let mut prices_map = HashMap::new();
		for (symbol, price) in &symbol_to_price {
			prices_map.insert(symbol.to_string(), *price as f64 / 10f64.powi(18));
		}

		let price_data = PriceData {
			prices: prices_map,
		};

		Ok((tx_hash, price_data))
	}
}

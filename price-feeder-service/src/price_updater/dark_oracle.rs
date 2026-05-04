use alloy::{
	primitives::{Address, B256},
	sol,
};
use log::info;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::sync::Arc;

use super::chain::{ChainClient, ChainProvider, HttpTransport, PriceData};
use crate::types::CoinInfo;

sol! {
	#[sol(rpc)]
	contract DarkOracle {
		function updatePriceFeeds(uint48[5] _prices, uint56 _timestamp) external returns (bool success_);
		function unregisterAsset(address _asset) external returns (bool success_);
		function registerAsset(address _assetAddress, uint8 _id, string memory _assetName, string memory _cannonicalName, bytes32 _priceFeedId) external returns (bool success_);
		function assetMeta(address arg0) external view returns (string name, string cannonicalName, bytes32 priceFeedId, address assetAddress, uint8 asset);
	}
}

type DarkOracleContract = DarkOracle::DarkOracleInstance<HttpTransport, ChainProvider>;

#[derive(Debug, Clone, PartialEq)]
pub struct AssetMetadata {
	pub name: String,
	pub canonical_name: String,
	pub price_feed_id: B256,
	pub asset_address: Address,
	pub id: u8,
}

#[derive(Clone)]
pub struct DarkOracleUpdater {
	oracle: Arc<DarkOracleContract>,
	client: Arc<ChainClient>,
	update_interval: std::time::Duration,
}

impl DarkOracleUpdater {
	pub fn new(
		client: Arc<ChainClient>,
		update_interval: std::time::Duration,
	) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let contract_address =
			std::env::var("CONTRACT_ADDRESS").map_err(|_| "CONTRACT_ADDRESS not set")?;
		let addr = contract_address.parse::<Address>()?;
		let oracle = DarkOracle::new(addr, (*client.provider).clone());
		Ok(Self { oracle: Arc::new(oracle), client, update_interval })
	}

	pub fn get_update_interval(&self) -> std::time::Duration {
		self.update_interval
	}

	fn get_asset_address(&self, symbol: &str) -> Option<Address> {
		let raw = match symbol {
			"USDC" => Some("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
			"EURC" => Some("0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42"),
			"BRL" | "BRLA" => Some("0x57180796D4082Ba903d86c4eA3C86490fA10512c"),
			_ => None,
		}?;
		raw.parse::<Address>().ok()
	}

	pub async fn fetch_asset_meta(
		&self,
		asset_addr: Address,
	) -> Result<AssetMetadata, Box<dyn Error + Send + Sync + 'static>> {
		let meta = self.oracle.assetMeta(asset_addr).call().await?;
		Ok(AssetMetadata {
			name: meta.name,
			canonical_name: meta.cannonicalName,
			price_feed_id: meta.priceFeedId,
			asset_address: meta.assetAddress,
			id: meta.asset,
		})
	}

	pub async fn disable_asset(
		&self,
		symbol: &str,
	) -> Result<(B256, AssetMetadata), Box<dyn Error + Send + Sync + 'static>> {
		info!("Disabling DarkOracle contract asset {}...", symbol);
		let asset_addr = self.get_asset_address(symbol).ok_or("Asset address not found in config")?;

		let meta = self.fetch_asset_meta(asset_addr).await?;

		let priority_fee = self.client.estimate_priority_fee().await?;
		let call_builder = self
			.oracle
			.unregisterAsset(asset_addr)
			.gas(500_000)
			.max_priority_fee_per_gas(priority_fee * 7);

		let tx_hash = self
			.client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;
		Ok((tx_hash, meta))
	}

	pub async fn enable_asset(
		&self,
		symbol: &str,
		meta: &AssetMetadata,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		info!("Enabling DarkOracle contract asset {}...", symbol);

		let priority_fee = self.client.estimate_priority_fee().await?;
		let call_builder = self
			.oracle
			.registerAsset(meta.asset_address, meta.id, meta.name.clone(), meta.canonical_name.clone(), meta.price_feed_id)
			.gas(500_000)
			.max_priority_fee_per_gas(priority_fee * 7);

		let tx_hash = self
			.client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;
		Ok(tx_hash)
	}

	pub async fn update_prices(
		&self,
		currencies: &Vec<CoinInfo>,
	) -> Result<(B256, PriceData), Box<dyn Error + Send + Sync + 'static>> {
		info!("Starting DarkOracle contract price update...");

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

		let priority_fee = self.client.estimate_priority_fee().await?;
		info!("DarkOracle priority fee: {} wei", priority_fee);

		let call_builder = self
			.oracle
			.updatePriceFeeds(prices, timestamp)
			.gas(1_000_000)
			.max_priority_fee_per_gas(priority_fee * 7);

		let tx_hash = self
			.client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;

		let mut prices_map = HashMap::new();
		for (symbol, price) in &symbol_to_price {
			prices_map.insert(symbol.to_string(), *price as f64 / 10f64.powi(18));
		}

		let price_data = PriceData { prices: prices_map };

		Ok((tx_hash, price_data))
	}
}

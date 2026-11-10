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
use super::configs;
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

	/// Shared provider handle, used to await tx confirmations off the
	/// fire-and-forget channel (e.g. when blocking the feed loop on a disable tx).
	pub fn provider(&self) -> Arc<ChainProvider> {
		self.client.provider.clone()
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

	/// Forces the nonce to the chain's pending nonce, e.g. after a tx got no receipt.
	pub async fn resync_nonce(&self) {
		self.client.nonce_manager.resync().await;
	}

	/// Sends an `unregisterAsset` tx and returns it with the asset's on-chain metadata. Returns
	/// `None` without sending when the asset is not registered, since that tx could only revert.
	pub async fn disable_asset(
		&self,
		symbol: &str,
	) -> Result<Option<(B256, AssetMetadata)>, Box<dyn Error + Send + Sync + 'static>> {
		let asset_addr = configs::get_asset_address(symbol)
			.ok_or_else(|| format!("Asset address not found for symbol: {}", symbol))?;

		let meta = self.fetch_asset_meta(asset_addr).await?;
		if meta.asset_address != asset_addr {
			info!("DarkOracle asset {} is not registered, nothing to disable", symbol);
			return Ok(None);
		}
		info!("Disabling DarkOracle contract asset {}...", symbol);

		let priority_fee = self.client.estimate_priority_fee().await?;
		let call_builder = self
			.oracle
			.unregisterAsset(asset_addr)
			.gas(500_000)
			.max_priority_fee_per_gas(priority_fee); // Uses the shared estimated priority fee, including any active watchdog bump.

		let tx_hash = self
			.client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;
		Ok(Some((tx_hash, meta)))
	}

	pub async fn is_asset_registered(
		&self,
		symbol: &str,
	) -> Result<bool, Box<dyn Error + Send + Sync + 'static>> {
		let asset_addr = configs::get_asset_address(symbol)
			.ok_or_else(|| format!("Asset address not found for symbol: {}", symbol))?;
		let meta = self.fetch_asset_meta(asset_addr).await?;
		Ok(meta.asset_address == asset_addr)
	}

	/// Sends a `registerAsset` tx. Returns `None` without sending when the asset is already
	/// registered, since that tx could only revert.
	pub async fn enable_asset(
		&self,
		symbol: &str,
		meta: &AssetMetadata,
	) -> Result<Option<B256>, Box<dyn Error + Send + Sync + 'static>> {
		if self.is_asset_registered(symbol).await? {
			info!("DarkOracle asset {} is already registered, nothing to enable", symbol);
			return Ok(None);
		}
		info!("Enabling DarkOracle contract asset {}...", symbol);

		let priority_fee = self.client.estimate_priority_fee().await?;
		let call_builder = self
			.oracle
			.registerAsset(
				meta.asset_address,
				meta.id,
				meta.name.clone(),
				meta.canonical_name.clone(),
				meta.price_feed_id,
			)
			.gas(500_000)
			.max_priority_fee_per_gas(priority_fee);

		let tx_hash = self
			.client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;
		Ok(Some(tx_hash))
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
			.max_priority_fee_per_gas(priority_fee);

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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::price_updater::{
		chain::{NonceManager, PriorityFeeMultiplier},
		pyth::test_support::spawn_mock_hermes,
	};
	use alloy::{
		network::EthereumWallet, providers::ProviderBuilder, signers::local::PrivateKeySigner,
		sol_types::SolCall,
	};

	/// An updater whose RPC answers a single request: an `assetMeta` read returning `meta`. Any
	/// further request, such as a fee estimate before sending, fails.
	async fn updater_reading(meta: (String, String, B256, Address, u8)) -> DarkOracleUpdater {
		let result = DarkOracle::assetMetaCall::abi_encode_returns(&meta);
		let body = format!(r#"{{"jsonrpc":"2.0","id":0,"result":"0x{}"}}"#, hex::encode(result));
		let (url, _) =
			spawn_mock_hermes("HTTP/1.1 200 OK", Box::leak(body.into_boxed_str()), 1).await;
		let signer = PrivateKeySigner::random();
		let address = signer.address();
		let provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(EthereumWallet::from(signer))
			.on_http(url.parse().unwrap());
		let client = Arc::new(ChainClient {
			provider: Arc::new(provider),
			nonce_manager: Arc::new(NonceManager::new(0, address, url)),
			address,
			priority_multiplier: Arc::new(PriorityFeeMultiplier::new(7.0)),
		});
		let oracle = DarkOracle::new(Address::ZERO, (*client.provider).clone());
		DarkOracleUpdater {
			oracle: Arc::new(oracle),
			client,
			update_interval: std::time::Duration::from_secs(10),
		}
	}

	#[tokio::test]
	async fn disable_sends_nothing_when_the_asset_is_already_unregistered() {
		let unregistered = (String::new(), String::new(), B256::ZERO, Address::ZERO, 0);
		let updater = updater_reading(unregistered).await;
		assert!(updater.disable_asset("BRL").await.unwrap().is_none());
	}

	#[tokio::test]
	async fn enable_sends_nothing_when_the_asset_is_already_registered() {
		let brl = configs::get_asset_address("BRL").unwrap();
		let registered =
			("BRLAUSD".to_string(), "BRLAUSD".to_string(), B256::repeat_byte(1), brl, 3);
		let meta = AssetMetadata {
			name: registered.0.clone(),
			canonical_name: registered.1.clone(),
			price_feed_id: registered.2,
			asset_address: brl,
			id: 3,
		};
		let updater = updater_reading(registered).await;
		assert!(updater.enable_asset("BRL", &meta).await.unwrap().is_none());
	}
}

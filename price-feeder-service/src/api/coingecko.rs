use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::info;

use crate::api::error::CoingeckoError;
use crate::args::CoingeckoConfig;
use crate::types::Quotation;
use crate::AssetSpecifier;

pub struct CoingeckoPriceApi {
	client: CoingeckoClient,
}

impl CoingeckoPriceApi {
	pub fn new_from_config(config: CoingeckoConfig) -> Self {
		let api_key = config.cg_api_key.expect("Please provide a CoinGecko API key");

		Self::new(config.cg_host_url, api_key)
	}

	pub fn new(host_url: String, api_key: String) -> Self {
		let client = CoingeckoClient::new(host_url, api_key);

		Self { client }
	}

	pub async fn get_prices(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, CoingeckoError> {
		// Map used for the reverse lookup of the CoinGecko ID to the asset
		let mut id_to_asset_map: HashMap<String, AssetSpecifier> = HashMap::new();
		// log all
		info!("Getting CoinGecko prices for assets: {:?}", assets);
		let coingecko_ids = assets
			.clone()
			.into_iter()
			.filter_map(|asset| {
				Self::convert_to_coingecko_id(asset)
					.and_then(|id| {
						id_to_asset_map.insert(id.clone(), asset.clone());
						Some(id)
					})
					.or_else(|| {
						log::warn!("Could not find CoinGecko ID for asset {:?}", asset);
						None
					})
			})
			.collect::<Vec<_>>();

		// mapped id's
		info!("Mapped CoinGecko IDs for assets: {:?}", id_to_asset_map);

		if coingecko_ids.is_empty() {
			return Ok(vec![]);
		}

		let id_to_price_map =
			self.client.price(&coingecko_ids, false, true, false, true).await.map_err(|e| {
				CoingeckoError(format!("Couldn't query CoinGecko prices {}", e.to_string()))
			})?;
		info!("Received CoinGecko price data for IDs: {:?}", id_to_price_map);
		let quotations = id_to_price_map
			.into_iter()
			.filter_map(|(id, price)| {
				let asset = id_to_asset_map.get(&id)?;
				let supply = price.usd_24h_vol.unwrap_or_default();
				let time = price.last_updated_at;

				Some(Quotation {
					symbol: asset.symbol.clone(),
					name: asset.symbol.clone(),
					blockchain: Some(asset.blockchain.clone()),
					price: price.usd, // Decimal to f64
					supply,
					time,
					provider: crate::types::Aggregator::Coingecko,
				})
			})
			.collect();

		Ok(quotations)
	}

	pub fn is_supported(asset: &AssetSpecifier) -> bool {
		Self::convert_to_coingecko_id(asset).is_some()
	}

	/// Maps the blockchain and symbol pair to the CoinGecko ID.
	fn convert_to_coingecko_id(asset: &AssetSpecifier) -> Option<String> {
		// Capitalize the blockchain and symbol
		let blockchain = asset.blockchain.to_uppercase();
		let symbol = asset.symbol.to_uppercase();
		match (blockchain.as_str(), symbol.as_str()) {
			("BASE", "EURC") => Some("euro-coin".to_string()),
			("BASE", "USDC") => Some("usd-coin".to_string()),
			("BASE", "BRLA") => Some("brla-digital-brla".to_string()),
			("BASE", "BRL") => Some("brla-digital-brla".to_string()),
			_ => None,
		}
	}
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SimplePing {
	pub gecko_says: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CoingeckoPrice {
	pub usd: Decimal,
	pub usd_market_cap: Option<Decimal>,
	pub usd_24h_vol: Option<Decimal>,
	pub usd_24h_change: Option<Decimal>,
	pub last_updated_at: u64,
}

/// CoinGecko network client
pub struct CoingeckoClient {
	host: String,
	api_key: String,
}

impl CoingeckoClient {
	pub fn new(host: String, api_key: String) -> Self {
		CoingeckoClient { host, api_key }
	}

	async fn get<R: DeserializeOwned>(&self, endpoint: &str) -> Result<R, CoingeckoError> {
		let client = reqwest::Client::builder()
			.build()
			.map_err(|e| CoingeckoError(e.to_string()))?;

		let url = reqwest::Url::parse(
			format!("{host}/{ep}", host = self.host.as_str(), ep = endpoint).as_str(),
		)
		.expect("Invalid URL");
		info!("Making request to CoinGecko API with URL: {}", url);

		let mut request = client.get(url).header("accept", "application/json");

		if self.host.contains("pro-api") {
			request = request.header("x-cg-pro-api-key", self.api_key.as_str());
		} else {
			request = request.header("x-cg-demo-api-key", self.api_key.as_str());
		}

		let response =
			request.send().await.map_err(|e| {
				CoingeckoError(format!("Failed to send request: {}", e.to_string()))
			})?;

		if !response.status().is_success() {
			let result = response.text().await;
			return Err(CoingeckoError(format!(
				"CoinGecko API error: {}",
				result.unwrap_or("Unknown".to_string()).trim()
			)));
		}

		let result = response.json().await;
		result.map_err(|e| CoingeckoError(format!("Could not decode CoinGecko response: {}", e)))
	}

	/// Check API server status
	#[allow(dead_code)]
	pub async fn ping(&self) -> Result<SimplePing, CoingeckoError> {
		self.get("/api/v3/ping").await
	}

	/// Get the current price of any cryptocurrencies vs USD with full precision
	pub async fn price<Id: AsRef<str>>(
		&self,
		ids: &[Id],
		include_market_cap: bool,
		include_24hr_vol: bool,
		include_24hr_change: bool,
		include_last_updated_at: bool,
	) -> Result<HashMap<String, CoingeckoPrice>, CoingeckoError> {
		let ids = ids.iter().map(AsRef::as_ref).collect::<Vec<_>>();
		// We always query for USD
		let vs_currencies = vec!["usd"];
		// We always query for full precision
		let precision = "full";
		let req = format!("/api/v3/simple/price?ids={}&vs_currencies={}&precision={}&include_market_cap={}&include_24hr_vol={}&include_24hr_change={}&include_last_updated_at={}", ids.join("%2C"), vs_currencies.join("%2C"), precision, include_market_cap, include_24hr_vol, include_24hr_change, include_last_updated_at);
		self.get(&req).await
	}
}

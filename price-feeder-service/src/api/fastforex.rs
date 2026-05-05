use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;

use crate::api::error::FastForexError;
use crate::api::Quotation;
use crate::args::FastForexConfig;
use crate::AssetSpecifier;

#[derive(Clone)]
pub struct FastForexPriceApi {
	client: FastForexClient,
}

impl FastForexPriceApi {
	pub fn new(config: FastForexConfig) -> Self {
		let client = FastForexClient::new(config);
		Self { client }
	}

	pub async fn get_prices(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, FastForexError> {
		let pairs: Vec<String> = assets
			.iter()
			.filter_map(|asset| Self::convert_to_pair(asset))
			.collect();

		if pairs.is_empty() {
			return Ok(Vec::new());
		}

		let response = self.client.get_quotes(&pairs).await?;

		let mut quotations = Vec::new();
		for asset in assets {
			if let Some(pair) = Self::convert_to_pair(asset) {
				let pair_key = pair.replace('/', "");
				if let Some(quote) = response.quotes.get(&pair_key) {
					let mid_price = (quote.bid + quote.ask) / Decimal::from(2);
					let quotation = Quotation {
						symbol: asset.symbol.clone(),
						name: asset.symbol.clone(),
						blockchain: Some(asset.blockchain.clone()),
						price: mid_price,
						supply: Decimal::ZERO,
						time: chrono::Utc::now().timestamp().unsigned_abs(),
						provider: crate::types::Aggregator::FastForex,
					};
					quotations.push(quotation);
				}
			}
		}

		Ok(quotations)
	}

	pub fn is_supported(asset: &AssetSpecifier) -> bool {
		Self::convert_to_pair(asset).is_some()
	}

	fn convert_to_pair(asset: &AssetSpecifier) -> Option<String> {
		let symbol = asset.symbol.to_uppercase();
		match symbol.as_str() {
			"EURC" => Some("EURUSD".to_string()),
			_ => None,
		}
	}
}

#[derive(Deserialize, Debug)]
struct FastForexResponse {
	#[serde(alias = "prices")]
	quotes: HashMap<String, FastForexQuote>,
}

#[derive(Deserialize, Debug)]
struct FastForexQuote {
	bid: Decimal,
	ask: Decimal,
}

#[derive(Clone)]
pub struct FastForexClient {
	host: String,
	api_key: String,
}

impl FastForexClient {
	pub fn new(config: FastForexConfig) -> Self {
		FastForexClient { host: config.ff_host_url, api_key: config.ff_api_key }
	}

	async fn get_quotes(
		&self,
		pairs: &[String],
	) -> Result<FastForexResponse, FastForexError> {
		let client = reqwest::Client::new();
		let pairs_str = pairs.join(",");
		let url = format!("{}/fx/quote?pairs={}&api_key={}", self.host, pairs_str, self.api_key);

		let response = client
			.get(&url)
			.send()
			.await
			.map_err(|e| FastForexError(format!("Failed to send request: {}", e)))?;

		if !response.status().is_success() {
			let result = response.text().await.unwrap_or("Unknown".to_string());
			return Err(FastForexError(format!("FastForex API error: {}", result)));
		}

		let quote_response: FastForexResponse = response
			.json()
			.await
			.map_err(|e| FastForexError(format!("Could not decode FastForex response: {}", e)))?;

		Ok(quote_response)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_convert_eurc_to_fx_pair() {
		let asset = AssetSpecifier {
			blockchain: "Base".to_string(),
			symbol: "EURC".to_string(),
		};
		let pair = FastForexPriceApi::convert_to_pair(&asset);
		assert_eq!(pair, Some("EURUSD".to_string()));
	}

	#[tokio::test]
	async fn test_is_supported() {
		let eurc = AssetSpecifier {
			blockchain: "Base".to_string(),
			symbol: "EURC".to_string(),
		};
		assert!(FastForexPriceApi::is_supported(&eurc));

		let xyz = AssetSpecifier {
			blockchain: "Base".to_string(),
			symbol: "XYZ".to_string(),
		};
		assert!(!FastForexPriceApi::is_supported(&xyz));
	}
}

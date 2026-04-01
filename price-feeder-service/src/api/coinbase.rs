use rust_decimal::Decimal;
use serde::Deserialize;

use crate::api::error::CoinbaseError;
use crate::api::Quotation;
use crate::AssetSpecifier;

#[derive(Clone)]
pub struct CoinbasePriceApi {
	client: CoinbaseClient,
}

impl CoinbasePriceApi {
	pub fn new() -> Self {
		let client = CoinbaseClient::new();
		Self { client }
	}

	pub async fn get_prices(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, CoinbaseError> {
		let mut futures = Vec::new();

		for asset in assets {
			let pair = Self::convert_to_pair(asset).ok_or_else(|| {
				CoinbaseError(format!("Unsupported asset: {:?}", asset))
			})?;
			let future = self.client.get_price(pair);
			futures.push((asset.clone(), future));
		}

		let results = futures::future::join_all(
			futures.into_iter().map(|(asset, fut)| async move {
				match fut.await {
					Ok(price_data) => Ok((asset, price_data)),
					Err(e) => Err((asset, e)),
				}
			})
		).await;

		let mut quotations = Vec::new();
		for result in results {
			match result {
				Ok((asset, price_data)) => {
					let quotation = Quotation {
						symbol: asset.symbol.clone(),
						name: asset.symbol.clone(),
						blockchain: Some(asset.blockchain.clone()),
						price: price_data.amount,
						supply: Decimal::ZERO, 
						time: chrono::Utc::now().timestamp().unsigned_abs(),
					};
					quotations.push(quotation);
				}
				Err((asset, e)) => {
					log::error!("Error getting Coinbase price for {:?}: {}", asset, e);
				}
			}
		}

		Ok(quotations)
	}

	pub fn is_supported(asset: &AssetSpecifier) -> bool {
		Self::convert_to_pair(asset).is_some()
	}

	/// Maps the asset to the Coinbase pair.
	/// The API doesn't specify a blockchain filter.
	fn convert_to_pair(asset: &AssetSpecifier) -> Option<String> {
		let symbol = asset.symbol.to_uppercase();
		match symbol.as_str() {
			"EURC" => Some("EURC-USD".to_string()),
			"BRL" => Some("BRL-USD".to_string()),
			"USDC" => Some("USDC-USD".to_string()),
			_ => None,
		}
	}
}

#[derive(Deserialize, Debug)]
struct CoinbasePriceResponse {
	data: CoinbasePriceData,
}

#[derive(Deserialize, Debug)]
struct CoinbasePriceData {
	amount: Decimal,
	base: String,
	currency: String,
}

/// Coinbase network client
#[derive(Clone)]
pub struct CoinbaseClient {
	host: String,
}

impl CoinbaseClient {
	pub fn new() -> Self {
		CoinbaseClient {
			host: "https://api.coinbase.com".to_string(),
		}
	}

	async fn get_price(&self, pair: String) -> Result<CoinbasePriceData, CoinbaseError> {
		let client = reqwest::Client::new();
		let url = format!("{}/v2/prices/{}/spot", self.host, pair);

		let response = client.get(&url).send().await.map_err(|e| {
			CoinbaseError(format!("Failed to send request: {}", e))
		})?;

		if !response.status().is_success() {
			let result = response.text().await.unwrap_or("Unknown".to_string());
			return Err(CoinbaseError(format!("Coinbase API error: {}", result)));
		}

		let price_response: CoinbasePriceResponse = response.json().await.map_err(|e| {
			CoinbaseError(format!("Could not decode Coinbase response: {}", e))
		})?;

		Ok(price_response.data)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_fetching_single_price() {
		let client = CoinbaseClient::new();
		let price_data = client.get_price("USDC-USD".to_string()).await.expect("Should return a price");
		assert_eq!(price_data.base, "USDC");
		assert_eq!(price_data.currency, "USD");
		assert!(price_data.amount > Decimal::ZERO);
	}

	#[tokio::test]
	async fn test_api_returns_prices() {
		let price_api = CoinbasePriceApi::new();

		let eurc_asset = AssetSpecifier { blockchain: "Base".to_string(), symbol: "EURC".to_string() };
		let brl_asset = AssetSpecifier { blockchain: "Base".to_string(), symbol: "BRL".to_string() };
		let usdc_asset = AssetSpecifier { blockchain: "Base".to_string(), symbol: "USDC".to_string() };

		let assets = vec![&eurc_asset, &brl_asset, &usdc_asset];

		let quotations = price_api.get_prices(assets).await.expect("Should return quotations");
		assert_eq!(quotations.len(), 3);

		for quotation in quotations {
			assert!(quotation.price > Decimal::ZERO);
			assert_eq!(quotation.supply, Decimal::ZERO);
		}
	}
}
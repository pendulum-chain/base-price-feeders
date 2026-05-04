use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::api::error::BinanceError;
use crate::types::Quotation;
use crate::AssetSpecifier;

#[derive(Clone)]
pub struct BinancePriceApi {
	client: BinanceClient,
}

impl BinancePriceApi {
	pub fn new() -> Self {
		let client = BinanceClient::default();
		Self { client }
	}

	pub async fn get_prices(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, BinanceError> {
		let mut futures = Vec::new();

		for asset in assets {
			let (pair, invert) = Self::convert_to_pair(asset)
				.ok_or_else(|| BinanceError(format!("Unsupported asset: {:?}", asset)))?;
			let future = self.client.price(pair);
			futures.push((asset.clone(), invert, future));
		}

		let results = futures::future::join_all(futures.into_iter().map(
			|(asset, invert, fut)| async move {
				match fut.await {
					Ok(price_data) => Ok((asset, invert, price_data)),
					Err(e) => Err((asset, e)),
				}
			},
		))
		.await;

		let mut quotations = Vec::new();
		for result in results {
			match result {
				Ok((asset, invert, price_data)) => {
					let mut final_price = price_data.price;
					if invert {
						if final_price.is_zero() {
							log::error!("Cannot invert zero price for {:?}", asset);
							continue;
						}
						final_price = Decimal::ONE / final_price;
					}

					let quotation = Quotation {
						symbol: asset.symbol.clone(),
						name: asset.symbol.clone(),
						blockchain: Some(asset.blockchain.clone()),
						price: final_price,
						supply: Decimal::ZERO,
						time: chrono::Utc::now().timestamp().unsigned_abs(),
						provider: crate::types::Aggregator::Binance,
					};
					quotations.push(quotation);
				},
				Err((asset, e)) => {
					log::error!("Error getting Binance price for {:?}: {}", asset, e);
				},
			}
		}

		Ok(quotations)
	}

	pub fn is_supported(asset: &AssetSpecifier) -> bool {
		Self::convert_to_pair(asset).is_some()
	}

	/// Maps the asset to the Binance pair and an invert flag.
	fn convert_to_pair(asset: &AssetSpecifier) -> Option<(String, bool)> {
		let symbol = asset.symbol.to_uppercase();
		match symbol.as_str() {
			"BRL" | "BRLA" => Some(("USDCBRL".to_string(), true)),
			_ => None,
		}
	}
}

#[derive(Deserialize, Debug)]
pub struct BinancePrice {
	pub symbol: String,
	pub price: Decimal,
}

#[derive(Clone)]
pub struct BinanceClient {
	host: String,
	inner: reqwest::Client,

	call_counter: Arc<AtomicU8>,
}

impl BinanceClient {
	pub fn default() -> Self {
		Self::new("https://api.binance.com".to_string())
	}

	pub fn new(host: String) -> Self {
		let inner = reqwest::Client::new();
		Self { host, inner, call_counter: Arc::new(AtomicU8::new(0)) }
	}

	async fn get<R: DeserializeOwned>(&self, endpoint: &str) -> Result<R, BinanceError> {
		let url = reqwest::Url::parse(
			format!("{host}/{ep}", host = self.host.as_str(), ep = endpoint).as_str(),
		)
		.expect("Invalid URL");

		// Mock failure window: succeed on calls up to 10, fail on calls 20-30, then
		// recover 
		let prev = self.call_counter.fetch_update(
			Ordering::SeqCst,
			Ordering::SeqCst,
			|c| Some(c.saturating_add(1)),
		).unwrap_or(0);
		let call_number = prev.saturating_add(1);
		if (20..30).contains(&call_number) {
			return Err(BinanceError(format!(
				"Simulated API failure (mock, call #{})",
				call_number
			)));
		}

		let response = self
			.inner
			.get(url)
			.send()
			.await
			.map_err(|e| BinanceError(format!("Failed to send request: {}", e.to_string())))?;

		if !response.status().is_success() {
			let result = response.text().await;
			return Err(BinanceError(format!(
				"Binance API error: {}",
				result.unwrap_or("Unknown".to_string()).trim()
			)));
		}

		let result = response.json().await;
		result.map_err(|e| BinanceError(format!("Could not decode Binance response: {}", e)))
	}

	pub async fn price(&self, symbol: String) -> Result<BinancePrice, BinanceError> {
		let endpoint = format!("api/v3/ticker/price?symbol={}", symbol);
		let response: BinancePrice = self.get(&endpoint).await?;
		Ok(response)
	}
}

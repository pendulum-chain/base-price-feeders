use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::api::error::BinanceError;
use crate::types::Quotation;
use crate::AssetSpecifier;

#[derive(Clone)]
pub struct BinancePriceApi {
	client: BinanceClient,
	brl_bps_adjustment: i64,
}

impl BinancePriceApi {
	pub fn new(brl_bps_adjustment: i64) -> Self {
		let client = BinanceClient::default();
		Self { client, brl_bps_adjustment }
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

						if self.brl_bps_adjustment != 0 {
							let bp = Decimal::from(self.brl_bps_adjustment.unsigned_abs());
							let adjustment = bp * final_price / Decimal::from(10_000u64);
							if self.brl_bps_adjustment > 0 {
								final_price = final_price
									.checked_sub(adjustment)
									.unwrap_or(final_price);
							} else {
								final_price = final_price
									.checked_add(adjustment)
									.unwrap_or(final_price);
							}
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
}

impl BinanceClient {
	pub fn default() -> Self {
		Self::new("https://api.binance.com".to_string())
	}

	pub fn new(host: String) -> Self {
		let inner = reqwest::Client::new();
		Self { host, inner }
	}

	async fn get<R: DeserializeOwned>(&self, endpoint: &str) -> Result<R, BinanceError> {
		let url = reqwest::Url::parse(
			format!("{host}/{ep}", host = self.host.as_str(), ep = endpoint).as_str(),
		)
		.map_err(|e| BinanceError(format!("Invalid URL: {}", e)))?;

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

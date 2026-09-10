use crate::api::binance::BinancePriceApi;
use crate::api::coinbase::CoinbasePriceApi;
use crate::api::coingecko::CoingeckoPriceApi;
use crate::api::error::{BinanceError, CoinbaseError, CoingeckoError, FastForexError};
use crate::api::fastforex::FastForexPriceApi;
use crate::args::{CoingeckoConfig, FastForexConfig};
use crate::types::{Aggregator, Quotation};
use crate::AssetSpecifier;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub mod binance;
pub mod coinbase;
pub mod coingecko;
pub mod error;
pub mod fastforex;

#[derive(Debug, Default)]
pub struct QuotationsOutcome {
	pub provider: Aggregator,
	pub quotations: Vec<Quotation>,
	pub had_error: bool,
}

pub type QuotationsFuture<'a> = Pin<Box<dyn Future<Output = QuotationsOutcome> + Send + 'a>>;
pub type AssetsByProvider<'a> = HashMap<Aggregator, Vec<&'a AssetSpecifier>>;

pub trait PriceApi {
	fn get_quotation_futures<'a>(
		&'a self,
		assets_by_provider: AssetsByProvider<'a>,
	) -> Vec<QuotationsFuture<'a>>;
}

pub struct PriceApiImpl {
	binance_price_api: BinancePriceApi,
	coinbase_price_api: CoinbasePriceApi,
	coingecko_price_api: CoingeckoPriceApi,
	fastforex_price_api: FastForexPriceApi,
}

impl PriceApiImpl {
	pub fn new(
		coingecko_config: CoingeckoConfig,
		fastforex_config: FastForexConfig,
		brl_bps_adjustment: i64,
	) -> Self {
		Self {
			binance_price_api: BinancePriceApi::new(brl_bps_adjustment),
			coinbase_price_api: CoinbasePriceApi::new(),
			coingecko_price_api: CoingeckoPriceApi::new_from_config(coingecko_config),
			fastforex_price_api: FastForexPriceApi::new(fastforex_config),
		}
	}
}

impl PriceApi for PriceApiImpl {
	fn get_quotation_futures<'a>(
		&'a self,
		mut assets_by_provider: AssetsByProvider<'a>,
	) -> Vec<QuotationsFuture<'a>> {
		let binance_assets: Vec<&AssetSpecifier> = assets_by_provider
			.remove(&Aggregator::Binance)
			.unwrap_or_default()
			.into_iter()
			.filter(|asset| BinancePriceApi::is_supported(asset))
			.collect();

		let coinbase_assets: Vec<&AssetSpecifier> = assets_by_provider
			.remove(&Aggregator::Coinbase)
			.unwrap_or_default()
			.into_iter()
			.filter(|asset| CoinbasePriceApi::is_supported(asset))
			.collect();

		let coingecko_assets: Vec<&AssetSpecifier> = assets_by_provider
			.remove(&Aggregator::Coingecko)
			.unwrap_or_default()
			.into_iter()
			.filter(|asset| CoingeckoPriceApi::is_supported(asset))
			.collect();

		let fastforex_assets: Vec<&AssetSpecifier> = assets_by_provider
			.remove(&Aggregator::FastForex)
			.unwrap_or_default()
			.into_iter()
			.filter(|asset| FastForexPriceApi::is_supported(asset))
			.collect();

		let mut futures: Vec<QuotationsFuture<'a>> = Vec::new();

		if !binance_assets.is_empty() {
			futures.push(Box::pin(async move {
				match self.get_binance_quotations(binance_assets).await {
					Ok(quotations) => QuotationsOutcome {
						provider: Aggregator::Binance,
						quotations,
						had_error: false,
					},
					Err(e) => {
						log::error!("Error getting Binance quotations: {}", e);
						QuotationsOutcome {
							provider: Aggregator::Binance,
							quotations: Vec::new(),
							had_error: true,
						}
					},
				}
			}));
		}

		if !coinbase_assets.is_empty() {
			futures.push(Box::pin(async move {
				match self.get_coinbase_quotations(coinbase_assets).await {
					Ok(quotations) => QuotationsOutcome {
						provider: Aggregator::Coinbase,
						quotations,
						had_error: false,
					},
					Err(e) => {
						log::error!("Error getting Coinbase quotations: {}", e);
						QuotationsOutcome {
							provider: Aggregator::Coinbase,
							quotations: Vec::new(),
							had_error: true,
						}
					},
				}
			}));
		}

		if !coingecko_assets.is_empty() {
			futures.push(Box::pin(async move {
				match self.get_coingecko_quotations(coingecko_assets).await {
					Ok(quotations) => QuotationsOutcome {
						provider: Aggregator::Coingecko,
						quotations,
						had_error: false,
					},
					Err(e) => {
						log::error!("Error getting CoinGecko quotations: {:?}", e);
						QuotationsOutcome {
							provider: Aggregator::Coingecko,
							quotations: Vec::new(),
							had_error: true,
						}
					},
				}
			}));
		}

		if !fastforex_assets.is_empty() {
			futures.push(Box::pin(async move {
				match self.get_fastforex_quotations(fastforex_assets).await {
					Ok(quotations) => QuotationsOutcome {
						provider: Aggregator::FastForex,
						quotations,
						had_error: false,
					},
					Err(e) => {
						log::error!("Error getting FastForex quotations: {}", e);
						QuotationsOutcome {
							provider: Aggregator::FastForex,
							quotations: Vec::new(),
							had_error: true,
						}
					},
				}
			}));
		}

		futures
	}
}

impl PriceApiImpl {
	async fn get_binance_quotations(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, BinanceError> {
		let quotations = self.binance_price_api.get_prices(assets).await?;
		Ok(quotations)
	}

	async fn get_coinbase_quotations(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, CoinbaseError> {
		let quotations = self.coinbase_price_api.get_prices(assets).await?;
		Ok(quotations)
	}

	async fn get_coingecko_quotations(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, CoingeckoError> {
		let quotations = self.coingecko_price_api.get_prices(assets).await?;
		Ok(quotations)
	}

	async fn get_fastforex_quotations(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> Result<Vec<Quotation>, FastForexError> {
		let quotations = self.fastforex_price_api.get_prices(assets).await?;
		Ok(quotations)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn api() -> PriceApiImpl {
		PriceApiImpl::new(
			CoingeckoConfig {
				cg_api_key: String::new(),
				cg_host_url: "https://example.invalid".into(),
			},
			FastForexConfig {
				ff_api_key: String::new(),
				ff_host_url: "https://example.invalid".into(),
			},
			0,
		)
	}

	#[test]
	fn builds_requests_only_for_providers_selected_by_the_hierarchy() {
		let api = api();
		let eurc = AssetSpecifier { blockchain: "Base".into(), symbol: "EURC".into() };
		let assets_by_provider = HashMap::from([(Aggregator::FastForex, vec![&eurc])]);

		let futures = api.get_quotation_futures(assets_by_provider);

		assert_eq!(futures.len(), 1);
	}
}

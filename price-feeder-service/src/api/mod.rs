use crate::api::binance::BinancePriceApi;
use crate::api::coinbase::CoinbasePriceApi;
use crate::api::coingecko::CoingeckoPriceApi;
use crate::api::error::{BinanceError, CoinbaseError, CoingeckoError, FastForexError};
use crate::api::fastforex::FastForexPriceApi;
use crate::args::{CoingeckoConfig, FastForexConfig};
use crate::types::Quotation;
use crate::AssetSpecifier;
use async_trait::async_trait;

pub mod binance;
pub mod coinbase;
pub mod coingecko;
pub mod error;
pub mod fastforex;

#[async_trait]
pub trait PriceApi {
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation>;
}

pub struct PriceApiImpl {
	binance_price_api: BinancePriceApi,
	coinbase_price_api: CoinbasePriceApi,
	coingecko_price_api: CoingeckoPriceApi,
	fastforex_price_api: FastForexPriceApi,
}

impl PriceApiImpl {
	pub fn new(coingecko_config: CoingeckoConfig, fastforex_config: FastForexConfig) -> Self {
		Self {
			binance_price_api: BinancePriceApi::new(),
			coinbase_price_api: CoinbasePriceApi::new(),
			coingecko_price_api: CoingeckoPriceApi::new_from_config(coingecko_config),
			fastforex_price_api: FastForexPriceApi::new(fastforex_config),
		}
	}
}

#[async_trait]
impl PriceApi for PriceApiImpl {
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation> {
		let mut quotations = Vec::new();

		let binance_assets: Vec<&AssetSpecifier> = assets
			.iter()
			.copied()
			.filter(|asset| BinancePriceApi::is_supported(asset))
			.collect();

		let binance_quotes = self.get_binance_quotations(binance_assets).await;
		match binance_quotes {
			Ok(binance_quotes) => quotations.extend(binance_quotes),
			Err(e) => log::error!("Error getting Binance quotations: {}", e),
		}

		let coinbase_assets: Vec<&AssetSpecifier> = assets
			.iter()
			.copied()
			.filter(|asset| CoinbasePriceApi::is_supported(asset))
			.collect();

		let coinbase_quotes = self.get_coinbase_quotations(coinbase_assets).await;
		match coinbase_quotes {
			Ok(coinbase_quotes) => quotations.extend(coinbase_quotes),
			Err(e) => log::error!("Error getting Coinbase quotations: {}", e),
		}

		let coingecko_assets: Vec<&AssetSpecifier> = assets
			.iter()
			.copied()
			.filter(|asset| CoingeckoPriceApi::is_supported(asset))
			.collect();
		let coingecko_quotes = self.get_coingecko_quotations(coingecko_assets).await;
		match coingecko_quotes {
			Ok(coingecko_quotes) => quotations.extend(coingecko_quotes),
			Err(e) => log::error!("Error getting CoinGecko quotations: {:?}", e),
		}

		let fastforex_assets: Vec<&AssetSpecifier> = assets
			.iter()
			.copied()
			.filter(|asset| FastForexPriceApi::is_supported(asset))
			.collect();

		let fastforex_quotes = self.get_fastforex_quotations(fastforex_assets).await;
		match fastforex_quotes {
			Ok(fastforex_quotes) => quotations.extend(fastforex_quotes),
			Err(e) => log::error!("Error getting FastForex quotations: {}", e),
		}

		quotations
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

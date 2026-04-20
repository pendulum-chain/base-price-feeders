use crate::api::coinbase::CoinbasePriceApi;
use crate::api::coingecko::CoingeckoPriceApi;
use crate::api::custom::CustomPriceApi;
use crate::api::error::{CoinbaseError, CoingeckoError, CustomError};
use crate::args::CoingeckoConfig;
use crate::types::Quotation;
use crate::AssetSpecifier;
use async_trait::async_trait;

pub mod coinbase;
pub mod coingecko;
pub mod custom;
pub mod error;

#[async_trait]
pub trait PriceApi {
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation>;
}

pub struct PriceApiImpl {
	coinbase_price_api: CoinbasePriceApi,
	coingecko_price_api: CoingeckoPriceApi,
	custom_price_api: CustomPriceApi,
}

impl PriceApiImpl {
	pub fn new(config: CoingeckoConfig) -> Self {
		Self {
			coinbase_price_api: CoinbasePriceApi::new(),
			coingecko_price_api: CoingeckoPriceApi::new_from_config(config),
			custom_price_api: CustomPriceApi::new(),
		}
	}
}

#[async_trait]
impl PriceApi for PriceApiImpl {
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation> {
		let mut quotations = Vec::new();

		let custom_assets: Vec<&AssetSpecifier> =
			assets.iter().copied().filter(|asset| self.custom_price_api.is_supported(asset)).collect();

		let (custom_quotes, custom_quote_errors) =
			self.get_custom_quotations(custom_assets).await;

		quotations.extend(custom_quotes);
		for error in custom_quote_errors {
			log::error!("Error getting custom quotation: {}", error);
		}

		let coinbase_assets: Vec<&AssetSpecifier> =
			assets.iter().copied().filter(|asset| CoinbasePriceApi::is_supported(asset)).collect();

		let coinbase_quotes = self.get_coinbase_quotations(coinbase_assets).await;
		match coinbase_quotes {
			Ok(coinbase_quotes) => quotations.extend(coinbase_quotes),
			Err(e) => log::error!("Error getting Coinbase quotations: {}", e),
		}

		let coingecko_assets: Vec<_> = assets
			.into_iter()
			.filter(|asset| CoingeckoPriceApi::is_supported(asset))
			.collect();
		let coingecko_quotes = self.get_coingecko_quotations(coingecko_assets).await;
		match coingecko_quotes {
			Ok(coingecko_quotes) => quotations.extend(coingecko_quotes),
			Err(e) => log::error!("Error getting CoinGecko quotations: {:?}", e),
		}

		quotations
	}
}

impl PriceApiImpl {
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

	async fn get_custom_quotations(
		&self,
		assets: Vec<&AssetSpecifier>,
	) -> (Vec<Quotation>, Vec<CustomError>) {
		let mut quotations = Vec::new();
		let mut errors = Vec::new();

		for asset in assets {
			let quotation_result = self.custom_price_api.get_price(asset).await;
			match quotation_result {
				Ok(quotation) => quotations.push(quotation),
				Err(e) => errors.push(e),
			};
		}

		(quotations, errors)
	}
}

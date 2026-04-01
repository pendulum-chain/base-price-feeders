use crate::api::coinbase::CoinbasePriceApi;
use crate::api::custom::CustomPriceApi;
use crate::api::error::{CoinbaseError, CustomError};
use crate::types::Quotation;
use crate::AssetSpecifier;
use async_trait::async_trait;

mod coinbase;
mod custom;
mod error;

#[async_trait]
pub trait PriceApi {
	/// A method to get quotations for a list of assets.
	/// The method will return a list of quotations for the assets that are supported by the API.
	/// If an asset is not supported, the method will log an error and continue.
	/// The method will return an empty list if no quotations are available.
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation>;
}

pub struct PriceApiImpl {
	coinbase_price_api: CoinbasePriceApi,
	custom_price_api: CustomPriceApi,
}

impl PriceApiImpl {
	pub fn new() -> Self {
		Self {
			coinbase_price_api: CoinbasePriceApi::new(),
			custom_price_api: CustomPriceApi::new(),
		}
	}
}

#[async_trait]
impl PriceApi for PriceApiImpl {
	async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation> {
		let mut quotations = Vec::new();

		// Split all assets into custom vs other assets. This is important because it could happen that
		// a custom asset is also supported by another API Impl. We want to always select the custom implementation.
		let (custom_assets, assets): (Vec<&AssetSpecifier>, Vec<&AssetSpecifier>) =
			assets.into_iter().partition(|asset| self.custom_price_api.is_supported(asset));

		let (custom_quotes, custom_quote_errors) =
			self.get_custom_quotations(custom_assets.clone()).await;

		quotations.extend(custom_quotes);
		for error in custom_quote_errors {
			log::error!("Error getting custom quotation: {}", error);
		}

		let coinbase_assets = assets
			.into_iter()
			.filter(|asset| CoinbasePriceApi::is_supported(asset))
			.collect::<Vec<_>>();

		let coinbase_assets: Vec<_> = coinbase_assets.clone().into_iter().filter(|asset| CoinbasePriceApi::is_supported(asset)).collect();
		let coinbase_quotes = self.get_coinbase_quotations(coinbase_assets).await;
		match coinbase_quotes {
			Ok(coinbase_quotes) => quotations.extend(coinbase_quotes),
			Err(e) => log::error!("Error getting Coinbase quotations: {}", e),
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

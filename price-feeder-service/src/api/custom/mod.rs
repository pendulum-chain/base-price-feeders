use crate::api::error::CustomError;
use crate::types::Quotation;
use crate::AssetSpecifier;
use async_trait::async_trait;
use std::string::ToString;

#[async_trait]
pub trait AssetCompatibility: Send + Sync {
	fn supports(&self, asset: &AssetSpecifier) -> bool;

	async fn get_price(&self, asset: &AssetSpecifier) -> Result<Quotation, CustomError>;
}

pub struct CustomPriceApi {
	apis: Vec<Box<dyn AssetCompatibility>>,
}

impl CustomPriceApi {
	pub fn new() -> Self {
		CustomPriceApi { apis: vec![] }
	}

	pub async fn get_price(&self, asset: &AssetSpecifier) -> Result<Quotation, CustomError> {
		let api = self
			.get_supported_api(asset)
			.ok_or(CustomError("Unsupported asset".to_string()))?;
		api.get_price(asset).await
	}

	pub fn is_supported(&self, asset: &AssetSpecifier) -> bool {
		self.get_supported_api(asset).is_some()
	}

	/// Iterates over all supported APIs and returns the first one that supports the given asset.
	fn get_supported_api(&self, asset: &AssetSpecifier) -> Option<&Box<dyn AssetCompatibility>> {
		for api in &self.apis {
			if api.supports(asset) {
				return Some(api);
			}
		}

		None
	}
}

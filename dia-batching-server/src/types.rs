use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// This struct is used to identify a specific asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct AssetSpecifier {
	pub blockchain: String,
	pub symbol: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Quotation {
	#[serde(rename(deserialize = "Symbol"))]
	pub symbol: String,
	#[serde(rename(deserialize = "Name"))]
	pub name: String,
	#[serde(rename(deserialize = "Blockchain"))]
	pub blockchain: Option<String>,
	#[serde(rename(deserialize = "Price"))]
	pub price: Decimal,
	#[serde(rename(deserialize = "Supply"))]
	pub supply: Decimal,
	#[serde(rename(deserialize = "Time"))]
	pub time: u64,
}

/// This struct is used to store information about a coin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoinInfo {
	pub symbol: SmolStr,
	pub name: SmolStr,
	pub blockchain: SmolStr,
	pub supply: u128,
	pub last_update_timestamp: u64,
	pub price: u128,
}

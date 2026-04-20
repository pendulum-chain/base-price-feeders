use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Aggregator {
	#[default]
	Unknown,
	Coinbase,
	Coingecko,
	Pyth,
	Custom(String),
}

impl From<&str> for Aggregator {
	fn from(s: &str) -> Self {
		match s.to_lowercase().as_str() {
			"coinbase" => Aggregator::Coinbase,
			"coingecko" => Aggregator::Coingecko,
			"pyth" => Aggregator::Pyth,
			_ => Aggregator::Custom(s.to_string()),
		}
	}
}

impl std::fmt::Display for Aggregator {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Aggregator::Unknown => write!(f, "unknown"),
			Aggregator::Coinbase => write!(f, "coinbase"),
			Aggregator::Coingecko => write!(f, "coingecko"),
			Aggregator::Pyth => write!(f, "pyth"),
			Aggregator::Custom(s) => write!(f, "{}", s),
		}
	}
}

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
	#[serde(default)]
	pub provider: Aggregator,
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
	pub provider: Aggregator,
}

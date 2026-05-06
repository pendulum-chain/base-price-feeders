use crate::types::Aggregator;
use alloy::primitives::Address;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ProviderHierarchy {
	pub default: Vec<Aggregator>,
	pub per_asset: HashMap<String, Vec<Aggregator>>,
	pub disable_on_exhaustion: HashMap<String, bool>,
}

// Default hierarchy: Binance > Nothing for BRL/BRLA,
// Coinbase > Coingecko > Pyth for everything else.
impl Default for ProviderHierarchy {
	fn default() -> Self {
		let mut per_asset = HashMap::new();
		per_asset.insert("BRLA".to_string(), vec![Aggregator::Binance]);
		per_asset.insert("BRL".to_string(), vec![Aggregator::Binance]);

		per_asset.insert(
			"EURC".to_string(),
			vec![Aggregator::FastForex, Aggregator::Coinbase, Aggregator::Coingecko, Aggregator::Pyth],
		);

		let mut disable_on_exhaustion = HashMap::new();
		disable_on_exhaustion.insert("BRLA".to_string(), true);
		disable_on_exhaustion.insert("BRL".to_string(), true);

		Self {
			default: vec![Aggregator::Coinbase, Aggregator::Coingecko, Aggregator::Pyth],
			per_asset,
			disable_on_exhaustion,
		}
	}
}

pub fn get_asset_address(symbol: &str) -> Option<Address> {
	let raw = match symbol {
		"USDC" => Some("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
		"EURC" => Some("0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42"),
		"BRL" | "BRLA" => Some("0xfCB34c47f850f452C15EA1B84d51231C38A61783"),
		_ => None,
	}?;
	raw.parse::<Address>().ok()
}

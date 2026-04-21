use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Default, Clone)]
pub struct CoinInfoStorage {
	pub timeframes: Arc<RwLock<HashMap<String, CoinInfo>>>, // Key: token_blockchain_provider
}

impl CoinInfoStorage {
	pub fn get_currencies_by_blockchains_and_symbols(
		&self,
		specs: Vec<AssetSpecifier>,
	) -> Vec<CoinInfo> {
		let mut result = Vec::new();
		for spec in specs {
			let mut selected_tf =
				self.get_timeframe(&spec.symbol, &spec.blockchain, Aggregator::Coinbase);
			if selected_tf.is_none() {
				selected_tf =
					self.get_timeframe(&spec.symbol, &spec.blockchain, Aggregator::Coingecko);
			}
			if selected_tf.is_none() {
				selected_tf = self.get_timeframe(&spec.symbol, "unknown", Aggregator::Pyth);
			}
			if let Some(tf) = selected_tf {
				result.push(tf);
			}
		}
		result
	}

	pub fn update_timeframe(&self, coin_info: CoinInfo) {
		let key = format!("{}_{}_{}", coin_info.symbol, coin_info.blockchain, coin_info.provider);
		self.timeframes.write().unwrap().insert(key, coin_info);
	}

	pub fn get_timeframe(
		&self,
		token: &str,
		blockchain: &str,
		provider: Aggregator,
	) -> Option<CoinInfo> {
		let key = format!("{}_{}_{}", token, blockchain, provider);
		self.timeframes.read().unwrap().get(&key).cloned()
	}
}

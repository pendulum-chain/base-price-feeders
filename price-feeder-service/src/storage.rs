use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

#[derive(Default, Clone)]
pub struct CoinInfoStorage {
	pub currencies: Arc<RwLock<HashMap<String, CoinInfo>>>,
	pub timeframes: Arc<RwLock<HashMap<String, CoinInfo>>>, // Key: token_blockchain_provider
}

impl CoinInfoStorage {
	pub fn get_currencies(&self) -> Vec<CoinInfo> {
		self.currencies.read().unwrap().values().cloned().collect()
	}

	pub fn get_currency(&self, symbol: &str) -> Option<CoinInfo> {
		self.currencies.read().unwrap().get(symbol).cloned()
	}

	pub fn get_currencies_by_blockchains_and_symbols(
		&self,
		specs: Vec<AssetSpecifier>,
	) -> Vec<CoinInfo> {
		let lock = self.currencies.read().unwrap();
		specs.into_iter().filter_map(|s| lock.get(&s.symbol).cloned()).collect()
	}

	pub fn replace_currencies_by_symbols(&self, new_currencies: Vec<CoinInfo>) {
		let mut lock = self.currencies.write().unwrap();
		for currency in new_currencies {
			lock.insert(currency.symbol.to_string(), currency);
		}
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

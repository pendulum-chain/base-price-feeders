use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Maximum age in seconds for a stored entry before it is considered stale.
/// Should be at least a few multiples of the fetch cadence to account for
/// brief delays, GC pauses, etc.
pub const MAX_ENTRY_AGE_SECS: u64 = 2;

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
			if let Some(mut tf) = selected_tf {
				tf.blockchain = spec.blockchain.clone().into();
				result.push(tf);
			}
		}
		result
	}

	pub fn update_timeframe(&self, coin_info: CoinInfo) {
		let key = format!("{}_{}_{}", coin_info.symbol, coin_info.blockchain, coin_info.provider);
		self.timeframes.write().unwrap().insert(key, coin_info);
	}

	/// Returns the stored entry only if it is fresher than `MAX_ENTRY_AGE_SECS`.
	pub fn get_timeframe(
		&self,
		token: &str,
		blockchain: &str,
		provider: Aggregator,
	) -> Option<CoinInfo> {
		let tf = self.get_timeframe_any(token, blockchain, provider)?;
		let now = chrono::Utc::now().timestamp().unsigned_abs();
		if now.saturating_sub(tf.last_update_timestamp) > MAX_ENTRY_AGE_SECS {
			None
		} else {
			Some(tf)
		}
	}

	/// Returns the stored entry regardless of its age.
	pub fn get_timeframe_any(
		&self,
		token: &str,
		blockchain: &str,
		provider: Aggregator,
	) -> Option<CoinInfo> {
		let key = format!("{}_{}_{}", token, blockchain, provider);
		self.timeframes.read().unwrap().get(&key).cloned()
	}
}

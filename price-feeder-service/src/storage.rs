use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use log::debug;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const MAX_AGE_MULTIPLIER_PERCENT: u64 = 200; // 200% of the update interval, should cover a missed fetch cycle.

#[derive(Clone)]
pub struct CoinInfoStorage {
	pub timeframes: Arc<RwLock<HashMap<String, CoinInfo>>>,
	max_age_ms: Arc<AtomicU64>,
}

impl Default for CoinInfoStorage {
	fn default() -> Self {
		Self::new(Duration::from_secs(3600))
	}
}

impl CoinInfoStorage {
	pub fn new(update_interval: std::time::Duration) -> Self {
		let millis = update_interval.as_millis() as u64;
		let max_age = (millis * MAX_AGE_MULTIPLIER_PERCENT + 99) / 100;
		Self {
			timeframes: Arc::new(RwLock::new(HashMap::new())),
			max_age_ms: Arc::new(AtomicU64::new(max_age)),
		}
	}

	pub fn set_update_interval(&self, update_interval: std::time::Duration) {
		let millis = update_interval.as_millis() as u64;
		let max_age = (millis * MAX_AGE_MULTIPLIER_PERCENT + 99) / 100;
		self.max_age_ms.store(max_age, Ordering::Relaxed);
	}

	pub fn max_entry_age_ms(&self) -> u64 {
		self.max_age_ms.load(Ordering::Relaxed)
	}

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

	/// Returns the stored entry only if it is fresher than the configured
	/// freshness window (see `max_entry_age_ms`).
	pub fn get_timeframe(
		&self,
		token: &str,
		blockchain: &str,
		provider: Aggregator,
	) -> Option<CoinInfo> {
		let tf = match self.get_timeframe_any(token, blockchain, provider.clone()) {
			Some(tf) => tf,
			None => {
				debug!(
					"get_timeframe: no entry for {}_{}_{}",
					token, blockchain, provider
				);
				return None;
			}
		};
		let now = chrono::Utc::now().timestamp_millis() as u64;
		let age = now.saturating_sub(tf.last_update_timestamp);
		let max_age = self.max_entry_age_ms();
		if age > max_age {
			debug!(
				"get_timeframe: rejecting {}_{}_{} - age {}ms > max {}ms",
				token, blockchain, provider, age, max_age
			);
			None
		} else {
			debug!(
				"get_timeframe: accepting {}_{}_{} - age {}ms <= max {}ms",
				token, blockchain, provider, age, max_age
			);
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

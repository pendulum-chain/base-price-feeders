use crate::types::{Aggregator, CoinInfo};
use crate::AssetSpecifier;
use log::info;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub const MAX_AGE_MULTIPLIER_PERCENT: u64 = 120;

#[derive(Clone)]
pub struct CoinInfoStorage {
	pub timeframes: Arc<RwLock<HashMap<String, CoinInfo>>>,
	max_age_secs: Arc<AtomicU64>,
}

impl CoinInfoStorage {
	pub fn new(update_interval: std::time::Duration) -> Self {
		let secs = update_interval.as_secs();
		let max_age = (secs * MAX_AGE_MULTIPLIER_PERCENT + 99) / 100;
		Self {
			timeframes: Arc::new(RwLock::new(HashMap::new())),
			max_age_secs: Arc::new(AtomicU64::new(max_age)),
		}
	}

	pub fn set_update_interval(&self, update_interval: std::time::Duration) {
		let secs = update_interval.as_secs();
		let max_age = (secs * MAX_AGE_MULTIPLIER_PERCENT + 99) / 100;
		self.max_age_secs.store(max_age, Ordering::Relaxed);
	}

	pub fn max_entry_age_secs(&self) -> u64 {
		self.max_age_secs.load(Ordering::Relaxed)
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
	/// freshness window (see `max_entry_age_secs`).
	pub fn get_timeframe(
		&self,
		token: &str,
		blockchain: &str,
		provider: Aggregator,
	) -> Option<CoinInfo> {
		let tf = self.get_timeframe_any(token, blockchain, provider)?;
		let now = chrono::Utc::now().timestamp().unsigned_abs();
		if now.saturating_sub(tf.last_update_timestamp) > self.max_entry_age_secs() {
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

	pub fn log_average_feed_age(&self) {
		let map = self.timeframes.read().unwrap();
		if map.is_empty() {
			return;
		}
		let now = chrono::Utc::now().timestamp().unsigned_abs();
		let total_age: u64 = map.values().map(|tf| now.saturating_sub(tf.last_update_timestamp)).sum();
		let avg_age_secs = total_age / map.len() as u64;
		info!("Average feed age: {}s ({} entries)", avg_age_secs, map.len());
	}
}

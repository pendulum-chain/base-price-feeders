use crate::types::Aggregator;
use alloy::primitives::Address;
use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc, Weekday};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

// ── Time window ───────────────────────────────────────────────────────────────

/// A recurring weekly time window in a given timezone.
///
/// If the window wraps across the week boundary (e.g. Friday 22:00 → Monday
/// 03:00) the `start_day` minutes will be greater than the `end_day` minutes
/// and the `contains` check handles the wrap.
#[derive(Debug, Clone)]
pub struct TimeWindow {
	pub start_day: Weekday,
	pub start_hour: u32,
	pub start_minute: u32,
	pub end_day: Weekday,
	pub end_hour: u32,
	pub end_minute: u32,
	/// UTC offset string, e.g. `"+00:00"` or `"-03:00"`.
	pub timezone_offset: String,
}

impl TimeWindow {
	/// Returns `true` if the given UTC timestamp falls inside this window.
	pub fn contains(&self, now: DateTime<Utc>) -> bool {
		let offset = match parse_utc_offset(&self.timezone_offset) {
			Some(o) => o,
			None => return false,
		};
		let local = now.with_timezone(&offset);
		let weekday = local.weekday().num_days_from_monday();
		let current = weekday * 24 * 60 + local.hour() * 60 + local.minute();

		let start = self.start_day.num_days_from_monday() * 24 * 60
			+ self.start_hour * 60 + self.start_minute;
		let end = self.end_day.num_days_from_monday() * 24 * 60
			+ self.end_hour * 60 + self.end_minute;

		if start <= end {
			current >= start && current < end
		} else {
			// Window wraps around the week boundary
			current >= start || current < end
		}
	}
}


fn parse_utc_offset(s: &str) -> Option<FixedOffset> {
	let (sign, rest) = if let Some(rest) = s.strip_prefix('-') {
		(-1, rest)
	} else if let Some(rest) = s.strip_prefix('+') {
		(1, rest)
	} else {
		return None;
	};
	let (hours_str, minutes_str) = rest.split_once(':')?;
	let hours: i32 = hours_str.parse().ok()?;
	let minutes: i32 = minutes_str.parse().ok()?;
	let secs = sign * (hours * 3600 + minutes * 60);
	FixedOffset::east_opt(secs)
}

/// A single entry in a provider hierarchy.
/// An optional [`TimeWindow`] restricts this entry to a specific weekly time
/// slot.  An entry **without** a window (`None`) is always eligible.
#[derive(Debug, Clone)]
pub struct HierarchyEntry {
	pub aggregator: Aggregator,
	pub window: Option<TimeWindow>,
}

impl HierarchyEntry {
	pub fn new(aggregator: Aggregator) -> Self {
		Self { aggregator, window: None }
	}

	pub fn with_window(aggregator: Aggregator, window: TimeWindow) -> Self {
		Self { aggregator, window: Some(window) }
	}
}


#[derive(Debug, Clone)]
pub struct ProviderHierarchy {
	pub default: Vec<HierarchyEntry>,
	pub per_asset: HashMap<String, Vec<HierarchyEntry>>,
	pub disable_on_exhaustion: HashMap<String, bool>,
}

#[derive(Debug, Clone)]
pub struct ProviderHierarchyError {
	message: String,
}

impl ProviderHierarchyError {
	fn invalid_timezone_offset(asset_symbol: &str, aggregator: Aggregator, offset: &str) -> Self {
		Self {
			message: format!(
				"invalid timezone offset '{offset}' for asset '{asset_symbol}' and aggregator '{aggregator}' (expected +HH:MM or -HH:MM)"
			),
		}
	}
}

impl fmt::Display for ProviderHierarchyError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.message)
	}
}

impl Error for ProviderHierarchyError {}

impl ProviderHierarchy {
	pub fn validate(&self) -> Result<(), ProviderHierarchyError> {
		for entry in &self.default {
			if let Some(window) = &entry.window {
				if parse_utc_offset(&window.timezone_offset).is_none() {
					return Err(ProviderHierarchyError::invalid_timezone_offset(
						"default",
						entry.aggregator.clone(),
						&window.timezone_offset,
					));
				}
			}
		}

		for (asset_symbol, entries) in &self.per_asset {
			for entry in entries {
				if let Some(window) = &entry.window {
					if parse_utc_offset(&window.timezone_offset).is_none() {
						return Err(ProviderHierarchyError::invalid_timezone_offset(
							asset_symbol,
							entry.aggregator.clone(),
							&window.timezone_offset,
						));
					}
				}
			}
		}

		Ok(())
	}

	/// Returns the hierarchy for `asset_symbol`, keeping only entries whose
	/// [`TimeWindow`] (if any) contains `now`.
	pub fn get_hierarchy(&self, asset_symbol: &str, now: DateTime<Utc>) -> Vec<&HierarchyEntry> {
		let entries = self.per_asset.get(asset_symbol).unwrap_or(&self.default);
		entries
			.iter()
			.filter(|entry| entry.window.as_ref().map_or(true, |w| w.contains(now)))
			.collect()
	}
}

// Default hierarchy:
//   BRL/BRLA  → Binance (always)
//   EURC      → FastForex during Forex market hours / Coinbase outside  (time‑based)
//   default   → Coinbase > Coingecko > Pyth
impl Default for ProviderHierarchy {
	fn default() -> Self {
		let mut per_asset = HashMap::new();
		per_asset.insert("BRLA".to_string(), vec![HierarchyEntry::new(Aggregator::Binance)]);
		per_asset.insert("BRL".to_string(), vec![HierarchyEntry::new(Aggregator::Binance)]);

		per_asset.insert("EURC".to_string(), vec![
			// FastForex during Forex market hours (Sun 22:00 → Fri 22:00 UTC)
			HierarchyEntry::with_window(
				Aggregator::FastForex,
				TimeWindow {
					start_day: Weekday::Sun,
					start_hour: 22,
					start_minute: 0,
					end_day: Weekday::Fri,
					end_hour: 21,
					end_minute: 0,
					timezone_offset: "+00:00".to_string(),
				},
			),
			HierarchyEntry::new(Aggregator::Coinbase),
			HierarchyEntry::new(Aggregator::Coingecko),
			HierarchyEntry::new(Aggregator::Pyth),
		]);

		let mut disable_on_exhaustion = HashMap::new();
		disable_on_exhaustion.insert("BRLA".to_string(), true);
		disable_on_exhaustion.insert("BRL".to_string(), true);

		Self {
			default: vec![
				HierarchyEntry::new(Aggregator::Coinbase),
				HierarchyEntry::new(Aggregator::Coingecko),
				HierarchyEntry::new(Aggregator::Pyth),
			],
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

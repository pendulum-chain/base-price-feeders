use super::chain::{ChainClient, PriceData};
use alloy::{
	primitives::{Address, Bytes, B256},
	sol,
};
use log::{debug, error, info};
use serde::Deserialize;
use std::error::Error;
use std::sync::Arc;

use crate::types::AssetSpecifier;
use std::collections::{HashSet, HashMap};

// ── Pyth Hermes API types ─────────────────────────────────────────────────────

pub fn get_pyth_id(symbol: &str) -> Option<&'static str> {
	match symbol.to_uppercase().as_str() {
		"USDC" => Some("eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a"),
		"EURC" => Some("76fa85158bf14ede77087fe3ae472f66213f6ea2f5b411cb2de472794990fa5c"),
		"BRL" | "BRLA" => Some("d2db4dbf1aea74e0f666b0e8f73b9580d407f5e5cf931940b06dc633d7a95906"),
		_ => None,
	}
}

#[derive(Debug, Deserialize)]
pub struct HermesPrice {
	pub price: String,
	pub conf: String,
	pub expo: i32,
	pub publish_time: u64,
}

#[derive(Debug, Deserialize)]
pub struct HermesParsedEntry {
	pub id: String,
	pub price: HermesPrice,
	pub ema_price: HermesPrice,
}

#[derive(Debug, Deserialize)]
pub struct HermesBinary {
	pub data: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct HermesResponse {
	pub binary: HermesBinary,
	pub parsed: Vec<HermesParsedEntry>,
}

sol! {
	#[sol(rpc)]
	contract PythAdapter {
		function getUpdateFee(bytes[] _updateData) external view returns (uint256 updateFee_);
		function updatePriceFeeds(bytes[] _priceUpdateData) external payable returns (bool success_);
	}
}

pub async fn fetch_pyth_prices(
	supported_currencies: &HashSet<AssetSpecifier>,
) -> Result<(HermesResponse, PriceData), Box<dyn Error + Send + Sync + 'static>> {
	let mut pyth_ids_to_symbols: HashMap<&str, String> = HashMap::new();
	let mut query_params = String::new();

	for asset in supported_currencies {
		if let Some(id) = get_pyth_id(&asset.symbol) {
			if !pyth_ids_to_symbols.contains_key(id) {
				pyth_ids_to_symbols.insert(id, asset.symbol.clone());
				query_params.push_str(&format!("ids%5B%5D={}&", id));
			}
		}
	}

	if pyth_ids_to_symbols.is_empty() {
		return Ok((
			HermesResponse { binary: HermesBinary { data: vec![] }, parsed: vec![] },
			PriceData { prices: HashMap::new() },
		));
	}

	// Remove trailing '&'
	query_params.pop();

    let api_url = format!(
        "https://hermes.pyth.network/v2/updates/price/latest?{}",
        query_params
    );

    debug!("Fetching Pyth prices from Hermes API...");
    let response = reqwest::get(&api_url).await?;
    if !response.status().is_success() {
        return Err(format!("Hermes API request failed: {}", response.status()).into());
    }

    let data: HermesResponse = response.json().await?;

    let mut prices = HashMap::new();
    for entry in &data.parsed {
        let price_val = entry
            .price
            .price
            .parse::<f64>()
            .map_err(|e| format!("Failed to parse price: {}", e))?;
        let mut actual_price = price_val * 10f64.powi(entry.price.expo);
        
        if let Some(symbol) = pyth_ids_to_symbols.get(entry.id.as_str()) {
			// BRL price comes as USD/BRL from Pyth, invert to BRL/USD
			if symbol.to_uppercase() == "BRL" || symbol.to_uppercase() == "BRLA" {
				actual_price = 1.0 / actual_price;
			}
			prices.insert(symbol.clone(), actual_price);
		}
    }

    let price_data = PriceData { prices };
    Ok((data, price_data))
}

// ── Pyth price updater ────────────────────────────────────────────────────────

pub struct PythPriceUpdater {
	adapter_address: Address,
	update_interval: std::time::Duration,
	last_update: Option<std::time::Instant>,
}

impl PythPriceUpdater {
	pub fn new(
		update_interval: std::time::Duration,
	) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let pyth_adapter_address =
			std::env::var("PYTH_ADAPTER_ADDRESS").map_err(|_| "PYTH_ADAPTER_ADDRESS not set")?;
		let addr = pyth_adapter_address.parse::<Address>()?;

		Ok(Self { adapter_address: addr, update_interval, last_update: None })
	}

	pub async fn run_update(
		&mut self,
		client: Arc<ChainClient>,
		supported_currencies: &HashSet<AssetSpecifier>,
	) -> Result<(Option<B256>, PriceData), Box<dyn Error + Send + Sync + 'static>> {
		let should_update_contract = match self.last_update {
			None => true,
			Some(t) => t.elapsed() >= self.update_interval,
		};

        let (data, price_data) = fetch_pyth_prices(supported_currencies).await?;

		let tx_hash = if should_update_contract {
			let bytes_data: Vec<Bytes> = data
				.binary
				.data
				.iter()
				.map(|hex_str| {
					let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
					Bytes::from(hex::decode(stripped).unwrap_or_default())
				})
				.collect();

			match self.update_contract(bytes_data, client).await {
				Ok(hash) => {
					self.last_update = Some(std::time::Instant::now());
					info!("Pyth contract tx submitted ✓");
					Some(hash)
				},
				Err(e) => {
					error!("Failed to submit Pyth contract tx: {:?}", e);
					None
				},
			}
		} else {
			None
		};

		Ok((tx_hash, price_data))
	}

	async fn update_contract(
		&self,
		bytes_data: Vec<Bytes>,
		client: Arc<ChainClient>,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		let pyth_adapter = PythAdapter::new(self.adapter_address, &*client.provider);

		let update_fee = pyth_adapter.getUpdateFee(bytes_data.clone()).call().await?.updateFee_;
		info!("Pyth update fee: {} wei", update_fee);

		let priority_fee = client.estimate_priority_fee().await?;
		info!("Pyth priority fee: {} wei", priority_fee);

		let nonce = client.nonce_manager.next_nonce();
		let call_builder = pyth_adapter
			.updatePriceFeeds(bytes_data)
			.value(update_fee)
			.gas(1_000_000)
			.max_priority_fee_per_gas(priority_fee * 7)
			.nonce(nonce);

		let pending_tx = call_builder.send().await?;
		let tx_hash = *pending_tx.tx_hash();
		info!("Pyth updatePriceFeeds tx hash: {:?}", tx_hash);

		Ok(tx_hash)
	}
}

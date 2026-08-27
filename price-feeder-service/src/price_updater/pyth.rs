use super::chain::{ChainClient, PriceData};
use crate::args::PythConfig;
use alloy::{
	primitives::{Address, Bytes, B256},
	sol,
};
use log::{debug, error, info, warn};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use std::error::Error;
use std::sync::Arc;

use crate::types::AssetSpecifier;
use std::collections::{HashMap, HashSet};

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

// ── Hermes client ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PythFetchError {
	/// Hermes rejected our credentials (HTTP 401/403). Retrying cannot succeed until the API key
	/// is fixed.
	Unauthorized(StatusCode),
	Other(String),
}

impl std::fmt::Display for PythFetchError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unauthorized(status) => {
				write!(f, "Hermes API rejected credentials ({}) — check PYTH_API_KEY", status)
			},
			Self::Other(msg) => write!(f, "{}", msg),
		}
	}
}

impl Error for PythFetchError {}

impl From<reqwest::Error> for PythFetchError {
	fn from(e: reqwest::Error) -> Self {
		Self::Other(e.to_string())
	}
}

impl From<String> for PythFetchError {
	fn from(msg: String) -> Self {
		Self::Other(msg)
	}
}

#[derive(Clone)]
pub struct HermesClient {
	client: reqwest::Client,
	base_url: String,
}

impl HermesClient {
	pub fn new(config: &PythConfig) -> Self {
		let mut headers = header::HeaderMap::new();
		match config.pyth_api_key.as_deref().map(str::trim) {
			Some(key) if !key.is_empty() =>
				match header::HeaderValue::from_str(&format!("Bearer {}", key)) {
					Ok(mut value) => {
						value.set_sensitive(true);
						headers.insert(header::AUTHORIZATION, value);
					},
					Err(_) => error!(
						"PYTH_API_KEY contains characters that are invalid in an HTTP header; sending Hermes requests unauthenticated"
					),
				},
			_ => warn!("PYTH_API_KEY is not set; sending Hermes requests unauthenticated"),
		}

		let client = reqwest::Client::builder()
			.default_headers(headers)
			.build()
			.expect("failed to build Hermes HTTP client");

		Self { client, base_url: config.hermes_url.trim_end_matches('/').to_string() }
	}

	pub async fn fetch_pyth_prices(
		&self,
		supported_currencies: &HashSet<AssetSpecifier>,
	) -> Result<(HermesResponse, PriceData), PythFetchError> {
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

		let api_url = format!("{}/v2/updates/price/latest?{}", self.base_url, query_params);

		debug!("Fetching Pyth prices from Hermes API...");
		let response = self.client.get(&api_url).send().await?;
		let status = response.status();
		if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
			error!("Hermes API returned {} — check PYTH_API_KEY", status);
			return Err(PythFetchError::Unauthorized(status));
		}
		if !status.is_success() {
			return Err(format!("Hermes API request failed: {}", status).into());
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
}

// ── Pyth price updater ────────────────────────────────────────────────────────

pub struct PythPriceUpdater {
	adapter_address: Address,
	hermes: HermesClient,
	update_interval: std::time::Duration,
	last_update: Option<std::time::Instant>,
}

impl PythPriceUpdater {
	pub fn new(
		hermes: HermesClient,
		update_interval: std::time::Duration,
	) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let pyth_adapter_address =
			std::env::var("PYTH_ADAPTER_ADDRESS").map_err(|_| "PYTH_ADAPTER_ADDRESS not set")?;
		let addr = pyth_adapter_address.parse::<Address>()?;

		Ok(Self { adapter_address: addr, hermes, update_interval, last_update: None })
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

		let (data, price_data) = self.hermes.fetch_pyth_prices(supported_currencies).await?;

		let tx_hash = if should_update_contract {
			let bytes_data: Result<Vec<Bytes>, _> = data
				.binary
				.data
				.iter()
				.map(|hex_str| {
					let hex_cleaned = hex_str.trim_start_matches("0x");
					hex::decode(hex_cleaned)
						.map(Bytes::from)
						.map_err(|e| format!("Failed to decode hex from Hermes: {}", e))
				})
				.collect();
			let bytes_data = bytes_data?;

			let hash = self.update_contract(bytes_data, client).await?;
			self.last_update = Some(std::time::Instant::now());
			Some(hash)
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
		let priority_fee = client.estimate_priority_fee().await?;
		info!("Pyth priority fee: {} wei", priority_fee);

		let call_builder = pyth_adapter
			.updatePriceFeeds(bytes_data)
			.value(update_fee)
			.gas(1_000_000)
			.max_priority_fee_per_gas(priority_fee);

		let tx_hash = client
			.send_tx_with_retry(call_builder.into_transaction_request(), self.update_interval)
			.await?;

		Ok(tx_hash)
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpListener,
		task::JoinHandle,
	};

	/// Minimal one-shot HTTP server: answers `responses` requests with the given status line and
	/// body, then returns the captured request heads. Once all responses are served the listener
	/// is dropped, so any further request fails with a connection error.
	pub(crate) async fn spawn_mock_hermes(
		status_line: &'static str,
		body: &'static str,
		responses: usize,
	) -> (String, JoinHandle<Vec<String>>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let handle = tokio::spawn(async move {
			let mut requests = Vec::new();
			for _ in 0..responses {
				let (mut socket, _) = listener.accept().await.unwrap();
				let mut head = Vec::new();
				let mut buf = [0u8; 1024];
				loop {
					let n = socket.read(&mut buf).await.unwrap();
					if n == 0 {
						break;
					}
					head.extend_from_slice(&buf[..n]);
					if head.windows(4).any(|window| window == b"\r\n\r\n") {
						break;
					}
				}
				requests.push(String::from_utf8_lossy(&head).to_string());
				let response = format!(
					"{}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
					status_line,
					body.len(),
					body
				);
				socket.write_all(response.as_bytes()).await.unwrap();
			}
			requests
		});
		(format!("http://{}", addr), handle)
	}
}

#[cfg(test)]
mod tests {
	use super::{test_support::spawn_mock_hermes, *};

	fn config(hermes_url: &str, api_key: Option<&str>) -> PythConfig {
		PythConfig { pyth_api_key: api_key.map(String::from), hermes_url: hermes_url.to_string() }
	}

	fn eurc_currencies() -> HashSet<AssetSpecifier> {
		vec![AssetSpecifier { blockchain: "Base".into(), symbol: "EURC".into() }]
			.into_iter()
			.collect()
	}

	const EURC_RESPONSE: &str = r#"{"binary":{"data":[]},"parsed":[{"id":"76fa85158bf14ede77087fe3ae472f66213f6ea2f5b411cb2de472794990fa5c","price":{"price":"116","conf":"1","expo":-2,"publish_time":1},"ema_price":{"price":"116","conf":"1","expo":-2,"publish_time":1}}]}"#;

	#[tokio::test]
	async fn sends_bearer_token_to_configured_hermes_url() {
		let (url, handle) = spawn_mock_hermes("HTTP/1.1 200 OK", EURC_RESPONSE, 1).await;
		let client = HermesClient::new(&config(&url, Some("test-key")));

		let (_, price_data) = client.fetch_pyth_prices(&eurc_currencies()).await.unwrap();
		let eurc_price = *price_data.prices.get("EURC").expect("EURC price missing");
		assert!((eurc_price - 1.16).abs() < 1e-9, "unexpected EURC price: {}", eurc_price);

		let requests = handle.await.unwrap();
		let head = requests[0].to_lowercase();
		assert!(head.contains("authorization: bearer test-key"), "missing auth header: {}", head);
		assert!(
			head.starts_with("get /v2/updates/price/latest?ids%5b%5d=76fa"),
			"unexpected request line: {}",
			head
		);
	}

	#[tokio::test]
	async fn missing_api_key_sends_unauthenticated_request() {
		let (url, handle) = spawn_mock_hermes("HTTP/1.1 200 OK", EURC_RESPONSE, 1).await;
		let client = HermesClient::new(&config(&url, None));

		client.fetch_pyth_prices(&eurc_currencies()).await.unwrap();

		let requests = handle.await.unwrap();
		let head = requests[0].to_lowercase();
		assert!(!head.contains("authorization:"), "unexpected auth header: {}", head);
	}

	#[tokio::test]
	async fn unauthorized_response_returns_typed_error() {
		let (url, _handle) = spawn_mock_hermes("HTTP/1.1 401 Unauthorized", "{}", 1).await;
		let client = HermesClient::new(&config(&url, Some("bad-key")));

		let err = client.fetch_pyth_prices(&eurc_currencies()).await.unwrap_err();
		assert!(matches!(err, PythFetchError::Unauthorized(_)), "got {:?}", err);
	}
}

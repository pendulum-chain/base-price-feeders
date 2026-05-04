use alloy::providers::ProviderBuilder;
use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use alloy::{
	network::{Ethereum, EthereumWallet},
	providers::{
		fillers::{ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller},
		RootProvider,
	},
};
use reqwest::Url;
use std::error::Error;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

const MAX_ELAPSED_INTERVAL_MULTIPLIER: f64 = 0.5;
const TX_RETRY_DELAY_MS: u64 = 250;

pub struct NonceManager {
	nonce: Mutex<u64>,
}

impl NonceManager {
	pub fn new(initial_nonce: u64) -> Self {
		Self { nonce: Mutex::new(initial_nonce) }
	}

	pub fn next_nonce(&self) -> u64 {
		let mut nonce = self.nonce.lock().unwrap();
		let current = *nonce;
		*nonce += 1;
		current
	}

	pub fn sync_nonce(&self, chain_nonce: u64) {
		let mut nonce = self.nonce.lock().unwrap();
		if chain_nonce > *nonce {
			*nonce = chain_nonce;
		}
	}
}

pub type HttpTransport = alloy::transports::http::Http<reqwest::Client>;
pub type ChainProvider = FillProvider<
	JoinFill<
		JoinFill<
			JoinFill<JoinFill<alloy::providers::Identity, GasFiller>, NonceFiller>,
			ChainIdFiller,
		>,
		WalletFiller<EthereumWallet>,
	>,
	RootProvider<HttpTransport>,
	HttpTransport,
	Ethereum,
>;

pub struct ChainClient {
	pub provider: Arc<ChainProvider>,
	pub nonce_manager: Arc<NonceManager>,
	pub address: Address,
}

impl ChainClient {
	pub async fn create_nonce_manager(
	) -> Result<Arc<NonceManager>, Box<dyn Error + Send + Sync + 'static>> {
		let private_key_str = std::env::var("PRIVATE_KEY").map_err(|_| "PRIVATE_KEY not set")?;
		let rpc_url = std::env::var("RPC_URL").map_err(|_| "RPC_URL not set")?;
		let signer = PrivateKeySigner::from_str(&private_key_str)?;
		let wallet_address = signer.address();

		let rpc_url_parsed = Url::parse(&rpc_url).expect("Invalid RPC_URL");
		let provider = ProviderBuilder::new().on_http(rpc_url_parsed);

		let initial_nonce =
			alloy::providers::Provider::get_transaction_count(&provider, wallet_address).await?;
		Ok(Arc::new(NonceManager::new(initial_nonce)))
	}

	pub async fn new(
		nonce_manager: Arc<NonceManager>,
	) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let private_key_str = std::env::var("PRIVATE_KEY").map_err(|_| "PRIVATE_KEY not set")?;
		let rpc_url = std::env::var("RPC_URL").map_err(|_| "RPC_URL not set")?;

		let signer = PrivateKeySigner::from_str(&private_key_str)?;
		let address = signer.address();
		let wallet = EthereumWallet::from(signer);

		let rpc_url_parsed = Url::parse(&rpc_url).expect("Invalid RPC_URL");

		let provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(wallet)
			.on_http(rpc_url_parsed);

		Ok(Self { provider: Arc::new(provider), nonce_manager, address })
	}

	pub async fn estimate_priority_fee(
		&self,
	) -> Result<u128, Box<dyn Error + Send + Sync + 'static>> {
		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let priority_fee = fees.max_priority_fee_per_gas;
		Ok(priority_fee)
	}

	pub async fn send_tx_with_retry(
		&self,
		mut tx_req: alloy::rpc::types::TransactionRequest,
		update_interval: std::time::Duration,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		let start_time = std::time::Instant::now();
		let max_elapsed = std::time::Duration::from_secs_f64(update_interval.as_secs_f64() * MAX_ELAPSED_INTERVAL_MULTIPLIER);
		let mut retries = 0;
		loop {
			let elapsed = start_time.elapsed();
			if retries > 0 && elapsed >= max_elapsed {
				return Err(format!("Dropped outdated transaction. Elapsed: {:?}, Max allowed: {:?}", elapsed, max_elapsed).into());
			}

			let nonce = self.nonce_manager.next_nonce();
			tx_req.nonce = Some(nonce);

			match alloy::providers::Provider::send_transaction(&*self.provider, tx_req.clone()).await {
				Ok(pending_tx) => return Ok(*pending_tx.tx_hash()),
				Err(e) => {
					retries += 1;
					if retries > 5 {
						return Err(e.into());
					}
					let err_msg = e.to_string();
					if err_msg.contains("nonce too low") {
						log::warn!("Caught 'nonce too low' (try {}). Syncing...", retries);
						let chain_nonce = alloy::providers::Provider::get_transaction_count(
							&*self.provider, self.address
						).await?;
						self.nonce_manager.sync_nonce(chain_nonce);
					} else {
						log::warn!("Tx error: {}. Retrying {}/5...", err_msg, retries);
						tokio::time::sleep(std::time::Duration::from_millis(TX_RETRY_DELAY_MS)).await;
					}
				}
			}
		}
	}
}

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PriceData {
	pub prices: HashMap<String, f64>,
}

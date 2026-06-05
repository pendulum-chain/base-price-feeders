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
use log::{error, info, warn};
use reqwest::Url;
use std::error::Error;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const MAX_ELAPSED_INTERVAL_MULTIPLIER: f64 = 0.5;
const TX_RETRY_DELAY_MS: u64 = 250;

const PRIORITY_FEE_STEPS: [u128; 6] = [7, 10, 12, 15, 20, 30];
const PRIORITY_FEE_BUMP_DOWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

pub struct PriorityFeeMultiplier {
	inner: Mutex<PriorityFeeInner>,
}

struct PriorityFeeInner {
	step_index: usize,
	last_bump_at: Option<std::time::Instant>,
}

impl PriorityFeeMultiplier {
	pub fn new() -> Self {
		Self {
			inner: Mutex::new(PriorityFeeInner {
				step_index: 0,
				last_bump_at: None,
			}),
		}
	}

	pub fn bump_up(&self) {
		let mut inner = self.inner.lock().unwrap();
		if inner.step_index < PRIORITY_FEE_STEPS.len() - 1 {
			inner.step_index += 1;
			inner.last_bump_at = Some(std::time::Instant::now());
			warn!(
				"[PriorityFee] Bumped up to multiplier: {}",
				PRIORITY_FEE_STEPS[inner.step_index]
			);
		}
	}

	pub fn bump_down(&self) {
		let mut inner = self.inner.lock().unwrap();
		if inner.step_index > 0 {
			inner.step_index = 0;
			inner.last_bump_at = None;
			warn!(
				"[PriorityFee] Bumped down to base multiplier: {}",
				PRIORITY_FEE_STEPS[0]
			);
		}
	}

	pub fn get(&self) -> u128 {
		let inner = self.inner.lock().unwrap();
		PRIORITY_FEE_STEPS[inner.step_index]
	}

	/// Checks if enough time has passed since the last bump to trigger a bump down.
	/// Returns true if a bump down was performed.
	pub fn try_bump_down(&self) -> bool {
		let inner = self.inner.lock().unwrap();
		if inner.step_index > 0 {
			if let Some(last_bump) = inner.last_bump_at {
				if last_bump.elapsed() >= PRIORITY_FEE_BUMP_DOWN_COOLDOWN {
					drop(inner);
					self.bump_down();
					return true;
				}
			}
		}
		false
	}
}

pub struct NonceManager {
	nonce: Mutex<u64>,
	address: Address,
	rpc_url: String,
}

impl NonceManager {
	pub fn new(initial_nonce: u64, address: Address, rpc_url: String) -> Self {
		Self { nonce: Mutex::new(initial_nonce), address, rpc_url }
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

	pub fn force_sync_nonce(&self, chain_nonce: u64) {
		let mut nonce = self.nonce.lock().unwrap();
		*nonce = chain_nonce;
	}

	pub fn get_current_nonce(&self) -> u64 {
		*self.nonce.lock().unwrap()
	}

	pub async fn get_onchain_nonce(&self) -> Result<u64, Box<dyn Error + Send + Sync + 'static>> {
		let rpc_url_parsed = Url::parse(&self.rpc_url).expect("Invalid RPC_URL");
		let provider = ProviderBuilder::new().on_http(rpc_url_parsed);
		let onchain_nonce = alloy::providers::Provider::get_transaction_count(&provider, self.address).await?;
		Ok(onchain_nonce)
	}

	pub fn address(&self) -> Address {
		self.address
	}

	pub fn rpc_url(&self) -> &str {
		&self.rpc_url
	}

	pub fn spawn_resync_handler(self: &Arc<Self>) -> mpsc::Sender<()> {
		let (tx, mut rx) = mpsc::channel::<()>(10);
		let mgr = Arc::clone(self);

		tokio::spawn(async move {
			info!("Starting nonce resync handler");
			while rx.recv().await.is_some() {
				warn!("[Resync] Received resync signal from watchdog");
				match mgr.get_onchain_nonce().await {
					Ok(onchain_nonce) => {
						info!("[Resync] Forcing nonce to onchain value: {}", onchain_nonce);
						mgr.force_sync_nonce(onchain_nonce);
					},
					Err(e) => {
						error!("[Resync] Failed to fetch onchain nonce: {:?}", e);
					},
				}
			}
		});

		tx
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
	pub priority_multiplier: Arc<PriorityFeeMultiplier>,
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
		Ok(Arc::new(NonceManager::new(initial_nonce, wallet_address, rpc_url)))
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

		Ok(Self {
			provider: Arc::new(provider),
			nonce_manager,
			address,
			priority_multiplier: Arc::new(PriorityFeeMultiplier::new()),
		})
	}

	pub async fn estimate_priority_fee(
		&self,
	) -> Result<u128, Box<dyn Error + Send + Sync + 'static>> {
		self.priority_multiplier.try_bump_down();
		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let priority_fee = fees.max_priority_fee_per_gas;
		let multiplier = self.priority_multiplier.get();
		Ok(priority_fee * multiplier)
	}

	pub async fn send_tx_with_retry(
		&self,
		mut tx_req: alloy::rpc::types::TransactionRequest,
		update_interval: std::time::Duration,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		let start_time = std::time::Instant::now();
		let max_elapsed = std::time::Duration::from_secs_f64(update_interval.as_secs_f64() * MAX_ELAPSED_INTERVAL_MULTIPLIER);
		let mut retries = 0;
		let mut nonce = self.nonce_manager.next_nonce();
		loop {
			let elapsed = start_time.elapsed();
			if retries > 0 && elapsed >= max_elapsed {
				return Err(format!("Dropped outdated transaction. Elapsed: {:?}, Max allowed: {:?}", elapsed, max_elapsed).into());
			}

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
						nonce = self.nonce_manager.next_nonce();
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn priority_fee_starts_at_base() {
		let pf = PriorityFeeMultiplier::new();
		assert_eq!(pf.get(), 7);
	}

	#[test]
	fn priority_fee_bumps_up_through_steps() {
		let pf = PriorityFeeMultiplier::new();
		assert_eq!(pf.get(), 7);

		pf.bump_up();
		assert_eq!(pf.get(), 10);

		pf.bump_up();
		assert_eq!(pf.get(), 12);

		pf.bump_up();
		assert_eq!(pf.get(), 15);

		pf.bump_up();
		assert_eq!(pf.get(), 20);

		// Should stay at max
		pf.bump_up();
		assert_eq!(pf.get(), 20);
	}

	#[test]
	fn priority_fee_bumps_down_to_base() {
		let pf = PriorityFeeMultiplier::new();
		pf.bump_up();
		pf.bump_up();
		assert_eq!(pf.get(), 12);

		pf.bump_down();
		assert_eq!(pf.get(), 7);
	}

	#[test]
	fn priority_fee_bump_down_noop_at_base() {
		let pf = PriorityFeeMultiplier::new();
		assert_eq!(pf.get(), 7);

		pf.bump_down();
		assert_eq!(pf.get(), 7);
	}
}

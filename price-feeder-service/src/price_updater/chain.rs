use alloy::providers::ProviderBuilder;
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
		let wallet = EthereumWallet::from(signer);

		let rpc_url_parsed = Url::parse(&rpc_url).expect("Invalid RPC_URL");

		let provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(wallet)
			.on_http(rpc_url_parsed);

		Ok(Self { provider: Arc::new(provider), nonce_manager })
	}

	pub async fn estimate_priority_fee(
		&self,
	) -> Result<u128, Box<dyn Error + Send + Sync + 'static>> {
		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let priority_fee = fees.max_priority_fee_per_gas;
		Ok(priority_fee)
	}
}

#[derive(Debug, Clone)]
pub struct PriceData {
	pub usdc: f64,
	pub eurc: f64,
	pub brla: f64,
}

use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::{
	network::{Ethereum, EthereumWallet},
	providers::{
		fillers::{ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller},
		RootProvider,
	},
};
use log::{info, warn};
use reqwest::Url;
use std::error::Error;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use super::tx_engine::TxEngine;
use super::tx_processor::UpdateTxKind;

const DEFAULT_BASE_FEE_MULTIPLIER: f32 = 7.0;

// Step multipliers applied on top of BASE_FEE_MULTIPLIER.
// Effective priority assuming BASE_FEE_MULTIPLIER = 7.0:
// Step 0: 7.0 * 1.1 = 7.7
// Step 1: 7.0 * 1.2 = 8.4
// Step 2: 7.0 * 1.4 = 9.8
// Step 3: 7.0 * 1.5 = 10.5
// Step 4: 7.0 * 2.0 = 14.0
// Step 5: 7.0 * 3.0 = 21.0
const PRIORITY_FEE_STEPS: [f32; 6] = [1.1, 1.2, 1.4, 1.5, 2.0, 3.0];
const PRIORITY_FEE_BUMP_DOWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

pub struct PriorityFeeMultiplier {
	base_multiplier: f32,
	inner: Mutex<PriorityFeeInner>,
}

struct PriorityFeeInner {
	step_index: usize,
	last_bump_at: Option<std::time::Instant>,
}

impl PriorityFeeMultiplier {
	pub fn new(base_multiplier: f32) -> Self {
		Self {
			base_multiplier,
			inner: Mutex::new(PriorityFeeInner { step_index: 0, last_bump_at: None }),
		}
	}

	pub fn bump_up(&self) {
		let mut inner = self.inner.lock().unwrap();
		if inner.step_index < PRIORITY_FEE_STEPS.len() - 1 {
			inner.step_index += 1;
			inner.last_bump_at = Some(std::time::Instant::now());
			warn!(
				"[PriorityFee] Bumped up to multiplier: {}",
				self.effective_multiplier(inner.step_index)
			);
		}
	}

	pub fn bump_down(&self) {
		let mut inner = self.inner.lock().unwrap();
		if inner.step_index > 0 {
			inner.step_index = 0;
			inner.last_bump_at = None;
			warn!("[PriorityFee] Bumped down to multiplier: {}", self.effective_multiplier(0));
		}
	}

	pub fn get(&self) -> f32 {
		let inner = self.inner.lock().unwrap();
		self.effective_multiplier(inner.step_index)
	}

	pub fn base(&self) -> f32 {
		self.base_multiplier
	}

	fn effective_multiplier(&self, step_index: usize) -> f32 {
		PRIORITY_FEE_STEPS[step_index] * self.base_multiplier
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
	pub tx_engine: Arc<TxEngine>,
	pub address: Address,
}

impl ChainClient {
	pub async fn new() -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
		let private_key_str = std::env::var("PRIVATE_KEY").map_err(|_| "PRIVATE_KEY not set")?;
		let rpc_url = std::env::var("RPC_URL").map_err(|_| "RPC_URL not set")?;

		let signer = PrivateKeySigner::from_str(&private_key_str)?;
		let address = signer.address();
		let wallet = EthereumWallet::from(signer);

		let rpc_url_parsed = Url::parse(&rpc_url)?;

		let base_fee_multiplier: f32 = std::env::var("BASE_FEE_MULTIPLIER")
			.ok()
			.and_then(|s| s.parse().ok())
			.unwrap_or(DEFAULT_BASE_FEE_MULTIPLIER);
		info!("Base fee multiplier: {}", base_fee_multiplier);

		let provider = Arc::new(
			ProviderBuilder::new()
				.with_recommended_fillers()
				.wallet(wallet)
				.on_http(rpc_url_parsed),
		);

		let priority_multiplier = Arc::new(PriorityFeeMultiplier::new(base_fee_multiplier));
		let tx_engine =
			Arc::new(TxEngine::new(provider.clone(), address, priority_multiplier).await?);

		Ok(Self { provider, tx_engine, address })
	}

	pub async fn send_tx_with_retry(
		&self,
		tx_req: alloy::rpc::types::TransactionRequest,
		kind: UpdateTxKind,
		_update_interval: std::time::Duration,
	) -> Result<B256, Box<dyn Error + Send + Sync + 'static>> {
		self.tx_engine.submit(tx_req, kind).await
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

	const BASE: f32 = 7.0;
	const EPS: f32 = 1e-4;

	fn approx_eq(a: f32, b: f32) -> bool {
		(a - b).abs() < EPS
	}

	#[test]
	fn priority_fee_starts_at_base() {
		let pf = PriorityFeeMultiplier::new(BASE);
		assert!(approx_eq(pf.get(), 7.7));
	}

	#[test]
	fn priority_fee_bumps_up_through_steps() {
		let pf = PriorityFeeMultiplier::new(BASE);
		assert!(approx_eq(pf.get(), 7.7));

		pf.bump_up();
		assert!(approx_eq(pf.get(), 8.4));

		pf.bump_up();
		assert!(approx_eq(pf.get(), 9.8));

		pf.bump_up();
		assert!(approx_eq(pf.get(), 10.5));

		pf.bump_up();
		assert!(approx_eq(pf.get(), 14.0));

		pf.bump_up();
		assert!(approx_eq(pf.get(), 21.0));

		// Should stay at max
		pf.bump_up();
		assert!(approx_eq(pf.get(), 21.0));
	}

	#[test]
	fn priority_fee_bumps_down_to_base() {
		let pf = PriorityFeeMultiplier::new(BASE);
		pf.bump_up();
		pf.bump_up();
		assert!(approx_eq(pf.get(), 9.8));

		pf.bump_down();
		assert!(approx_eq(pf.get(), 7.7));
	}

	#[test]
	fn priority_fee_bump_down_noop_at_base() {
		let pf = PriorityFeeMultiplier::new(BASE);
		assert!(approx_eq(pf.get(), 7.7));

		pf.bump_down();
		assert!(approx_eq(pf.get(), 7.7));
	}
}

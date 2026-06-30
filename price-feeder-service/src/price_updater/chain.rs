use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::types::TransactionRequest;
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

/// EIP-1559 replacement fee bump factor applied on top of the current network
/// estimate. The replacement tx must have strictly higher `max_fee_per_gas` and
/// `max_priority_fee_per_gas` than the original; 1.25× (25 %) is well above the
/// 10 % minimum required by the mempool rules. The replacement builder also
/// floors each field at 1.125× the tracked old fee to defend against a network
/// fee drop between submit and timeout.
const REPLACEMENT_FEE_BUMP: f64 = 1.25;
/// Minimum bump over the tracked old fees, applied to both EIP-1559 fields
/// when the *current* estimate × `REPLACEMENT_FEE_BUMP` is not enough on its
/// own (network fees dropped between submit and timeout).
const REPLACEMENT_FEE_FLOOR: f64 = 1.125;

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
		// Use the "pending" block tag so the returned nonce accounts for txs
		// already in the mempool. Querying the default ("latest") nonce would
		// ignore those and race against in-flight submissions, causing the
		// nonce manager to regress below pending transactions.
		let onchain_nonce =
			alloy::providers::Provider::get_transaction_count(&provider, self.address)
				.pending()
				.await?;
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

		let rpc_url_parsed = Url::parse(&rpc_url)?;

		let base_fee_multiplier: f32 = std::env::var("BASE_FEE_MULTIPLIER")
			.ok()
			.and_then(|s| s.parse().ok())
			.unwrap_or(DEFAULT_BASE_FEE_MULTIPLIER);
		info!("Base fee multiplier: {}", base_fee_multiplier);

		let provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(wallet)
			.on_http(rpc_url_parsed);

		Ok(Self {
			provider: Arc::new(provider),
			nonce_manager,
			address,
			priority_multiplier: Arc::new(PriorityFeeMultiplier::new(base_fee_multiplier)),
		})
	}

	pub async fn estimate_priority_fee(
		&self,
	) -> Result<u128, Box<dyn Error + Send + Sync + 'static>> {
		self.priority_multiplier.try_bump_down();
		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let priority_fee = fees.max_priority_fee_per_gas;
		let multiplier = self.priority_multiplier.get();
		let scaled = (priority_fee as f64) * (multiplier as f64);
		if !scaled.is_finite() || scaled < 0.0 || scaled > u128::MAX as f64 {
			return Err(format!(
				"priority fee overflow: base fee {} * multiplier {} exceeds u128",
				priority_fee, multiplier
			)
			.into());
		}
		Ok(scaled as u128)
	}

	pub async fn send_tx_with_retry(
		&self,
		mut tx_req: TransactionRequest,
		update_interval: std::time::Duration,
	) -> Result<SentTx, Box<dyn Error + Send + Sync + 'static>> {
		let start_time = std::time::Instant::now();
		let max_elapsed = std::time::Duration::from_secs_f64(
			update_interval.as_secs_f64() * MAX_ELAPSED_INTERVAL_MULTIPLIER,
		);
		let mut retries = 0;
		let mut nonce = self.nonce_manager.next_nonce();
		loop {
			let elapsed = start_time.elapsed();
			if retries > 0 && elapsed >= max_elapsed {
				return Err(format!(
					"Dropped outdated transaction. Elapsed: {:?}, Max allowed: {:?}",
					elapsed, max_elapsed
				)
				.into());
			}

			tx_req.nonce = Some(nonce);

			// Track the fees we are actually submitting with. If the caller
			// did not set `max_fee_per_gas` explicitly (the common case: the
			// GasFiller would otherwise fill it post-submit and we would not be
			// able to track the value), estimate it now and stamp it onto the
			// request. This is required for hybrid recovery: the watchdog
			// needs the old fees in order to compute a same-nonce replacement
			// bump.
			let tracked_max_priority_fee_per_gas = tx_req.max_priority_fee_per_gas.unwrap_or(0);
			let tracked_max_fee_per_gas = match tx_req.max_fee_per_gas {
				Some(v) => {
					let max_fee = v.max(tracked_max_priority_fee_per_gas);
					tx_req.max_fee_per_gas = Some(max_fee);
					max_fee
				},
				None => {
					let fees = match alloy::providers::Provider::estimate_eip1559_fees(
						&*self.provider,
						None,
					)
					.await
					{
						Ok(fees) => fees,
						Err(e) => {
							retries += 1;
							if retries > 5 {
								return Err(e.into());
							}
							log::warn!(
								"Failed to estimate EIP-1559 fees before submit: {}. Retrying {}/5...",
								e,
								retries
							);
							tokio::time::sleep(std::time::Duration::from_millis(TX_RETRY_DELAY_MS))
								.await;
							continue;
						},
					};
					let max_fee = max_fee_with_priority_headroom(
						fees.max_fee_per_gas,
						fees.max_priority_fee_per_gas,
						tracked_max_priority_fee_per_gas,
					);
					tx_req.max_fee_per_gas = Some(max_fee);
					max_fee
				},
			};

			let request_template = Arc::new(tx_req.clone());
			match alloy::providers::Provider::send_transaction(&*self.provider, tx_req.clone())
				.await
			{
				Ok(pending_tx) => {
					return Ok(SentTx {
						tx_hash: *pending_tx.tx_hash(),
						nonce,
						max_priority_fee_per_gas: tracked_max_priority_fee_per_gas,
						max_fee_per_gas: tracked_max_fee_per_gas,
						request_template,
					});
				},
				Err(e) => {
					retries += 1;
					if retries > 5 {
						return Err(e.into());
					}
					let err_msg = e.to_string();
					if err_msg.contains("nonce too low") {
						log::warn!("Caught 'nonce too low' (try {}). Syncing...", retries);
						// Use the "pending" nonce to account for txs already in
						// the node's mempool; otherwise we'd skip in-flight txs.
						let chain_nonce = alloy::providers::Provider::get_transaction_count(
							&*self.provider,
							self.address,
						)
						.pending()
						.await?;
						self.nonce_manager.sync_nonce(chain_nonce);
						nonce = self.nonce_manager.next_nonce();
					} else {
						log::warn!("Tx error: {}. Retrying {}/5...", err_msg, retries);
						tokio::time::sleep(std::time::Duration::from_millis(TX_RETRY_DELAY_MS))
							.await;
					}
				},
			}
		}
	}

	/// Submit a same-nonce replacement for a stuck price-update tx. The
	/// `nonce` is reused from the tracked original; `NonceManager` is **not**
	/// touched (we are not allocating a new nonce, we are overwriting a
	/// mempool entry at an existing nonce).
	///
	/// Bumps both EIP-1559 fee fields by `REPLACEMENT_FEE_BUMP` over the
	/// current network estimate (priority also scaled by the active
	/// `priority_multiplier` step) and floors each field at
	/// `REPLACEMENT_FEE_FLOOR` over the tracked old fees — this guards
	/// against a network fee drop between submit and timeout producing a
	/// sub-spec replacement.
	///
	/// If the original tx already confirmed in the meantime, the RPC will
	/// return "nonce too low"; the caller should treat that as harmless
	/// (the original data is on-chain, which is the correct outcome).
	pub async fn send_replacement_tx(
		&self,
		mut tx_req: TransactionRequest,
		nonce: u64,
		old_max_priority_fee_per_gas: u128,
		old_max_fee_per_gas: u128,
	) -> Result<SentTx, Box<dyn Error + Send + Sync + 'static>> {
		tx_req.nonce = Some(nonce);

		let fees = alloy::providers::Provider::estimate_eip1559_fees(&*self.provider, None).await?;
		let mult = self.priority_multiplier.get();
		let (new_priority, new_max_fee) = compute_replacement_fees(
			old_max_priority_fee_per_gas,
			old_max_fee_per_gas,
			fees.max_priority_fee_per_gas,
			fees.max_fee_per_gas,
			mult,
		);
		tx_req.max_priority_fee_per_gas = Some(new_priority);
		tx_req.max_fee_per_gas = Some(new_max_fee);
		let request_template = Arc::new(tx_req.clone());

		let pending = alloy::providers::Provider::send_transaction(&*self.provider, tx_req).await?;
		Ok(SentTx {
			tx_hash: *pending.tx_hash(),
			nonce,
			max_priority_fee_per_gas: new_priority,
			max_fee_per_gas: new_max_fee,
			request_template,
		})
	}
}

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PriceData {
	pub prices: HashMap<String, f64>,
}

/// Metadata returned by `send_tx_with_retry` and `send_replacement_tx` so the
/// caller (and the tx_processor tracking layer) knows the exact fees used and
/// the nonce the tx was submitted at. All fields are populated for every
/// successful submission: when the original request did not have explicit
/// `max_fee_per_gas`, the sender fills it from `estimate_eip1559_fees` before
/// submitting, so the tracked value is always non-zero and trustworthy for
/// later same-nonce replacement.
#[derive(Debug, Clone)]
pub struct SentTx {
	pub tx_hash: B256,
	pub nonce: u64,
	pub max_priority_fee_per_gas: u128,
	pub max_fee_per_gas: u128,
	pub request_template: Arc<TransactionRequest>,
}

/// Computes EIP-1559 replacement fees for a same-nonce replacement.
///
/// Both new fields are required to be at least the bumped target. The
/// replacement therefore takes the max of two candidates:
///   - `current_estimate * REPLACEMENT_FEE_BUMP` (priority also scaled by the
///     active `priority_multiplier` step), and
///   - `old_fee * REPLACEMENT_FEE_FLOOR` (so a network fee drop between
///     submit and timeout cannot silently produce a sub-spec replacement).
///
/// `max_priority_fee_per_gas` is then capped to `max_fee_per_gas` (EIP-1559
/// invariant: priority ≤ max). All math is in `f64`; outputs are
/// `ceil`'d before the `u128` cast so the "at least" guarantee survives
/// floating-point rounding, and clamped to `u128::MAX` to avoid overflow.
pub(crate) fn compute_replacement_fees(
	old_max_priority_fee_per_gas: u128,
	old_max_fee_per_gas: u128,
	current_priority_estimate: u128,
	current_max_fee_estimate: u128,
	priority_multiplier: f32,
) -> (u128, u128) {
	let mult = priority_multiplier as f64;
	let current_priority = (current_priority_estimate as f64) * mult;
	let priority_delta = current_priority - (current_priority_estimate as f64);
	let current_max_fee = (current_max_fee_estimate as f64) + priority_delta.max(0.0);

	let bumped_priority = current_priority * REPLACEMENT_FEE_BUMP;
	let bumped_max_fee = current_max_fee * REPLACEMENT_FEE_BUMP;

	let min_priority = (old_max_priority_fee_per_gas as f64) * REPLACEMENT_FEE_FLOOR;
	let min_max_fee = (old_max_fee_per_gas as f64) * REPLACEMENT_FEE_FLOOR;

	let final_priority = bumped_priority.max(min_priority);
	let final_max_fee = bumped_max_fee.max(min_max_fee);

	// priority ≤ max_fee (EIP-1559 invariant).
	let final_priority = final_priority.min(final_max_fee);

	let final_priority =
		if final_priority > u128::MAX as f64 { u128::MAX } else { final_priority.ceil() as u128 };
	let final_max_fee =
		if final_max_fee > u128::MAX as f64 { u128::MAX } else { final_max_fee.ceil() as u128 };

	(final_priority, final_max_fee)
}

fn max_fee_with_priority_headroom(
	estimated_max_fee_per_gas: u128,
	estimated_max_priority_fee_per_gas: u128,
	actual_max_priority_fee_per_gas: u128,
) -> u128 {
	estimated_max_fee_per_gas.saturating_add(
		actual_max_priority_fee_per_gas.saturating_sub(estimated_max_priority_fee_per_gas),
	)
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

	// ── compute_replacement_fees tests ─────────────────────────────────────

	#[test]
	fn replacement_fees_bump_above_estimate_and_floor() {
		// Old fees are low; current estimate * 1.25 dominates. Priority
		// is also scaled by the multiplier (BASE * PRIORITY_FEE_STEPS[0]
		// = 7.7 at the default step).
		let (prio, max_fee) = compute_replacement_fees(100, 1_000, 1_000, 10_000, 7.7);
		// priority: max(1000 * 7.7 * 1.25, 100 * 1.125) = max(9625, 112.5) = 9625
		assert_eq!(prio, 9_625);
		// max_fee preserves the RPC estimate's base-fee headroom before applying the bump:
		// max((10_000 + (7_700 - 1_000)) * 1.25, 1_000 * 1.125) = 20_875
		assert_eq!(max_fee, 20_875);
	}

	#[test]
	fn replacement_fees_floor_when_network_fee_dropped() {
		// Network estimate dropped dramatically between submit and timeout;
		// we must still beat the old fees by 12.5% to be EIP-1559 compliant.
		let (prio, max_fee) = compute_replacement_fees(10_000, 100_000, 100, 200, 7.7);
		// priority: max(100 * 7.7 * 1.25, 10_000 * 1.125) = max(962.5, 11_250) = 11_250
		assert_eq!(prio, 11_250);
		// max_fee: max(200 * 1.25, 100_000 * 1.125) = max(250, 112_500) = 112_500
		assert_eq!(max_fee, 112_500);
	}

	#[test]
	fn replacement_priority_capped_by_max_fee() {
		// Make priority candidate exceed max_fee candidate to verify cap.
		// old_prio huge vs old_max_fee small; multiplier 1.0 to keep math simple.
		let (prio, max_fee) = compute_replacement_fees(u128::MAX / 4, 1, 1, 1, 1.0);
		// max_fee from ceil'd max(1*1.25, 1*1.125) = ceil(1.25) = 2
		assert_eq!(max_fee, 2);
		// priority from huge floor, capped to max_fee = 2
		assert_eq!(prio, 2);
	}

	#[test]
	fn replacement_fees_meet_minimum_12_5_percent_bump() {
		// Sanity: any (old, current) pair yields a replacement fee that
		// is at least 1.125 * old in both fields (the user's "bump by at
		// least 12.5%" requirement).
		let (prio, max_fee) = compute_replacement_fees(1_000_000, 10_000_000, 2_000, 20_000, 1.0);
		// priority: max(2000 * 1.25, 1_000_000 * 1.125) = max(2500, 1_125_000) = 1_125_000
		assert_eq!(prio, 1_125_000);
		assert!(prio >= (1_125_000_u128));
		// max_fee: max(20_000 * 1.25, 10_000_000 * 1.125) = max(25_000, 11_250_000) = 11_250_000
		assert_eq!(max_fee, 11_250_000);
		assert!(max_fee >= (11_250_000_u128));
	}

	#[test]
	fn max_fee_preserves_priority_headroom_when_priority_is_scaled() {
		assert_eq!(max_fee_with_priority_headroom(10_000, 1_000, 7_700), 16_700);
	}

	#[test]
	fn max_fee_unchanged_when_priority_is_not_above_estimate() {
		assert_eq!(max_fee_with_priority_headroom(10_000, 1_000, 500), 10_000);
	}
}

use alloy::{
	primitives::B256,
	providers::{Provider, ProviderBuilder},
	rpc::types::TransactionReceipt,
};
use log::{error, info};
use reqwest::Url;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::price_updater::alerts;

#[derive(Debug, Clone, Copy)]
pub enum UpdateTxKind {
	DarkOracle,
	Pyth,
}

impl fmt::Display for UpdateTxKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			UpdateTxKind::DarkOracle => write!(f, "DarkOracle"),
			UpdateTxKind::Pyth => write!(f, "Pyth"),
		}
	}
}

pub struct UpdateTx {
	pub kind: UpdateTxKind,
	pub tx_hash: B256,
}

/// Receives `UpdateTx` messages and handles them concurrently
pub async fn run_tx_processor(mut rx: mpsc::Receiver<UpdateTx>) {
	let rpc_url = std::env::var("RPC_URL").expect("RPC_URL not set");
	let provider = Arc::new(
		ProviderBuilder::new()
			.on_http(Url::parse(&rpc_url).expect("Invalid RPC_URL"))
			.boxed(),
	);

	while let Some(tx) = rx.recv().await {
		let provider = Arc::clone(&provider);
		tokio::spawn(async move { confirm_tx(provider, tx.kind, tx.tx_hash).await });
	}
}

async fn confirm_tx<P>(provider: Arc<P>, kind: UpdateTxKind, tx_hash: B256)
where
	P: Provider + ?Sized,
{
	loop {
		match provider.get_transaction_receipt(tx_hash).await {
			Ok(Some(receipt)) => {
				if receipt.status() {
					info!(
						"[{}] transaction confirmed: tx_hash={:?}, block={:?}",
						kind, receipt.transaction_hash, receipt.block_number,
					);
				} else {
					on_tx_reverted(kind, &receipt).await;
				}
				break;
			},
			Ok(None) => {
				// Transaction not yet mined, wait and poll again
				tokio::time::sleep(std::time::Duration::from_secs(2)).await;
			},
			Err(e) => {
				on_tx_error(kind, Box::new(e)).await;
				break;
			},
		}
	}
}

async fn on_tx_reverted(kind: UpdateTxKind, receipt: &TransactionReceipt) {
	let message = format!(
		"[{}] transaction REVERTED on-chain: tx_hash={:?}, block={:?}",
		kind, receipt.transaction_hash, receipt.block_number,
	);
	error!("{}", message);

	alerts::send_slack_alert(message).await;
}

async fn on_tx_error(kind: UpdateTxKind, err: Box<dyn Error + Send + Sync + 'static>) {
	let message = format!("[{}] failed to confirm transaction: {:?}", kind, err);
	error!("{}", message);

	alerts::send_slack_alert(message).await;
}

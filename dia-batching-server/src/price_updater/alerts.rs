use crate::args::DiaApiArgs;
use clap::Parser;
use log::{error, info};
use serde::Serialize;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct PriceDivergenceAlert {
	pub asset: String,
	pub bp_divergence: f64,
	pub threshold_bp: u64,
	pub dark_oracle_price: f64,
	pub pyth_price: f64,
}

#[derive(Serialize)]
struct SlackMessage {
	channel: String,
	text: String,
}

pub async fn send_slack_alert(message: String) {
	let args = DiaApiArgs::parse();

	if let (Some(token), Some(channel)) = (args.slack_token, args.slack_channel_id) {
		let client = reqwest::Client::new();
		let body = SlackMessage { channel, text: message };

		let res = client
			.post("https://slack.com/api/chat.postMessage")
			.header("Authorization", format!("Bearer {}", token))
			.header("Content-Type", "application/json; charset=utf-8")
			.json(&body)
			.send()
			.await;

		match res {
			Ok(response) => {
				if !response.status().is_success() {
					error!("Failed to send slack alert: status {}", response.status());
				} else {
					info!("Slack alert sent successfully");
				}
			},
			Err(e) => error!("Error sending slack alert: {}", e),
		}
	}
}

pub async fn run_divergence_alert_processor(mut rx: mpsc::Receiver<PriceDivergenceAlert>) {
	while let Some(alert) = rx.recv().await {
		let message = format!(
			"{} price divergence too high: {:.2} bp > {} bp \
             (prices: DarkOracle: {}, Pyth: {})",
			alert.asset,
			alert.bp_divergence,
			alert.threshold_bp,
			alert.dark_oracle_price,
			alert.pyth_price,
		);

		error!("{}", message);

		send_slack_alert(message).await;
	}
}

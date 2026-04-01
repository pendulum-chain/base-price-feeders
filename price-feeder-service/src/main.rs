use crate::handlers::{currencies_post, health};
use crate::storage::CoinInfoStorage;
use std::collections::HashSet;
use std::error::Error;

use crate::api::PriceApiImpl;
use crate::args::DiaApiArgs;
use crate::types::AssetSpecifier;
use actix_web::{web, App, HttpServer};
use clap::Parser;
use log::{error, info};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::price_updater::{
	PriceDivergenceAlert, UpdateTx,
	alerts, tx_processor,
};

mod api;
mod args;
mod handlers;
mod price_updater;
mod storage;
mod types;

#[actix_web::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
	pretty_env_logger::init();
	dotenv::dotenv().ok();

	let args: DiaApiArgs = DiaApiArgs::parse();
	let storage = Arc::new(CoinInfoStorage::default());
	let data = web::Data::from(storage.clone());

	let supported_currencies_vec = args.supported_currencies.0;
	let supported_currencies: HashSet<AssetSpecifier> = supported_currencies_vec
		.iter()
		.filter_map(|asset| {
			let (blockchain, symbol) =
				asset.trim().split_once(":").or_else(|| {
					error!("Invalid asset '{}' – every asset needs to have the form <blockchain>:<symbol>", asset);
					None
				})?;

			Some(AssetSpecifier { blockchain: blockchain.into(), symbol: symbol.into() })
		})
		.collect();

	if supported_currencies.is_empty() {
		error!("No supported currencies provided. Exiting.");
		return Ok(());
	}

	let update_interval_seconds = args.update_interval_seconds;
	let pyth_update_interval_seconds = args.pyth_update_interval_seconds;
	let price_divergence_threshold_bp = args.price_divergence_threshold_bp;

	let (divergence_tx, divergence_rx) = mpsc::channel::<PriceDivergenceAlert>(100);

	tokio::spawn(async move {
		info!("Starting price divergence alert processor");
		alerts::run_divergence_alert_processor(divergence_rx).await;
	});

	let (update_tx, update_rx) = mpsc::channel::<UpdateTx>(200);

	tokio::spawn(async move {
		info!("Starting on-chain transaction processor");
		tx_processor::run_tx_processor(update_rx).await;
	});

	tokio::spawn(async move {
		info!("Starting price updater");
		let price_api = PriceApiImpl::new();
		let _ = price_updater::run_update_prices_loop(
			storage,
			supported_currencies,
			std::time::Duration::from_secs(update_interval_seconds),
			std::time::Duration::from_secs(pyth_update_interval_seconds),
			price_divergence_threshold_bp,
			price_api,
			divergence_tx,
			update_tx,
		)
		.await;
	});

	let port = 10000;
	println!("Running dia-batching-server on port {port}... (Press CTRL+C to quit)");
	HttpServer::new(move || {
		App::new().app_data(data.clone()).service(currencies_post).service(health)
	})
	.on_connect(|_, _| println!("Serving Request"))
	.bind(format!("0.0.0.0:{port}"))?
	.run()
	.await?;

	Ok(())
}

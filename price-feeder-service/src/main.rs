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
	alerts, chain::ChainClient, tx_processor, DarkOracleUpdater, PriceDivergenceAlert,
	PythPriceUpdater, UpdateTx,
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

	let pyth_updater =
		PythPriceUpdater::new(std::time::Duration::from_secs(pyth_update_interval_seconds))?;
	let nonce_manager = ChainClient::create_nonce_manager().await?;
	let dark_oracle_client = Arc::new(ChainClient::new(nonce_manager.clone()).await?);
	let pyth_client = Arc::new(ChainClient::new(nonce_manager).await?);
	let dark_oracle_updater = DarkOracleUpdater::new(
		dark_oracle_client.clone(),
		std::time::Duration::from_secs(update_interval_seconds),
	)?;

	let fetch_storage = storage.clone();
	let fetch_currencies = supported_currencies.clone();
	let fetch_update_tx = update_tx.clone();
	let coingecko_config = args.coingecko.clone();
	let fastforex_config = args.fastforex.clone();

	tokio::spawn(async move {
		info!("Starting fetch loop");
		let price_api = PriceApiImpl::new(coingecko_config, fastforex_config);
		let _ = price_updater::run_fetch_loop(
			fetch_storage,
			fetch_currencies,
			std::time::Duration::from_secs(1),
			price_api,
			fetch_update_tx,
		)
		.await;
	});

	let feed_storage = storage.clone();
	let feed_currencies = supported_currencies.clone();

	tokio::spawn(async move {
		info!("Starting feed loop");
		let _ = price_updater::run_feed_loop(
			feed_storage,
			feed_currencies,
			price_divergence_threshold_bp,
			dark_oracle_updater,
			divergence_tx,
			update_tx,
			pyth_updater,
			pyth_client,
		)
		.await;
	});

	let port = 10000;
	println!("Running price-feeder-service on port {port}... (Press CTRL+C to quit)");
	HttpServer::new(move || {
		App::new().app_data(data.clone()).service(currencies_post).service(health)
	})
	.on_connect(|_, _| println!("Serving Request"))
	.bind(format!("0.0.0.0:{port}"))?
	.run()
	.await?;

	Ok(())
}

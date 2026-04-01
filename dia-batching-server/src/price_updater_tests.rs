use crate::api::PriceApi;
use crate::storage::CoinInfoStorage;
use crate::types::{Quotation};
use crate::AssetSpecifier;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::*;

#[cfg(test)]
mod tests {
	use super::*;

	struct MockDia {
		quotation: HashMap<AssetSpecifier, Quotation>,
	}

	impl MockDia {
		pub fn new() -> Self {
			let mut quotation = HashMap::new();
			quotation.insert(
				AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
				Quotation {
					name: "BTC".into(),
					price: dec!(1.000000000000),
					symbol: "BTC".into(),
					time: Utc::now().timestamp().unsigned_abs(),
					blockchain: Some("Bitcoin".into()),
					supply: Decimal::from(1),
				},
			);
			quotation.insert(
				AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETH".into() },
				Quotation {
					name: "ETH".into(),
					price: dec!(1.000000000000),
					symbol: "ETH".into(),
					time: Utc::now().timestamp().unsigned_abs(),
					blockchain: Some("Ethereum".into()),
					supply: Decimal::from(1),
				},
			);
			quotation.insert(
				AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDT".into() },
				Quotation {
					name: "USDT".into(),
					price: dec!(1.000000000001),
					symbol: "USDT".into(),
					time: Utc::now().timestamp().unsigned_abs(),
					blockchain: Some("Ethereum".into()),
					supply: Decimal::from(1),
				},
			);
			quotation.insert(
				AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDC".into() },
				Quotation {
					name: "USDC".into(),
					price: dec!(123456789.123456789012345),
					symbol: "USDC".into(),
					time: Utc::now().timestamp().unsigned_abs(),
					blockchain: Some("Ethereum".into()),
					supply: Decimal::from(1),
				},
			);
			quotation.insert(
				AssetSpecifier { blockchain: "FIAT".into(), symbol: "MXN-USD".into() },
				Quotation {
					name: "MXNUSD=X".into(),
					price: dec!(0.053712327),
					symbol: "MXN-USD".into(),
					time: Utc::now().timestamp().unsigned_abs(),
					blockchain: None,
					supply: Decimal::from(1),
				},
			);
			quotation.insert(
				AssetSpecifier { blockchain: "FIAT".into(), symbol: "USD-USD".into() },
				Quotation {
					symbol: "USD-USD".to_string(),
					name: "USD-X".to_string(),
					blockchain: None,
					price: Decimal::new(1, 0),
					time: Utc::now().timestamp().unsigned_abs(),
					supply: Decimal::from(1),
				},
			);
			Self { quotation }
		}
	}

	#[async_trait]
	impl PriceApi for MockDia {
		async fn get_quotations(&self, assets: Vec<&AssetSpecifier>) -> Vec<Quotation> {
			let mut quotations = Vec::new();
			for asset in assets {
				if let Some(q) = self.quotation.get(asset) {
					quotations.push(q.clone());
				}
			}
			quotations
		}
	}

	fn setup_test_env() {
		std::env::set_var("PRIVATE_KEY", "0000000000000000000000000000000000000000000000000000000000000001");
		std::env::set_var("RPC_URL", "http://localhost:8545");
		std::env::set_var("CONTRACT_ADDRESS", "0x0000000000000000000000000000000000000000");
		std::env::set_var("PYTH_ADAPTER_ADDRESS", "0x0000000000000000000000000000000000000000");
	}

	async fn setup_updaters() -> (PythPriceUpdater, DarkOracleUpdater, Arc<ChainClient>, Arc<ChainClient>) {
		setup_test_env();
		let pyth_updater = PythPriceUpdater::new(std::time::Duration::from_secs(300)).unwrap();
		let dark_oracle_updater = DarkOracleUpdater::new().unwrap();
		let nonce_manager = Arc::new(chain::NonceManager::new(0));
		
		// In tests, we don't actually need a working provider for these update_prices calls 
		// because we are just testing the storage part in these specific tests.
		// However, ChainClient::new expects a valid URL and performs some setup.
		// Since we can't easily mock the whole provider chain without more complex setup,
		// we just ensure construction doesn't panic.
		let dark_oracle_client = Arc::new(ChainClient::new(nonce_manager.clone()).await.unwrap());
		let pyth_client = Arc::new(ChainClient::new(nonce_manager).await.unwrap());
		
		(pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client)
	}

	#[tokio::test]
	async fn test_update_prices() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let mut all_currencies = HashSet::default();
		let supported_currencies = vec![
			AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETH".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDT".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDC".into() },
		];
		for currency in supported_currencies.clone() {
			all_currencies.insert(currency);
		}

		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(supported_currencies);

		assert_eq!(4, c.len());
		assert_eq!(c[1].price, 1000000000000000000);
		assert_eq!(c[1].name, "ETH");
	}

	#[tokio::test]
	async fn test_update_prices_with_fiat_and_crypto_asset_works() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);

		let mut all_currencies = HashSet::new();
		all_currencies
			.insert(AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() });
		all_currencies
			.insert(AssetSpecifier { blockchain: "FIAT".into(), symbol: "MXN-USD".into() });

		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(vec![
			AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
			AssetSpecifier { blockchain: "FIAT".into(), symbol: "MXN-USD".into() },
		]);

		assert_eq!(2, c.len());
		assert_eq!(c[1].price, 53712327000000000);
		assert_eq!(c[1].name, "MXNUSD=X");
	}

	#[tokio::test]
	async fn test_update_prices_with_fiat_usd_works() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);

		let mut all_currencies = HashSet::new();
		all_currencies
			.insert(AssetSpecifier { blockchain: "FIAT".into(), symbol: "USD-USD".into() });

		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(vec![AssetSpecifier {
			blockchain: "FIAT".into(),
			symbol: "USD-USD".into(),
		}]);

		assert_eq!(1, c.len());
		assert_eq!(c[0].price, 1000000000000000000);
		assert_eq!(c[0].name, "USD-X");
	}

	#[tokio::test]
	async fn test_update_prices_non_existent() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let all_currencies = HashSet::default();
		
		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(vec![
			AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTCCash".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETHCase".into() },
		]);

		assert_eq!(0, c.len());
	}

	#[tokio::test]
	async fn test_update_prices_one_available() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let mut all_currencies = HashSet::default();
		let supported_currencies = vec![
			AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETHCase".into() },
		];
		for currency in supported_currencies.clone() {
			all_currencies.insert(currency);
		}
		
		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(supported_currencies);

		assert_eq!(1, c.len());
		assert_eq!(c[0].price, 1000000000000000000);
		assert_eq!(c[0].name, "BTC");
	}

	#[tokio::test]
	async fn test_update_prices_get_nothing() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let all_currencies = HashSet::default();
		
		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(vec![]);

		assert_eq!(0, c.len());
	}

	#[tokio::test]
	async fn test_update_prices_get_integers() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let all_currencies = HashSet::default();

		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(vec![AssetSpecifier {
			blockchain: "Bitcoin".into(),
			symbol: "123".into(),
		}]);

		assert_eq!(0, c.len());
	}

	#[tokio::test]
	async fn test_convert_result() {
		let mock_api = MockDia::new();
		let storage = Arc::new(CoinInfoStorage::default());
		let coins = Arc::clone(&storage);
		let mut all_currencies = HashSet::default();
		let supported_currencies = vec![
			AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDC".into() },
			AssetSpecifier { blockchain: "Ethereum".into(), symbol: "USDT".into() },
		];
		for currency in supported_currencies.clone() {
			all_currencies.insert(currency);
		}

		let (mut pyth_updater, dark_oracle_updater, dark_oracle_client, pyth_client) = setup_updaters().await;

		let (alert_tx, _) = tokio::sync::mpsc::channel(1);
		let (tx_tx, _) = tokio::sync::mpsc::channel(1);

		update_prices(
			coins,
			&all_currencies,
			&mock_api,
			&mut pyth_updater,
			&dark_oracle_updater,
			dark_oracle_client,
			pyth_client,
			0,
			&alert_tx,
			&tx_tx,
		)
		.await;

		let c = storage.get_currencies_by_blockchains_and_symbols(supported_currencies);

		assert_eq!(c[0].price, 1000000000000000000);
		assert_eq!(c[0].supply, 1000000000000000000);

		assert_eq!(c[1].price, 123456789123456789012345000);
		assert_eq!(c[1].supply, 1000000000000000000);

		assert_eq!(c[2].price, 1000000000001000000);
		assert_eq!(c[2].supply, 1000000000000000000);

		assert_eq!(c[0].name, "BTC");
		assert_eq!(c[1].name, "USDC");
		assert_eq!(c[2].name, "USDT");
	}
}

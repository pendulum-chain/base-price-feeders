use crate::storage::CoinInfoStorage;
use crate::types::CoinInfo;
use crate::AssetSpecifier;
use actix_web::web::Json;
use actix_web::{get, post, web, HttpResponse};

#[post("/currencies")]
pub async fn currencies_post(
	Json(currencies): Json<Vec<AssetSpecifier>>,
	storage: web::Data<CoinInfoStorage>,
) -> Json<Vec<CoinInfo>> {
	println!("Request currencies {:?}", currencies);
	Json(storage.get_ref().get_currencies_by_blockchains_and_symbols(currencies))
}

#[get("/health")]
pub async fn health() -> HttpResponse {
	HttpResponse::Ok().finish()
}

#[cfg(test)]
mod tests {
	use super::*;
	use actix_web::{http, test, App};
	use std::sync::Arc;

	fn get_storage() -> Arc<CoinInfoStorage> {
		let storage = Arc::new(CoinInfoStorage::default());
		storage.replace_currencies_by_symbols(vec![
			CoinInfo { symbol: "BTC".into(), blockchain: "Bitcoin".into(), ..Default::default() },
			CoinInfo { symbol: "ETH".into(), blockchain: "Ethereum".into(), ..Default::default() },
		]);
		storage
	}

	#[tokio::test]
	async fn test_currencies_post() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(&vec![
				AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "BTC".into() },
				AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETH".into() },
			])
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 2);
	}

	#[tokio::test]
	async fn test_currencies_post_empty() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let vec: Vec<AssetSpecifier> = vec![];
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(vec)
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 0);
	}
	#[tokio::test]
	async fn test_currencies_post_non_existent() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(&vec![AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "DASH".into() }])
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 0);
	}

	#[tokio::test]
	async fn test_currencies_post_non_existent_plus_existing() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(&vec![
				AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "DASH".into() },
				AssetSpecifier { blockchain: "Ethereum".into(), symbol: "ETH".into() },
			])
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 1);

		assert_eq!(r[0].symbol, smol_str::SmolStr::new_inline("ETH".into()))
	}

	#[tokio::test]
	async fn test_currencies_post_empty_string() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let vec: Vec<AssetSpecifier> = vec![];
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(vec)
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 0);
	}
	#[tokio::test]
	async fn test_currencies_post_special_char() {
		let storage = get_storage();
		let data = web::Data::from(storage.clone());

		let mut app =
			test::init_service(App::new().app_data(data.clone()).service(currencies_post)).await;
		let req = test::TestRequest::post()
			.uri("http://localhost:8080/currencies")
			.set_json(vec![AssetSpecifier { blockchain: "Bitcoin".into(), symbol: "$COIN".into() }])
			.to_request();

		let resp = test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), http::StatusCode::OK);

		let r: Vec<CoinInfo> = test::read_body_json(resp).await;

		assert_eq!(r.len(), 0);
	}
}

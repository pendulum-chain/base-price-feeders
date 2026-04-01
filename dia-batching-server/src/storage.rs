use crate::types::CoinInfo;
use crate::AssetSpecifier;
use arc_swap::ArcSwap;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct CoinInfoStorage {
	currencies_by_blockchain_and_symbol: ArcSwap<HashMap<(SmolStr, SmolStr), CoinInfo>>,
}

impl CoinInfoStorage {
	pub fn get_currencies_by_blockchains_and_symbols(
		&self,
		blockchain_and_symbols: Vec<AssetSpecifier>,
	) -> Vec<CoinInfo> {
		let reference = self.currencies_by_blockchain_and_symbol.load();
		blockchain_and_symbols
			.iter()
			.filter_map(|AssetSpecifier { blockchain, symbol }| {
				reference.get(&(blockchain.into(), symbol.into()))
			})
			.cloned()
			.collect()
	}

	#[allow(dead_code)]
	pub fn replace_currencies_by_symbols(&self, currencies: Vec<CoinInfo>) {
		let map_to_replace_with = currencies
			.into_iter()
			.map(|x| ((x.blockchain.clone(), x.symbol.clone()), x))
			.collect();

		self.currencies_by_blockchain_and_symbol.store(Arc::new(map_to_replace_with));
	}
}

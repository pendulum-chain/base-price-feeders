use crate::types::{CoinInfo, Quotation};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::error::Error;
use std::fmt::{Display, Formatter};

pub const BIPS_DIVISOR: u64 = 10000;

#[derive(Debug)]
pub enum ConvertingError {
	DecimalTooLarge,
}

impl Display for ConvertingError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			ConvertingError::DecimalTooLarge => write!(f, "Decimal given is too large"),
		}
	}
}

impl Error for ConvertingError {}

pub fn convert_decimal_to_u128(input: &Decimal) -> Result<u128, ConvertingError> {
	let fract = (input.fract() * Decimal::from(1_000_000_000_000_000_000_u128))
		.to_u128()
		.ok_or(ConvertingError::DecimalTooLarge)?;
	let trunc = (input.trunc() * Decimal::from(1_000_000_000_000_000_000_u128))
		.to_u128()
		.ok_or(ConvertingError::DecimalTooLarge)?;

	Ok(trunc.saturating_add(fract))
}

pub fn convert_to_coin_info(value: Quotation) -> Result<CoinInfo, Box<dyn Error + Sync + Send>> {
	let Quotation { name, symbol, blockchain, price, time, supply, provider, .. } = value;

	let price = convert_decimal_to_u128(&price)?;
	let supply = convert_decimal_to_u128(&supply)?;

	let coin_info = CoinInfo {
		name: name.into(),
		symbol: symbol.into(),
		blockchain: blockchain.unwrap_or("FIAT".to_string()).into(),
		price,
		last_update_timestamp: time,
		supply,
		provider: provider.as_str().into(),
	};

	Ok(coin_info)
}

use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct CoinbaseError(pub String);

impl Display for CoinbaseError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "CoinbaseError: {}", self.0)
	}
}

impl std::error::Error for CoinbaseError {}

#[derive(Debug)]
pub struct CoingeckoError(pub String);

impl Display for CoingeckoError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "CoingeckoError: {}", self.0)
	}
}

impl std::error::Error for CoingeckoError {}

#[derive(Debug)]
pub struct BinanceError(pub String);

impl Display for BinanceError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "BinanceError: {}", self.0)
	}
}

impl std::error::Error for BinanceError {}

#[derive(Debug)]
pub struct FastForexError(pub String);

impl Display for FastForexError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "FastForexError: {}", self.0)
	}
}

impl std::error::Error for FastForexError {}

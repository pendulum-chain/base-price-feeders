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
pub struct CustomError(pub String);

impl Display for CustomError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "CustomError: {}", self.0)
	}
}

impl std::error::Error for CustomError {}

#[derive(Debug)]
pub struct CoingeckoError(pub String);

impl Display for CoingeckoError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "CoingeckoError: {}", self.0)
	}
}

impl std::error::Error for CoingeckoError {}

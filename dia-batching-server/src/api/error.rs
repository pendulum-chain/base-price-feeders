use std::fmt;

#[derive(Debug)]
pub struct CustomError(pub String);

impl fmt::Display for CustomError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		let CustomError(ref err_msg) = *self;
		// Log the error message
		log::error!("CustomError: {}", err_msg);
		// Write the error message to the formatter
		write!(f, "{}", err_msg)
	}
}

#[derive(Debug)]
pub struct CoinbaseError(pub String);

impl fmt::Display for CoinbaseError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		let CoinbaseError(ref err_msg) = *self;
		// Log the error message
		log::error!("CoinbaseError: {}", err_msg);
		// Write the error message to the formatter
		write!(f, "{}", err_msg)
	}
}

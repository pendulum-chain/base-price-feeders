use clap::Parser;

fn parse_currency_vec(src: &str) -> SupportedCurrencies {
	let mut vec = Vec::new();
	for s in src.split(',') {
		vec.push(s.to_string());
	}
	SupportedCurrencies(vec)
}

// We need the extra struct to be able to parse the currencies to a Vec
#[derive(Clone, Debug)]
pub struct SupportedCurrencies(pub Vec<String>);

#[derive(Parser, Debug, Clone)]
#[clap(name = "dia-batching-server")]
pub struct DiaApiArgs {
	/// Iteration duration after one batch of requests
	#[clap(short, long, env = "UPDATE_INTERVAL_SECONDS", default_value = "1")]
	pub update_interval_seconds: u64,

	/// How often (in seconds) to update Pyth price feeds on-chain
	#[clap(long, env = "PYTH_UPDATE_INTERVAL_SECONDS", default_value = "5")]
	pub pyth_update_interval_seconds: u64,

	/// Maximum allowed price divergence in basis points (default 50 bps)
	#[clap(long, env = "PRICE_DIVERGENCE_THRESHOLD_BP", default_value = "50")]
	pub price_divergence_threshold_bp: u64,

	/// Currencies to support
	/// Each currency needs to have the format <blockchain>:<symbol>
	/// Fiat currencies need to have the format FIAT:<from>-<to>
	#[clap(short, long,
        parse(from_str = parse_currency_vec),
		env = "SUPPORTED_CURRENCIES",
        default_value = "Base:EURC,Base:USDC,Base:BRL"
    )]
	pub supported_currencies: SupportedCurrencies,

	/// The port to run the server on
	#[clap(short, long, env = "PORT", default_value = "10000")]
	pub port: u16,

	/// Slack token for alerts
	#[clap(long, env = "SLACK_TOKEN")]
	pub slack_token: Option<String>,

	/// Slack channel ID for alerts
	#[clap(long, env = "SLACK_CHANNEL_ID")]
	pub slack_channel_id: Option<String>,

	#[clap(flatten)]
	pub coingecko: CoingeckoConfig,

	#[clap(flatten)]
	pub fastforex: FastForexConfig,
}

#[derive(Parser, Debug, Clone)]
pub struct FastForexConfig {
	#[clap(long, env = "FF_API_KEY", default_value = "")]
	pub ff_api_key: String,

	#[clap(long, env = "FF_HOST_URL", default_value = "https://api.fastforex.io")]
	pub ff_host_url: String,
}

#[derive(Parser, Debug, Clone)]
pub struct CoingeckoConfig {
	#[clap(long, env = "CG_API_KEY")]
	pub cg_api_key: String,

	#[clap(long, env = "CG_HOST_URL", default_value = "https://pro-api.coingecko.com")]
	pub cg_host_url: String,
}

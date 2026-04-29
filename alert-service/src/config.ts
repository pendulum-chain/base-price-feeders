import dotenv from "dotenv";

dotenv.config();

const DEFAULT_SAFETY_MARGIN = "0.8";
const MIN_ETH_BALANCE_THRESHOLD_DEFAULT = "0.25";

export const config = {
  ALCHEMY_RPC_URL: process.env.ALCHEMY_RPC_URL || "",
  DARK_ORACLE_ADDRESS: (process.env.DARK_ORACLE_ADDRESS || "") as `0x${string}`,
  PYTH_ADAPTER_ADDRESS: (process.env.PYTH_ADAPTER_ADDRESS || "") as `0x${string}`,
  FEEDER_ACCOUNT_ADDRESS: (process.env.FEEDER_ACCOUNT_ADDRESS || "0x707e17f496a4a0cc6e0eda73480809b2385a7213") as `0x${string}`,
  SLACK_TOKEN: process.env.SLACK_TOKEN,
  SLACK_CHANNEL_ID: process.env.SLACK_CHANNEL_ID,

  // Monitoring thresholds
  MIN_ETH_BALANCE_THRESHOLD: Object.is(parseFloat(process.env.MIN_ETH_BALANCE_THRESHOLD || ""), NaN)
    ? parseFloat(MIN_ETH_BALANCE_THRESHOLD_DEFAULT)
    : parseFloat(process.env.MIN_ETH_BALANCE_THRESHOLD || MIN_ETH_BALANCE_THRESHOLD_DEFAULT),

  SAFETY_MARGIN: Object.is(parseFloat(process.env.SAFETY_MARGIN || ""), NaN)
    ? parseFloat(DEFAULT_SAFETY_MARGIN)
    : parseFloat(process.env.SAFETY_MARGIN || DEFAULT_SAFETY_MARGIN),

  PYTH_FEEDS: {
    eurcUsd: "0x76fa85158bf14ede77087fe3ae472f66213f6ea2f5b411cb2de472794990fa5c" as `0x${string}`,
    usdcUsd: "0xeaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a" as `0x${string}`,
    brlaUsd: "0x1b9c8e5a7c3f2d9e4b1a5c6d8e7f9a0b2c3d4e5f67890abcdef1234567890abcd" as `0x${string}`,
  }
};

// Validate required configurations
if (!config.ALCHEMY_RPC_URL) throw new Error("Missing ALCHEMY_RPC_URL");
if (!config.DARK_ORACLE_ADDRESS) throw new Error("Missing DARK_ORACLE_ADDRESS");
if (!config.PYTH_ADAPTER_ADDRESS) throw new Error("Missing PYTH_ADAPTER_ADDRESS");

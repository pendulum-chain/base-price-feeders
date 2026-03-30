import { createPublicClient, http, parseAbiItem, parseAbi } from 'viem';
import { base } from 'viem/chains';
import { config } from './config';

export const publicClient = createPublicClient({
  chain: base,
  transport: http(config.ALCHEMY_RPC_URL),
});

export const DARK_ORACLE_ABI = parseAbi([
  'function priceMaxAge() external view returns (uint256)',
  'event PricesUpdated(address indexed sender, uint48 eth, uint48 btc, uint48 stable, uint48 native, uint48 asset0, uint56 timestamp)'
]);

export const PYTH_ADAPTER_ABI = parseAbi([
  'function getPythContractAddress() external view returns (address)',
  'function getPriceMaxAge(address target) external view returns (uint256)'
]);

export const PYTH_CONTRACT_ABI = parseAbi([
  'function latestPriceInfoPublishTime(bytes32 priceFeedId) external view returns (uint64)'
]);

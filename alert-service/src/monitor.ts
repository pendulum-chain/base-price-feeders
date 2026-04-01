import { formatEther } from 'viem';
import { config } from './config';
import { sendSlackAlert } from './slack';
import { publicClient, DARK_ORACLE_ABI, PYTH_ADAPTER_ABI, PYTH_CONTRACT_ABI } from './contracts';

const RPC_TIMEOUT_MS = 10000;

function withTimeout<T>(promise: Promise<T>, ms: number = RPC_TIMEOUT_MS): Promise<T> {
  let timeoutId: NodeJS.Timeout;
  const timeoutPromise = new Promise<never>((_, reject) => {
    timeoutId = setTimeout(() => {
      reject(new Error(`Operation timed out after ${ms}ms`));
    }, ms);
  });
  return Promise.race([promise, timeoutPromise]).finally(() => clearTimeout(timeoutId));
}


export async function checkAccountBalance() {
  try {
    const balance = await withTimeout(publicClient.getBalance({ address: config.FEEDER_ACCOUNT_ADDRESS }));
    const balanceEth = parseFloat(formatEther(balance));

    if (balanceEth < config.MIN_ETH_BALANCE_THRESHOLD) {
      await sendSlackAlert(`🚨 Low Balance Alert: Feeder account ${config.FEEDER_ACCOUNT_ADDRESS} has ${balanceEth.toFixed(4)} ETH (Below threshold of ${config.MIN_ETH_BALANCE_THRESHOLD} ETH).`);
    } else {
      console.log(`✅ Balance check passed: ${balanceEth.toFixed(4)} ETH`);
    }
  } catch (error) {
    console.error("Failed to check account balance:", error);
    await sendSlackAlert(`⚠️ Monitor Error: Failed to check account balance. ${(error as Error).message}`);
  }
}

export async function checkStaleness() {
  try {
    const currentBlock = await withTimeout(publicClient.getBlockNumber());

    let darkOracleMaxAgeMs = await withTimeout(publicClient.readContract({
      address: config.DARK_ORACLE_ADDRESS,
      abi: DARK_ORACLE_ABI,
      functionName: 'priceMaxAge',
    }));

    const maxAgeMs = darkOracleMaxAgeMs;

    const stalenessThresholdMs = (maxAgeMs * BigInt(config.SAFETY_MARGIN * 100)) / BigInt(100);

    // Use a fixed number of blocks instead of inferring based on block time
    const fromBlock = currentBlock - BigInt(8);

    const logs = await withTimeout(publicClient.getLogs({
      address: config.DARK_ORACLE_ADDRESS,
      event: DARK_ORACLE_ABI[1],
      fromBlock,
      toBlock: currentBlock
    }));

    // Logs are returned from oldest to newest, reverse to get the most recent first
    let logsDescending = logs.reverse();

    if (logs.length === 0) {
      await sendSlackAlert(`🚨 DarkOracle Staleness Alert: No PricesUpdated events emitted by DarkOracle (${config.DARK_ORACLE_ADDRESS}) in the last ${8} blocks.`);
    } else {
      let eventTimestampMs = 0;

      for (const log of logsDescending) {
        if (log.eventName === "PricesUpdated") {
          const ts = Number((log.args as any).timestamp);
          if (ts > 0) {
            eventTimestampMs = ts;
            break;
          }
        }
      }

      if (eventTimestampMs === 0) {
        await sendSlackAlert(`🚨 DarkOracle Staleness Alert: No valid timestamp found in the last ${logs.length} PricesUpdated events from DarkOracle (${config.DARK_ORACLE_ADDRESS}).`);
      } else {
        const currentTimestampMs = Math.floor(Date.now());
        const ageMs = (currentTimestampMs - eventTimestampMs);

        if (BigInt(ageMs) > stalenessThresholdMs) {
          await sendSlackAlert(
            `🚨 DarkOracle Staleness Alert:\n` +
            `• Current Age: ${ageMs}ms\n` +
            `• Max Age: ${maxAgeMs}ms\n` +
            `• Safety Margin: ${config.SAFETY_MARGIN * 100}%\n` +
            `• Effective Threshold: ${stalenessThresholdMs}ms`
          );
        } else {
          console.log(
            `✅ DarkOracle Staleness check passed:\n` +
            `• Current Age: ${ageMs}ms\n` +
            `• Max Age: ${maxAgeMs}ms\n` +
            `• Safety Margin: ${config.SAFETY_MARGIN * 100}%\n` +
            `• Effective Threshold: ${stalenessThresholdMs}ms`
          );
        }
      }
    }
  } catch (error) {
    console.error("Failed to check staleness:", error);
    await sendSlackAlert(`⚠️ Monitor Error: Failed to check DarkOracle staleness. ${(error as Error).message}`);
  }
}

export async function checkOraclePrices() {
  try {

    let pythAdapterContractAddress = await withTimeout(publicClient.readContract({
      address: config.PYTH_ADAPTER_ADDRESS,
      abi: PYTH_ADAPTER_ABI,
      functionName: 'getPythContractAddress',
    }));


    // Pyth adapter priceMaxAgeByContract is in SECONDS according to user reqs.
    let pythAdapterMaxAgeSeconds = await withTimeout(publicClient.readContract({
      address: config.PYTH_ADAPTER_ADDRESS,
      abi: PYTH_ADAPTER_ABI,
      functionName: 'getPriceMaxAge',
      args: [config.DARK_ORACLE_ADDRESS]
    }));

    const pythContractAgeLimitSecs = Number(pythAdapterMaxAgeSeconds) * config.SAFETY_MARGIN;
    const currentTimestampSecs = Math.floor(Date.now() / 1000);

    // 2. Check each feed
    for (const [feedName, feedId] of Object.entries(config.PYTH_FEEDS)) {
      const publishTimeSecs = await withTimeout(publicClient.readContract({
        address: pythAdapterContractAddress,
        abi: PYTH_CONTRACT_ABI,
        functionName: 'latestPriceInfoPublishTime',
        args: [feedId]
      }));

      const priceAgeSecs = currentTimestampSecs - Number(publishTimeSecs);

      if (priceAgeSecs > pythContractAgeLimitSecs) {
        await sendSlackAlert(
          `🚨 Oracle Staleness Alert: Pyth Feed ${feedName} is stale!\n` +
          `• Current Age: ${priceAgeSecs}s\n` +
          `• Max Age: ${pythAdapterMaxAgeSeconds}s\n` +
          `• Safety Margin: ${config.SAFETY_MARGIN * 100}%\n` +
          `• Effective Threshold: ${pythContractAgeLimitSecs}s`
        );
      } else {
        console.log(
          `✅ Pyth ${feedName} feed is fresh:\n` +
          `• Current Age: ${priceAgeSecs}s\n` +
          `• Max Age: ${pythAdapterMaxAgeSeconds}s\n` +
          `• Safety Margin: ${config.SAFETY_MARGIN * 100}%\n` +
          `• Effective Threshold: ${pythContractAgeLimitSecs}s`
        );
      }
    }

  } catch (error) {
    console.error("Failed to check oracle prices:", error);
    await sendSlackAlert(`⚠️ Monitor Error: Failed to check corresponding Pyth oracle prices against DarkOracle configuration. ${(error as Error).message}`);
  }
}

export async function runAllChecks() {
  console.log(`\n--- Running Monitor Checks at ${new Date().toISOString()} ---`);
  await Promise.allSettled([
    checkAccountBalance(),
    checkStaleness(),
    checkOraclePrices()
  ]);
  console.log(`--- Finished Checks ---\n`);
}

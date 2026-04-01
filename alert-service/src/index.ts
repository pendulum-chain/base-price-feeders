
import { runAllChecks } from './monitor';

async function main() {
  console.log("Starting Oracle Price Feeder Monitor Service...");
  await runAllChecks();

}

main().catch((error) => {
  console.error("service error:", error);
  process.exit(1);
});

const { test } = require('node:test');
const assert = require('node:assert/strict');
const { findDarkOracleHistory } = require('../dist/darkOracleHistory');
const NOW = 1800000000000;
function fixture({ cadence = 200, eventAge = 9327, eventBlock, fail = false, lag = 0 } = {}) {
  const calls = [];
  const head = 100000n;
  const updateBlock = eventBlock ?? head - BigInt(Math.ceil(eventAge / cadence));
  return { calls, reader: {
    async getBlock(n = head) { return { number: n, timestamp: BigInt(Math.floor((NOW - lag - Number(head - n) * cadence) / 1000)) }; },
    async getLogs(from, to) {
      calls.push([from, to]);
      assert.ok(to - from < 500n);
      if (fail) throw new Error('RPC unavailable');
      return eventAge !== null && updateBlock >= from && updateBlock <= to ? [{ eventName: 'PricesUpdated', args: { timestamp: BigInt(NOW - eventAge) } }] : [];
    },
  }};
}
test('healthy update outside eight 200ms blocks is found', async () => {
  const f = fixture(); const r = await findDarkOracleHistory(f.reader, 48000, NOW);
  assert.equal(Number(r.logs[0].args.timestamp), NOW - 9327);
  assert.ok(100000n - r.fromBlock > 8n);
});
test('same healthy event is found with two-second blocks', async () => {
  const r = await findDarkOracleHistory(fixture({cadence: 2000}).reader, 48000, NOW);
  assert.equal(r.logs.length, 1);
});
test('fast cadence scans multiple nonoverlapping chunks to cover actual time', async () => {
  const f = fixture({cadence: 20, eventAge: null});
  const r = await findDarkOracleHistory(f.reader, 48000, NOW);
  assert.ok(f.calls.length > 1);
  for (let i = 1; i < f.calls.length; i++) assert.equal(f.calls[i][1], f.calls[i-1][0] - 1n);
  assert.ok(Number((await f.reader.getBlock(r.fromBlock)).timestamp) * 1000 <= NOW - 78000);
});
test('transport failure and stale RPC head are errors, not missing-event results', async () => {
  await assert.rejects(findDarkOracleHistory(fixture({fail: true}).reader, 48000, NOW), /RPC unavailable/);
  await assert.rejects(findDarkOracleHistory(fixture({lag: 40000}).reader, 48000, NOW), /head/);
});
test('scan that outlasts its time limit fails closed', async t => {
  let clock = NOW;
  t.mock.method(Date, 'now', () => clock);
  const f = fixture({cadence: 1, eventAge: null});
  const reader = { ...f.reader, getLogs: (from, to) => { clock += 20000; return f.reader.getLogs(from, to); } };
  await assert.rejects(findDarkOracleHistory(reader, 48000, NOW), /time limit/);
  assert.ok(f.calls.length <= 4);
});
test('block cadence does not cap how far a scan can reach', async () => {
  const f = fixture({cadence: 1, eventAge: null});
  const r = await findDarkOracleHistory(f.reader, 48000, NOW);
  assert.equal(r.logs.length, 0);
  assert.ok(f.calls.length > 32);
});

process.env.ALCHEMY_RPC_URL = 'http://127.0.0.1:9';
process.env.DARK_ORACLE_ADDRESS = '0x0000000000000000000000000000000000000001';
process.env.SAFETY_MARGIN = '0.8';
const contracts = require('../dist/contracts');
const slack = require('../dist/slack');
const { checkStaleness } = require('../dist/monitor');
async function runCheck(t, options) {
  t.mock.method(Date, 'now', () => NOW);
  const f = fixture(options);
  const alerts = [];
  t.mock.method(slack, 'sendSlackAlert', async message => { alerts.push(message); });
  t.mock.method(contracts.publicClient, 'getBlock', ({blockNumber}) => f.reader.getBlock(blockNumber));
  t.mock.method(contracts.publicClient, 'getLogs', ({fromBlock,toBlock}) => f.reader.getLogs(fromBlock,toBlock));
  t.mock.method(contracts.publicClient, 'readContract', async () => 60000n);
  await checkStaleness();
  return alerts;
}
for (const [label, options, expectedAlert] of [
  ['false positive',{eventAge:9327},false],
  ['threshold boundary',{eventAge:48000},false],
  ['genuinely stale',{eventAge:49000},true],
  ['no updates',{eventAge:null},true],
  // Head 25s behind: this update is 30s old at the head but 55s against the wall clock.
  ['lagging head does not inflate age',{eventAge:55000,eventBlock:99850n,lag:25000},false],
  // 49s old at that same head, so still stale.
  ['lagging head still reports staleness',{eventAge:74000,eventBlock:99755n,lag:25000},true],
]) {
  test(`upstream freshness check: ${label}`, async t => {
    const alerts = await runCheck(t, options);
    assert.equal(alerts.length > 0, expectedAlert);
    if (expectedAlert) assert.match(alerts[0], /Staleness Alert/);
  });
}
test('SAFETY_MARGIN that is inexact in floating point still gives a threshold', async t => {
  const { config } = require('../dist/config');
  const margin = config.SAFETY_MARGIN;
  config.SAFETY_MARGIN = 0.55;
  t.after(() => { config.SAFETY_MARGIN = margin; });
  assert.deepEqual(await runCheck(t, {eventAge: 9327}), []);
});

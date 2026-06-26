# Transaction Recovery Strategy

## Goal

Minimize time spent without new transactions entering the mempool. Old
transactions are irrelevant — if tick `t` has not confirmed by tick `t+1`,
we only care about getting `t+1` in.

## Core Idea: Latest-Wins Nonce Replacement

A single `TxEngine` owns one nonce slot and one active tx at a time. On
every feed tick, the newest payload replaces the pending one at the same
nonce with a higher fee. No queue of higher-nonce txs is built up behind a
stuck tx.

```
tick t:   submit latest update with nonce N
tick t+1: if nonce N not confirmed, replace nonce N with t+1's payload + higher fee
tick t+2: if still not confirmed, replace again with t+2's payload + even higher fee
when N confirms: advance to N+1, immediately submit latest pending intent
```

This is the fastest path to inclusion because EVM account txs are
nonce-ordered. Sending `N+1` while `N` is stuck does not help — it sits
behind `N`. Replacing `N` directly competes for the next block slot.

## Why Not Fire-And-Forget

The previous design fired a new tx every tick and tracked hashes
after-the-fact. During congestion this built a backlog of stale
higher-nonce txs that could not mine until the blocking nonce cleared.
Recovery was passive — it waited for old txs to confirm or revert on
their own. The new design is active: it attacks the blocking nonce
directly.

## TxEngine State

```
TxEngine {
    next_nonce: u64,            // next fresh nonce to use
    active: Option<ActiveTx>,   // the one currently pending tx
    priority_multiplier: ...    // shared fee step ladder
}

ActiveTx {
    nonce, hash, kind, submitted_at,
    priority_fee, max_fee, replacement_count
}
```

No rolling `Vec<TrackedTx>`. No bounded history that can silently drop
unresolved entries. One nonce, one tx, full visibility.

## Submission Flow

1. Feed loop builds the latest tx request (DarkOracle or Pyth).
2. Calls `send_tx_with_retry(tx_req, kind)`.
3. Engine locks state:
   - If `active` is a replaceable price tx (DarkOracle/Pyth), reuse its
     nonce and replace with higher fees.
   - If `active` is a critical tx (Enable/Disable) or `None`, use
     `next_nonce` and advance.
4. Fee is computed as the max of the network estimate (with multiplier)
   and the previous fee + 12.5% (minimum replacement bump).
5. On success, `active` is updated. On "nonce too low", engine syncs to
   the RPC pending nonce (advancing only, never backwards) and retries.

## Replacement Fee Strategy

Fees use the existing `PriorityFeeMultiplier` step ladder
(`7.7x → 8.4x → 9.8x → 10.5x → 14.0x → 21.0x` of base priority fee).

On replacement:

```
new_priority_fee = max(network_estimate * multiplier, old_priority_fee * 1.125)
new_max_fee      = max(network_estimate * multiplier, old_max_fee * 1.125)
```

The multiplier is bumped up on each replacement and decays back to base
after 5 minutes of successful confirmations.

## Tx Kind Priority

| Active tx kind     | Can be replaced by?         |
|--------------------|-----------------------------|
| DarkOracle         | DarkOracle, Pyth, Enable, Disable |
| Pyth               | DarkOracle, Pyth, Enable, Disable |
| EnableAsset        | (nothing — sticky until confirmed/reverted) |
| DisableAsset       | (nothing — sticky until confirmed/reverted) |

Critical state-transition txs (enable/disable) are sticky. A price update
will not replace them, but a critical tx can replace a pending price
update.

## Confirmation Loop

A background task polls every 2 seconds:

- **Receipt found, success**: clear `active`, set `next_nonce = nonce + 1`.
- **Receipt found, reverted**: clear `active`, set `next_nonce = nonce + 1`,
  send Slack alert.
- **No receipt, but on-chain nonce advanced past active nonce**: the tx
  was replaced or consumed. Clear `active`, advance `next_nonce`.
- **No receipt, nonce unchanged**: tx still pending. Next tick's
  `submit()` will replace it.

## Nonce Sync Rules

- **Startup**: `next_nonce = latest nonce` (not pending). This avoids
  building on top of any stale queued txs from a previous run.
- **During operation**: `next_nonce` only ever advances.
  `next_nonce = max(current, rpc_pending_nonce)`. Never forced
  backwards.
- **After confirmation**: `next_nonce = confirmed_nonce + 1`.

## What Was Removed

- `NonceManager` and its `force_sync_nonce` / `spawn_resync_handler`.
- `run_tx_processor` watchdog and `TrackedTx` rolling buffer.
- `check_watchdog_timeout` scan and resync signal channel.
- The `update_tx` mpsc channel and `send_tx` forwarder.
- `estimate_priority_fee` calls from individual updaters (engine owns
  fees now).
- `MAX_ELAPSED_INTERVAL_MULTIPLIER` / "Dropped outdated transaction"
  timeout (replacement makes this unnecessary).
- `nonce_tx_timeout_secs` arg (no longer used).

## Expected Log Output

```
INFO  [TxEngine] New tx submitted: nonce=2777814 hash=0xaaa kind=DarkOracle priority_fee=7700000 wei
INFO  [TxEngine] Confirmed: nonce=2777814 hash=0xaaa block=Some(47811055)
INFO  [TxEngine] New tx submitted: nonce=2777815 hash=0xbbb kind=DarkOracle priority_fee=7700000 wei
WARN  [TxEngine] Replacing active tx: nonce=2777815 old_hash=0xbbb kind=DarkOracle replacement=1 priority_fee=9800000 wei
INFO  [TxEngine] Replacement submitted: nonce=2777815 hash=0xccc kind=DarkOracle priority_fee=9800000 wei
INFO  [TxEngine] Confirmed: nonce=2777815 hash=0xccc block=Some(47811057)
```

No more `nonce too low` loops. No more `Dropped outdated transaction`.
No more passive recovery waiting for old txs to resolve.

## Verification Audit

Audited against EVM protocol rules (EIP-1559, EIP-2718), geth/erigon
mempool policy, and the `alloy` Rust provider documentation
(`Provider::get_transaction_count` defaults to `Latest`;
`estimate_eip1559_fees`; `send_transaction`).

### Correct Assumptions

| # | Claim | EVM / alloy reality | Code |
|---|-------|---------------------|------|
| 1 | EVM account txs execute nonce-ordered; sending `N+1` behind stuck `N` doesn't help | True — protocol requires lower nonce to confirm first; higher-nonce txs sit in mempool unusable until `N` clears | — (design premise) |
| 2 | Replace same-nonce tx with higher fee to compete directly | True — txpool accepts replacement at identical `(from, nonce)` with required fee bump | `tx_engine.rs:103-105` sets same nonce + higher fees |
| 3 | 12.5% bump satisfies the replacement rule | geth/erigon require ≥10% increase on **both** `maxFeePerGas` AND `maxPriorityFeePerGas`. `old + old/8` = 1.125× clears it with margin | `tx_engine.rs:214-217` bumps both, each via `max(est, old*1.125)` |
| 4 | Fees = max(`network_estimate*multiplier`, `old*1.125`) | Correct — guarantees protocol bump AND stays competitive when congestion rises | `tx_engine.rs:206-218` matches the doc formula exactly |
| 5 | Step ladder `7.7 → 8.4 → 9.8 → 10.5 → 14.0 → 21.0` | base `7.0 * [1.1, 1.2, 1.4, 1.5, 2.0, 3.0]`; `bump_up` escalates, `try_bump_down` resets after 300s cooldown | `chain.rs:22-99` matches |
| 6 | `next_nonce` never goes backwards | All writes guarded by `if chain_nonce > next_nonce` / `if next_nonce <= nonce` | `tx_engine.rs:132, 168-170, 245-247, 290-292` |
| 7 | Startup uses `latest` (not `pending`) — avoids replaying stale queued txs | alloy `get_transaction_count(addr)` defaults to `BlockNumberOrTag::Latest`. `latest` is the right choice: `pending` would return `N+1` after a queued tx at `N`, forcing submit at `N+1` stuck behind `N`. `latest` returns `N` so the new (higher-fee) tx *replaces* any stale queued tx at `N` | `tx_engine.rs:52-53` |
| 8 | "nonce too low" → sync to pending nonce (advance only), retry | Correct use of `.pending()` (includes mempool) so the next submit doesn't collide with already-broadcast txs | `tx_engine.rs:151-170` (uses `.pending()` explicitly) |
| 9 | Critical txs (Enable/Disable) sticky; can replace a price update but cannot be replaced | `is_replaceable()` gates on the *old* active tx's kind. Enable/Disable → false → not replaceable; pending DarkOracle → true → new Enable tx reuses the nonce and replaces it | `tx_engine.rs:28-32, 82-93` matches the doc's table |
| 10 | Reverted tx → clear active, `next_nonce = nonce+1`, Slack | Reverted txes still consume the nonce, so advance is correct | `tx_engine.rs:244-265` |
| 11 | No-receipt but on-chain nonce advanced → tx was consumed/replaced, advance | Uses `.latest()` (confirmed), so this only fires once a tx at that nonce is actually mined | `tx_engine.rs:282-294` |
| 12 | Async race guard on stale receipts | `if active.nonce != nonce { return }` prevents an old hash's receipt from clobbering a newer replacement's state | `tx_engine.rs:237-242` |

### Minor Inaccuracies / Operational Caveats

1. **"Decays back to base after 5 minutes of successful confirmations"** (line 80-81) is imprecise. The decay is *lazy*: `try_bump_down()` is only invoked on the next **fresh** tx path (`estimate_fees`, `tx_engine.rs:192`), not actively on each confirmation. It is a hard reset to step 0, not a gradual step-by-step decay. Effectively equivalent, but the wording oversells it.

2. **"when N confirms: advance to N+1, immediately submit latest pending intent"** (line 20-21) is misleading. There is **no proactive re-submit on confirmation**. The engine only clears `active` and ratchets `next_nonce`. The next feed tick drives the next submit. Same outcome in steady state, but not literally "immediate."

3. **Replacement-race edge case (not a bug):** If the original tx confirms in the same/next block as a replacement is submitted, the new (replacement) hash gets dropped from the mempool and will never produce a receipt. Recovery occurs via the "no receipt, on-chain nonce advanced" branch (`tx_engine.rs:282-294`) within ≤2 s — but you won't see a `Confirmed:` log for that nonce; you'll see a `Nonce … consumed on-chain` log instead.

4. **12.5% bump may be insufficient on some non-mainnet EVMs.** Standard Ethereum geth/erigon require 10%, so 12.5% is safe. A few sidechains/L2s impose higher floors (e.g. 20%, or a fixed absolute delta). If this feeder ever targets such a chain, replacement could be rejected with "replacement underpriced." Worth a sanity check per chain before deploying.

5. **Lock held across RPC await.** `submit()` holds `inner` from line 79 through the `send_transaction` await (line 115). This serializes submit vs. the confirmation loop correctly, but the confirmation poll can be blocked for the duration of an RPC round-trip. No correctness issue; minor latency only.

### Bottom Line

The doc accurately describes the code, and the code correctly implements
EIP-1559 transaction-replacement + nonce-management rules. No correctness
bugs found. The five notes above are wording nuances or operational caveats,
not logic errors.

## Open Questions & Behaviors To Be Tested

### OQ-1: Replacement race when tick interval ≲ block time

The confirmation loop polls every 2 s
(`CONFIRMATION_POLL_INTERVAL`, `tx_engine.rs:14`). When the feed tick
interval is close to or shorter than the block time, tick `t+1`'s
`submit()` will frequently find `active` still set even though nonce `N`
has *already* confirmed on-chain — the poll simply hasn't observed it yet.
`submit()` then branches on `active.is_replaceable()` (`tx_engine.rs:85`)
and attempts a replacement at nonce `N`.

Two sub-cases arise and must be tested:

- **Case A — `N` already confirmed (false-positive replacement):** the
  replacement is rejected with `"nonce too low"`. The retry path
  (`tx_engine.rs:150-172`) syncs to the `.pending()` nonce, clears
  `active`, and resubmits a fresh tx at `N+1`. Outcome is correct, but we
  pay for a wasted replacement RPC + a nonce-sync RPC + extra latency.
- **Case B — `N` still genuinely pending:** the replacement is a legitimate
  RBF swap of `t`'s stale payload for `t+1`'s. This is the intended
  "latest-wins" behavior (see Core Idea above), not a bug.

**Open question:** is the Case A overhead acceptable at the production
tick interval, or should `submit()` pre-check the on-chain nonce
(`get_transaction_count(.latest())`) before deciding replacement vs.
fresh?

**To be tested:**
- Run with tick interval == chain block time and measure the fraction of
  ticks that take the `"nonce too low"` retry path.
- Run with tick interval 2× block time and compare.
- Confirm the retry path always converges (no stuck `active`, `next_nonce`
  advances correctly) under sustained Case A pressure.

### OQ-2: Permanent fee-ladder escalation under frequent replacements

`compute_replacement_fees` calls `priority_multiplier.bump_up()`
(`tx_engine.rs:212`) **unconditionally** whenever a replaceable tx is
active at submit time — including the false-positive Case A above.
`bump_up()` advances the shared step index
(`7.7 → 8.4 → 9.8 → 10.5 → 14.0 → 21.0`, `chain.rs:51-61`) and pegs at
21×.

`try_bump_down()` (`chain.rs:87-99`) only resets to step 0 after
`PRIORITY_FEE_BUMP_DOWN_COOLDOWN = 300 s` (`chain.rs:31`) have elapsed
since the last bump, and it is only invoked on the *fresh*-tx fee path
(`estimate_fees`, `tx_engine.rs:193`), never on replacements.

In a high-frequency regime (ticks ≈ block time), replacements fire on
nearly every tick, so:
- The ladder pegs at 21× within ~5 ticks.
- The 300 s cooldown can never elapse between frequent replacements, so
  the multiplier never decays — it sits permanently at 21× even when
  confirmations are perfectly healthy.

The feeder therefore overpays on priority fees for txs that confirm
without issue. Doc note #3 covers the *recovery* side of the
replacement-race (the "no receipt, on-chain nonce advanced" branch) but
does not address this fee-escalation side effect.

**Open question:** should `bump_up()` fire only after a replacement is
actually *accepted* (inside the `Ok(pending_tx)` arm, `tx_engine.rs:119`)
rather than before the submit attempt? This would stop false-positive
escalation from Case A. Should the cooldown / decay logic be revisited so
the multiplier can step down under sustained-but-healthy replacement
pressure?

**To be tested:**
- Drive replacements on every tick for >5 min and observe whether the
  multiplier ever steps back down (expected: it does not).
- Measure priority fees paid vs. a baseline run with tick interval ≫
  block time.
- Verify the ladder pegs at 21× and never exceeds it (no unbounded
  growth).

### Candidate mitigations (not yet decided)

1. **Pre-submit nonce check** — before deciding `is_replacement`, call
   `get_transaction_count(.latest())`; if on-chain nonce > `active.nonce`,
   skip straight to the fresh-tx path. One extra RPC per tick, but
   eliminates the wasted replacement RPC *and* the false `bump_up()`.
2. **Move `bump_up()` past acceptance** — fire it only inside the
   `Ok(pending_tx)` arm when `is_replacement`. Minimal change; stops
   false-positive escalation but does not remove the wasted RPC.
3. **Shorten the poll interval** below the tick interval (e.g. 1 s poll
   vs 3 s tick) to shrink the confirmed-but-undetected window. Helps but
   does not eliminate either issue.

Options 1 and 2 are complementary; option 3 is independent.

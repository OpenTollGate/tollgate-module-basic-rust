# CRITICAL — the Spilman rail accepts unsigned payments (credit forgery)

**Found:** 2026-10-21 by the host E2E (`host-e2e-spilman.sh`), which is exactly the
test that exists to catch what unit tests miss.
**Severity:** critical / fund-safety. Anyone who can reach the HTTP surface can
mint credit against an arbitrary channel id.
**Status:** OPEN. Not fixed in this commit — recorded so it is not lost, and the
reproduce is exact.

## The reproduction

```
POST /spilman/payment
{"channel_id":"deadbeef","balance":5,"signature":"00"}
-> 200 {"status":1,"outcome":"Accepted"}
```

Expected: `409` (refused). Observed: **accepted**.

Note the channel `deadbeef` was **never funded and never opened**, and the
signature `"00"` is 2 hex characters of nonsense.

The same run proves the surface is otherwise behaving — `/spilman/params`, the
unknown-channel 404 on `/spilman/status`, the ledger file, and all three
disabled-rail 404s passed. The failure is isolated to payment acceptance.

## Root cause

`SpilmanRail::record_payment` delegates straight to
`ChannelLedger::record_payment` (`src/spilman/ledger.rs:183`), which does exactly
three things:

1. reads the stored `(balance, payment_count)`;
2. refuses `balance <= previous_balance` (monotonicity), and refuses a
   zero-balance first record;
3. CAS-writes the row.

**It never verifies the signature.** Monotonicity is not authenticity: a *higher*
balance is always accepted, from anyone, for any channel id, with any signature.

The rail's own docstrings assert the opposite intent — `rail.rs` says "durably
record a signed balance update before any bytes are served", and `mod.rs` says
fail-closed on an unknown channel. The `authorize_bytes` path *does* deny unknown
channels (the `/spilman/status` 404 proves that), so the flaw is specifically that
`record_payment` trusts its arguments.

## What upstream gives us (the fix is available, not invented)

`cdk-spilman` ships the verification we are not calling:

- `SpilmanBridge::validate_payment(channel_id, balance, signature, context)`
  — `bridge.rs:1312`. It:
  - rejects an unknown channel (`BridgeError::UnknownChannel`, :1323),
  - loads the funding record, which must exist (:1325),
  - rejects `balance > capacity` (:1332),
  - and crucially calls **`self.verify_signature(...)`** (:1335) over the channel
    params, funding proofs, channel secret and keyset info.
- `verify_payment_covers_amount_due` (:1384) and the JSON / base64-header variants
  (:1395, :1405) wrap it.

So the rail must hold enough channel state to call this: at minimum the funding
params/secret/keyset info per channel, which means **the payee side needs a funded
channel record before it can validate anything** — which is consistent with the
fact that the payee-side host (`BaseSpilmanHost` / `configurable_host`) is the
missing half of this integration.

## Why the unit tests did not catch it

`ledger_test.rs` asserts the CAS and monotonicity behaviour that *does* exist —
reject stale, reject same-balance, accept exact-next. Those tests are correct and
they pass. What no test asserted was **authenticity**, because the ledger has no
notion of it. The gap was in the design, not in the tests' rigour — which is why
the live E2E was the thing that surfaced it.

## Minimal fix

1. Before `ChannelLedger::record_payment`, the rail must call the cdk-spilman
   validator for the channel (`validate_payment` or
   `verify_payment_covers_amount_due`) and **refuse** on any error.
2. That requires a per-channel funding record on the payee. Until the payee-side
   host exists, `record_payment` must **fail closed** — refuse every payment —
   rather than accept unverified balances. Accepting is strictly worse than
   refusing: it hands out credit, and the ledger then treats the forged balance as
   the CAS baseline.
3. Add the missing test: `record_payment` with a garbage signature on a known
   channel must be refused. Then the same for a valid signature to prove the fix
   did not simply deny everything.

## Also noted: the zero-balance opening signature is not recorded

`ledger.rs:213` refuses to record a first record with `balance == 0`. The comment
calls a zero-balance opening signature "legitimate, but it must not be re-recorded
as if it were progress". That may be right for progress accounting, but it means
the *opening* signature — the thing that binds a channel to a session before any
value moves — has nowhere to live in this table. Worth revisiting alongside the
payee host, since a channel that cannot store its opening state cannot be
validated later either.

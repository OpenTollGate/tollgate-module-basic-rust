# ADR-0002: Batched writes for the usage counter

- **Status:** Accepted
- **Date:** 2026-10-18
- **Relates to:** ADR-0001 (which sets `synchronous=FULL` for money state)

## Context

ADR-0001 makes every state transition an fsync. That is correct for sessions and
quotes, because those carry money: a lost session is a customer who paid and got
nothing, and a lost paid quote is an invoice that can never be redeemed.

The **usage counter is different**, and it is worth being precise about why:

| Field | Lost on power failure | Result |
|---|---|---|
| session row | customer cut off, token already spent | money lost, no recovery |
| quote `state=paid` | invoice unredeemable | money lost, no recovery |
| `used` counter | customer keeps the last few seconds of allocation | **bounded give-away, self-correcting** |

And crucially, `used` is **reconstructible from the dataplane**: `ndsctl` knows the
real byte/byte-time counters (`src/metering/`). Persistence is a cache of a value that
has an authoritative source, not the source itself.

So applying the money-grade durability rule to the usage counter buys almost nothing
and costs a fsync per metering tick.

## Decision

**Batch usage-counter writes. Keep `synchronous=FULL` for everything else.**

- `update_usage()` updates the in-memory value on every tick (no I/O).
- The value is flushed to SQLite when **any** of these is true:
  - **5 seconds** have elapsed since the last flush, or
  - the counter has moved by **≥ 5% of `allotment`** since the last flush, or
  - the session is being **revoked, expired, or closed** — always flush on terminal
    transitions, so the final number is exact.
- A flush is a single `UPDATE` on one row.
- `SessionStore::set_usage` is unchanged; the batching lives in `SessionManager`.

### Why those two thresholds

- **5 seconds** bounds the give-away to whatever a customer can transfer in 5s — on a
  metered link that is small, and it also bounds WAL growth.
- **5% of allotment** bounds the *relative* error regardless of link speed: a fast
  link moving a lot gets flushed sooner, so a customer cannot overshoot a meaningful
  fraction of what they paid for.
- Flushing on terminal transitions means the persisted record is **exact** at the
  moment it stops mattering to write more — so audit and dispute resolution see the
  true number, not an approximation.

## Explicitly rejected

- **Flush every tick** (the previous behaviour): one fsync per metering tick, for a
  value that is a cache. Wrong use of the money rule.
- **Never flush** and re-derive from `ndsctl` on restart: `ndsctl` counters reset on
  daemon restart and on MAC re-association, so a restart would hand out a fresh
  allotment to a client that had actually consumed most of it. That is exactly the
  bug ADR-0001 exists to prevent, reintroduced through a different door.
- **A time-based flush only** (no percentage trigger): on a fast link, 5 seconds is a
  large absolute overshoot.

## Consequences

- Worst-case over-grant on power loss: 5 seconds of traffic, or 5% of the allotment,
  whichever is smaller in effect.
- WAL grows at most one row-update per 5 seconds per session. Bounded.
- A crash cannot lose a **terminal** state, because terminal transitions flush.
- Tests must assert the flush triggers, not just that the value round-trips — including
  that a revocation flushes the final value.

## Revisit if

- A deployment reports usage drift that exceeds 5% at session close. That would mean
  a terminal transition is not flushing, which is a bug, not a policy question.
- `ndsctl` gains a durable counter, at which point the persisted `used` becomes
  redundant and could be dropped entirely.

# ADR-0006: Close the gate, and keep it open across restarts

- **Status:** Accepted — implemented (#47) and measured on hardware (#49, #50)
- **§4 decision:** 30s sweep interval (operator, 2026-10-19: "continue" on the proposal)
- **§5 decision:** continuity — re-auth valid sessions at boot (operator, 2026-10-19)
- **Date:** 2026-10-19

## Context

PR #43 made `/ln-invoice` authenticate the client with `ndsctl auth` after payment.
That closed the worst defect in this module: a paying customer received a session
record and stayed blocked, because nothing ever told nodogsplash to let them through.

Opening the gate was half the job. **There is no `ndsctl deauth` call anywhere in
`src/`.** So the gate opens and nothing closes it. A session can expire, be
exhausted, or be revoked, and the client keeps the access it paid for. On an
allotment model that is unbounded free bandwidth after the first payment.

Two sibling problems travel with it, and they share a root cause: **the module treats
nodogsplash's client state as if it were its own.**

1. **Expiry is only swept at startup.** `SessionManager::open` calls
   `store.close_expired(now)` once. There is no periodic sweep, so with the daemon
   running, an expired session stays "active" in memory and in sqlite indefinitely.
2. **Restart loses the gate.** Sessions persist in sqlite; nodogsplash's auth state
   does not. A module restart re-reads sessions but never re-authenticates them, and a
   router reboot wipes nodogsplash's client list entirely.

### Measured on tollgate-A04E, 2026-10-19 (nodogsplash 5.0.2)

The `deauth` contract, captured verbatim rather than assumed:

```
# ndsctl auth 50:76:af:2f:33:ed
Client 50:76:af:2f:33:ed authenticated.
# ndsctl json 50:76:af:2f:33:ed   -> "state":"Authenticated"

# ndsctl deauth 50:76:af:2f:33:ed
Client 50:76:af:2f:33:ed deauthenticated.
# ndsctl json 50:76:af:2f:33:ed   -> "state":"Preauthenticated"   (still in the list)

# ndsctl deauth aa:bb:cc:dd:ee:99
Client aa:bb:cc:dd:ee:99 not found.
```

And the restart evidence: after the box rebooted, `ndsctl clients` printed `0` while
the module's sqlite store still held the session. nodogsplash remembered nothing.

## Decision

### 1. Mirror the open path with a close path (accepted)

Add `ndsctl deauth` on every terminal session transition — expiry, allotment
exhaustion, revocation, and shutdown. Outcomes are classified exactly as `auth` is
(`Opened`/`UnknownClient`/`Refused`/`Unavailable` becomes
`Closed`/`UnknownClient`/`Refused`/`Unavailable`), because "nodogsplash does not know
this MAC" is a **success** for a close operation and must not be logged as a failure.

### 2. Never fail a purchase because the close path failed (accepted)

A failed `deauth` must not block anything. It is logged with the MAC and the tool's
own words, and retried on the next sweep. The asymmetry is deliberate: failing to open
denies a paying customer (bad, visible); failing to close leaks bandwidth (bad, quiet).
The quiet one gets a retry and a counter, not a transaction failure.

### 3. Periodic sweep, not startup-only (accepted in principle)

Replace the startup-only `close_expired` with a sweep that runs on an interval and
does the same work: close expired sessions, deauth their MACs, record final usage to
`closed_usage` per ADR-0002.

**§4 — OPEN: what should a sweep interval be?** Proposal: **30s**, on the grounds that
an allotment overrun of up to 30s of traffic is a bounded, explainable loss, while a
shorter interval costs a `ndsctl` exec per tick for no user-visible benefit. The Go
module runs a valve loop on a comparable cadence. This is the number to argue about.

### 4. Durability: re-auth from the store on boot (§5 — OPEN)

On startup, after `store.close_expired`, every still-valid session is **re-deauthed
first, then re-authed** — deauth to clear any stale state, auth to restore access.
The deauth-then-auth order matters: a reboot leaves nodogsplash with no state, but a
module-only restart leaves it with state that may not match the store.

**§5 — OPEN: is silent re-auth the right behaviour?** Two readings, and this is an
operator decision not a code decision:

- **Continuity (my recommendation).** A customer paid for a window of access. A daemon
  restart is our fault, not theirs, so restore it without asking. Predictable billing,
  no support tickets.
- **Fail-safe.** Re-auth only on the next request from that client (lazy restore). More
  conservative — access is never restored to a device that is no longer present — but
  it means a customer's connection silently drops until they retry.

The recommendation is continuity, because the session was paid for and the store is
authoritative. The counter-argument is worth stating: a MAC-based identity means a
recycled MAC (DHCP reuse, a replaced device) could inherit a paid session. Continuity
sharpens that risk; lazy restore does not.

### 5. Identity caveat stays a documented constraint

`x-forwarded-for` / lease-derived MAC is trusted as identity. That is safe **only**
while nodogsplash fronts the gateway on the managed interface. ADR documents it as a
deployment requirement; it is not fixed here.

## Consequences

- A paid session now ends when it should. Bandwidth after allotment exhaustion stops
  being free.
- One more `ndsctl` exec per terminal transition and per sweep tick — cheap, and the
  exec is already the metering mechanism, so no new dependency.
- The sweep gives `/usage` and `/balance` a consistent notion of "active": a session
  stops being reported active within one sweep of expiry.
- Re-auth on boot makes the module's uptime invisible to customers, at the cost of the
  MAC-recycling risk in §5.

## Alternatives considered

| Option | Why not |
|---|---|
| Rely on nodogsplash's own `Session Timeout` (60d on this box) | Disconnected from the paid allotment. A customer who exhausts 4000 bytes in a minute keeps access for 60 days. |
| Preauth/trusted-client list instead of `auth`/`deauth` | `trust` is IP-based and bypasses accounting; the whole point is per-MAC paid metering. |
| Deauth only on shutdown | Misses the running case entirely, which is where overruns actually happen. |
| Let the Go module own the valve and have Rust delegate | Two implementations, one truth. The Rust clone must stand alone (Path B is router parity). |

## Evidence references

- `conformance/metering-round-trip-measured.json` — the measured open path (PR #44)
- `ci/hardware_roundtrip.py` — the lane that re-measures it (PR #45)
- This ADR's measurements: `ndsctl auth`/`deauth` output on nodogsplash 5.0.2, and the
  empty client list after reboot with sessions intact in sqlite.

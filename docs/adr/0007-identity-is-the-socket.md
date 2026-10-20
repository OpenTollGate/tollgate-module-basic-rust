# ADR-0007: Identity is the socket, not a header a caller can write

- **Status:** Accepted — implemented (#54); hardware verification outstanding
- **§3 decision:** (a) `identity_from_socket` config flag, default `true` (operator, 2026-10-20: "3: a)")
- **Date:** 2026-10-20

## Context

Issue #12 records two security questions. Chasing them found a **divergence from the
Go module that is more serious than either question alone suggested**.

### What the Rust module does today

Every client-scoped handler (`/pay`, `/ln-invoice`, `/balance`, `/usage`, `/whoami`)
resolves the client's identity from headers the CALLER writes:

```rust
// src/http/mac/mod.rs
client_ip_from_headers(x_forwarded_for, x_real_ip, x_tollgate_socket_addr)
    -> first IP that appears
```

- `X-Forwarded-For` — trusted outright, first entry wins
- `X-Real-Ip` — trusted as fallback
- `X-Tollgate-Socket-Addr` — trusted as final fallback; **never set by anything**, so
  today it is only a third way for a caller to name its own identity

The IP is then resolved to a MAC via dnsmasq leases / ARP. The MAC is never taken from
a query parameter or body — that part is right. But the IP a caller asserts is
accepted without any confirmation, and on this deployment (nodogsplash → module, no
reverse proxy in between) **there is no legitimate proxy to strip or rewrite XFF**.

### The concrete attack

A LAN client can mint tokens, then:

```
GET /balance        X-Forwarded-For: <victim's IP>
```

The module resolves the victim's MAC from the lease table, finds the victim's paid
session, and reports the victim's `usage` / `allotment` / `remaining`. Same for
`/usage`, and — the serious one — `/ln-invoice`: a paid quote's GRANT is keyed to the
identity the handler resolved. An attacker polls a quote with a forged header and the
paid allotment lands on THEIR session record (the earlier ndsctl auth tests already
showed identity resolution is what decides who gets granted).

`/whoami` is the honest one: it names the MAC it resolved, so it leaks the mapping but
grants nothing.

### What the Go origin does (measured from its contract test)

The Go module's identity contract is **socket-scoped** and pins the opposite decision
on every point:

1. Identity is `r.RemoteAddr` — the address of the TCP peer. `X-Forwarded-For` is never
   consulted for identity (it only appears in log lines).
2. Every client-scoped response names the identity it answered for
   (`X-TollGate-Client-MAC`), so a caller can compare it with its own socket address
   instead of with the claim it sent.
3. "Ignored" is not silent: when a caller asserts a different MAC in a query/body
   claim, the response says so BY NAME (`X-TollGate-MAC-Claim-Ignored`), because a
   probe once read a 21023/1022 answer, saw no session on the MAC it named, and
   concluded "the gate never opened" — silent ignoring cost real bench time.

The Rust clone diverges on all three points.

## Decision

### §1 — Identity is the TCP peer's address (accepted)

Resolve the client IP from the connection itself (`ConnectInfo<SocketAddr>` in axum),
NOT from `X-Forwarded-For` / `X-Real-Ip` / `X-Tollgate-Socket-Addr`. The MAC still comes
from lease/ARP resolution of that address. This is the Go module's contract and the
correct one for a gateway that is called directly by clients.

`client_ip_from_headers` is retained but demoted: it becomes the documented escape
hatch for deployments where a proxy genuinely fronts the module (see §3), and in that
mode the proxy is responsible for overwriting the header — the module must document
that it trusts whatever arrives.

### §2 — Responses name the identity they answered for (accepted)

Mirror the Go wire contract: every client-scoped response carries
`X-TollGate-Client-MAC` with the MAC actually resolved, and when a caller asserted a
`mac` claim that was ignored, `X-TollGate-MAC-Claim-Ignored: <claimed-mac>` names it.
Silent ignoring is a measured support cost (the Go contract test says so in its own
comment), not a security feature.

### §3 — Trust modes: DECIDED — (a)

The socket-scoped default breaks a deployment where a proxy fronts the module and the
proxy's address would become every client's identity. Two candidate designs:

- **(a) Config flag `identity_from_socket: bool` (default true).** One boolean in
  `config.json`; false restores header trust for proxied deployments. Cost: a mis-set
  flag re-opens the attack. Benefit: trivial, explicit, auditable.
- **(b) `trusted_proxies: [CIDR]` list (empty default).** XFF is consulted only when
  the TCP peer is inside a trusted CIDR, and only the rightmost untrusted entry is
  taken. Cost: more logic, more failure modes. Benefit: precise, matches how nginx and
  friends do it.

My recommendation is **(a)** for this module's actual deployments (nodogsplash calls it
directly; there is no proxy), with (b) deferred until a real proxied deployment exists.
Simple, and the default is the safe one.

## Consequences

- The attack closes: a LAN client can no longer name someone else's MAC by writing a
  header.
- `/balance`, `/usage`, `/ln-invoice` grant/report only for the TCP peer's identity.
- The conformance runner drives via a raw socket and sets `X-Forwarded-For:
  192.168.1.50` — under §1 that header stops deciding identity, so the runner's HTTP-02/
  03 shape verdicts keep passing only if the runner's source address resolves through
  lease/ARP to the expected MAC. **This must be re-measured, not assumed** — the runner
  may need `--identity-header` mode for proxied-test deployments.
- Corpus rows that encode XFF-based identity (if any) surface as divergences and go to
  the corpus repo as questions, not as code changes.

## Alternatives considered

| Option | Why not |
|---|---|
| Keep XFF trust, document it | The attack is one `curl -H` away from any LAN client; a doc is not a control |
| MAC-in-header with HMAC | Requires key management the deployment has no channel for; solves a smaller problem than the socket contract |
| mTLS for all clients | Captive-portal clients cannot do mTLS before authentication |

## Evidence references

- `src/http/mac/mod.rs` — current header-trust resolution
- Go origin `src/client_identity_contract_test.go` — the socket-scoped contract this
  ADR mirrors, including the "silent ignoring cost real bench time" rationale
- Issue #12 — both security questions recorded
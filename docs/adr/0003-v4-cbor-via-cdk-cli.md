# ADR-0003: Decode V4 (CBOR) tokens via `cdk-cli`, not in-process

- **Status:** Accepted
- **Date:** 2026-10-18

## Context

`ci/conformance_runner.py` needs to read V4 (`cashuB`) tokens to run NUT-07
checkstate for the `verify` and `spend` stages. V4 tokens are **CBOR**, and the
runner is stdlib-only Python with no CBOR dependency.

Three options were considered.

## Decision

**Shell out to `cdk-cli` for token decoding and checkstate. Do not implement a CBOR
decoder in the runner.**

## Why

### Correctness: agreement with a certified implementation

`cdk-cli 0.18.1` is the corpus's **reference consumer** — `KNOWN-GOOD.md` defines the
known-good state as tokens "received cleanly by the CDK reference wallet (decode,
verify, swap)". A conformance runner is supposed to measure *agreement*, so the
decoder it uses should be one the corpus already trusts.

A hand-written decoder would be a second opinion implemented by the party being
measured. That is the structural problem: if my decoder and my daemon disagree with
the corpus, my decoder is not an arbiter — and if they agree, I have learned nothing
because both are mine.

### Cost: a CBOR dependency in a conformance tool

Adding `cbor2` (or a vendored decoder) to a runner whose value is that it is trivially
auditable and runs on a bare host is a real cost. Token decoding involves keyset-id
prefix resolution, short-vs-long id handling and proof-shape validation that `cdk-cli`
already gets right and that a fresh implementation would have to re-derive — and get
tested.

### Precedent

The corpus already treats `cdk-cli` as the reference toolchain, and the `spend` stage
in the corpus is defined in terms of `cdk-cli` behaviour. Using it keeps the runner
inside that frame instead of inventing a parallel one.

## Consequences

- The runner gains a **dependency on a binary**, not a library: `cdk-cli` must be on
  `PATH` for the live lane.
- If `cdk-cli` is absent, `verify`/`spend` stay **UNVERIFIED** with the reason stated.
  A missing tool must never become a pass — same rule as an unreachable mint.
- `cdk-cli`'s version becomes part of the measurement's provenance. The verdict set
  records it, so a later disagreement can be attributed to a toolchain change rather
  than argued about.
- `ci/mint_probe.py` keeps its own NUT-06 info parsing and NUT-07 POST (plain JSON,
  no CBOR) and keeps raising `MintError` for anything unreadable. Only the CBOR layer
  is delegated.

## Rejected alternatives

- **Implement CBOR in `mint_probe.py`.** Rejected: a second decoder implemented by the
  measured party, plus a dependency, plus the keyset-prefix edge cases.
- **Leave `verify`/`spend` permanently UNVERIFIED.** Rejected only because ADR-0004
  supplies reachable fixtures; on its own this would have been the honest outcome.

## Revisit if

- `cdk-cli` cannot be shipped to the environments where the runner must run (e.g. a
  constrained CI container), at which point a *vendored, pinned* copy of its decoding
  logic with its own conformance test would be the fallback — still not a fresh
  implementation.
- A future corpus revision certifies a different reference toolchain.

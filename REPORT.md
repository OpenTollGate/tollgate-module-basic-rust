# REPORT — Path A (tollgate-module-basic-rust)

Reference: issue #12. Branch: stacked on `fix/tip-conformance-and-cdk-018` (#11).

Consumers must not be added to `corpus kg_state.green_consumers` on the strength
of this report. See "What is NOT verified" at the end.

## Summary

| Item | Status | Tests |
|---|---|---|
| A1 sign 1022, allotment = steps x step_size | DONE | 70 (was 62) |
| A2 real 21023 with level + code | DONE | 74 |
| A4 absent Content-Type, 1 MiB cap | DONE | 79 |
| A3 MAC resolution, sessions keyed on it | DONE | 86 |
| A7 wire the dead metering module | DONE | 92 |
| A5 mint quote state + NUT-09 restore | DONE (state machine only, see below) | 101 |
| A6 multi-mint selection | DONE (selection only, single wallet remains) | 107 |
| A9 remove the tautological tests | DONE | 108 |
| A8 conformance runner + committed verdict set | DONE | runner exit 0 |

`cargo test`: **108 passed, 0 failed**. `cargo fmt --check`: clean. `cargo build`: clean.

## The two findings that mattered most

### A3 — the reason nothing worked end to end

The payment handler created every session under the literal
`00:00:00:00:00:00`, and `/usage` keyed on `X-Forwarded-For` (an address). The
two could never agree, so `/usage` answered `-1/-1` for every caller and metering
did nothing — no matter how correct the wire format was. A1/A2/A4 alone would have
produced a conformant gateway that did not work.

### A6 — a bug that only exists with 2+ mints

`accepted_mints.first()` was used both to price a payment and to decide whether a
mint is acceptable. A token from the second configured mint was priced with the
first mint's table. With one mint configured the bug is invisible, which is how
Phase 4 came to be called "complete".

## Evidence per item

Every item was test-first: the test was written, observed failing for the right
reason, then the implementation added.

* **A1.** RED: `cannot find function calculate_allotment`. GREEN: 8 tests.
  Mutation-checked both ways — reverting the formula fails 4 tests
  (`left: 5000, right: 110100480`); reverting the signing fails the signature
  test (`sig len 0 != 128`).
* **A2.** RED: `cannot find function build_notice_event`. GREEN: 4 tests.
  `grep '"kind": 21023"' src/` returns nothing — all five bare sites routed
  through one builder.
* **A4.** RED: 2 of 5 failed (`absent CT` → 415; `>1 MiB` accepted). GREEN: 5.
  The other 3 passed before the change and act as regression guards.
* **A3.** GREEN: 7 tests against fixture lease/ARP files.
* **A7.** GREEN: 6 tests against a fake `ndsctl` shell script.
* **A5.** GREEN: 9 tests for quote state and restore classification.
* **A6.** GREEN: 6 tests. RED initially: `cannot find function select_mint_for_token`.
* **A9.** 3 tautologies replaced by 4 tests that drive the router via `oneshot`.
* **A8.** Runner drives the real binary and compares to `corpus/manifest.json`.

## A8 measured output (committed as `conformance/verdicts.json`)

```
PASS 13   FAIL 0   UNVERIFIED 14
```

Reachable and passing: 7/7 fixture decode, the four HTTP-01 contract vectors
(absent Content-Type, unsupported type, >1 MiB body, signed coded notice),
HTTP-02 `/whoami`, HTTP-03 `/usage`.

## What is NOT verified (read this before claiming conformance)

1. **`verify` and `spend` are UNVERIFIED — 14 of 27 checks.** Both require a live
   mint for NUT-07 checkstate. The offline run cannot certify them and does not
   pretend to. A run with `--mint` is required, and even then this runner does not
   yet implement the live check (§ "not implemented" below).
2. **A5 does not obtain a real invoice.** The `request` field is empty and the
   quote is not payable: fetching an invoice needs a CDK wallet bound to a mint,
   which the wallet layer does not do. The state machine and its failure modes are
   real and tested; the mint call is absent. Deliberately not faked.
3. **A6 is selection only.** `state.wallet` still holds a single `TollWallet`.
   Per-mint wallet instances are a CDK wiring change, not a selection change.
4. **Hardware paths.** `/tmp/dhcp.leases`, `/proc/net/arp`, `ndsctl state <mac>`
   behaviour, and the MT6000 dataplane are all Path B. The resolvers and the
   metering parser are tested against fixtures and fake binaries respectively.
5. **Sessions and quotes are in-memory.** A restart loses them. That is a
   pre-existing documented gap, unchanged here, and it is the same class of defect
   TIP-05 (#40) argues must be specified rather than left silent.
6. **`/whoami` returned `mac=` (empty) in the conformance run** because the test
   container has no DHCP lease for `192.168.1.50`. The shape passes; real identity
   resolution needs a router.

## Bugs the runner found in my own work

Two, both recorded because they are the point of A8:

1. **The handler hung on an unaccepted mint.** `verify()` performed a NUT-07
   network call BEFORE the allowlist check, so a token from an unaccepted mint
   cost a full 30 s mint timeout before being refused. Fixed by moving mint
   selection ahead of any network call, in `pay.rs`.
2. **My runner's own absent-Content-Type test was a false FAIL.** `urllib` adds
   `Content-Type: application/x-www-form-urlencoded` whenever `data=` is passed,
   so the server saw a type and answered 415 for a reason the test never intended.
   Rewritten to POST over a raw socket so the header is genuinely absent.

## Not in this Path

A8 does not implement: a 405 for non-POST methods, converting axum's own
body-limit rejection into a notice, or a live-mint verify/spend lane. All three
are named rather than guessed at.

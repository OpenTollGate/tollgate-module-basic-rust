# Path A progress — tollgate-module-basic-rust

One line per item: `A<n> | finding | status | files touched`
A1 | 1022 was unsigned hand-built JSON, allotment was `received*1000` (unrelated to the price table) | DONE | src/http/routes/pay.rs, src/http/routes/pay_session_test.rs, src/nostr_event.rs
A2 | every failure path emitted bare {kind,content} JSON with no level/code/sig | DONE | src/http/routes/pay.rs, src/http/routes/pay_notice_test.rs, src/http/routes/mod.rs
A4 | absent Content-Type answered 415; String extractor unbounded (no 1 MiB cap) | DONE | src/http/routes/pay.rs, src/http/routes/pay_body_test.rs, src/http/routes/mod.rs
A3 | sessions keyed on the 00:00:00:00:00:00 sentinel; /usage keyed on an IP => never matched; metering dead | DONE | src/http/mac/{mod,tests}.rs, src/http/routes/{pay,usage,whoami}.rs, src/http/mod.rs
A7 | metering module never called: `/usage` always reported 0 used | DONE (wired, not deleted) | src/metering/{mod,wiring_test}.rs, src/http/routes/usage.rs
A5 | /ln-invoice was a stub: fake quote ids, hardcoded unpaid, expiry 0 | DONE (state machine; invoice fetch still needs a bound CDK wallet) | src/http/routes/{quote,ln_invoice,ln_invoice_quote_test}.rs, src/http/mod.rs, src/main.rs, src/cli/mod.rs, src/http/routes/discovery_test.rs, Cargo.toml
A6 | accepted_mints.first() both priced payments and gated acceptance: 2nd-mint tokens mispriced | DONE (selection; wallet still single-instance) | src/http/routes/{select,select_test,pay}.rs, src/wallet/verify.rs, src/http/routes/mod.rs
A9 | 3 tests in usage.rs compared literals to literals and never called the handler | DONE | src/http/routes/usage.rs
A8 | no way to measure the binary against the corpus; runner found a 30s hang on unaccepted mints | DONE | ci/conformance_runner.py, conformance/verdicts.json, src/main.rs, src/http/routes/pay.rs
P1 | sessions/quotes were in-memory: restart lost every paid session and every unpaid quote | DONE (SQLite WAL+FULL, fail-closed) | src/session/{store,store_test}.rs, src/session/mod.rs, src/main.rs, src/cli/mod.rs, Cargo.toml
P2 | runner reported verify/spend UNVERIFIED even with a mint; no live lane existed | DONE (mint reachable + NUT-07 detected; per-fixture proof check needs V4 CBOR) | ci/mint_probe.py, ci/test_mint_probe.py, ci/conformance_runner.py, conformance/verdicts.json
REMEASURE | corpus KG twins regenerated from a reachable mint (tollgate#47); verify/spend became measurable | PASS 13->15, UNVERIFIED 14->12 | conformance/verdicts.json, REPORT.md
ADR-0002 | usage counter paid a fsync per metering tick (money rule applied to a cache) | DONE (5s OR 5% OR terminal flush) | src/session/{mod,store,usage_batching_test}.rs
P3 | /ln-invoice invented quote ids and returned an empty request, so no client could pay | DONE (real BOLT11 from the mint) | src/http/routes/{ln_invoice,ln_invoice_live_test}.rs, src/wallet/wallet.rs
P4 | two-mint behaviour was untested; the code already keyed wallets by mint | DONE (regression tests, no code change) | src/wallet/{per_mint_test,mod}.rs
P5 | /ln-invoice shape did not match corpus; PAID quote never minted or granted | PARTIAL: shape fixed, grant hardware-gated | src/http/routes/{ln_invoice,ln_invoice_contract_test}.rs, src/wallet/wallet.rs
P5a | aarch64 musl cross-build was recorded as blocked; it is not - musl.cc toolchain works | DONE (verified on hardware) | ci/cross-build-aarch64.sh
P5b | /balance returned a wallet dump, not the corpus client-session shape; found on hardware | DONE (verified on device) | src/http/routes/{balance,balance_contract_test}.rs
P5c | runner had no way to measure a DEPLOYED gateway; added --url mode and ran it against A04E | DONE (13 PASS on hardware) | ci/conformance_runner.py, conformance/verdicts-hardware-a04e.json
P5d | /balance had the same three-None identity bug; second grant reset usage instead of extending | DONE (grant measured on hardware) | src/http/routes/{balance,balance_contract_test,purchase_path_test}.rs, src/session/mod.rs
P5e | measured grant E2E + additive top-up + /balance session visibility on A04E | DONE (3 PASS measured) | conformance/hardware-grant-e2e-a04e.json
P5f | metering called `ndsctl state`, which does not exist; usage was a permanent silent zero | command+parser fixed, HW round-trip UNVERIFIED | src/metering/mod.rs
P5g | parser expected an INVENTED nested shape; real ndsctl output is flat | fixed against captured bytes, counters still UNVERIFIED | src/metering/mod.rs
P5h | module never called ndsctl auth; /usage still on the dead `state` command; suite flaky 3/10 | DONE unit+HW-reachable, counters UNVERIFIED | src/metering/{mod,wiring_test,}.rs, src/http/routes/ln_invoice.rs, src/config/tests.rs
P5i | metering round trip measured end to end: pay->gate->bytes counted->/usage live | DONE (MEASURED) | conformance/metering-round-trip-measured.json
P5j | whole chain repeatable as one command | DONE — lane PASS on hardware | ci/hardware_roundtrip.py
P6a | ADR-0006 close path: sweep+deauth+continuity; live-counter sync bug found on HW | DONE unit+mutation, loop UNVERIFIED pending re-grant fix | src/session/{mod,gate_close_test}.rs, src/metering/mod.rs, src/main.rs
P6b | GET re-polls re-granted 1000->12000 (measured); pure poll_grant_decision + Issued mark as idempotency token | FIXED, unit+mutation green; HW re-measure next | src/http/routes/ln_invoice.rs
P6c | ADR-0006 loop measured on HW: idempotency PASS (12 polls, flat 1000), sweep close PASS (-1/-1 within 30s), gate closed PASS (307 splash) | DONE — MEASURED | conformance/adr-0006-close-measured.json
P6d | §5 boot continuity measured: SIGKILL -> restart -> gate restored, session intact (2000 allotment, live usage 8) | DONE — MEASURED | conformance/boot-continuity-measured.json
P6e | hardware verdict set re-measured WITH live mint: PASS 13->17, verify/spend kg181 twins now real NUT-07 | DONE — MEASURED | conformance/verdicts-hardware-a04e.json
P6f | security: XFF trust = LAN identity forgery (curl away); Go origin pins socket-scoped contract, Rust diverges all 3 points | ADR-0007 proposed, §3 open | docs/adr/0007-identity-is-the-socket.md
P7a | identity from TCP peer per ADR-0007 §3(a); XFF trust closed | DONE unit+mutation; HW unverified | src/http/mac/mod.rs src/http/*.rs src/config/schema.rs
P7b | ADR-0007 Accepted, §3=(a) implemented #54 | HW verification outstanding | docs/adr/0007-identity-is-the-socket.md
P7c | ADR-0007 §2 wire headers (named identity + ignored claim) via middleware | DONE unit+mutation; HW unverified | src/http/mod.rs src/http/wire_identity_test.rs
P7d | ADR-0007 forgery MEASURED BLOCKED on HW, incl. live paid session; §2 headers on the wire | DONE — MEASURED | conformance/adr-0007-identity-measured.json

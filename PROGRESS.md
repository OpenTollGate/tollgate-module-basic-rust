# Spilman channel rail — progress

## Phase 1 — dependency unify (DONE)

**Finding.** The module took `cdk`/`cdk-sqlite`/`cashu` from crates.io (resolving
`cashu 0.18.2`), while `cdk-spilman` takes `cashu` from **git tag v0.18.1**. Cargo treats
git and registry sources as distinct crates, so the two `Proof` types were nominally
different. `[patch.crates-io]` cannot fix it: `cdk-common 0.18.2` pins `cashu = "=0.18.2"`
exactly. The proven fix (gap analysis §1) is to move the whole stack to the one git source.

**Status.** DONE. `cargo check --all-targets` clean (pre-existing warnings only),
`cargo test --all-targets` **194 passed / 0 failed** — unchanged count, zero behavioural
change. `Cargo.lock` now resolves a single `cashu 0.18.1` from
`git+https://github.com/cashubtc/cdk.git?tag=v0.18.1#a056e0f0`, and package count went
332 → 382 (+50, matching the gap analysis estimate for cdk-spilman's
yaml_serde/zstd/brotli/bls12_381/bitcoin pulls).

**Files touched.** `Cargo.toml` (cdk, cdk-sqlite, cashu → git v0.18.1;
`cdk-spilman` added at rev `c2083def…` features `["wallet","configurable-host-reqwest"]`),
`Cargo.lock`.

## Phase 2 — rail skeleton + exact billing + CAS ledger (DONE)

**RED observed.** `cargo test --test test_spilman_billing` failed with
`could not find spilman in tollgate_module_basic_rust` (4 × E0433/E0432) before any
implementation existed. Implemented to GREEN: **14/14** integration tests, and the lib
unit suite went 194 → 213 (billing property tests ported from MONAD, 2 ledger concurrency
tests, 4 pricing tests). Total 194 → 227 across targets, zero failures.

**Finding (a real arithmetic error in the gap analysis, corrected here).** §4 says
`in_bytes_per_millisat = 22_020` "reproduces TollGate's price line exactly". It does not:
`22_020_096 / 1_000 = 22_020.096`, so at rate 22_020 the exact ceil is
`floor(22_020_096/22_020) + 1 = 1001` msat per 21 MiB step. At sat granularity that is
**2 sat per step instead of 1** — a 100% overcharge at the step-granularity the gap
doc itself recommends. The smallest integer rate that hits exactly 1000 msat per step is
**22_021**. Implementation: `DEFAULT_IN_BYTES_PER_MILLISAT = 22_020` is kept as the
instructed config default (documented as a known +1 msat/step drift, 45 ppm), and
`TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT = 22_021` is added as the exact-parity rate.
Both behaviours are pinned by tests so neither can drift silently.

**Files touched.** `src/spilman/mod.rs` (PaymentRail trait, NoopRail, SpilmanError,
RailDecision), `src/spilman/billing.rs` (MONAD port verbatim, 234 L incl. property tests),
`src/spilman/payment_units.rs` (msat↔sat, round-up), `src/spilman/ledger.rs`
(rusqlite CAS ledger, WAL + synchronous=FULL), `src/spilman/pricing.rs`
(SessionPricing, StepPricing, PricingModel), `src/lib.rs` (mod), `Cargo.toml`
(async-trait), `tests/test_spilman_billing.rs` (new).

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

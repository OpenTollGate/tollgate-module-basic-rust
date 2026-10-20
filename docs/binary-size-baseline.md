# Binary Size Baseline — tollgate-module-basic-rust

**Phase 0 smoke test** — 2026-07-19

## Headline Result

| Target | Size (stripped) | Linking | Notes |
|--------|----------------|---------|-------|
| `x86_64-unknown-linux-musl` | **1.5 MB** (1,575,032 B) | static-pie | Self-contained, no runtime deps |
| `x86_64-unknown-linux-gnu` | **1.4 MB** (1,461,456 B) | dynamic | Links against system glibc |

**vs Go (tollgate-module-basic-go):** 17 MB (17,597,841 B, unstripped) → **11x smaller**. Even accounting for Go's debug symbols (~30% when stripped to ~12 MB), Rust is still 8x smaller.

## Important Caveats

1. **Underestimate.** The smoke test exercises CDK Amount/Proof serde + tokio, but axum and reqwest are stripped by LTO (TypeId references alone don't prevent dead-code elimination). When Phase 1 adds real HTTP routes and Phase 3 wires CDK's HttpClient, expect the binary to grow to approximately 3–5 MB. Still 3–4x smaller than Go.

2. **Optimization flags active:**
   ```toml
   [profile.release]
   panic = "abort"     # no unwinding tables
   strip = true        # strip debug symbols
   opt-level = "z"     # optimize for size
   lto = true          # link-time optimization across crates
   codegen-units = 1   # maximum optimization opportunity
   ```

3. **ARM targets not yet measured.** `aarch64-unknown-linux-musl` and `armv7-unknown-linux-musleabihf` targets are installed but the cross-linker (zig cc) produces massive warning spam. Builds likely succeed but need testing. Will measure in Phase 5 (OpenWrt packaging).

## Build Times

| Build | Time | Notes |
|-------|------|-------|
| Cold compile (full CDK tree) | ~5 min 49s | 333 crates, first build |
| Incremental native | 11.5s | deps cached, recompile + LTO |
| Incremental musl | 11.7s | deps cached, recompile + LTO |

## Dependency Tree

- **333 crates** in Cargo.lock
- **660 entries** in `cargo tree` (includes version duplicates)
- Key heavy crates: `ring` (crypto), `rustls` (TLS), `tokio` (async runtime), `rusqlite`/`libsqlite3-sys` (SQLite), `reqwest` (HTTP client), `axum` (HTTP server), `cdk`/`cdk-common`/`cashu` (Cashu protocol)

## CDK Version

Pinned: `cdk = "0.17"` → resolved to **v0.17.3** (latest stable as of 2026-07-19)
Persistence: `cdk-sqlite = "0.17"` → resolved to **v0.17.3**

## Comparison: Go vs Rust Binary Footprint

| Metric | Go (gonuts) | Rust (CDK) | Ratio |
|--------|-------------|------------|-------|
| Binary size (stripped, est.) | ~12 MB | ~1.5 MB (smoke) / ~3–5 MB (projected full) | 3–8x smaller |
| Static linking | No (dynamic) | Yes (musl static-pie) | Rust is self-contained |
| Runtime deps | glibc, libc | None (musl) | Simpler deployment |
| Cold compile | ~30s | ~6 min | Slower (333 crates) |
| Incremental | ~5s | ~12s | Comparable |

## Further Optimization Options (if needed)

If the binary grows beyond 5 MB in later phases:

1. **`opt-level = "z"`** already active — could try `"s"` if `"z"` breaks something
2. **`panic = "abort"`** already active — saves ~100-200KB
3. **Strip** already active
4. **LTO = true** already active
5. **UPX compression** — can compress the binary 50-70% further (1.5MB → ~500KB). The Go CI already uses UPX variants (`upx-*` compression tags in NIP-94 events). Apply the same to Rust.
6. **Feature trimming** — audit CDK features, disable `mint` (we only need `wallet`)
7. **`cargo-bloat`** — identify which crates contribute most to binary size

## Conclusion

The Rust+CDK binary is dramatically smaller than Go+gonuts. Even with the most pessimistic projection (5 MB after full integration), it's still less than half the Go binary's size, and it's statically linked — ideal for OpenWrt deployment on resource-constrained routers.

# tollgate-module-basic-rust

> **Status: WIP — Phase 7 (test parity on physical hardware) in progress.**
> Phases 0–6 are complete (58 unit tests passing). The Go original remains
> the production binary until this Rust clone passes full test-parity on
> real OpenWrt hardware.

Rust rewrite of `tollgate-module-basic-go` — a drop-in replacement that
uses [CDK](https://github.com/cashubtc/cdk) (Cashu Dev Kit) instead of
the Go Cashu library for the wallet layer.

---

## Table of Contents

- [Why](#why)
- [Goal](#goal)
- [Phase Status](#phase-status)
- [Tech Stack](#tech-stack)
- [Build](#build)
- [Configuration](#configuration)
- [HTTP Endpoints](#http-endpoints)
- [CLI (Unix Socket)](#cli-unix-socket)
- [Testing](#testing)
- [Migration from Go](#migration-from-go)
- [Architecture](#architecture)
- [Binary Size](#binary-size)
- [License](#license)

---

## Why

The Go Cashu library (v0.7.1–v0.7.3) had an unrecoverable **swap-counter
race**: a transient mint `/swap` failure could leave the wallet's internal
keyset counter advanced past the highest stored proof derivation index.
This produced error `10002 "blinded message already signed"` on every
subsequent operation — permanently bricking the wallet. The only recovery
was a manual database edit.

This is not a cosmetic rewrite. CDK is the maintained Rust implementation
of the Cashu protocol. Its **saga pattern** makes wallet operations atomic:
either the full receive/send/melt completes and persists, or no state
changes at all. The swap-counter race cannot occur.

See [`docs/brick-detection.md`](docs/brick-detection.md) for the full
technical analysis of the race condition.

## Goal

A single Rust binary that is a **drop-in replacement** for
`tollgate-module-basic-go`:

- **Same CLI** over Unix socket: `version`, `status`, `wallet info`,
  `wallet balance`, `migrate <path>`.
- **Same HTTP surface** — same routes, same response shapes.
- **Same config files** at `/etc/tollgate/` — `config.json`,
  `identities.json`, `install.json` load without modification.
- **Same Nostr event shapes** (kinds 10021, 1022, 21000, 21023).
- **Same persistence model** — SQLite (via `cdk-sqlite`) replaces bbolt.
- **Cross-compiled for OpenWrt musl targets** — statically linked, no
  runtime dependencies.

## Phase Status

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | Scaffolding + binary size smoke test | ✅ Complete |
| 1 | Contract surface (HTTP routes, config, identity, Nostr events) | ✅ Complete |
| 2 | Token verifier (NUT-07 checkstate) | ✅ Complete |
| 3 | CDK wallet integration (receive, send, melt, quotes, balance) | ✅ Complete |
| 4 | Session management + metering + payment wiring | ✅ Complete |
| 5 | OpenWrt packaging (Makefile, procd init, ARM cross-build) | ✅ Complete |
| 6 | Wallet migration (gonuts-export → CDK receive) | ✅ Complete |
| 7 | **Test parity on physical hardware** | **🔄 In progress** |

**58 unit tests pass** (`cargo test`). What remains is validation on real
OpenWrt routers — verifying end-to-end payment flows, ndsctl integration,
and migration of production wallets under live network conditions.

## Tech Stack

- **Rust** edition 2021, MSRV 1.85 (musl cross-build toolchains lag).
- **`tokio 1`** — async runtime (multi-thread, macros, signal, net, fs, process).
- **`axum 0.8`** — HTTP server with WebSocket support.
- **`tower 0.5` + `tower-http 0.6`** — middleware (CORS, timeout, tracing).
- **`cdk 0.17`** — Cashu Dev Kit (wallet feature, no mint feature).
- **`cdk-sqlite 0.17`** — CDK persistence backend (SQLite).
- **`cashu 0.17`** — Cashu protocol primitives (Token parsing, NUT types).
- **`reqwest 0.12`** — HTTP client (rustls-tls, no OpenSSL for musl compat).
- **`secp256k1 0.29`** — BIP-340 Schnorr signing for Nostr events.
- **`serde 1` / `serde_json 1`** — JSON serialization for config, CLI, events.
- **`tracing 0.1` + `tracing-subscriber 0.3`** — structured logging with env-filter.
- **`thiserror 1`** — error derive macro.
- **`sha2 0.10` + `hex 0.4`** — event ID hashing.
- **`rand 0.8`** — key/seed generation.

**No OpenSSL dependency.** TLS is handled by rustls for static musl linking.

## Build

### Native build

```bash
cargo build --release
```

Binary: `target/release/tollgate-module-basic-rust`

### Cross-compile (musl static)

```bash
# Install targets (one-time)
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf

# Build
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
cargo build --release --target armv7-unknown-linux-musleabihf
```

The musl targets produce **statically linked, position-independent
executables** — self-contained with zero runtime dependencies, ideal for
OpenWrt deployment.

### Release profile

Aggressive size optimization is configured in `Cargo.toml`:

```toml
[profile.release]
panic = "abort"      # no unwinding tables
strip = true         # strip debug symbols
opt-level = "z"      # optimize for size
lto = true           # link-time optimization across all crates
codegen-units = 1    # maximum optimization opportunity
```

See [`docs/binary-size-baseline.md`](docs/binary-size-baseline.md) for the
size comparison against the Go binary and further optimization options.

## Configuration

All config files live in `/etc/tollgate/`. The directory can be overridden
via the `TOLLGATE_TEST_CONFIG_DIR` environment variable (used by tests).

### `config.json`

```jsonc
{
  "config_version": "v0.0.8",
  "log_level": "info",
  "accepted_mints": [
    {
      "url": "https://mint.example.com",
      "min_balance": 64,
      "balance_tolerance_percent": 10,
      "payout_interval_seconds": 60,
      "min_payout_amount": 128,
      "price_per_step": 1,
      "price_unit": "sats",
      "purchase_min_steps": 0
    }
  ],
  "profit_share": [
    { "factor": 0.79, "identity": "operator" },
    { "factor": 0.21, "identity": "shareholder_a" }
  ],
  "step_size": 22020096,
  "margin": 0.1,
  "metric": "bytes",
  "show_setup": true,
  "reseller_mode": false,
  "redirect_url": null,
  "auth_delay_seconds": null,
  "upstream_detector": {
    "probe_timeout": "10s",
    "probe_retry_count": 3,
    "probe_retry_delay": "2s",
    "require_valid_signature": true,
    "ignore_interfaces": ["lo", "docker0", "br-lan", "hostap0"],
    "only_interfaces": [],
    "discovery_timeout": "5m0s"
  },
  "upstream_session_manager": {
    "max_price_per_millisecond": 0.002777777778,
    "max_price_per_byte": 0.00003725782414,
    "trust": {
      "default_policy": "trust_all",
      "allowlist": [],
      "blocklist": []
    },
    "sessions": {
      "preferred_session_increments_milliseconds": 60000,
      "preferred_session_increments_bytes": 131100000,
      "millisecond_renewal_offset": 10000,
      "bytes_renewal_offset": 131100000
    },
    "usage_tracking": {
      "data_monitoring_interval": "0.5s"
    }
  },
  "upstream_wifi": {
    "scan_interval_seconds": 300,
    "fast_check_seconds": 30,
    "lost_threshold": 2,
    "hysteresis_db": 12,
    "signal_floor": -85,
    "blacklist_ttl_minutes": 60,
    "emergency_penalty": 20,
    "max_consecutive_failures": 3,
    "switch_cooldown_minutes": 10,
    "startup_grace_seconds": 90,
    "post_switch_wait_seconds": 5,
    "dhcp_timeout_seconds": 180,
    "manual_pause_seconds": 120
  }
}
```

**Validation:** `profit_share` factors must sum to 1.0 (±1e-6 tolerance).
If they don't, a warning is emitted and the residual fraction remains in the
wallet each payout cycle.

### `identities.json`

Stores owned and public Nostr identities. On first boot, the binary
auto-generates a merchant secp256k1 keypair if none exists and saves it
here (file mode `0600`).

### `install.json`

Installation metadata — package path, release channel, install timestamp,
installed version. Parsed for compatibility; not actively modified by the
Rust binary.

### Persistence files

| File | Purpose |
|------|---------|
| `wallet_seed.bin` | 64-byte wallet seed (mode `0600`). Auto-generated on first boot. |
| `wallet.sqlite` | CDK wallet state (proofs, keysets, quotes, saga state). One per mint URL, sanitized filename. |
| `wallet.db` | Legacy gonuts bbolt wallet. **Preserved as backup** after migration. |
| `wallet.db.pre-migration` | Renamed original wallet.db after successful migration. |
| `.migration_complete` | Marker file — prevents re-running auto-migration. |
| `tokens.jsonl` | Exported Cashu tokens from gonuts (one per line). |

## HTTP Endpoints

The HTTP server listens on `127.0.0.1:2121`. All routes set
`Access-Control-Allow-Origin: *`.

| Method | Path | Status | Description |
|--------|------|--------|-------------|
| `GET` | `/` | ✅ Implemented | **Discovery** — Returns Nostr kind 10021 event with metric, step_size, price, mint URL, and purchase minimums. |
| `POST` | `/` | ✅ Implemented | **Payment** — Accepts `text/plain` (Cashu token) or `application/json` (Nostr kind 21000). Verifies token via NUT-07, receives into wallet, creates session, returns kind 1022 on success or kind 21023 + HTTP 402 on failure. |
| `GET` | `/whoami` | ⚠️ Partial | Returns `mac=<MAC>` as plain text. **Currently returns placeholder MAC `00:00:00:00:00:00`** — not yet wired to ARP/remote_addr lookup. |
| `GET` | `/usage` | ✅ Implemented | Returns `used/total` plain text for the requesting client's session. Returns `-1/-1` if no active session. Client identified via `X-Forwarded-For` or `X-Real-IP`. |
| `GET` | `/balance` | ✅ Implemented | Returns JSON `{"balance": <total>, "mintBalances": [{"url": "...", "balance": N}]}`. |
| `POST` | `/ln-invoice` | ⚠️ **Stub** | Returns hardcoded `stub-quote-*` / `stub-invoice` / `stub-pubkey`. **Not wired to CDK mint quotes.** |
| `GET` | `/ln-invoice?quote=<id>` | ⚠️ **Stub** | Returns `state: "unpaid"`, `checkState: "UNPAID"`, `expiry: 0`. **Not wired to CDK quote status.** |

### Nostr event shapes

| Kind | Direction | Purpose |
|------|-----------|---------|
| 10021 | Merchant → Client | Discovery event (GET `/`) with pricing and metric tags. |
| 1022 | Merchant → Client | Session granted (POST `/` success) with allotment tags. |
| 21000 | Client → Merchant | Payment event (POST `/` body) wrapping a Cashu token. |
| 21023 | Merchant → Client | Payment rejected (POST `/` failure, HTTP 402) with error content. |

> **Note:** The kind 1022 response in the payment handler currently uses
> placeholder `id` and empty `sig` fields. Full Nostr signing of session
> events is a Phase 7 task.

## CLI (Unix Socket)

The CLI server listens on `/var/run/tollgate.sock` (mode `0660`).
Communicates via line-delimited text: one command per line, JSON response.

Can be overridden with `TOLLGATE_TEST_CONFIG_DIR` (for testing).

| Command | Status | Response |
|---------|--------|----------|
| `version` | ✅ | Multi-line text: `version`, `commit`, `build_time`, `rust_version`, `openwrt: target=<arch>`. |
| `status` | ✅ | JSON `{"success": true, "message": "running"}`. |
| `wallet info` | ✅ | JSON `{"success": true, "message": "<JSON array of mint URLs + balances>"}`. |
| `wallet balance` | ✅ | JSON `{"success": true, "message": "<total_sats>"}`. |
| `migrate <path>` | ✅ | JSON `{"success": true, "message": "<migration report JSON>"}`. See [Migration](#migration-from-go). |
| Unknown | ✅ | JSON `{"success": false, "error": "unknown command: <cmd>"}`. |

**Usage example:**

```bash
echo "version" | socat - UNIX-CONNECT:/var/run/tollgate.sock
echo "wallet balance" | socat - UNIX-CONNECT:/var/run/tollgate.sock
echo "migrate /etc/tollgate/tokens.jsonl" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

## Testing

```bash
# Run all 58 unit tests
cargo test

# Run with output visible
cargo test -- --nocapture

# Run specific module
cargo test wallet
cargo test config
cargo test session
cargo test metering
cargo test cli
```

**Test coverage by module:**

| Module | Tests | What's covered |
|--------|-------|----------------|
| `config` | 7 | Round-trip Go config/identities/install JSON, defaults, missing/empty files. |
| `cli` | 7 | Version string, status, wallet balance, wallet info, unknown command, migrate (nonexistent/empty/invalid tokens). |
| `session` | 9 | Create, get, is_active, expiry, usage exhaustion, revoke, cleanup, overwrite. |
| `metering` | 6 | ndsctl output parsing (download+upload sum, missing fields, empty, garbage, non-numeric, whitespace). |
| `wallet::wallet` | 8 | Open/close cycle, mint acceptance, seed roundtrip, balance, per-mint balance, db path sanitization, receive errors, concurrency, timeout protection. |
| `wallet::verify` | 6 | Token parsing, Y-value extraction, mint filtering, invalid tokens, milli-unit scaling. |
| `http::routes::pay` | 6 | Nostr event token extraction (valid/wrong kind/missing tag/invalid JSON/multiple tags), session creation, 402 status. |
| `http::routes::usage` | 3 | No session, active session, expired session. |

### What is NOT tested

- **End-to-end payment flow** against a live mint (requires network).
- **ndsctl integration** on real OpenWrt (the parse function is tested; the
  `poll_usage` subprocess call is not exercised in CI).
- **Physical hardware deployment** — this is Phase 7.
- **Bricked wallet detection** — documented in
  [`docs/brick-detection.md`](docs/brick-detection.md) but **not implemented
  in code** (the migration sidesteps it by importing tokens into a fresh
  CDK wallet).

## Migration from Go

See [`MIGRATION.md`](MIGRATION.md) for the full operator runbook covering:

- Pre-migration checklist
- The swap-counter race (why migration is needed)
- Automated first-boot migration
- Manual migration via CLI
- Bricked wallet detection and recovery
- Troubleshooting

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the full module
diagram, data flow, and design rationale covering:

- Module structure (config, http, wallet, session, metering, identity,
  nostr_event, cli)
- HTTP routing (axum)
- Wallet module (CDK integration with saga pattern)
- Session management (in-memory)
- Nostr event signing
- Migration flow (gonuts-export → CDK receive)
- Persistence model
- Binary size optimization strategy

## Binary Size

The Rust binary is dramatically smaller than the Go original:

| Target | Size (stripped) | Linking |
|--------|----------------|---------|
| `x86_64-unknown-linux-musl` | ~1.5 MB | static-pie |
| `x86_64-unknown-linux-gnu` | ~1.4 MB | dynamic (glibc) |
| Go original (stripped est.) | ~12 MB | dynamic |

> The 1.5 MB figure is from the Phase 0 smoke test. With full HTTP routing
> and CDK wallet integration, the binary is expected to settle around
> 3–5 MB — still 3–4× smaller than Go.

See [`docs/binary-size-baseline.md`](docs/binary-size-baseline.md) for
detailed analysis.

## License

MIT — see [`LICENSE-MIT`](LICENSE-MIT).

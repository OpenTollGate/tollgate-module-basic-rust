# Architecture

> Module diagram, data flow, and design rationale for
> `tollgate-module-basic-rust`.

---

## Module Structure

```
tollgate-module-basic-rust
├── src/
│   ├── main.rs              — entry point: parse args, start server or CLI
│   ├── lib.rs               — module declarations
│   ├── config/
│   │   ├── mod.rs           — config loading (TOLLGATE_TEST_CONFIG_DIR support)
│   │   ├── schema.rs        — Go-compatible Config, MintConfig, Identities structs
│   │   └── tests.rs         — 7 tests: round-trip, defaults, missing/empty files
│   ├── http/
│   │   ├── mod.rs           — axum router setup, CORS, timeouts, route registration
│   │   └── routes/
│   │       ├── mod.rs       — route definitions
│   │       ├── discovery.rs — GET /  (Nostr kind 10021)
│   │       ├── pay.rs       — POST / (payment, Nostr kind 1022/21023)
│   │       ├── whoami.rs    — GET /whoami (mac= plain text)
│   │       ├── usage.rs     — GET /usage (used/total plain text)
│   │       ├── balance.rs   — GET /balance (JSON)
│   │       └── ln_invoice.rs— POST/GET /ln-invoice (stub)
│   ├── wallet/
│   │   ├── mod.rs           — WalletHandle type alias
│   │   ├── wallet.rs        — CDK wallet: open, receive, balance, seed mgmt (570 LOC)
│   │   └── verify.rs        — Token verification: parse, Y-value, NUT-07 checkstate
│   ├── session/
│   │   ├── mod.rs           — CustomerSession + SessionManager (in-memory)
│   │   └── tests.rs         — 9 tests: create, get, expiry, revoke, cleanup
│   ├── metering/
│   │   ├── mod.rs           — ndsctl output parser (download+upload bytes)
│   │   └── tests.rs         — 6 tests: parsing edge cases
│   ├── cli/
│   │   └── mod.rs           — Unix socket CLI: version, status, wallet, migrate
│   ├── identity.rs          — secp256k1 keypair load-or-generate (0600 perms)
│   ├── nostr_event.rs       — Nostr event creation + signing (BIP-340)
│   └── tracing_setup.rs     — tracing-subscriber init with env-filter
├── tools/
│   └── gonuts-export/       — Go tool: bbolt → Cashu tokens export
│       └── main.go          — reads wallet.db, emits tokens.jsonl
├── openwrt/
│   ├── Makefile             — OpenWrt package build definition
│   └── tollgate.init        — procd init script
└── .cargo/
    └── config.toml           — musl cross-compile linker config (zig cc)
```

## Data Flow: Payment Lifecycle

```
                    ┌──────────────────┐
                    │  Client (phone)  │
                    │  Captive portal  │
                    └────────┬─────────┘
                             │ POST / (Cashu token or Nostr kind 21000)
                             ▼
┌──────────────────────────────────────────────────┐
│  axum HTTP server (127.0.0.1:2121)               │
│                                                  │
│  1. Content-Type negotiation                     │
│     ├─ text/plain → raw token                    │
│     └─ application/json → parse Nostr event      │
│                                                  │
│  2. Token verification (wallet::verify)          │
│     ├─ Parse Cashu token (cashu crate)           │
│     ├─ Extract Y-values from proofs              │
│     ├─ Filter: mint in accepted_mints?           │
│     └─ NUT-07 checkstate (if mint reachable)     │
│                                                  │
│  3. Wallet receive (wallet::wallet)              │
│     ├─ CDK Wallet::receive(token)                │
│     │   └─ Contacts mint for swap                │
│     ├─ Saga pattern: atomic or nothing           │
│     └─ Returns received amount                   │
│                                                  │
│  4. Session creation (session::SessionManager)   │
│     ├─ allotment = amount × step_size            │
│     ├─ ClientSession { ip, allotment, expiry }   │
│     └─ Stored in-memory HashMap                  │
│                                                  │
│  5. Nostr response                               │
│     ├─ Success: kind 1022 (session granted)      │
│     └─ Failure: kind 21023 + HTTP 402            │
└──────────────────────┬───────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────┐
│  ndsctl (metering)                               │
│                                                  │
│  poll_usage(client_ip)                           │
│  ├─ ndsctl status → parse download/upload bytes  │
│  ├─ Compare against session allotment             │
│  └─ If exhausted → revoke access                 │
└──────────────────────────────────────────────────┘
```

## Wallet Module (CDK Integration)

The wallet is the core difference from the Go original. It replaces
gonuts (bbolt + non-atomic swap) with CDK (SQLite + saga pattern).

### Key components

```
┌─────────────────────────────────────────┐
│ WalletHandle = Arc<Mutex<Wallet>>        │
│                                         │
│ Wallet {                                │
│   cdk_wallet: cdk::wallet::Wallet       │
│   └─ backed by cdk-sqlite               │
│   └─ /etc/tollgate/wallet_<mint>.sqlite │
│                                         │
│   seed: [u8; 64]                        │
│   └─ /etc/tollgate/wallet_seed.bin      │
│   └─ mode 0600                          │
│ }                                       │
└─────────────────────────────────────────┘
```

### CDK saga pattern (why the race is fixed)

```
gonuts (BROKEN):               CDK (FIXED):
┌──────────────┐               ┌──────────────────────┐
│ 1. swap()    │               │ 1. Begin saga        │
│ 2. Increment │               │ 2. Derive secrets    │
│    counter   │               │ 3. POST /swap        │
│ 3. SaveProofs│               │ 4. Persist proofs    │
│              │               │    + advance counter │
│ Race: crash  │               │    (single txn)      │
│ between 2,3  │               │ 5. Commit saga       │
│ = bricked    │               │                      │
└──────────────┘               │ OR                   │
                               │ 5. Rollback saga     │
                               │    (nothing persisted)│
                               └──────────────────────┘
```

### Operations

| Operation | CDK method | Network? | Notes |
|-----------|-----------|----------|-------|
| Receive token | `Wallet::receive(token)` | Yes (mint swap) | Atomic via saga. Timeout-protected. |
| Check balance | `Wallet::get_balance()` | No (local DB) | Sum of all unspent proofs. |
| Per-mint balance | `Wallet::get_balance(mint_url)` | No | Filtered by mint URL. |
| Send/melt | Future work | Yes | Not needed for v1 (we're a merchant, not spending). |
| Mint quotes | Future work | Yes | `/ln-invoice` currently stubbed. |

## Session Management

Sessions are **in-memory only** — they do not survive process restart.
This matches Go behavior (Go writes to `sessions.json` but the data is
ephemeral client state).

```
SessionManager {
    sessions: Arc<Mutex<HashMap<String, CustomerSession>>>
}

CustomerSession {
    client_ip: String,
    allotment_bytes: u64,    // from payment amount × step_size
    used_bytes: u64,         // updated by metering polls
    created_at: Instant,
    expires_at: Instant,     // allotment-based or time-based
}
```

- `get_or_create(ip)` — lookup or create
- `is_active(ip)` — not expired, usage < allotment
- `cleanup_expired()` — removes stale entries
- Thread-safe via `Arc<Mutex<>>`

## Metering (ndsctl Integration)

```
poll_usage(client_ip)
│
├── Execute: ndsctl status <client_ip>
│
├── Parse output:
│   "Download session data: 1234567"
│   "Upload session data: 890123"
│   → used_bytes = download + upload
│
├── Compare: used_bytes vs session.allotment_bytes
│
└── If exhausted:
    └── ndsctl deauth <client_ip>
```

The parser handles:
- Normal output (download + upload sum)
- Missing fields
- Empty output
- Non-numeric values
- Garbage/whitespace

## Nostr Event Signing

Identity is loaded from `/etc/tollgate/identities.json` on startup.
If no merchant keypair exists, one is auto-generated using `secp256k1`
and saved with mode `0600`.

```
NostrEvent {
    id: <SHA-256 of canonical form>,
    pubkey: <merchant X-only pubkey>,
    created_at: <unix timestamp>,
    kind: <10021 | 1022 | 21023>,
    tags: [[...], ...],
    content: <string>,
    sig: <BIP-340 Schnorr signature>,
}
```

Event ID is computed as SHA-256 over the canonical JSON array:
`[0, pubkey, created_at, kind, tags, content]`.

Signature is `secp256k1::schnorr::sign()` over the ID hash.

## CLI (Unix Socket)

```
┌──────────────────┐     ┌──────────────────────┐
│ socat / nc       │────►│ /var/run/tollgate.sock│
│ (mode 0660)      │◄────│                      │
└──────────────────┘     └──────────┬───────────┘
                                    │
                         ┌──────────▼───────────┐
                         │ Line-delimited text   │
                         │ Read line             │
                         │ Parse command         │
                         │ Dispatch:             │
                         │  version → format     │
                         │  status → "running"   │
                         │  wallet info → JSON   │
                         │  wallet balance → int │
                         │  migrate → import     │
                         │ Write response + \n   │
                         └──────────────────────┘
```

## Migration Flow

```
Old (Go)                          New (Rust)
┌─────────────────┐               ┌──────────────────────┐
│ wallet.db       │               │ wallet.sqlite        │
│ (bbolt)         │               │ (cdk-sqlite)         │
│                 │               │                      │
│ Bucket: proofs  │──gonuts-────►│ CDK Wallet::receive()│
│ Bucket: keysets │  export       │  per token           │
│ Bucket: seed    │               │                      │
└─────────────────┘               └──────────────────────┘
        │                                  │
        ▼                                  ▼
  Preserved as                     Fresh counter
  wallet.db.pre-migration           management (saga)
```

1. `gonuts-export` reads bbolt buckets, groups proofs by keyset,
   emits Cashu tokens (NUT-00 format) to `tokens.jsonl`.
2. Rust `migrate` command reads `tokens.jsonl`, calls
   `Wallet::receive(token)` for each line.
3. CDK contacts mint for swap — requires network.
4. Failed tokens are logged; operator can retry.

## Persistence Model

```
/etc/tollgate/
├── config.json              — Go-compatible JSON config (read-only)
├── identities.json          — Nostr keypairs (read/write, 0600)
├── install.json             — Package metadata (read-only)
├── wallet_seed.bin          — 64-byte CDK seed (auto-gen, 0600)
├── wallet_<mint>.sqlite     — CDK wallet state (one per accepted mint)
│   ├── proofs table
│   ├── keysets table
│   ├── mint_quotes table
│   ├── melt_quotes table
│   ├── keyset_counter table
│   └── wallet_sagas table
├── wallet.db                — Legacy bbolt (preserved, never modified)
├── tokens.jsonl             — Migration export (ephemeral)
└── .migration_complete      — Marker file
```

### Why SQLite over redb

- **Inspectability**: `sqlite3 wallet.sqlite` on any router via SSH.
  Field operators can query balances, proofs, and saga state directly.
- **Portability**: `.dump` produces text backups. redb files are not
  portable across versions.
- **Universality**: Every Linux distro ships `sqlite3`. redb requires
  a Rust binary to inspect.
- **Migrations**: SQL migrations are diff-able and reviewable.

## Binary Size Optimization

The release profile is tuned for minimal size on resource-constrained
OpenWrt routers:

```toml
[profile.release]
panic = "abort"      # Removes unwinding tables (~100-200 KB)
strip = true          # Removes debug symbols
opt-level = "z"       # Optimize for binary size (not speed)
lto = true            # Link-time optimization across all crates
codegen-units = 1     # Single codegen unit = maximum optimization
```

### Cross-compilation

```
┌─────────────────────────────────────────────────┐
│ Build host (x86_64 Linux)                       │
│                                                 │
│  cargo build --release --target <target>        │
│                                                 │
│  Targets:                                       │
│  ├─ x86_64-unknown-linux-musl    (x86 routers)  │
│  ├─ aarch64-unknown-linux-musl   (ARM64 routers)│
│  └─ armv7-unknown-linux-musleabihf (ARM32)      │
│                                                 │
│  Linker: musl-gcc (x86), zig cc (ARM)           │
│  Result: static PIE, zero runtime deps          │
└─────────────────────────────────────────────────┘
```

The `.cargo/config.toml` configures zig cc wrappers for ARM musl
targets, producing fully static binaries compatible with OpenWrt's
musl libc.

## Thread Model

```
tokio runtime (multi-thread)
├── HTTP server task (axum)
│   ├── Accept loop on 127.0.0.1:2121
│   └── Per-connection tasks (hyper)
├── CLI server task
│   └── Accept loop on Unix socket
├── Signal handler task
│   └── Listens for SIGTERM/SIGINT → graceful shutdown
└── Background tasks
    └── Session cleanup (periodic sweep)
```

The `WalletHandle` (`Arc<Mutex<Wallet>>`) serializes all wallet
operations. This is acceptable because wallet ops are infrequent
(one per payment) and the mutex prevents concurrent CDK state
mutations.

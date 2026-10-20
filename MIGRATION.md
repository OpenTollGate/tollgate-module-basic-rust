# Migration Guide: Go → Rust

**For operators upgrading from `tollgate-module-basic-go` to
`tollgate-module-basic-rust`.**

This guide covers everything you need to safely migrate a production
tollgate router from the Go binary (using the Go Cashu library + bbolt) to
the Rust binary (using CDK + SQLite).

---

## Table of Contents

- [Pre-Migration Checklist](#pre-migration-checklist)
- [Why Migration Is Needed](#why-migration-is-needed)
- [Automated Migration (First Boot)](#automated-migration-first-boot)
- [Manual Migration](#manual-migration)
- [Bricked Wallet Detection](#bricked-wallet-detection)
- [What to Expect](#what-to-expect)
- [Troubleshooting](#troubleshooting)

---

## Pre-Migration Checklist

Before swapping the binary, verify the following:

### 1. Back up existing state

```bash
# Back up the entire tollgate config directory
cp -a /etc/tollgate /etc/tollgate.backup.$(date +%Y%m%d)
```

This preserves `wallet.db`, `config.json`, `identities.json`, and
`install.json`.

### 2. Note the current wallet balance

```bash
# Using the Go binary (still running):
echo "wallet balance" | socat - UNIX-CONNECT:/var/run/tollgate.sock
echo "wallet info" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

Record the balance. After migration, verify the Rust binary reports the
same amount.

### 3. Ensure mint is reachable

The migration requires **live mint connectivity** — CDK's `receive()` must
contact the mint to complete the token swap. Verify the mint URL is
accessible:

```bash
curl -s https://<your-mint-url>/v1/keys | head -c 200
```

If the mint is down, the migration will fail. Wait until it's back up.

### 4. Stop the Go binary

```bash
/etc/init.d/tollgate stop
# or
kill $(pidof tollgate-module-basic-go)
```

### 5. Ensure the `gonuts-export` tool is available

The automated migration calls `gonuts-export` (default path:
`/usr/bin/gonuts-export`). This tool reads the bbolt `wallet.db` and
exports all proofs as Cashu token strings to `tokens.jsonl`.

Override the path with:
```bash
export GONUTS_EXPORT_PATH=/path/to/gonuts-export
```

> If `gonuts-export` is not installed, the automated migration will skip
> token export and start with an empty wallet. You can still run the
> migration manually (see below).

---

## Why Migration Is Needed

The Go Cashu library (v0.7.1–v0.7.3) has a non-atomic swap operation.
The keyset counter — which tracks the next derivation index for
deterministic secret generation — can be persisted **before** the
resulting proofs are saved to disk.

### The race condition

```
Normal flow:
  1. Derive secrets at indices [counter, counter+1, ...]
  2. Increment counter in memory
  3. POST /swap to mint → get blinded signatures
  4. Persist advanced counter to bbolt
  5. Save new proofs to bbolt

Race: crash or network failure between steps 4 and 5:
  - Counter = N (persisted)
  - Highest proof index = M, where M < N-1

Result: next operation derives secrets at indices [N, N+1, ...]
but the mint has already signed those blinded messages.
Error: 10002 "blinded message already signed"
The wallet is permanently bricked.
```

CDK (Cashu Dev Kit) uses a **saga pattern** that makes all wallet
operations atomic. Either the full operation completes and persists, or
no state changes at all. The swap-counter race cannot occur.

For the full technical analysis, see
[`docs/brick-detection.md`](docs/brick-detection.md).

---

## Automated Migration (First Boot)

When the Rust binary starts for the first time and detects:

1. **`wallet.db` exists** (legacy gonuts bbolt wallet), AND
2. **`wallet.sqlite` does NOT exist** (no CDK wallet yet), AND
3. **`.migration_complete` marker is absent**

...it automatically attempts migration:

```
                    ┌──────────────┐
                    │  Start Rust  │
                    │  binary      │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │ wallet.db    │    wallet.sqlite   .migration_complete
                    │  exists?     │      exists?           exists?
                    └──┬───┬───┬───┘
                       │   │   │
              No ─────┘   │   └───── Yes ──→ Skip migration (already done)
                           │
                     Yes ──┘  (wallet.db exists, wallet.sqlite does not)
                           │
                    ┌──────▼───────┐
                    │ Run          │
                    │ gonuts-export│
                    │ wallet.db →  │
                    │ tokens.jsonl │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │ Success?     │
                    ├───┬──────────┤
                    │   │          │
               Yes ─┘   │    No ───┘──→ Log warning, start with empty wallet
                    │                           (manual migration available)
           ┌────────▼─────────┐
           │ tokens.jsonl     │
           │ written.         │
           │ Operator runs    │
           │ 'migrate' CLI    │
           │ to import.       │
           └──────────────────┘
```

### What happens automatically

1. `gonuts-export /etc/tollgate/wallet.db /etc/tollgate/tokens.jsonl` is
   executed.
2. If successful, `tokens.jsonl` is written containing one Cashu token
   string per line (one per keyset batch).
3. A log message instructs the operator to run the `migrate` CLI command
   to import the tokens.
4. If `gonuts-export` fails or is not found, a warning is logged and the
   wallet starts empty. Manual migration is still possible.

### What does NOT happen automatically

- Tokens are **not** auto-imported into the CDK wallet on first boot.
  Importing requires mint connectivity (CDK `receive()` contacts the mint),
  and the binary defers this to the `migrate` CLI command so the operator
  can control timing and verify the export first.
- The old `wallet.db` is **not** renamed or deleted — it remains as a
  backup until the operator confirms migration success.

---

## Manual Migration

If the automated export succeeded but tokens haven't been imported yet,
or if you're migrating from a pre-exported `tokens.jsonl`:

### Step 1: Ensure the binary is running

```bash
/etc/init.d/tollgate start
# Verify it's up:
echo "status" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

### Step 2: Run the migrate command

```bash
echo "migrate /etc/tollgate/tokens.jsonl" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

### Step 3: Check the migration report

The response is JSON:

```json
{
  "success": true,
  "message": "{\"total\":5,\"imported\":5,\"failed\":0,\"errors\":[]}"
}
```

| Field | Meaning |
|-------|---------|
| `total` | Number of token lines in the file. |
| `imported` | Number successfully received into the CDK wallet. |
| `failed` | Number that failed (network error, already spent, invalid). |
| `errors` | Up to 10 error messages for failed tokens. |

### Step 4: Verify the balance

```bash
echo "wallet balance" | socat - UNIX-CONNECT:/var/run/tollgate.sock
echo "wallet info" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

Compare against the pre-migration balance from the checklist.

### Step 5: Mark migration complete

```bash
touch /etc/tollgate/.migration_complete
```

This prevents the auto-migration logic from re-running on next boot.

### Step 6: Back up the old wallet (optional but recommended)

```bash
mv /etc/tollgate/wallet.db /etc/tollgate/wallet.db.pre-migration
```

---

## Bricked Wallet Detection

A gonuts wallet is **bricked** when its keyset counter has been advanced
past the highest stored proof derivation index — a gap that means the mint
has already signed those blinded messages.

### Detection heuristic

For each keyset in the bbolt wallet:

1. Extract `Counter` from the keyset entry.
2. For each proof, derive its index using the NUT-13 HD path and compare
   against the stored secret.
3. Find `maxProofIndex` — the highest derivation index among stored proofs.
4. Compare:

| Condition | Status | Meaning |
|-----------|--------|---------|
| `counter == maxProofIndex + 1` | **HEALTHY** | Counter correctly points to next unused index. |
| `counter <= maxProofIndex` | **HEALTHY** | Counter wasn't advanced yet (safe). |
| `counter > maxProofIndex + 1` | **BRICKED** | Tail gap — indices between `maxProofIndex+1` and `counter-1` were sent to mint but proofs never saved. |
| No proofs for keyset | **SUSPECT** | Empty keyset — may indicate data loss. |
| Proofs exist but counter is 0 | **WARNING** | Counter was never persisted. |

### Detection implementation status

> **⚠️ Important:** Bricked wallet detection is **documented** (see
> [`docs/brick-detection.md`](docs/brick-detection.md)) but **not
> implemented as a standalone tool** in this codebase. The migration
> strategy sidesteps the issue entirely:
>
> 1. Export all existing proofs as Cashu tokens (proofs themselves are
>    valid even in a bricked wallet).
> 2. Import into a fresh CDK wallet (which has its own counter management).
> 3. CDK's saga pattern prevents the race from recurring.
>
> If you suspect your wallet is bricked, proceed with migration — the
> export/import path works regardless of bricking state.

---

## What to Expect

### Balance transfer

- **All existing proofs** are exported as Cashu tokens and re-imported
  into the CDK wallet.
- The total balance should be **identical** before and after migration.
- CDK may reorganize proofs into different keyset denominations during
  `receive()`, but the total amount is preserved.

### Old wallet.db preserved

- The original `wallet.db` is **never deleted or modified** by the Rust
  binary.
- After successful migration, rename it as a backup:
  `wallet.db.pre-migration`.
- You can revert to the Go binary at any time by restoring the original
  `wallet.db` and removing `wallet.sqlite`.

### Session state

- **Sessions are in-memory only** — they do not survive process restart.
  This matches Go behavior.
- During migration (stop Go → start Rust), all active sessions are lost.
  Clients will need to re-authenticate.

### Config compatibility

- `config.json`, `identities.json`, and `install.json` load without
  modification — the Rust binary uses identical JSON schemas.
- If `identities.json` lacks a merchant keypair, the Rust binary
  auto-generates one. This changes the merchant pubkey (used in Nostr
  events). To preserve the old identity, ensure `identities.json` contains
  the merchant private key from the Go installation.

### What changes

- **Persistence**: bbolt (`wallet.db`) → SQLite (`wallet.sqlite`).
- **Identity file**: If no merchant key exists, a new one is generated.
  The old key can be manually copied into `identities.json`.
- **Socket path**: Unchanged (`/var/run/tollgate.sock`).
- **HTTP port**: Unchanged (`127.0.0.1:2121`).

---

## Troubleshooting

### `gonuts-export` not found

```
WARNING: gonuts-export not found or failed, starting with empty wallet.
```

**Fix:** Install the `gonuts-export` tool or set `GONUTS_EXPORT_PATH`:

```bash
export GONUTS_EXPORT_PATH=/path/to/gonuts-export
```

If unavailable, you'll need to manually export proofs from the bbolt wallet
using any bbolt reader and format them as Cashu tokens in `tokens.jsonl`.

### Mint unreachable during migration

```
ERROR: migration failed: token 0: CDK error: connection refused
```

**Fix:** The migration requires live mint connectivity (CDK `receive()`
contacts the mint). Wait until the mint is back up, then re-run:

```bash
echo "migrate /etc/tollgate/tokens.jsonl" | socat - UNIX-CONNECT:/var/run/tollgate.sock
```

Each token is imported independently — previously imported tokens won't be
double-counted (CDK tracks received proofs).

### Partial migration (some tokens failed)

If `failed > 0` in the migration report:

1. Check the `errors` array for specifics.
2. Common causes:
   - **"already spent"**: Token was already received in a previous attempt.
     Safe to ignore.
   - **"connection timeout"**: Mint was briefly unreachable. Retry.
   - **"invalid token"**: Corrupted export. Re-export from `wallet.db`.
3. Re-run the migrate command — CDK will skip already-imported tokens.

### Balance mismatch after migration

If the Rust binary reports a different balance than the Go binary:

1. Check for failed tokens in the migration report.
2. Verify all tokens were in `tokens.jsonl`.
3. Check CDK logs for receive errors:
   ```bash
   logread | grep -i "migration\|receive\|cdk"
   ```
4. If proofs are missing, re-export from the original `wallet.db` (still
   preserved) and re-run migration.

### Bricked wallet recovery

If the Go wallet was already bricked before migration:

1. The export still works — bricked wallets can send existing proofs.
2. Import succeeds — the proofs are valid.
3. The new CDK wallet starts fresh with correct counter management.
4. **No manual counter fix-up is needed.** CDK's HD wallet derives its own
   indices independent of the gonuts counter.

### Binary fails to start

```bash
# Check logs
logread | grep tollgate

# Common issues:
# - Permission denied on /etc/tollgate/wallet.sqlite → check ownership
# - Address already in use → old process still running
# - Config parse error → validate config.json with: jq . /etc/tollgate/config.json
```

### Reverting to the Go binary

If migration fails and you need to revert:

```bash
# Stop Rust binary
/etc/init.d/tollgate stop

# Remove CDK wallet and migration marker
rm /etc/tollgate/wallet.sqlite
rm /etc/tollgate/.migration_complete

# Restore original wallet.db if renamed
mv /etc/tollgate/wallet.db.pre-migration /etc/tollgate/wallet.db

# Start Go binary
/etc/init.d/tollgate-go start
```

The Go wallet is unaffected — all its files remain untouched.

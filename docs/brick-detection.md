# Bricked-Wallet Detection Heuristic

## Background

The gonuts wallet (used by `tollgate-module-basic-go` v0.7.1–v0.7.3) stores
proofs and keyset counters in a bbolt database at `<wallet_path>/wallet.db`.
A non-atomic swap operation can advance the keyset counter past the highest
proof derivation index, leaving "holes" in the deterministic secret sequence.
When the wallet later tries to mint or swap using the counter's current
position, the mint has already signed that blinded message, returning:

> `"blinded message already signed"`

This bricks the wallet — it can still send existing proofs, but any
operation that derives new secrets (mint, receive-and-swap) fails.

## bbolt Schema (gonuts v0.7.4)

The bbolt DB has these top-level buckets:

| Bucket             | Key              | Value                          |
|--------------------|------------------|--------------------------------|
| `keysets`          | mint URL (nested) | `WalletKeyset` JSON per keyset ID |
| `proofs`           | proof secret (hex string) | `Proof` JSON              |
| `pending_proofs`   | Y (compressed pubkey bytes) | `DBProof` JSON          |
| `mint_quotes`      | quote ID         | `MintQuote` JSON               |
| `melt_quotes`      | quote ID         | `MeltQuote` JSON               |
| `seed`             | `"seed"` / `"mnemonic"` | raw seed bytes / mnemonic string |

### Keysets bucket — nested structure

```
keysets/
  <mint_url>/
    <keyset_id> → JSON(WalletKeyset{Id, MintURL, Unit, Active, PublicKeys, Counter, InputFeePpk})
```

The `Counter` field tracks the next derivation index for deterministic
secret generation via NUT-13 HD paths.

### Proofs bucket

```
proofs/
  <secret_hex> → JSON(Proof{Amount, Id, Secret, C, Witness, DLEQ})
```

Key is the proof's `Secret` field (a hex string). Value is the full proof.

## NUT-13 HD Wallet Derivation Path

gonuts uses BIP-32 HD derivation for deterministic secret/blinding-factor
generation:

```
m/129372'/0'/<keyset_k_int>'/<counter>'/0   → secret (hex-encoded private key bytes)
m/129372'/0'/<keyset_k_int>'/<counter>'/1   → blinding factor r (private key)
```

Where `keyset_k_int = bigEndianUint64(keysetId_bytes) % (2^31 - 1)`.

The counter starts at 0 and increments for each proof minted in that keyset.
The `WalletKeyset.Counter` field stores the next unused counter value.

## The Swap-Counter Race

### Normal flow

1. `createBlindedMessages(splitAmounts, keysetId, &counter)` derives
   secrets and blinding factors for indices `counter, counter+1, ...`.
2. Counter is incremented in-place during derivation.
3. After successful mint/swap, `IncrementKeysetCounter(keysetId, numNewProofs)`
   persists the advanced counter to bbolt.
4. New proofs are saved to the `proofs` bucket.

### Race condition

If the process crashes or the network fails **after** the counter is
incremented (step 3) but **before** proofs are saved (step 4), or if
the counter is incremented speculatively for a swap that partially fails:

- Counter = N (persisted)
- Highest proof index = M, where M < N-1

The next operation derives secrets at indices N, N+1, ... but the mint
has already signed the blinded messages for some of those indices in the
failed swap. Result: "blinded message already signed" error.

## Detection Heuristic

To detect a bricked wallet, walk the bbolt DB and check for tail gaps:

### Step 1: Load keysets

For each mint URL in the `keysets` bucket, iterate nested keyset entries.
Extract `Id` and `Counter` for each `WalletKeyset`.

### Step 2: Load proofs per keyset

For each keyset ID, iterate the `proofs` bucket and collect proofs where
`proof.Id == keysetId`.

### Step 3: Derive secrets for each proof

For each proof, we know the secret string. To determine its derivation
index, derive secrets at indices 0, 1, 2, ... using the NUT-13 path
and compare:

```
for counter := 0; ; counter++ {
    derivedSecret = DeriveSecret(keysetDerivationPath, counter)
    if derivedSecret == proof.Secret {
        proofIndex = counter
        break
    }
}
```

This is O(N) per proof where N is the proof's index. For wallets with
many proofs, batch-derive a range of secrets and build a reverse map
(`secret → counter`) to avoid re-deriving.

### Step 4: Check for tail gaps

After mapping all proofs to their derivation indices:

```
maxProofIndex = max(all proof indices)
keysetCounter  = WalletKeyset.Counter
```

**Bricked if:** `keysetCounter > maxProofIndex + 1`

The "+1" accounts for the counter pointing to the *next* unused index.
A gap of more than 1 means indices between `maxProofIndex+1` and
`keysetCounter-1` were derived, sent to the mint, signed, but the
resulting proofs were never saved — the mint will reject re-use of
those indices.

**Healthy if:** `keysetCounter == maxProofIndex + 1` (or `keysetCounter <= maxProofIndex`,
which means the counter wasn't advanced yet — also safe).

### Step 5: Report per-keyset health

For the migration report, emit per-keyset status:

```json
{
  "keyset_id": "00abcdef123456",
  "mint_url": "https://mint.example",
  "counter": 42,
  "max_proof_index": 38,
  "proof_count": 35,
  "status": "bricked",
  "gap": 3,
  "total_amount": 21050
}
```

Possible statuses:
- `"healthy"` — counter == max_proof_index + 1 (or counter <= max_proof_index + 1)
- `"bricked"` — counter > max_proof_index + 1 (tail gap detected)
- `"empty"` — no proofs found for this keyset
- `"warning"` — proofs exist but counter is 0 (counter was never persisted)

## Migration Impact

A bricked wallet can still export its existing proofs as Cashu tokens.
The proofs themselves are valid — only future derivation is affected.
The migration strategy (Option D) sidesteps the issue entirely:

1. **Export**: Read all proofs from bbolt, serialize to Cashu V3 tokens
2. **Import**: CDK `wallet.receive(token)` imports the proofs into a fresh
   CDK-sqlite wallet with its own counter management
3. **Counter advance**: After import, advance the CDK keyset counter past
   the gonuts max index to avoid any overlap

CDK's saga pattern makes operations atomic, so the swap-counter race
cannot occur in the new wallet.
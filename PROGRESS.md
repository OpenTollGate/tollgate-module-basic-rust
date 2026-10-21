

## Phase 3 — payer-side channel lifecycle + crash recovery (GREEN)

**Finding (the seam is usage-context, not the payment balance).** The payee's
`get_amount_due` is computed from its own **accumulated usage variables**, fed by the
`context` JSON passed to `process_payment` — *not* from the balance the payer signed. The
cooperative close then hard-checks `balance == host.get_amount_due(channel_id, None)` and
rejects with `ValidationFailed { expected: 0, got: N }` when the two disagree. So the byte
counter must be driven into the payee on every payment, and the payee's `pricing_scale`
must be the byte price of one step. In this rail that is `pricing_scale = STEP`
(22,020,096), i.e. one 21 MiB step costs exactly 1 sat. This is the concrete form of the
"compute amount_due once from cumulative bytes" rule in §4, and it is now asserted in the
integration test (`accepted.amount_due == balance`).

**Second finding (a bug this phase caught).** The protocol's `sender_pubkey_hex` argument
is a **public** key; passing the secret's raw scalar fails deep inside the bridge with
`No key found for pubkey: ...`. The test now has separate `secret_hex()` / `pubkey_hex()`
helpers so the two can never be confused again.

**Third finding (no accessors).** cdk-spilman implements `SpilmanClientHost` on the
concrete `ConfigurableClientHost<S>` only — no blanket `Arc<H>` impl — and
`SpilmanClientBridge` owns its host with no accessor and consumes its networking by value.
Rather than build a ~20-method forwarding wrapper for no benefit, the rail:
- passes the `SpilmanClientNetworking` **into** `submit_funding_swap` (a mint client is
  stateless config, so a second instance is equivalent), and
- proves durability by opening a **separate read-only SQLite connection**
  (`ConfigurableClientHost::open_sqlite_read_only`) to the same file. That is a stronger
  proof than reading back through the writing handle, and it is why
  `payer::open_read_only_host` exists.
- `step_payment` deliberately takes **no** networking argument, so the hot path is
  structurally incapable of a mint round trip.

**Tests.** `tests/spilman_payer_lifecycle.rs`, all `#[ignore]`-gated (they need a mint),
run with `cargo test --test spilman_payer_lifecycle -- --ignored` against the in-process
`cdk-spilman-test-mint` harness. **3/3 pass**:
1. `payer_opens_signs_and_settles_a_channel` — two-phase open, 10 signed updates at
   exactly 1 sat/step, zero mint calls between open and settle, payee settles for exactly
   the signed balance after a restart, and the quantised retail model bills the same line.
2. `killed_process_between_persist_and_swap_recovers_the_channel` — crash before the
   swap: state survives as `OpeningFromSwap`, the re-prepared instance refuses to swap,
   and a failed restore never promotes the channel to `Open`.
3. `killed_after_swap_but_before_mark_open_recovers_the_channel` — crash *after* the
   swap, before `mark_open`: NUT-09 restore from the store alone recovers the funding
   proofs, and the recovered channel is immediately payable with no second mint spend.

Plus 3 unit tests in `payer.rs` pinning the fund-safety gate, clock sanity, and the
expiry margin (§6 hazard 5).

**Counts.** `cargo test --all-targets`: 216 lib + 14 billing, 0 failed. The 3 integration
tests are `ignored` by default, so the default run is unchanged for CI.

**Files touched.** `src/spilman/payer.rs` (new: `ChannelOpening` two-phase +
`step_payment` + `recover_open_channel` + `open_read_only_host` + `ensure_persisted`),
`src/spilman/mod.rs` (`pub mod payer`), `tests/spilman_payer_lifecycle.rs` (new),
`Cargo.toml` (dev-dependency `cdk-spilman-test-mint`).

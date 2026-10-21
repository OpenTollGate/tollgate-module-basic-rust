//! Phase 3 — payer-side channel lifecycle against a real mint.
//!
//! `#[ignore]`-gated: this drives the full two-phase open, one signed balance update per
//! 21 MiB step, a cooperative settle, and a crash-recovery replay. It runs against an
//! in-process `cdk-spilman-test-mint` (a FakeWallet-backed real `cdk::Mint`), so it needs
//! no external service:
//!
//! ```text
//! cargo test --test spilman_payer_lifecycle -- --ignored --nocapture
//! ```
//!
//! Set `TOLLGATE_SPILMAN_MINT_URL` to run against a standalone `cdk-spilman-test-mint`
//! daemon instead; unset, the in-memory harness is used.
//!
//! # What it proves
//!
//! 1. **Two-phase ordering.** The `OpeningFromSwap` record — carrying the original input,
//!    i.e. the NUT-09 restore material — is durable *before* the funding swap leaves the
//!    process. Durability is read back through a **separate read-only SQLite
//!    connection**, not through the writing handle, so the assertion cannot be satisfied
//!    by an in-process cache.
//! 2. **One signature per 21 MiB step**, and the payee's durable ledger ends at exactly
//!    the balance the payer signed.
//! 3. **Zero mint calls on the hot path.** `step_payment` takes no networking argument at
//!    all: it is structurally incapable of a mint round trip.
//! 4. **Crash recovery.** A fresh host over the same file, channel left in
//!    `OpeningFromSwap`, is restored by `recover_open_channel_from_swap`.
//! 5. **Settlement.** The payee receives exactly the balance it holds a signature for.

use cashu::nuts::{CurrencyUnit, SecretKey};
use cdk_spilman::configurable_host::{ConfigurableHost, StorageConfig, UnitPricingConfig};
use cdk_spilman::{
    build_cashu_b_token, ClientChannelState, ConfigurableClientHost, SpilmanBridge,
    SpilmanClientBridge, SpilmanClientHost, SqliteClientStorage,
};
use cdk_spilman_test_mint::{InMemoryMintNetworking, TestMintHelper};
use tollgate_module_basic_rust::spilman::payer::{
    self, ChannelOpening, DEFAULT_CHANNEL_LIFETIME_SECS,
};
use tollgate_module_basic_rust::spilman::{
    ChannelLedger, SessionPricing, StepPricing, TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
    TOLLGATE_STEP_SIZE_BYTES,
};

/// One 21 MiB step.
const STEP: u64 = TOLLGATE_STEP_SIZE_BYTES;

/// The URL the in-memory harness advertises for its mint.
const MINT_URL: &str = "https://test-mint";

/// How many 21 MiB steps the happy path signs.
const STEPS: u64 = 10;

/// Hex of a secret key's raw scalar — used to seed a payer host.
fn secret_hex(secret: &SecretKey) -> String {
    secret
        .secret_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Hex of a key's *public* half — this is what the protocol takes as `sender_pubkey_hex`.
fn pubkey_hex(secret: &SecretKey) -> String {
    secret.public_key().to_hex()
}

/// Payee host config, in the shape the live TollGate router advertises.
///
/// The payee prices *bytes*: `amount_due = ceil(accumulated_bytes / pricing_scale)`.
/// Setting the scale to one 21 MiB step makes one step cost exactly 1 sat, which is the
/// §4 billing rule. The scale is the only place this number lives on the payee side; the
/// payer derives the same figure from `in_bytes_per_millisat`.
fn payee_config(path: &std::path::Path) -> cdk_spilman::configurable_host::ConfigurableHostConfig {
    cdk_spilman::configurable_host::ConfigurableHostConfig {
        mints: [(MINT_URL.to_string(), vec!["sat".to_string()])].into(),
        min_expiry_seconds: 60,
        pricing_scale: STEP,
        storage: StorageConfig::Sqlite {
            path: path.to_str().expect("utf8 path").to_string(),
        },
        pricing: [(
            "sat".to_string(),
            UnitPricingConfig {
                min_capacity: 1,
                max_amount_per_output: None,
                variables: [("bytes".to_string(), 1u64)].into(),
            },
        )]
        .into(),
    }
}

fn payee_host(
    path: &std::path::Path,
    receiver: &SecretKey,
    keyset_info_json: &str,
) -> ConfigurableHost {
    let host =
        ConfigurableHost::new(payee_config(path), &secret_hex(receiver)).expect("payee host");
    host.set_keyset(
        MINT_URL,
        cdk_spilman::parse_keyset_info_from_json(keyset_info_json)
            .expect("parse keyset info")
            .keyset_id,
        cdk_spilman::configurable_host::KeysetCacheEntry {
            info_json: keyset_info_json.to_string(),
            active: true,
            unit: CurrencyUnit::Sat,
        },
    )
    .expect("payee keyset");
    host
}

fn payer_bridge(
    db: &std::path::Path,
    sender: &SecretKey,
    mint: std::sync::Arc<cdk::Mint>,
) -> SpilmanClientBridge<ConfigurableClientHost<SqliteClientStorage>, InMemoryMintNetworking> {
    let mut host =
        ConfigurableClientHost::open_sqlite(db.to_str().expect("utf8 path")).expect("payer store");
    host.add_key(sender.clone());
    SpilmanClientBridge::new(host, InMemoryMintNetworking::new(mint))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn payer_opens_signs_and_settles_a_channel() {
    let helper = TestMintHelper::new().await.expect("test mint");
    let mint = helper.mint();
    let keyset_info_json = helper.keyset_info_json().expect("keyset info");

    let dir = tempfile::tempdir().expect("tempdir");
    let payee_db = dir.path().join("payee.sqlite");
    let payer_db = dir.path().join("payer.sqlite");

    // ── payee side: the upstream router's real SpilmanHost ──────────────────
    let receiver_secret = SecretKey::generate();
    let payee_bridge =
        SpilmanBridge::new(payee_host(&payee_db, &receiver_secret, &keyset_info_json));

    // ── payer side ───────────────────────────────────────────────────────────
    let sender_secret = SecretKey::generate();
    let client_bridge = payer_bridge(&payer_db, &sender_secret, std::sync::Arc::clone(&mint));

    let proofs = helper.mint_proofs(100).await.expect("mint proofs");
    let token = build_cashu_b_token(
        MINT_URL,
        "sat",
        &serde_json::to_string(&proofs).expect("proofs json"),
    )
    .expect("token");

    let expiry = payer::now_seconds() + DEFAULT_CHANNEL_LIFETIME_SECS;

    // ── phases 1 & 2: prepare (pure), then persist BEFORE the swap ──────────
    let mut opening = ChannelOpening::prepare(
        &client_bridge,
        &token,
        &receiver_secret.public_key().to_hex(),
        &pubkey_hex(&sender_secret),
        expiry,
        &keyset_info_json,
        64,
    )
    .expect("prepare open");
    let channel_id = opening.channel_id().to_string();
    assert!(!opening.is_persisted(), "prepare must not touch storage");
    assert!(
        opening.capacity() > STEPS,
        "the funding token must buy more than {STEPS} sat: got {}",
        opening.capacity()
    );

    opening
        .save_opening(&client_bridge)
        .expect("persist opening");
    assert!(opening.is_persisted());

    // Durability, proven from a *separate read-only connection* to the same file.
    {
        let inspector =
            ConfigurableClientHost::open_sqlite_read_only(payer_db.to_str().expect("utf8 path"))
                .expect("read-only inspector");
        let persisted = inspector
            .get_channel_opening_from_swap(&channel_id)
            .expect("read opening")
            .expect("opening must be durable before the swap");
        assert!(
            !persisted.input_token.is_empty(),
            "the original funding input is the NUT-09 restore material and must be persisted"
        );
        assert!(
            !persisted.channel_secret_hex.is_empty(),
            "the ECDH channel secret must be persisted"
        );
        assert_eq!(
            inspector.get_channel_state(&channel_id),
            Some(ClientChannelState::OpeningFromSwap),
            "the record exists, so the state is exactly this"
        );
        assert!(
            inspector.get_channel_funding(&channel_id).is_none(),
            "no funding yet: the swap has not been submitted"
        );
    }

    // ── phase 3: the point of no return ─────────────────────────────────────
    // A mint client is stateless configuration, so a second one is equivalent to the
    // bridge's own; the swap is the only place the payer touches the mint.
    let funding_networking = InMemoryMintNetworking::new(std::sync::Arc::clone(&mint));
    opening
        .submit_funding_swap(&funding_networking)
        .expect("funding swap");

    // ── phases 4-5: unblind + verify + mark open ────────────────────────────
    let opened = opening.finish_open(&client_bridge).expect("finish open");
    assert_eq!(opened.channel_id, channel_id);

    // Register with the payee: the registration carries params + funding proofs.
    let registration = client_bridge
        .sign_channel_registration(&channel_id)
        .expect("sign registration");
    assert_eq!(registration.balance, 0);
    assert!(
        registration.has_funding(),
        "the registration must carry the funding proofs so the payee can verify them"
    );
    payee_bridge
        .process_payment(
            &registration.channel_id,
            registration.balance,
            &registration.signature,
            registration.params.as_ref(),
            registration.funding_proofs.as_deref(),
            &String::new(),
        )
        .expect("payee registers channel");

    // ── the payee's durable ledger ──────────────────────────────────────────
    let ledger = ChannelLedger::open(&dir.path().join("spilman.db")).expect("ledger");
    assert_eq!(
        ledger.get_balance(&channel_id).expect("balance"),
        None,
        "a zero-balance registration is not a payment"
    );

    // ── one signed balance update per 21 MiB, sat-denominated ───────────────
    let pricing =
        SessionPricing::tollgate_parity(TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT).expect("pricing");
    let mut cumulative_bytes = 0u64;
    for step in 1..=STEPS {
        cumulative_bytes += STEP;
        let balance = pricing
            .raw_balance_due("sat", cumulative_bytes, 0)
            .expect("raw balance");
        assert_eq!(
            balance, step,
            "step {step} must bill exactly {step} sat at the parity rate"
        );

        // No networking argument: the hot path cannot reach a mint.
        let payment =
            payer::step_payment(&client_bridge, &channel_id, balance).expect("sign payment");
        assert_eq!(payment.channel_id, channel_id);
        assert_eq!(payment.balance, balance);
        assert!(
            !payment.has_funding(),
            "only the registration carries funding; later updates are balance-only"
        );

        // The payee validates the signature, records the balance, AND applies the
        // usage increment carried in the context. The context is the byte counter:
        // it is what makes the payee's own `amount_due` agree with the balance the
        // payer signed, which the cooperative close later re-checks.
        let context = serde_json::json!({ "bytes": STEP }).to_string();
        let accepted = payee_bridge
            .process_payment(
                &payment.channel_id,
                payment.balance,
                &payment.signature,
                payment.params.as_ref(),
                payment.funding_proofs.as_deref(),
                &context,
            )
            .expect("payee accepts payment");
        assert_eq!(accepted.balance, balance);
        assert_eq!(
            accepted.amount_due, balance,
            "the payee's own byte accounting must land on the same figure the payer signed"
        );

        // ...and records it durably BEFORE serving any byte.
        ledger
            .record_payment(&channel_id, payment.balance, &payment.signature)
            .expect("durable ledger write");
        assert_eq!(
            ledger.get_balance(&channel_id).expect("balance"),
            Some(balance)
        );
        assert_eq!(ledger.get_amount_due(&channel_id).expect("due"), balance);
    }

    // ── settle: the payee gets exactly the balance it holds a signature for ──
    let latest = ledger
        .get_latest_payment(&channel_id)
        .expect("query")
        .expect("present");
    assert_eq!(latest.balance, STEPS);
    assert_eq!(latest.payment_count, STEPS);

    let close_request = client_bridge
        .sign_cooperative_close_request(&channel_id, latest.balance)
        .expect("sign close");
    let close_json = serde_json::to_string(&close_request).expect("close json");

    // Reopen the payee store to prove the balance and signature survived a restart
    // before settling — that pair is the only thing settlement depends on.
    let payee2 = payee_host(&payee_db, &receiver_secret, &keyset_info_json);
    let restored = payee2
        .get_balance(&channel_id)
        .expect("balance must survive a restart");
    assert_eq!(restored.balance, latest.balance);
    assert_eq!(restored.signature, latest.signature);

    let settle_networking = InMemoryMintNetworking::new(std::sync::Arc::clone(&mint));
    let closed = SpilmanBridge::new(payee2)
        .execute_cooperative_close(&close_json, &settle_networking, &settle_networking)
        .expect("cooperative close");

    assert_eq!(
        closed.receiver_sum, latest.balance,
        "the payee must receive exactly the signed balance"
    );
    assert!(!closed.already_closed, "this is the first close");

    // ── the quantised retail model bills the same line ──────────────────────
    let steps = StepPricing::default();
    assert_eq!(steps.steps_for(STEPS * 1_000), STEPS);
    assert_eq!(steps.allotment_bytes_for(STEPS * 1_000), cumulative_bytes);
}

/// Crash recovery: persist an opening, die before the swap, restart from the file alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn killed_process_between_persist_and_swap_recovers_the_channel() {
    let helper = TestMintHelper::new().await.expect("test mint");
    let mint = helper.mint();
    let keyset_info_json = helper.keyset_info_json().expect("keyset info");

    let dir = tempfile::tempdir().expect("tempdir");
    let payer_db = dir.path().join("payer.sqlite");

    let receiver_secret = SecretKey::generate();
    let sender_secret = SecretKey::generate();

    let proofs = helper.mint_proofs(50).await.expect("mint proofs");
    let token = build_cashu_b_token(
        MINT_URL,
        "sat",
        &serde_json::to_string(&proofs).expect("json"),
    )
    .expect("token");

    let expiry = payer::now_seconds() + DEFAULT_CHANNEL_LIFETIME_SECS;
    let channel_id;

    // ── first "process": prepare + persist, then stop before the swap ───────
    {
        let bridge = payer_bridge(&payer_db, &sender_secret, std::sync::Arc::clone(&mint));
        let mut opening = ChannelOpening::prepare(
            &bridge,
            &token,
            &receiver_secret.public_key().to_hex(),
            &pubkey_hex(&sender_secret),
            expiry,
            &keyset_info_json,
            64,
        )
        .expect("prepare");
        channel_id = opening.channel_id().to_string();
        opening.save_opening(&bridge).expect("persist");
        // `opening` and `bridge` drop here: exactly the crash window, minus the swap.
    }

    // ── second "process": a fresh bridge over the same file ────────────────
    let bridge2 = payer_bridge(&payer_db, &sender_secret, std::sync::Arc::clone(&mint));
    {
        let inspector =
            ConfigurableClientHost::open_sqlite_read_only(payer_db.to_str().expect("utf8 path"))
                .expect("read-only inspector");
        assert_eq!(
            inspector.get_channel_state(&channel_id),
            Some(ClientChannelState::OpeningFromSwap),
            "the crash window leaves exactly this state, and it must survive a reopen"
        );
        let restored = inspector
            .get_channel_opening_from_swap(&channel_id)
            .expect("read opening")
            .expect("opening must be durable");
        assert!(
            !restored.input_token.is_empty(),
            "the original input is the restore material"
        );
        assert!(
            inspector.get_channel_funding(&channel_id).is_none(),
            "the swap never landed in this scenario"
        );
    }

    // A re-prepared instance is a *different* in-memory object whose own `persisted`
    // flag is false: it must refuse to submit a swap. That refusal is what stops a
    // recovery path from spending the original proofs a second time.
    let mut repo = ChannelOpening::prepare(
        &bridge2,
        &token,
        &receiver_secret.public_key().to_hex(),
        &pubkey_hex(&sender_secret),
        expiry,
        &keyset_info_json,
        64,
    )
    .expect("re-prepare");
    assert_eq!(repo.channel_id(), channel_id, "channel id is deterministic");
    let mint_client = InMemoryMintNetworking::new(std::sync::Arc::clone(&mint));
    let refusal = repo
        .submit_funding_swap(&mint_client)
        .expect_err("unpersisted instance must refuse to swap");
    assert!(
        format!("{refusal}").contains("OpeningFromSwap"),
        "the refusal must name the missing record: {refusal}"
    );

    // Recovery drives NUT-09 restore off the persisted record. The swap was never
    // submitted, so restore finds nothing and must fail *loudly* rather than mark the
    // channel open on unverified proofs.
    match payer::recover_open_channel(&bridge2, &channel_id) {
        Ok(_) => {
            // If the mint does return the outputs (an ambiguous-submit mint), the
            // channel must at least be usable afterwards.
            let after = ConfigurableClientHost::open_sqlite_read_only(
                payer_db.to_str().expect("utf8 path"),
            )
            .expect("inspector");
            assert_eq!(
                after.get_channel_state(&channel_id),
                Some(ClientChannelState::Open),
                "a successful restore must leave the channel payable"
            );
        }
        Err(err) => {
            // The expected branch: nothing was funded, so nothing can be restored.
            assert!(
                !format!("{err}").is_empty(),
                "a failed restore must carry a message"
            );
            let after = ConfigurableClientHost::open_sqlite_read_only(
                payer_db.to_str().expect("utf8 path"),
            )
            .expect("inspector");
            assert_eq!(
                after.get_channel_state(&channel_id),
                Some(ClientChannelState::OpeningFromSwap),
                "a failed restore must not silently promote the channel to Open"
            );
        }
    }
}

/// The recovery path that *does* have a funded channel: persist, submit the swap, then
/// drop the process before `finish_open`, and recover from the store alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn killed_after_swap_but_before_mark_open_recovers_the_channel() {
    let helper = TestMintHelper::new().await.expect("test mint");
    let mint = helper.mint();
    let keyset_info_json = helper.keyset_info_json().expect("keyset info");

    let dir = tempfile::tempdir().expect("tempdir");
    let payer_db = dir.path().join("payer.sqlite");

    let receiver_secret = SecretKey::generate();
    let sender_secret = SecretKey::generate();
    let sender_hex = pubkey_hex(&sender_secret);

    let proofs = helper.mint_proofs(50).await.expect("mint proofs");
    let token = build_cashu_b_token(
        MINT_URL,
        "sat",
        &serde_json::to_string(&proofs).expect("json"),
    )
    .expect("token");

    let expiry = payer::now_seconds() + DEFAULT_CHANNEL_LIFETIME_SECS;
    let channel_id;

    {
        let bridge = payer_bridge(&payer_db, &sender_secret, std::sync::Arc::clone(&mint));
        let mut opening = ChannelOpening::prepare(
            &bridge,
            &token,
            &receiver_secret.public_key().to_hex(),
            &sender_hex,
            expiry,
            &keyset_info_json,
            64,
        )
        .expect("prepare");
        channel_id = opening.channel_id().to_string();
        opening.save_opening(&bridge).expect("persist");
        let networking = InMemoryMintNetworking::new(std::sync::Arc::clone(&mint));
        opening.submit_funding_swap(&networking).expect("swap");
        // Drop before `finish_open`: the proofs are unblinded but the channel is still
        // `OpeningFromSwap`, which is the ambiguous window.
    }

    // The state must be exactly "opening", with the original input retained.
    {
        let inspector =
            ConfigurableClientHost::open_sqlite_read_only(payer_db.to_str().expect("utf8 path"))
                .expect("inspector");
        assert_eq!(
            inspector.get_channel_state(&channel_id),
            Some(ClientChannelState::OpeningFromSwap)
        );
    }

    let bridge2 = payer_bridge(&payer_db, &sender_secret, std::sync::Arc::clone(&mint));
    let recovered = payer::recover_open_channel(&bridge2, &channel_id).expect("recover open");
    assert_eq!(recovered.channel_id, channel_id);

    {
        let after =
            ConfigurableClientHost::open_sqlite_read_only(payer_db.to_str().expect("utf8 path"))
                .expect("inspector");
        assert_eq!(
            after.get_channel_state(&channel_id),
            Some(ClientChannelState::Open),
            "recovery must leave the channel payable, not stuck in OpeningFromSwap"
        );
        assert!(
            after.get_channel_funding(&channel_id).is_some(),
            "the recovered funding proofs must be durable"
        );
    }

    // A recovered channel is immediately payable: no re-open, no second mint spend.
    let payment = payer::step_payment(&bridge2, &channel_id, 1).expect("sign after recovery");
    assert_eq!(payment.balance, 1);
}

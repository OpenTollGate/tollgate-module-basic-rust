//! Payee-side payment verification, end to end against a real mint.
//!
//! `CRITICAL-UNVERIFIED-PAYMENT.md` found that `record_payment` accepted a forged
//! balance — `{"channel_id":"deadbeef","balance":5,"signature":"00"}` — because the
//! ledger checked monotonicity and never authenticity. The hotfix refused *every*
//! payment; this suite is the other half of the fix, and it is the half that has to
//! prove the refusal is now a *check* rather than a blanket no.
//!
//! # Why the positive case is the load-bearing one
//!
//! Tests 1–6 are all refusals, and "refuse everything" passes every one of them.
//! Only [`a_real_signed_payment_is_accepted`] distinguishes a working verifier from
//! a dead one, so it runs the full protocol: open a real channel against the
//! in-process `cdk-spilman-test-mint` (a FakeWallet-backed real `cdk::Mint`), sign a
//! balance update with the payer's bridge, and require the payee to accept it.
//!
//! # The funding record is upstream's own
//!
//! The payee's funding record is not hand-assembled in these tests. A reference
//! `ConfigurableHost` — cdk-spilman's own payee host — processes the registration,
//! and the five values are copied out of it via `get_funding_data`. So the bytes the
//! rail verifies against are exactly the bytes upstream derived, which is what makes
//! the positive case a statement about the protocol rather than about this test's
//! JSON.
//!
//! # Running
//!
//! `#[ignore]`-gated, matching `tests/spilman_payer_lifecycle.rs` — they need the
//! in-process mint harness, so the default suite (and CI) is unchanged:
//!
//! ```text
//! cargo test --test spilman_payee_verification -- --ignored --nocapture
//! ```

use std::sync::Arc;

use cashu::nuts::{CurrencyUnit, SecretKey};
use cdk_spilman::configurable_host::{
    ConfigurableHost, ConfigurableHostConfig, KeysetCacheEntry, StorageConfig, UnitPricingConfig,
};
use cdk_spilman::{
    build_cashu_b_token, parse_keyset_info_from_json, ConfigurableClientHost, SpilmanBridge,
    SpilmanClientBridge, SqliteClientStorage,
};
use cdk_spilman_test_mint::{InMemoryMintNetworking, TestMintHelper};
use tollgate_module_basic_rust::spilman::ledger::{ChannelFundingRecord, ChannelLedger};
use tollgate_module_basic_rust::spilman::payer::{
    self, ChannelOpening, DEFAULT_CHANNEL_LIFETIME_SECS,
};
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;
use tollgate_module_basic_rust::spilman::{
    PaymentRail, RailDecision, SpilmanError, TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
    TOLLGATE_STEP_SIZE_BYTES,
};

/// The URL the in-memory harness advertises for its mint.
const MINT_URL: &str = "https://test-mint";

/// One 21 MiB step — also the payee's `pricing_scale`, so one step costs 1 sat.
const STEP: u64 = TOLLGATE_STEP_SIZE_BYTES;

/// Hex of a secret key's raw scalar — used to seed the reference payee host.
fn secret_hex(secret: &SecretKey) -> String {
    secret
        .secret_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Hex of a key's *public* half — what the protocol takes as `sender_pubkey_hex`.
fn pubkey_hex(secret: &SecretKey) -> String {
    secret.public_key().to_hex()
}

/// Payee host config in the shape the live router advertises (same as the payer
/// lifecycle suite): the payee prices bytes, one 21 MiB step per sat.
fn reference_payee_config(path: &std::path::Path) -> ConfigurableHostConfig {
    ConfigurableHostConfig {
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

/// A funded channel, with the payee rail under test wired to its funding record.
struct FundedPayee {
    /// Kept alive so the ledger/payer files survive the test.
    _dir: tempfile::TempDir,
    /// Kept alive because the mint client borrows nothing but the Arc.
    _helper: TestMintHelper,
    rail: SpilmanRail,
    ledger: ChannelLedger,
    channel_id: String,
    capacity: u64,
    client:
        SpilmanClientBridge<ConfigurableClientHost<SqliteClientStorage>, InMemoryMintNetworking>,
}

impl FundedPayee {
    /// Sign `balance` with the payer's real bridge — a genuine Schnorr signature
    /// over `(channel_id, balance)`.
    fn sign(&self, balance: u64) -> String {
        payer::step_payment(&self.client, &self.channel_id, balance)
            .expect("the payer signs a balance update")
            .signature
    }
}

/// Open a real channel against the in-process mint, register it with a reference
/// cdk-spilman payee host, and persist the resulting funding record into the rail's
/// ledger.
async fn open_funded_payee() -> FundedPayee {
    let helper = TestMintHelper::new().await.expect("test mint");
    let mint = helper.mint();
    let keyset_info_json = helper.keyset_info_json().expect("keyset info");

    let dir = tempfile::tempdir().expect("tempdir");
    let payer_db = dir.path().join("payer.sqlite");
    let reference_db = dir.path().join("reference-payee.sqlite");
    let ledger_path = dir.path().join("spilman.db");

    let receiver_secret = SecretKey::generate();
    let sender_secret = SecretKey::generate();

    // ── payer side ──────────────────────────────────────────────────────────
    let mut client_host =
        ConfigurableClientHost::open_sqlite(payer_db.to_str().expect("utf8 path"))
            .expect("payer store");
    client_host.add_key(sender_secret.clone());
    let client =
        SpilmanClientBridge::new(client_host, InMemoryMintNetworking::new(Arc::clone(&mint)));

    let proofs = helper.mint_proofs(100).await.expect("mint proofs");
    let token = build_cashu_b_token(
        MINT_URL,
        "sat",
        &serde_json::to_string(&proofs).expect("proofs json"),
    )
    .expect("token");

    let expiry = payer::now_seconds() + DEFAULT_CHANNEL_LIFETIME_SECS;
    let mut opening = ChannelOpening::prepare(
        &client,
        &token,
        &receiver_secret.public_key().to_hex(),
        &pubkey_hex(&sender_secret),
        expiry,
        &keyset_info_json,
        64,
    )
    .expect("prepare open");
    let channel_id = opening.channel_id().to_string();

    opening.save_opening(&client).expect("persist opening");
    let funding_networking = InMemoryMintNetworking::new(Arc::clone(&mint));
    opening
        .submit_funding_swap(&funding_networking)
        .expect("funding swap");
    opening.finish_open(&client).expect("finish open");

    // ── the reference payee: upstream's own host derives the funding record ──
    let reference = ConfigurableHost::new(
        reference_payee_config(&reference_db),
        &secret_hex(&receiver_secret),
    )
    .expect("reference payee host");
    reference
        .set_keyset(
            MINT_URL,
            parse_keyset_info_from_json(&keyset_info_json)
                .expect("parse keyset info")
                .keyset_id,
            KeysetCacheEntry {
                info_json: keyset_info_json.clone(),
                active: true,
                unit: CurrencyUnit::Sat,
            },
        )
        .expect("reference keyset");

    let registration = client
        .sign_channel_registration(&channel_id)
        .expect("sign registration");
    assert!(
        registration.has_funding(),
        "the registration must carry the funding proofs, or there is nothing to verify against"
    );
    SpilmanBridge::new(reference.clone())
        .process_payment(
            &registration.channel_id,
            registration.balance,
            &registration.signature,
            registration.params.as_ref(),
            registration.funding_proofs.as_deref(),
            &String::new(),
        )
        .expect("the reference payee registers the channel");

    // Copied out of upstream's own store — not hand-built here.
    let funding = reference
        .get_funding_data(&channel_id)
        .expect("the reference payee must hold a funding record after registration");
    let capacity = serde_json::from_str::<serde_json::Value>(&funding.params_json)
        .expect("params json")
        .get("capacity")
        .and_then(serde_json::Value::as_u64)
        .expect("params must carry the capacity");

    let ledger = ChannelLedger::open(&ledger_path).expect("ledger");
    ledger
        .save_channel_funding(&ChannelFundingRecord {
            channel_id: channel_id.clone(),
            params_json: funding.params_json.clone(),
            funding_proofs_json: funding.funding_proofs_json.clone(),
            channel_secret_hex: funding.channel_secret_hex.clone(),
            keyset_info_json: funding.keyset_info_json.clone(),
            capacity,
            created_at: payer::now_seconds(),
        })
        .expect("persist the funding record");

    let rail = SpilmanRail::open(
        &ledger_path,
        TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
        Some(MINT_URL.to_string()),
    )
    .expect("rail");

    FundedPayee {
        _dir: dir,
        _helper: helper,
        rail,
        ledger,
        channel_id,
        capacity,
        client,
    }
}

/// THE LOAD-BEARING TEST: a genuinely signed payment is ACCEPTED.
///
/// Without this, every other test here would also pass if `record_payment` simply
/// refused everything — which is exactly the state the hotfix left it in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_real_signed_payment_is_accepted() {
    let p = open_funded_payee().await;
    assert!(
        p.capacity >= 1,
        "the funding token must buy at least one sat: capacity {}",
        p.capacity
    );

    let signature = p.sign(1);
    let outcome = p
        .rail
        .record_payment(&p.channel_id, 1, &signature)
        .await
        .expect("a genuinely signed payment MUST be accepted");

    assert_eq!(
        outcome,
        tollgate_module_basic_rust::spilman::LedgerOutcome::Accepted
    );
    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        Some(1),
        "the verified balance must be durable"
    );
    assert_eq!(p.ledger.payment_count(&p.channel_id).expect("count"), 1);

    // And the credit is spendable: 1 sat = 1000 msat, and at 22_021 bytes/msat that
    // is exactly 22_021_000 bytes.
    assert_eq!(
        p.rail.authorize_bytes(&p.channel_id, 22_021_000, 0).await,
        RailDecision::Allow
    );
    assert_eq!(
        p.rail.authorize_bytes(&p.channel_id, 22_021_001, 0).await,
        RailDecision::Deny,
        "one byte past the paid-for credit must not be served"
    );
}

/// TEST 1 — the exact forgery from `CRITICAL-UNVERIFIED-PAYMENT.md`, now on a
/// channel that *is* funded. Before the fix this was accepted; if verification is
/// ever skipped again, this is the test that fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_garbage_signature_on_a_funded_channel_is_refused() {
    let p = open_funded_payee().await;

    let err = p
        .rail
        .record_payment(&p.channel_id, 5, "00")
        .await
        .expect_err("a garbage signature on a funded channel MUST be refused");

    assert!(
        matches!(err, SpilmanError::PaymentNotVerified { .. }),
        "expected PaymentNotVerified, got {err:?}"
    );
    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        None,
        "a refused payment must not create the balance row it would become the CAS baseline for"
    );
}

/// TEST 2 — correctly SHAPED but wrong. Two flavours: 128 hex chars of nonsense
/// (the right shape for a 64-byte signature), and a *real* signature for a
/// different balance. Both must be refused, and the second is the sharper case
/// because everything except the signed message is valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_correctly_shaped_but_wrong_signature_is_refused() {
    let p = open_funded_payee().await;

    let shaped = "ab".repeat(64);
    assert_eq!(shaped.len(), 128, "this is the shape of a real signature");
    let err = p
        .rail
        .record_payment(&p.channel_id, 1, &shaped)
        .await
        .expect_err("a correctly-shaped nonsense signature MUST be refused");
    assert!(
        matches!(err, SpilmanError::PaymentNotVerified { .. }),
        "expected PaymentNotVerified, got {err:?}"
    );

    // A real signature — for balance 1 — presented as balance 2. The signature
    // parses, the channel re-derives, and only the commitment check fails.
    let signature_for_one = p.sign(1);
    let err = p
        .rail
        .record_payment(&p.channel_id, 2, &signature_for_one)
        .await
        .expect_err("a signature for a different balance MUST be refused");
    assert!(
        matches!(err, SpilmanError::PaymentNotVerified { .. }),
        "expected PaymentNotVerified, got {err:?}"
    );

    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        None,
        "neither refusal may leave a balance row"
    );
}

/// TEST 3 — a balance ABOVE the channel's capacity is refused even though the
/// capacity check runs before the signature check, so this must be refused on the
/// capacity alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_balance_above_capacity_is_refused() {
    let p = open_funded_payee().await;
    let over = p.capacity + 1;

    let err = p
        .rail
        .record_payment(&p.channel_id, over, &"ab".repeat(64))
        .await
        .expect_err("a balance above capacity MUST be refused");

    match err {
        SpilmanError::PaymentNotVerified {
            balance,
            ref detail,
            ..
        } => {
            assert_eq!(balance, over, "the refusal must name the balance offered");
            assert!(
                detail.contains("capacity"),
                "the refusal must say why: got {detail:?}"
            );
        }
        other => panic!("expected PaymentNotVerified, got {other:?}"),
    }

    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        None,
        "an over-capacity balance must not be recorded"
    );
}

/// TEST 4 — the exact previous balance (a replay) is refused by the CAS.
///
/// This is the case authenticity alone cannot catch: the signature verifies
/// perfectly, because it *is* a valid signature — for a payment already credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_replay_of_the_exact_previous_balance_is_refused() {
    let p = open_funded_payee().await;
    let signature = p.sign(1);

    p.rail
        .record_payment(&p.channel_id, 1, &signature)
        .await
        .expect("the first, genuine payment is accepted");

    let err = p
        .rail
        .record_payment(&p.channel_id, 1, &signature)
        .await
        .expect_err("a replay of the exact previous balance MUST be refused");

    assert!(
        matches!(err, SpilmanError::BalanceNotAdvanced { .. }),
        "verification passes on a replay — the CAS is what must refuse it, got {err:?}"
    );
    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        Some(1),
        "the replay must not advance anything"
    );
    assert_eq!(
        p.ledger.payment_count(&p.channel_id).expect("count"),
        1,
        "a refused replay is not newly accepted funds"
    );
}

/// TEST 5 — a refusal leaves NO balance row, so a forged balance can never become
/// the baseline the next CAS is measured against.
///
/// The proof is not just "no row": after the forgery is refused, a *genuine*
/// payment must still be accepted at the balance it actually signed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn a_refused_payment_leaves_no_balance_row() {
    let p = open_funded_payee().await;

    // A forged, high balance first — the shape of the original attack.
    let forged = p.capacity;
    assert!(
        p.rail
            .record_payment(&p.channel_id, forged, "00")
            .await
            .is_err(),
        "the forgery must be refused"
    );
    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        None,
        "the forged balance must not have become the CAS baseline"
    );

    // The baseline is still "nothing recorded", so the genuine payment lands.
    let signature = p.sign(1);
    p.rail
        .record_payment(&p.channel_id, 1, &signature)
        .await
        .expect("a genuine payment must still be accepted after a refused forgery");
    assert_eq!(
        p.ledger.get_balance(&p.channel_id).expect("balance"),
        Some(1)
    );
}

/// The unfunded case must stay exactly as it was: `UnknownChannel`, not a new
/// error. (The full pin for that is `tests/spilman_payment_authenticity.rs`; this
/// asserts the rail did not change it while gaining verification.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the in-process mint harness; run with --ignored"]
async fn an_unfunded_channel_still_reports_unknown() {
    let p = open_funded_payee().await;

    let err = p
        .rail
        .record_payment("deadbeef", 5, "00")
        .await
        .expect_err("an unfunded channel MUST be refused");

    assert!(
        matches!(err, SpilmanError::UnknownChannel { .. }),
        "an absent funding record is UnknownChannel, unchanged, got {err:?}"
    );
}

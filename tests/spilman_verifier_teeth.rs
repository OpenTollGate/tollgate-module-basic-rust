//! A payment whose SIGNATURE was never verified must never be credited.
//!
//! # Why this file exists
//!
//! A cold review found that this property had no test with teeth. Mutation-testing
//! `src/spilman/rail.rs` — deleting the entire `bridge.validate_payment(...)` block
//! (:293-315) — left the **whole default suite green**. The only tests that caught
//! it were the seven in `spilman_payee_verification.rs`, and all seven are
//! `#[ignore]`d, so CI never runs them.
//!
//! `spilman_payment_authenticity.rs` *claimed* to pin the refusal but could not:
//! every case in it used `"deadbeef"`, an UNREGISTERED channel, so the
//! `UnknownChannel` guard returned before the verifier was ever reached. Deleting
//! the verifier therefore changed nothing those tests could observe.
//!
//! # The discriminator that was missing
//!
//! Register a channel with a REAL funding record, then offer a balance with a
//! signature that cannot possibly verify. Now the two guards are separable:
//!
//!   - `UnknownChannel` fires only for an absent record;
//!   - the verifier is the ONLY thing that can refuse a *known* channel with a bad
//!     signature.
//!
//! If someone deletes the verifier, this test stops refusing and fails. If someone
//! makes verification a no-op, same. That is the property, stated so that it can
//! break.
//!
//! # Why this runs in the DEFAULT suite
//!
//! It needs no mint: the funding record is a syntactically arbitrary blob as far as
//! the refusal is concerned, because a bad signature is refused regardless of
//! whether the blob is coherent. Keeping it out of `#[ignore]` is the entire point —
//! a test the CI never runs is how the previous gap survived.

use tollgate_module_basic_rust::config::schema::SpilmanConfig;
use tollgate_module_basic_rust::spilman::ledger::{ChannelFundingRecord, ChannelLedger};
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;
use tollgate_module_basic_rust::spilman::SpilmanError;

fn rail_in(dir: &std::path::Path) -> SpilmanRail {
    let cfg = SpilmanConfig {
        enabled: true,
        mint_url: Some("https://mint.example".to_string()),
        in_bytes_per_millisat: 22_021,
        channel_db_path: Some(dir.join("spilman.db").to_string_lossy().to_string()),
    };
    SpilmanRail::from_config(&cfg)
        .expect("enabled config is not an error")
        .expect("enabled config must build a rail")
}

fn ledger_in(dir: &std::path::Path) -> ChannelLedger {
    ChannelLedger::open(&dir.join("spilman.db")).expect("ledger")
}

fn funding(channel_id: &str) -> ChannelFundingRecord {
    // Deliberately NOT a coherent channel. That is fine and it is the point: a
    // payment for THIS channel must be refused either because the signature does
    // not verify (the property under test) or because the record is not parseable
    // (also a refusal). What must never happen is acceptance.
    ChannelFundingRecord {
        channel_id: channel_id.to_string(),
        params_json: "{}".to_string(),
        funding_proofs_json: "[]".to_string(),
        channel_secret_hex: "00".to_string(),
        keyset_info_json: "{}".to_string(),
        capacity: 1_000_000,
        created_at: 0,
    }
}

/// THE REGRESSION THAT HAD NO TEST.
///
/// A channel exists (so `UnknownChannel` cannot be what refuses it), and the
/// offered balance carries a signature that cannot verify. It must be REFUSED, and
/// no balance may be recorded.
#[tokio::test]
async fn a_known_channel_with_a_forged_signature_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = ledger_in(dir.path());
    ledger
        .register_channel_funding(&funding("chan-real"))
        .expect("register a funding record");

    // Sanity: the channel is known, so the refusal below cannot be `UnknownChannel`.
    assert!(
        ledger
            .get_channel_funding("chan-real")
            .expect("read")
            .is_some(),
        "the funding record must exist, or this test proves nothing"
    );

    let rail = rail_in(dir.path());
    // `record_payment` is the PaymentRail trait method, not an inherent one.
    use tollgate_module_basic_rust::spilman::PaymentRail;
    let err = rail
        .record_payment("chan-real", 5, "00")
        .await
        .expect_err("a forged signature must be REFUSED");

    // Name the property, not just the failure: the refusal must come from the
    // verifier path, NOT from the channel-missing guard. If a future change routes
    // this to UnknownChannel, the test would still pass while testing nothing --
    // so the variant is asserted.
    match err {
        SpilmanError::PaymentNotVerified { .. } => {}
        SpilmanError::UnknownChannel { .. } => panic!(
            "refused as UnknownChannel, but the channel IS registered -- the \
             verifier was never reached, so this test cannot observe it. \
             That is exactly the vacuity a previous version of \
             spilman_payment_authenticity.rs had."
        ),
        other => panic!(
            "refused for an unexpected reason: {other:?} -- the \
                         refusal must be a FAILED VERIFICATION"
        ),
    }

    // And nothing was written: a refusal that still records is worse than useless.
    let balance = ledger.get_balance("chan-real").expect("read");
    assert_eq!(
        balance, None,
        "a refused payment must leave NO balance row; got {balance:?}"
    );
}

/// Mutation companion: if the verifier is bypassed, the SAME call is accepted.
///
/// This asserts the negative control exists — that `record_payment` on a known
/// channel *can* reach the durable write when verification is skipped. It runs
/// through the ledger directly, which is the only way to accept without a real
/// signature, and it exists so the test above is meaningful: there IS a path that
/// writes, so "nothing was written" is a real observation rather than an
/// impossibility.
#[test]
fn the_ledger_can_write_which_is_why_the_refusal_matters() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = ledger_in(dir.path());
    ledger
        .register_channel_funding(&funding("chan-ctl"))
        .expect("register");

    // The raw ledger has no notion of authenticity -- it never did, and that is
    // why the RAIL must be the guard rather than the ledger. Documenting the
    // bypass here makes the rail's job explicit.
    ledger
        .record_payment("chan-ctl", 5, "00")
        .expect("the ledger accepts whatever it is handed");
    assert_eq!(
        ledger.get_balance("chan-ctl").expect("read"),
        Some(5),
        "the ledger's CAS accepts this, so the RAIL is the only thing standing \
         between a forged signature and credited credit"
    );
}

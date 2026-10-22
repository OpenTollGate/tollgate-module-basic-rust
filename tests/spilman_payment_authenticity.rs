//! Regression test for CRITICAL-UNVERIFIED-PAYMENT.md.
//!
//! The host E2E found that `POST /spilman/payment` accepted a FORGED balance:
//! `{"channel_id":"deadbeef","balance":5,"signature":"00"}` on a channel that was
//! never funded returned `200 Accepted`. The ledger checked monotonicity only —
//! never authenticity — so anyone could mint credit for any channel id.
//!
//! These tests pin the refusal so the hole cannot silently reopen.

use tollgate_module_basic_rust::config::schema::SpilmanConfig;
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;
use tollgate_module_basic_rust::spilman::{PaymentRail, SpilmanError};

fn enabled_rail(dir: &std::path::Path) -> SpilmanRail {
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

/// The exact forgery the live host E2E sent. Must be REFUSED.
#[tokio::test]
async fn forged_payment_on_unfunded_channel_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = enabled_rail(dir.path());

    let err = rail
        .record_payment("deadbeef", 5, "00")
        .await
        .expect_err("an unsigned payment for an unfunded channel MUST be refused");

    assert!(
        matches!(err, SpilmanError::UnknownChannel { .. }),
        "expected UnknownChannel, got {err:?}"
    );
}

/// A plausible-looking but unverified payment must also be refused: the rail has no
/// funding record to verify a signature against, so it cannot distinguish this from
/// the forgery above.
#[tokio::test]
async fn well_formed_but_unverified_payment_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = enabled_rail(dir.path());

    let fake_sig = "ab".repeat(64); // 128 hex chars, the right shape
    let err = rail
        .record_payment("a".repeat(64).as_str(), 1000, &fake_sig)
        .await
        .expect_err("a correctly-shaped but unverified signature must still be refused");

    assert!(matches!(err, SpilmanError::UnknownChannel { .. }));
}

/// And the refusal must be a REFUSAL, not silent credit: nothing may be recorded.
#[tokio::test]
async fn refused_payment_records_no_balance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = enabled_rail(dir.path());

    let _ = rail.record_payment("deadbeef", 5, "00").await;

    // The status path must still report the channel as unknown — i.e. the forged
    // payment did not create a row the rest of the system would trust.
    let decision = rail.authorize_bytes("deadbeef", 0, 0).await;
    assert!(
        matches!(
            decision,
            tollgate_module_basic_rust::spilman::RailDecision::Deny
        ),
        "a refused payment must leave the channel unknown, got {decision:?}"
    );
}

//! TASK A (S2 phase 4) — the config flag that turns the Spilman rail on, and the
//! proof that leaving it off builds no rail at all.
//!
//! The rule under test is "enabled = false MUST make behaviour byte-identical to
//! today". The strongest form of that is structural: with the flag off there is no
//! rail object to consult, so no route can behave differently. That is what
//! `disabled_config_builds_no_rail` pins, and
//! `src/http/spilman_mount_test.rs` pins the observable half (same route bodies,
//! 404 on every `/spilman` path).

use tollgate_module_basic_rust::config::schema::{Config, SpilmanConfig};
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;
use tollgate_module_basic_rust::spilman::{PaymentRail, RailDecision};

fn enabled_config(db_path: &str) -> SpilmanConfig {
    SpilmanConfig {
        enabled: true,
        mint_url: Some("https://mint.example".to_string()),
        in_bytes_per_millisat: 22_021,
        channel_db_path: Some(db_path.to_string()),
    }
}

#[test]
fn default_config_builds_no_rail() {
    let cfg = Config::default();
    assert!(
        !cfg.spilman.enabled,
        "the default config must leave the Spilman rail off"
    );
    let rail = SpilmanRail::from_config(&cfg.spilman).expect("disabled config is not an error");
    assert!(
        rail.is_none(),
        "a disabled config must build NO rail, so no call site can branch on one"
    );
}

#[test]
fn enabled_config_builds_a_rail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("spilman.db");
    let cfg = enabled_config(&db.to_string_lossy());

    let rail = SpilmanRail::from_config(&cfg)
        .expect("enabled config builds a rail")
        .expect("enabled config must produce Some(rail)");
    assert_eq!(rail.name(), "spilman");
    assert!(
        db.exists(),
        "building the rail must create the durable ledger at {db:?}"
    );
}

/// The rail's credit rule, end to end through the `PaymentRail` seam: an unknown
/// channel is denied (never "assume zero due"), a recorded balance buys exactly the
/// bytes it pays for at the 22_021 parity rate, and a replay is refused.
#[tokio::test]
#[cfg(feature = "test-support")]
async fn rail_credits_exactly_what_was_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("spilman.db");
    let cfg = enabled_config(&db.to_string_lossy());
    let rail = SpilmanRail::from_config(&cfg)
        .expect("build")
        .expect("some");

    // No channel record at all: fail closed, do not serve.
    assert_eq!(
        rail.authorize_bytes("unknown", 1, 0).await,
        RailDecision::Deny,
        "an unknown channel has no credit and must be denied"
    );

    // 22_021 bytes is exactly 1 msat, which rounds up to 1 sat of raw balance.
    //
    // This goes through the TEST-ONLY ledger seam, not `record_payment`: the
    // production path deliberately refuses every payment until a per-channel
    // funding record exists to verify a signature against (see
    // CRITICAL-UNVERIFIED-PAYMENT.md). The refusal is pinned separately in
    // tests/spilman_payment_authenticity.rs; what is under test HERE is the credit
    // arithmetic, which must stay exercised or a payee could serve a byte nobody
    // paid for.
    rail.record_payment_for_tests("c", 1, "sig-1")
        .expect("first payment is durable");
    assert_eq!(
        rail.authorize_bytes("c", 22_021, 0).await,
        RailDecision::Allow
    );

    // The credit is 1 sat = 1000 msat, and at 22_021 bytes/msat that is exactly
    // 22_021_000 bytes. One byte more bills 1001 msat, which rounds up to a second
    // sat of raw balance the payer has not signed — so it must be refused.
    assert_eq!(
        rail.authorize_bytes("c", 22_021_000, 0).await,
        RailDecision::Allow
    );
    assert_eq!(
        rail.authorize_bytes("c", 22_021_001, 0).await,
        RailDecision::Deny,
        "one byte past the recorded credit must not be served"
    );

    // Replay of the same balance is not newly accepted funds.
    assert!(rail.record_payment_for_tests("c", 1, "sig-1").is_err());
    // A rollback is refused too.
    assert!(rail.record_payment_for_tests("c", 0, "sig-0").is_err());
}

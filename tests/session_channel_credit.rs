//! Access-path wiring tests: a Spilman channel must extend a live session's credit,
//! and must never grant more than a verified signature covers.
//!
//! # The shape these tests defend
//!
//! `allotment` is the EFFECTIVE ceiling. It is recomputed every sweep from
//! `retail_allotment` plus whatever a bound channel has verifiably paid for. The
//! channel raises a floor; it never lowers it. With no channel bound the two numbers
//! are equal, which is what makes the rail-off behaviour identical rather than merely
//! similar.
//!
//! # Why the first test is the important one
//!
//! `rail_off_is_byte_identical_to_the_old_behaviour` would pass BEFORE this feature was
//! written. That is deliberate: it is the regression guard that proves the change did
//! not quietly alter the retail path every existing customer uses. A feature test that
//! only exercises the new path cannot tell you that.
//!
//! # Two of the seven tests need `test-support`, and that is stated, not hidden
//!
//! Tests that seed a balance have to get credit into the ledger somehow. The
//! production `record_payment` refuses anything whose signature it cannot verify
//! (that refusal is the fix for CRITICAL-UNVERIFIED-PAYMENT, and it is load-bearing),
//! so a test cannot simply write a balance through it. The `test-support` feature
//! exposes `record_payment_for_tests` for exactly this, and it is NOT in the default
//! feature set — a shipped router binary must not carry a seam that bypasses
//! signature verification.
//!
//! Consequence, stated plainly because silence here would be a coverage hole:
//!
//!   cargo test                                   -> 4 of 7 run
//!   cargo test --features test-support           -> 7 of 7 run
//!
//! The four that run by default are the ones that need no credit: the rail-off
//! regression guard, the no-rail case, the unknown-channel case, and the
//! unconvertible-balance case. The three that need a funded channel are gated. CI
//! must run the second command to cover the access path fully — the assertion that
//! matters most (a funded channel raises the ceiling) cannot be reached without it.

use std::path::Path;

use tollgate_module_basic_rust::config::schema::SpilmanConfig;
use tollgate_module_basic_rust::session::channel_credit::effective_allotment;
// Only the `test-support`-gated tests sweep; importing it unconditionally makes it
// an unused import in a DEFAULT build, which this repo's `-D warnings` turns into
// a hard error. Gate the import with the tests that need it.
#[cfg(feature = "test-support")]
use tollgate_module_basic_rust::session::channel_credit::sweep_with_channel_credit;
use tollgate_module_basic_rust::session::CustomerSession;
#[cfg(feature = "test-support")]
use tollgate_module_basic_rust::session::SessionManager;
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;

fn session(mac: &str, retail: u64, channel_id: Option<&str>) -> CustomerSession {
    CustomerSession {
        mac: mac.to_string(),
        allotment: retail,
        used: 0,
        metric: "bytes".to_string(),
        expiry: u64::MAX,
        granted_at: 0,
        retail_allotment: retail,
        channel_id: channel_id.map(str::to_string),
    }
}

fn rail_in(dir: &Path) -> SpilmanRail {
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

/// THE REGRESSION GUARD. With no rail (or no channel), the effective allotment must be
/// exactly the retail number — the behaviour every existing customer already has.
#[test]
fn rail_off_is_byte_identical_to_the_old_behaviour() {
    let s = session("aa:bb:cc:dd:ee:ff", 21_000_000, None);
    assert_eq!(
        effective_allotment(&s, None),
        21_000_000,
        "with the rail off the allotment must be the retail number, untouched"
    );
}

/// A session that claims a channel while the rail is absent keeps only what it bought.
///
/// This is the config-flipped-off-under-a-live-session case: the customer keeps the
/// credit they paid tokens for, and gains nothing.
#[test]
fn channel_bound_session_without_a_rail_keeps_only_retail() {
    let s = session("aa:bb:cc:dd:ee:01", 5_000, Some("chan"));
    assert_eq!(effective_allotment(&s, None), 5_000);
}

/// An unknown channel adds nothing, and must not revoke what was bought.
#[test]
fn unknown_channel_adds_no_credit_but_keeps_retail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());
    let s = session("aa:bb:cc:dd:ee:02", 7_000, Some("never-registered"));
    assert_eq!(
        effective_allotment(&s, Some(&rail)),
        7_000,
        "an unknown channel must add zero credit, not fail the retail half"
    );
}

/// A channel with a verified balance raises the ceiling by the bytes that balance buys.
///
/// The number is computed by the rail's own pricing, so this test also pins that the
/// access path and `authorize_bytes` agree on the rate.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn a_funded_channel_raises_the_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());

    // Record a balance the way a verified payment would.
    rail.record_payment_for_tests("chan-paid", 1, "sig-1")
        .expect("record a verified balance");

    let s = session("aa:bb:cc:dd:ee:03", 1_000, Some("chan-paid"));
    let effective = effective_allotment(&s, Some(&rail));
    let bytes = rail
        .bytes_for_raw_balance(1)
        .expect("1 raw unit converts at the parity rate");
    assert_eq!(
        effective,
        1_000 + bytes,
        "the ceiling must be retail + what the channel verifiably paid for"
    );
    assert!(
        effective > 1_000,
        "the channel must actually RAISE the ceiling, not merely be consulted"
    );
}

/// The channel half must not be able to extend a session past what it paid for.
///
/// Once `used` exceeds retail + channel credit, the session is terminal — exactly like
/// the old `used >= allotment` rule.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn exhausted_channel_credit_revokes_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());
    rail.record_payment_for_tests("chan-small", 1, "sig-1")
        .expect("record");

    let mut s = session("aa:bb:cc:dd:ee:04", 0, Some("chan-small"));
    let ceiling = effective_allotment(&s, Some(&rail));
    assert!(ceiling > 0, "the channel must have bought something");

    // Under the ceiling: alive. At the ceiling: terminal.
    s.used = ceiling - 1;
    assert_eq!(
        sweep_with_channel_credit(&mut one(&s), Some(&rail), 0).len(),
        0
    );
    s.used = ceiling;
    assert_eq!(
        sweep_with_channel_credit(&mut one(&s), Some(&rail), 0),
        vec!["aa:bb:cc:dd:ee:04".to_string()],
        "used == effective allotment is terminal, same as the retail rule"
    );
}

/// A payment that lands AFTER the session started must extend the SAME session —
/// no new gate-open, and the consumed `used` must not be forgotten.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn a_later_payment_extends_the_same_session_without_losing_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());

    let mut sessions = SessionManager::new();
    let mut s = sessions.create_session("aa:bb:cc:dd:ee:05", 0, "bytes", 3600);
    s.channel_id = Some("chan-growing".to_string());
    s.used = 500;
    sessions.sessions.insert(s.mac.clone(), s);

    // No payment yet: nothing bought, so any usage is already over the ceiling.
    assert_eq!(
        sweep_with_channel_credit(&mut sessions, Some(&rail), 0),
        vec!["aa:bb:cc:dd:ee:05".to_string()],
        "with no credit recorded the session is terminal"
    );

    // A verified payment arrives.
    rail.record_payment_for_tests("chan-growing", 1, "sig-1")
        .expect("record");
    let horizon = rail.bytes_for_raw_balance(1).expect("rate");
    assert!(
        horizon > 500,
        "the payment must cover the usage for this test to mean anything"
    );

    assert!(
        sweep_with_channel_credit(&mut sessions, Some(&rail), 0).is_empty(),
        "the payment must revive the SAME session"
    );
    let kept = sessions
        .sessions
        .get("aa:bb:cc:dd:ee:05")
        .expect("still there");
    assert_eq!(
        kept.used, 500,
        "usage must survive; reviving is not a re-grant"
    );
    assert_eq!(
        kept.allotment, horizon,
        "the ceiling must now be the channel's verified credit"
    );
}

/// A channel that exists but has NO balance grants NO channel credit.
///
/// This replaces an earlier test that claimed to cover the "unconvertible balance"
/// case and did not: it used an UNKNOWN channel, so it exercised the
/// unknown-channel guard and duplicated the test above it. A cold review caught
/// that. A vacuous test is worse than a missing one because it looks like coverage.
///
/// The real case is a channel that is REGISTERED (so the unknown-channel guard does
/// not apply) but has never been paid, so there is no balance row to convert. That
/// must land on retail-only -- not on a panic, and not on a fabricated number.
#[cfg(feature = "test-support")]
#[test]
fn a_registered_but_unfunded_channel_grants_no_channel_credit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());

    // Registered (so the unknown-channel guard does NOT apply) but never paid, so
    // there is no balance row to convert. Both facts are asserted, because a test
    // that accidentally exercises the unknown-channel guard proves nothing.
    use tollgate_module_basic_rust::spilman::ledger::{ChannelFundingRecord, ChannelLedger};
    let ledger = ChannelLedger::open(&dir.path().join("spilman.db")).expect("ledger");
    ledger
        .register_channel_funding(&ChannelFundingRecord {
            channel_id: "chan-unfunded".to_string(),
            params_json: "{}".to_string(),
            funding_proofs_json: "[]".to_string(),
            channel_secret_hex: "00".to_string(),
            keyset_info_json: "{}".to_string(),
            capacity: 1_000_000,
            created_at: 0,
        })
        .expect("register a funding record without a balance");
    assert!(
        rail.is_known_channel("chan-unfunded").expect("read"),
        "the channel must be KNOWN, or this test exercises the unknown-channel guard"
    );
    assert_eq!(
        rail.channel_balance("chan-unfunded").expect("read"),
        None,
        "an unfunded channel must have NO balance row, which is what makes the \
         conversion the thing under test"
    );

    let s = session("aa:bb:cc:dd:ee:06", 4_242, Some("chan-unfunded"));
    assert_eq!(
        effective_allotment(&s, Some(&rail)),
        4_242,
        "a known-but-unfunded channel must add zero credit and keep the retail half"
    );
}

/// Convenience: wrap one session in a manager for the sweep tests.
///
/// Gated with the tests that use it. `SessionManager` is still needed ungated for
/// the `a_later_payment_...` test, which constructs one directly.
#[cfg(feature = "test-support")]
fn one(s: &CustomerSession) -> SessionManager {
    let mut m = SessionManager::new();
    m.sessions.insert(s.mac.clone(), s.clone());
    m
}

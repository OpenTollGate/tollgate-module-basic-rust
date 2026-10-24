//! Tests for the two defects a cold review found in the access path.
//!
//! Both are the same shape: code that was correct and UNREACHABLE or that quietly
//! changed a different field. Neither could be caught by the tests that existed,
//! because those set the fields directly instead of going through the path a real
//! router uses.
//!
//! 1. `channel_id` had NO production writer — `/spilman/access` is now it.
//! 2. `extend_session` added to the EFFECTIVE allotment and re-created the session
//!    with `retail_allotment = allotment` and `channel_id = None`, folding a
//!    channel's verified credit into token money and unbinding the channel.

// Only the `test-support`-gated tests read credit or build a rail. Importing these
// unconditionally makes them unused in a DEFAULT build, and this repo denies
// warnings -- so an ungated import is a build failure. (Cost me one round trip
// earlier in this same PR; worth a comment so it does not happen a third time.)
#[cfg(feature = "test-support")]
use tollgate_module_basic_rust::config::schema::SpilmanConfig;
#[cfg(feature = "test-support")]
use tollgate_module_basic_rust::session::channel_credit::effective_allotment;
use tollgate_module_basic_rust::session::SessionManager;
#[cfg(feature = "test-support")]
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;

#[cfg(feature = "test-support")]
use std::path::Path;

#[cfg(feature = "test-support")]
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

/// THE UNREACHABILITY DEFECT.
///
/// Before `bind_channel` existed, `channel_id` was `None` for every session a
/// router could ever have, so `effective_allotment` always returned at its first
/// branch. This test goes through the production writer and asserts the credit
/// actually arrives.
#[cfg(feature = "test-support")]
#[test]
fn binding_a_session_to_a_funded_channel_grants_channel_credit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());
    rail.record_payment_for_tests("chan-bound", 1, "sig-1")
        .expect("record a verified balance");

    let mut sessions = SessionManager::new();
    sessions.create_session("aa:bb:cc:dd:ee:10", 1_000, "bytes", 3600);

    // Before the binding the session draws only on retail.
    let s = sessions.sessions.get("aa:bb:cc:dd:ee:10").expect("session");
    assert_eq!(
        effective_allotment(s, Some(&rail)),
        1_000,
        "an unbound session must draw only on retail"
    );

    let effective = sessions
        .bind_channel("aa:bb:cc:dd:ee:10", "chan-bound")
        .expect("bind")
        .expect("a live session exists");
    assert_eq!(
        effective, 1_000,
        "binding must not RAISE the ceiling before a verified balance has been read \
         by the sweep; the binding is a pointer, not a payment"
    );

    let s = sessions.sessions.get("aa:bb:cc:dd:ee:10").expect("session");
    assert_eq!(
        s.channel_id.as_deref(),
        Some("chan-bound"),
        "the binding must be recorded on the session"
    );
    let with_channel = effective_allotment(s, Some(&rail));
    assert!(
        with_channel > 1_000,
        "after binding, the channel's verified credit must reach the access path \
         (got {with_channel})"
    );
}

/// A binding must attach to an EXISTING session, never create access.
///
/// If binding created a session, anyone able to POST /spilman/access could conjure
/// a session with default access. Returning `None` is the refusal.
#[test]
fn binding_an_unknown_mac_does_not_create_access() {
    let mut sessions = SessionManager::new();
    let got = sessions
        .bind_channel("aa:bb:cc:dd:ee:11", "chan")
        .expect("no error for a missing mac");
    assert_eq!(got, None, "an unknown mac must not get a session");
    assert!(
        !sessions.sessions.contains_key("aa:bb:cc:dd:ee:11"),
        "no session may be created by a binding"
    );
}

/// THE RETAIL-BLEED DEFECT, reproduced.
///
/// A cold review observed: retail=1000 with a channel worth 22_021_000 became
/// retail=22_023_000 with channel_id=None after an extension — so the channel's
/// credit turned into token money and SURVIVED the rail being switched off.
#[cfg(feature = "test-support")]
#[test]
fn extending_a_channel_session_binds_surviving_rail_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rail = rail_in(dir.path());
    rail.record_payment_for_tests("chan-ext", 1, "sig-1")
        .expect("record");

    let mut sessions = SessionManager::new();
    sessions.create_session("aa:bb:cc:dd:ee:12", 1_000, "bytes", 3600);
    sessions
        .bind_channel("aa:bb:cc:dd:ee:12", "chan-ext")
        .expect("bind")
        .expect("session");

    // Simulate the sweep having raised the effective ceiling from the channel.
    {
        let s = sessions
            .sessions
            .get_mut("aa:bb:cc:dd:ee:12")
            .expect("session");
        s.allotment = 22_022_000;
    }

    // A second purchase extends the session.
    let new_total = sessions
        .extend_session("aa:bb:cc:dd:ee:12", 500, "bytes", 3600)
        .expect("extend");

    let s = sessions.sessions.get("aa:bb:cc:dd:ee:12").expect("session");
    assert_eq!(
        s.retail_allotment, 1_500,
        "an extension adds to RETAIL only (1000 + 500); adding to the effective \
         total would fold 22_021_000 bytes of channel credit into token money"
    );
    assert_eq!(
        s.channel_id.as_deref(),
        Some("chan-ext"),
        "an extension must not silently unbind the channel"
    );
    assert_eq!(
        new_total, 1_500,
        "the returned total must be the retail figure, not the effective one"
    );

    // THE CONSEQUENCE THAT MATTERS: with the rail off, the client must fall back to
    // what it actually paid for. Before the fix it kept 22_023_000.
    let off = effective_allotment(s, None);
    assert_eq!(
        off, 1_500,
        "with the rail off a channel-funded session must fall back to its retail \
         allotment; got {off}, which means channel credit was baked in"
    );
}

/// An extension on a plain retail session is unchanged by this fix.
#[test]
fn extending_a_retail_session_still_works() {
    let mut sessions = SessionManager::new();
    sessions.create_session("aa:bb:cc:dd:ee:13", 1_000, "bytes", 3600);
    let total = sessions
        .extend_session("aa:bb:cc:dd:ee:13", 500, "bytes", 3600)
        .expect("extend");
    assert_eq!(total, 1_500);
    let s = sessions.sessions.get("aa:bb:cc:dd:ee:13").expect("session");
    assert_eq!(s.retail_allotment, 1_500);
    assert_eq!(s.channel_id, None);
}

/// Extending a session that does not exist still returns `None`.
#[test]
fn extending_an_absent_session_returns_none() {
    let mut sessions = SessionManager::new();
    assert_eq!(
        sessions.extend_session("aa:bb:cc:dd:ee:14", 500, "bytes", 3600),
        None
    );
}

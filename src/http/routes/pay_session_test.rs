//! A1 — the kind-1022 session event must be a real, signed Nostr event.
//!
//! Measured against TIP-01 (`TIP-01.md:38-57`) and the reference builder
//! (`tollgate-module-basic-go` `src/merchant/merchant.go:2132-2152`, read for
//! its *shape*, not copied).
//!
//! Before this item the handler emitted hand-built JSON with:
//!   * `id` = `rand::random::<u64>()` padded to 64 hex — not a NIP-01 id
//!   * `sig` = "" — unsigned
//!   * tags `allotment` + `metric` only — no `device-identifier`, no `start-time`
//!   * `allotment = received_amount * 1000` — unrelated to the price table
//!
//! These tests fail on that code and pass after A1.

use crate::config::schema::Config;
use secp256k1::Secp256k1;
use serde_json::Value;

fn test_identity() -> crate::identity::MerchantIdentity {
    let secp = Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    crate::identity::MerchantIdentity {
        name: "test".to_string(),
        secret_key: sk,
    }
}

/// The allotment a payment of `amount_sats` buys, per TIP-01/TIP-02:
/// steps = amount / price_per_step (integer division — partial steps are NOT
/// granted), allotment = steps * step_size.
#[test]
fn allotment_is_steps_times_step_size() {
    // 5 sats at 1 sat/step, 22020096 bytes per step -> 5 full steps
    let got = super::pay::calculate_allotment(5, 1, 22020096, "bytes").expect("allotment");
    assert_eq!(got, 5 * 22020096);
}

#[test]
fn allotment_floors_partial_steps() {
    // 7 sats at 3 sats/step -> 2 full steps (1 sat is not a whole step)
    let got = super::pay::calculate_allotment(7, 3, 1000, "bytes").expect("allotment");
    assert_eq!(got, 2000, "7/3 = 2 steps, remainder is not granted");
}

#[test]
fn allotment_is_zero_below_one_step() {
    // 2 sats at 3 sats/step -> no whole step purchasable
    let got = super::pay::calculate_allotment(2, 3, 1000, "bytes").expect("allotment");
    assert_eq!(got, 0);
}

#[test]
fn allotment_uses_step_size_for_milliseconds() {
    let got = super::pay::calculate_allotment(2, 1, 60_000, "milliseconds").expect("allotment");
    assert_eq!(got, 120_000, "2 steps x 60000 ms");
}

/// A zero price would divide by zero; it must be rejected, not panic.
#[test]
fn allotment_rejects_zero_price() {
    assert!(super::pay::calculate_allotment(5, 0, 1000, "bytes").is_err());
}

// ---------------------------------------------------------------------------
// The event itself
// ---------------------------------------------------------------------------

fn build_session_event_for_test() -> Value {
    let mut cfg = Config::default();
    cfg.metric = "bytes".to_string();
    cfg.step_size = 1000;
    let ident = test_identity();
    super::pay::build_session_event(
        &cfg,
        &ident,
        "aa:bb:cc:dd:ee:ff",
        5000,          // allotment
        1_700_000_000, // start time
        None,          // no customer pubkey
    )
}

#[test]
fn session_event_is_signed_and_id_is_a_real_nip01_id() {
    let ev = build_session_event_for_test();

    let sig = ev["sig"].as_str().expect("sig is a string");
    assert_eq!(sig.len(), 128, "schnorr sig is 64 bytes = 128 hex chars");
    assert!(!sig.is_empty(), "A1: the session event must be SIGNED");

    let id = ev["id"].as_str().expect("id is a string");
    assert_eq!(id.len(), 64, "NIP-01 id is 32 bytes = 64 hex chars");

    // recompute the id from the serialised event and compare
    let parsed: crate::nostr_event::NostrEvent =
        serde_json::from_value(ev.clone()).expect("event deserialises");
    let expected = crate::nostr_event::compute_id(&parsed);
    assert_eq!(
        id, expected,
        "A1: `id` must be the NIP-01 hash of the serialised event, not rand::random"
    );
}

#[test]
fn session_event_carries_every_tip01_tag() {
    let ev = build_session_event_for_test();
    let tags = ev["tags"].as_array().expect("tags");

    let has = |name: &str| tags.iter().any(|t| t[0].as_str() == Some(name));

    assert!(has("allotment"), "TIP-01 requires allotment");
    assert!(has("metric"), "TIP-01 requires metric");
    assert!(
        has("device-identifier"),
        "TIP-01 requires device-identifier; A1 added it"
    );
    assert!(has("start-time"), "TIP-01 requires start-time");

    // p MUST NOT be present when the payment carried no customer pubkey
    assert!(
        !has("p"),
        "TIP-01/TIP-02: p is optional and pubkey-only; a raw-token payment has no customer pubkey"
    );

    // shape of the device-identifier tag
    let did = tags
        .iter()
        .find(|t| t[0].as_str() == Some("device-identifier"))
        .expect("device-identifier");
    assert_eq!(
        did[1].as_str(),
        Some("mac"),
        "TIP-01: [device-identifier, <type>, <value>]"
    );
    assert_eq!(did[2].as_str(), Some("aa:bb:cc:dd:ee:ff"));
}

#[test]
fn session_event_emits_p_only_when_a_customer_pubkey_exists() {
    let mut cfg = Config::default();
    cfg.metric = "bytes".to_string();
    let ident = test_identity();
    let ev = super::pay::build_session_event(
        &cfg,
        &ident,
        "aa:bb:cc:dd:ee:ff",
        5000,
        1_700_000_000,
        Some("3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"),
    );
    let tags = ev["tags"].as_array().expect("tags");
    let p = tags.iter().find(|t| t[0].as_str() == Some("p"));
    assert!(p.is_some(), "a customer pubkey MUST appear as a p tag");
    assert_eq!(
        p.unwrap()[1].as_str(),
        Some("3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")
    );
}

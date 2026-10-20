//! A2 — kind-21023 notices must be real, signed Nostr events with
//! `level` and `code` (TIP-01 `TIP-01.md:59-127`).
//!
//! Before A2 the handler emitted bare JSON such as
//! `{"kind":21023,"content":"payment rejected: ..."}` — no pubkey, id,
//! created_at, tags or sig, and therefore no machine-readable code. A client
//! cannot act on that: it can only display a string.

use crate::identity::MerchantIdentity;
use secp256k1::Secp256k1;
use serde_json::Value;

fn ident() -> MerchantIdentity {
    let secp = Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    MerchantIdentity {
        name: "t".into(),
        secret_key: sk,
    }
}

fn notice(code: &str, content: &str) -> Value {
    let i = ident();
    super::pay::build_notice_event(&i, "error", code, content, None)
}

#[test]
fn notice_is_a_full_signed_event() {
    let ev = notice("payment-error-token-spent", "token already spent");
    assert_eq!(ev["kind"].as_u64(), Some(21023));
    assert_eq!(
        ev["sig"].as_str().map(str::len),
        Some(128),
        "must be signed"
    );
    assert_eq!(ev["id"].as_str().map(str::len), Some(64), "NIP-01 id");
    assert!(ev["pubkey"].as_str().is_some(), "pubkey must be present");
    assert!(
        ev["created_at"].as_u64().is_some(),
        "created_at must be present"
    );
    assert_eq!(ev["content"].as_str(), Some("token already spent"));
}

#[test]
fn notice_carries_level_and_code() {
    let ev = notice("payment-error-mint-not-accepted", "mint not allowed");
    let tags = ev["tags"].as_array().expect("tags");
    let get = |n: &str| tags.iter().find(|t| t[0].as_str() == Some(n)).cloned();
    assert_eq!(
        get("level").and_then(|t| t[1].as_str().map(str::to_string)),
        Some("error".to_string()),
        "TIP-01 requires a level tag"
    );
    assert_eq!(
        get("code").and_then(|t| t[1].as_str().map(str::to_string)),
        Some("payment-error-mint-not-accepted".to_string()),
        "TIP-01 requires a machine-readable code"
    );
}

#[test]
fn notice_omits_p_unless_addressing_a_pubkey() {
    let i = ident();
    let general = super::pay::build_notice_event(&i, "info", "session-error", "hi", None);
    assert!(
        !general["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t[0].as_str() == Some("p")),
        "a general notice carries no p tag"
    );

    let pk = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
    let addressed = super::pay::build_notice_event(&i, "error", "session-error", "hi", Some(pk));
    let p = addressed["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t[0].as_str() == Some("p"))
        .expect("addressed notice has p");
    assert_eq!(p[1].as_str(), Some(pk));
}

#[test]
fn notice_level_vocabulary_is_constrained() {
    let i = ident();
    // TIP-01: error | warning | info | debug
    for lvl in ["error", "warning", "info", "debug"] {
        let ev = super::pay::build_notice_event(&i, lvl, "session-error", "x", None);
        let tags = ev["tags"].as_array().unwrap();
        let got = tags
            .iter()
            .find(|t| t[0].as_str() == Some("level"))
            .and_then(|t| t[1].as_str());
        assert_eq!(got, Some(lvl));
    }
}

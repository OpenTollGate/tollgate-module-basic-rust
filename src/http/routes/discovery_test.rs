//! Regression tests for the kind-10021 discovery advertisement.
//!
//! These pin three spec violations that shipped in the clone and were found by
//! reading it against TIP-01/TIP-02 rather than against its own README:
//!
//!   1. `metric` emitted `"data"`/`"time"`; the spec requires
//!      `"bytes"`/`"milliseconds"`.
//!   2. `price_per_step` was emitted once, for `accepted_mints.first()` only,
//!      and the 6-element form was misnamed `"block"` — so a multi-mint
//!      TollGate advertised one price and no client could parse the rich tag.
//!   3. `min_purchase_steps` defaulted to 0 and was advertised verbatim;
//!      TIP-02 says absent-or-0 MUST be treated as 1, and known clients reject
//!      `min_steps=0` outright.
//!
//! Each test fails on the pre-fix code and passes after it.

use crate::config::schema::{Config, MintConfig};
use secp256k1::Secp256k1;
use serde_json::Value;

fn identity() -> crate::identity::MerchantIdentity {
    let secp = Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    crate::identity::MerchantIdentity {
        name: "test".to_string(),
        secret_key: sk,
    }
}

async fn discovery_json(config: Config) -> Value {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    let state = crate::http::AppState {
        config: std::sync::Arc::new(config),
        identity: std::sync::Arc::new(identity()),
        wallet: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    };
    let app = crate::http::create_test_router(state);
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("discovery body is JSON")
}

fn tags_of(v: &Value) -> Vec<Vec<String>> {
    v["tags"]
        .as_array()
        .expect("event has tags")
        .iter()
        .map(|t| {
            t.as_array()
                .expect("tag is array")
                .iter()
                .map(|x| x.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .collect()
}

fn tag<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a Vec<String>> {
    tags.iter()
        .find(|t| t.first().map(String::as_str) == Some(name))
}

#[tokio::test]
async fn metric_uses_the_spec_vocabulary() {
    for (configured, expected) in [("bytes", "bytes"), ("time", "milliseconds")] {
        let mut cfg = Config::default();
        cfg.metric = configured.to_string();
        let v = discovery_json(cfg).await;
        let tags = tags_of(&v);
        let m = tag(&tags, "metric").expect("metric tag present");
        assert_eq!(
            m.get(1).map(String::as_str),
            Some(expected),
            "TIP-01 requires metric to be `milliseconds` or `bytes`; \
             configured {configured:?} must advertise {expected:?}"
        );
    }
}

#[tokio::test]
async fn price_per_step_is_one_tag_per_mint() {
    let mut cfg = Config::default();
    let mut a = MintConfig::default_production("https://a.example");
    a.url = "https://a.example".to_string();
    a.price_per_step = 10;
    a.min_purchase_steps = 3;
    let mut b = MintConfig::default_production("https://b.example");
    b.url = "https://b.example".to_string();
    b.price_per_step = 20;
    cfg.accepted_mints = vec![a, b];

    let v = discovery_json(cfg).await;
    let tags = tags_of(&v);

    let prices: Vec<&Vec<String>> = tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some("price_per_step"))
        .collect();
    assert_eq!(
        prices.len(),
        2,
        "multi-mint TollGate must advertise one price_per_step per accepted mint"
    );

    // Every price tag carries the full TIP-02 shape.
    for p in &prices {
        assert_eq!(p.len(), 6, "TIP-02 shape is 6 elements, got {p:?}");
        assert_eq!(p[1], "cashu", "second element is the bearer asset type");
    }

    // The misnamed `block` tag must be gone.
    assert!(
        tag(&tags, "block").is_none(),
        "the 6-element tag was misnamed `block`; clients parse `price_per_step`"
    );

    let urls: Vec<&str> = prices.iter().map(|p| p[4].as_str()).collect();
    assert!(urls.contains(&"https://a.example"));
    assert!(urls.contains(&"https://b.example"));
}

#[tokio::test]
async fn min_steps_zero_is_floored_to_one() {
    let mut cfg = Config::default();
    let mut m = MintConfig::default_production("https://a.example");
    m.url = "https://a.example".to_string();
    m.min_purchase_steps = 0; // the config default, and what used to be advertised
    cfg.accepted_mints = vec![m];

    let v = discovery_json(cfg).await;
    let tags = tags_of(&v);
    let p = tag(&tags, "price_per_step").expect("price tag present");
    assert_eq!(
        p.get(5).map(String::as_str),
        Some("1"),
        "TIP-02: absent or 0 min_steps MUST be treated as 1; known clients \
         reject an advertisement carrying min_steps=0"
    );
}

#[tokio::test]
async fn tips_carries_values() {
    let v = discovery_json(Config::default()).await;
    let tags = tags_of(&v);
    let t = tag(&tags, "tips").expect("tips tag present");
    assert!(
        t.len() > 1,
        "a bare [\"tips\"] with no values is not a valid TIP-01 advertisement"
    );
    assert!(t.iter().skip(1).any(|x| x == "1"), "TIP-01 is implemented");
    assert!(t.iter().skip(1).any(|x| x == "2"), "TIP-02 is implemented");
}

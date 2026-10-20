//! A4 — the raw-body POST contract on `/` (HTTP-01 + corpus vectors).
//!
//! Three defects, per the corpus (`http/http01-payment.json`) and the reference:
//!   * an ABSENT Content-Type must be accepted as a raw token body; Rust
//!     answered 415.
//!   * a body over 1 MiB must be refused; the axum `String` extractor was
//!     unbounded, so the gateway would buffer whatever it was sent.
//!   * a JSON body that is not a kind-21000 event with a payment tag answered
//!     with a bare notice; it is now a 21023 with a code (A2), and this pins the
//!     status/`code` pair.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    let mut cfg = crate::config::schema::Config::default();
    cfg.metric = "bytes".to_string();
    cfg.step_size = 1000;
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    let state = crate::http::AppState {
        config: std::sync::Arc::new(cfg),
        identity: std::sync::Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    };
    crate::http::create_test_router(state)
}

async fn post(content_type: Option<&str>, body: Vec<u8>) -> (StatusCode, String) {
    let mut b = Request::builder().method("POST").uri("/");
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    let resp = app()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// HTTP-01: the token IS the body. A client that omits Content-Type is still
/// sending a payment, and the reference accepts it.
#[tokio::test]
async fn absent_content_type_is_accepted_as_a_raw_token() {
    let (status, _body) = post(None, b"cashuBnotarealtoken".to_vec()).await;
    assert_ne!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "A4: an absent Content-Type must not be rejected as 415"
    );
}

/// A genuinely unsupported type still gets 415 — the gate must not be removed
/// wholesale, only relaxed for the absent case.
#[tokio::test]
async fn unsupported_content_type_is_still_refused() {
    let (status, _b) = post(Some("application/xml"), b"<x/>".to_vec()).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

/// 1 MiB is the documented ceiling (corpus vector http01-post/body-over-1mib).
#[tokio::test]
async fn body_over_one_mib_is_refused() {
    let big = vec![b'a'; 1024 * 1024 + 1];
    let (status, body) = post(Some("text/plain"), big).await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE,
        "A4: a >1 MiB body must be refused, got {status}"
    );
    assert!(
        body.contains("21023"),
        "the refusal must be a notice event, got: {body}"
    );
}

/// Exactly 1 MiB is allowed through the size gate (it will fail later, at token
/// parsing — that is a different error, and the point is the GATE).
#[tokio::test]
async fn body_at_exactly_one_mib_passes_the_size_gate() {
    let edge = vec![b'a'; 1024 * 1024];
    let (status, body) = post(Some("text/plain"), edge).await;
    assert_ne!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "exactly 1 MiB must not be refused by the size gate"
    );
    assert!(
        !body.contains("body too large"),
        "exactly 1 MiB must not be refused by the size gate"
    );
}

/// A JSON body that is not a kind-21000 event with a payment tag is a 402 with a
/// machine-readable code, not an opaque string.
#[tokio::test]
async fn invalid_json_event_returns_a_coded_notice() {
    let (status, body) = post(
        Some("application/json"),
        br#"{"kind":21000,"tags":[["other","x"]]}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    let v: serde_json::Value = serde_json::from_str(&body).expect("notice is JSON");
    assert_eq!(v["kind"].as_u64(), Some(21023));
    let tags = v["tags"].as_array().expect("tags");
    assert!(
        tags.iter().any(|t| t[0].as_str() == Some("code")),
        "A2/A4: a notice must carry a machine-readable code"
    );
}

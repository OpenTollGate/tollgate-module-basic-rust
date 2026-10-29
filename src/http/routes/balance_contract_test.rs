//! `/balance` must match `corpus/http/http03-balance.json`, not a wallet dump.
//!
//! Found on real hardware: the module returned
//! `{"balance":0,"mintBalances":[{"url":"...","balance":0}]}`
//! while the corpus (and the Go gateway) specify
//! `{status, session_active, metric, usage, allotment, remaining, start_time, mac}`.
//!
//! The file's own doc comment claimed "wallet balance in Go schema" -- it was neither
//! the Go schema nor the corpus one. A client reading `session_active` saw `undefined`,
//! which reads as "no session" only by accident; a client reading `allotment` to decide
//! whether to buy again got nothing at all.

use crate::http::routes::balance::handle_balance;
use crate::http::AppState;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::response::IntoResponse;
use std::sync::{Arc, Mutex};

async fn body_of(resp: axum::response::Response) -> (u16, serde_json::Value) {
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

fn empty_state() -> AppState {
    AppState {
        config: Arc::new(crate::config::Config::default()),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "merchant".to_string(),
            secret_key: secp256k1::SecretKey::from_slice(&[3u8; 32]).unwrap(),
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(None)),
        sessions: Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: Arc::new(Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    }
}

/// Corpus `http03-balance/no-session`: `{status, session_active, mac}` plus the
/// observed zero fields (`usage:0, allotment:0, remaining:0` — the corpus's
/// `additive_fields_observed`, with live rig evidence; the Go serializer emits
/// them unconditionally).
///
/// `status: 1` is the corpus's success marker. The previous shape had no `status` at
/// all, so the JSON parsed but every field a client looked for was missing.
#[tokio::test]
async fn no_session_matches_corpus_shape() {
    let resp = handle_balance(
        State(empty_state()),
        ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 50], 44444))),
        axum::http::HeaderMap::new(),
    )
    .await
    .into_response();
    let (status, v) = body_of(resp).await;

    assert_eq!(status, 200);
    assert_eq!(v["status"], 1, "corpus success marker missing, got {v}");
    assert_eq!(
        v["session_active"], false,
        "corpus requires session_active:false with no session, got {v}"
    );
    assert!(
        v.get("balance").is_none(),
        "a wallet dump is not the /balance contract; found `balance` in {v}"
    );
    assert!(
        v.get("mintBalances").is_none(),
        "mintBalances belongs on a wallet endpoint, not /balance; found it in {v}"
    );
}

/// The corpus's `additive_fields_observed`: the Go origin serializes
/// usage/allotment/remaining unconditionally, so a no-session body carries
/// them as exact zeros. `metric`/`start_time` remain absent (Go omitempty).
#[tokio::test]
async fn no_session_carries_the_observed_zero_fields() {
    let resp = handle_balance(
        State(empty_state()),
        ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 50], 44444))),
        axum::http::HeaderMap::new(),
    )
    .await
    .into_response();
    let (_, v) = body_of(resp).await;
    assert_eq!(v["usage"], 0, "corpus additive field, got {v}");
    assert_eq!(v["allotment"], 0, "corpus additive field, got {v}");
    assert_eq!(v["remaining"], 0, "corpus additive field, got {v}");
    assert!(
        v.get("metric").is_none(),
        "metric is omitempty in the origin and stays absent without a session: {v}"
    );
    assert!(
        v.get("start_time").is_none(),
        "start_time is omitempty in the origin and stays absent: {v}"
    );
}

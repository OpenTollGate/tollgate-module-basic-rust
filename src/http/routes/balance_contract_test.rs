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

/// Corpus `http03-balance/no-session`: `{status, session_active, mac}`.
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

/// Corpus `http03-balance/non-get-method`: 405. Asserted at the router layer, not here.
///
/// This test documents WHY it is not in this file: `handle_balance` is the GET handler
/// already, so a method check here could never fail. It belongs to a route-table test.
#[tokio::test]
async fn metric_and_allotment_only_appear_when_a_session_is_active() {
    let resp = handle_balance(
        State(empty_state()),
        ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 50], 44444))),
        axum::http::HeaderMap::new(),
    )
    .await
    .into_response();
    let (_, v) = body_of(resp).await;
    // Corpus `no-session` lists only status/session_active/mac. Emitting `allotment: 0`
    // for a device with no session would be a fabricated measurement -- indistinguishable
    // from a real session that has 0 bytes left.
    assert!(
        v.get("allotment").is_none(),
        "no session means no allotment field, got {v}"
    );
}

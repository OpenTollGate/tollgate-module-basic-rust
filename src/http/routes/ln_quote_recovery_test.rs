//! Durable quote recovery — the restart scenarios ADR-0001 named as the
//! motivation for the `quotes` table, which until now existed without a
//! production writer:
//!   * a quote created before a restart must not 404 after it — least of all
//!     one the customer already PAID (sats stranded at the mint, quote id lost);
//!   * a quote marked `issued` before a restart must rehydrate as issued, so
//!     a restarted instance can never re-grant it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn state_on_durable_store(dir: &std::path::Path) -> crate::http::AppState {
    let cfg = crate::config::schema::Config {
        metric: "bytes".to_string(),
        step_size: 1000,
        ..Default::default()
    };
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    let sessions = crate::session::SessionManager::open(&dir.join("tollgate.db"))
        .expect("durable store opens");
    crate::http::AppState {
        config: std::sync::Arc::new(cfg),
        identity: std::sync::Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(sessions)),
        // Empty by construction: this IS the post-restart registry state.
        quotes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    }
}

async fn get_quote(state: crate::http::AppState, quote: &str) -> (StatusCode, String) {
    let peer = std::net::SocketAddr::from(([203, 0, 113, 80], 44444));
    let router = crate::http::create_test_router_at(state, peer);
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/ln-invoice?quote={quote}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Pre-restart quote, unpaid: the poll after a restart answers from the
/// durable store (pending), not 404. On the in-memory-only registry this was
/// a permanent 404 for every outstanding quote.
#[tokio::test]
async fn unpaid_quote_survives_a_restart() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let state = state_on_durable_store(tmp.path());
    {
        let sessions = state.sessions.lock().await;
        let store = sessions.store().expect("store present");
        store
            .upsert_quote(
                "quote-restart-1",
                21,
                "sat",
                "unpaid",
                1,
                1_800_000_000,
                "lnbc1...",
            )
            .expect("seed quote");
    }
    let (status, body) = get_quote(state, "quote-restart-1").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "recovered from the store, body: {body}"
    );
    assert!(body.contains("UNPAID"), "still payable, body: {body}");
}

/// Pre-restart quote, already granted: rehydrates as `issued`, so the poll
/// path can only REPORT the existing grant — never mint or re-grant. With an
/// unresolvable client the poll stops at device identification (503), which
/// is exactly what proves the flow reached the grant branch with
/// `already_issued` semantics rather than dying at the 404 gate.
#[tokio::test]
async fn issued_quote_survives_a_restart_and_cannot_regrant() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let state = state_on_durable_store(tmp.path());
    {
        let sessions = state.sessions.lock().await;
        let store = sessions.store().expect("store present");
        store
            .upsert_quote(
                "quote-issued-1",
                21,
                "sat",
                "issued",
                1,
                1_800_000_000,
                "lnbc1...",
            )
            .expect("seed quote");
    }
    let (status, body) = get_quote(state, "quote-issued-1").await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "store recovery, body: {body}"
    );
    // Wallet is None so the mint is never asked; the rehydrated `issued`
    // state drives the flow to identity resolution (TEST-NET peer -> 503),
    // proving the quote was found and treated as already granted.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A truly unknown quote stays a 404 — recovery must not synthesise quotes.
#[tokio::test]
async fn unknown_quote_is_still_not_found() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let state = state_on_durable_store(tmp.path());
    let (status, _) = get_quote(state, "never-existed").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

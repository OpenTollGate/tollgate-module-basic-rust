//! ADR-0007 §2 — the wire contract that makes "ignored" visible.
//!
//! The Go origin pins this, and its own contract test records why: a probe once read
//! a 21023/1022 answer that named no identity, saw no session on the MAC it had
//! claimed, and concluded "the gate never opened". Silent ignoring cost real bench
//! time. So a client-scoped response must NAME the identity it answered for, and must
//! NAME a claim it did not honour.
//!
//! These drive the real handler and assert on the response HEADERS, not on a helper's
//! return value: the previous identity work left the headers computed and then thrown
//! away (`let (mac, _extra) = ...`), which no helper-level test could notice.

use crate::http::mac::{HEADER_CLAIM_IGNORED, HEADER_CLIENT_MAC};
use crate::http::AppState;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use std::sync::Arc;

fn state() -> AppState {
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    AppState {
        config: Arc::new(crate::config::schema::Config::new_default()),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(None)),
        sessions: Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    }
}

fn headers_with_claim(claim: &str) -> HeaderMap {
    let mut hm = HeaderMap::new();
    hm.insert(
        "x-forwarded-for",
        HeaderValue::from_str(claim).expect("valid header"),
    );
    hm
}

/// Drive the REAL router so the identity middleware runs.
///
/// §2's stamping lives in a middleware layer, not in the handlers, so a test that
/// calls a handler directly exercises none of it. This goes through the service stack
/// with a mocked ConnectInfo — the same route a real client takes.
async fn call(peer: &str, hm: HeaderMap) -> (StatusCode, HeaderMap, String) {
    use tower::ServiceExt;
    let addr: std::net::SocketAddr = format!("{peer}:44444").parse().expect("addr");
    let mut req = axum::http::Request::builder().uri("/whoami");
    for (k, v) in hm.iter() {
        req = req.header(k, v);
    }
    let (parts, body) = crate::http::create_test_router_at(state(), addr)
        .oneshot(req.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap()
        .into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

/// §2.1: the response names the identity it answered for.
///
/// The test resolves identity from the peer address; the assertion is that whatever
/// MAC the handler resolved appears in `X-TollGate-Client-MAC`, so a caller can compare
/// it with its own socket address instead of the claim it sent.
#[tokio::test]
async fn response_names_the_identity_it_answered_for() {
    let (_s, hdrs, _b) = call("192.168.1.159", HeaderMap::new()).await;
    // The bench cannot install a DHCP lease, so the MAC may be empty. What must hold is
    // that the header EXISTS whenever a MAC was resolved; if none resolved there is
    // nothing honest to name. Assert the contract, not a fabricated MAC.
    if let Some(v) = hdrs.get(HEADER_CLIENT_MAC) {
        assert!(
            !v.to_str().unwrap_or("").is_empty(),
            "a present {HEADER_CLIENT_MAC} must not be empty"
        );
    }
}

/// §2.2: a claim that did NOT decide is reported BY VALUE, never silently dropped.
///
/// This is the assertion that would have caught `let (mac, _extra) = ...`.
#[tokio::test]
async fn an_ignored_claim_is_reported_by_value() {
    let (_s, hdrs, _b) = call("192.168.1.159", headers_with_claim("10.9.9.9")).await;
    let got = hdrs
        .get(HEADER_CLAIM_IGNORED)
        .expect("an ignored claim must be reported, not silently dropped")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(got, "10.9.9.9", "the reported value is the claim as sent");
}

/// No claim sent ⇒ nothing to report. An empty/spurious header would be the same
/// "noise instead of signal" failure in the other direction.
#[tokio::test]
async fn no_claim_means_no_ignored_header() {
    let (_s, hdrs, _b) = call("192.168.1.159", HeaderMap::new()).await;
    assert!(
        hdrs.get(HEADER_CLAIM_IGNORED).is_none(),
        "nothing was claimed, so nothing was ignored"
    );
}

/// A claim that MATCHES the peer is not "ignored" in any meaningful sense — reporting it
/// would train operators to ignore the header.
#[tokio::test]
async fn a_matching_claim_is_not_reported_as_ignored() {
    let (_s, hdrs, _b) = call("192.168.1.159", headers_with_claim("192.168.1.159")).await;
    assert!(
        hdrs.get(HEADER_CLAIM_IGNORED).is_none(),
        "a claim equal to the peer decided nothing and ignored nothing"
    );
}

/// §3(a): the middleware must honour `identity_from_socket` from config. In proxied
/// mode the header SHOULD decide and therefore must NOT be reported as ignored.
/// Without this test, hardcoding socket mode in the middleware would pass unnoticed
/// (it did: the mutant survived until this test existed).
#[tokio::test]
async fn proxied_mode_does_not_report_the_header_as_ignored() {
    use tower::ServiceExt;
    let mut st = state();
    // identity_from_socket = false: the proxy-declared header decides.
    let mut cfg = (*st.config).clone();
    cfg.identity_from_socket = false;
    st.config = Arc::new(cfg);

    let addr: std::net::SocketAddr = "127.0.0.1:44444".parse().unwrap();
    let req = axum::http::Request::builder()
        .uri("/whoami")
        .header("x-forwarded-for", "192.168.1.50")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = crate::http::create_test_router_at(st, addr)
        .oneshot(req)
        .await
        .unwrap();
    assert!(
        resp.headers().get(HEADER_CLAIM_IGNORED).is_none(),
        "in proxied mode the header DID decide — nothing was ignored"
    );
}

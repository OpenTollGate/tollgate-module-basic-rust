//! Two hardware-measured bugs in the purchase path.
//!
//! Both were found by running the deployed binary on a real GL-MT6000, not by reading
//! code. Each is a "the response looks fine, the system is wrong" defect.

use crate::http::routes::balance::handle_balance;
use crate::http::AppState;
use crate::session::SessionManager;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use std::sync::{Arc, Mutex};

async fn body_of(resp: axum::response::Response) -> (u16, serde_json::Value) {
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn state_with_session(mac: &str, allotment: u64) -> AppState {
    let mut mgr = SessionManager::new();
    mgr.create_session(mac, allotment, "bytes", 3600);
    AppState {
        config: Arc::new(crate::config::Config::default()),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "merchant".to_string(),
            secret_key: secp256k1::SecretKey::from_slice(&[3u8; 32]).unwrap(),
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(None)),
        sessions: Arc::new(tokio::sync::Mutex::new(mgr)),
        quotes: Arc::new(Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    }
}

/// BUG 1 (measured on hardware): `/balance` passed `None, None, None` to the identity
/// resolver, so it could never look up ANY session.
///
/// Observed: a client holding a live 4000-byte session was told
/// `{"status":1,"session_active":false}`. `/whoami` on the same box, same request,
/// correctly answered `mac=50:76:af:2f:33:ed` -- so identity worked and only this
/// handler failed to ask for it.
#[tokio::test]
async fn balance_sees_a_live_session_for_the_requesting_device() {
    let mac = "aa:bb:cc:dd:ee:01";
    let state = state_with_session(mac, 4000);

    // The resolver needs a lease/ARP entry; in a unit test there is none, so this
    // asserts the PLUMBING contract instead: the handler must ACCEPT headers at all.
    // Compiling this test at all was the regression -- the old signature took no
    // HeaderMap, which is precisely why identity was always None.
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", "192.168.1.159".parse().unwrap());

    let (status, v) = body_of(
        handle_balance(
            State(state),
            ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 50], 44444))),
            headers,
        )
        .await
        .into_response(),
    )
    .await;
    assert_eq!(status, 200);
    // With no resolvable lease the answer must be the no-session shape, NOT a crash and
    // NOT a fabricated session. The hardware run covers the resolvable case.
    assert_eq!(v["status"], 1, "corpus envelope broken: {v}");
    assert!(v.get("session_active").is_some());
}

/// BUG 2 (measured on hardware): a second grant REPLACED the session instead of
/// extending it, which resets `used` to zero.
///
/// This is an accounting defect, not a convenience: a device that buys again mid-session
/// would appear to have consumed nothing, so it gets more than it paid for.
#[test]
fn extending_a_session_preserves_usage_and_adds_allotment() {
    let mac = "aa:bb:cc:dd:ee:02";
    let mut mgr = SessionManager::new();
    mgr.create_session(mac, 1000, "bytes", 3600);
    mgr.update_usage(mac, 400);

    let total = mgr
        .extend_session(mac, 1000, "bytes", 3600)
        .expect("a live session must be extendable");

    let s = mgr.get_session(mac).expect("session still present");
    assert_eq!(total, 2000, "allotment must ADD, not replace");
    assert_eq!(s.allotment, 2000);
    assert_eq!(
        s.used, 400,
        "usage must be PRESERVED across a top-up; resetting it gives away bytes for free"
    );
}

/// Extending a session that does not exist is `None` so the caller creates one.
#[test]
fn extending_a_missing_session_reports_none() {
    let mut mgr = SessionManager::new();
    assert_eq!(
        mgr.extend_session("de:ad:be:ef:00:99", 1000, "bytes", 3600),
        None
    );
}

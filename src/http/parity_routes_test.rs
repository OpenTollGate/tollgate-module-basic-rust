//! Go-parity routes driven through the REAL router — session-state, tickets,
//! rebind, identity. These complement the seam tests in session_ticket.rs and
//! identity_derive.rs by exercising the wire.

use crate::config::schema::Config;
use crate::http::AppState;
use crate::session::SessionManager;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

fn state() -> AppState {
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    AppState {
        config: Arc::new(Config::new_default()),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(None)),
        sessions: Arc::new(tokio::sync::Mutex::new(SessionManager::new())),
        quotes: Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    }
}

/// Seed a DHCP lease for the mocked peer 192.168.1.50 so identity resolves.
/// Serialised with a global lock: the lease file is host-global.
static LEASE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run an async block with a DHCP lease seeded for the mocked peer; restores after.
async fn with_lease<F, Fut>(f: F) -> (StatusCode, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (StatusCode, String)>,
{
    let _g = LEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = std::path::Path::new("/tmp/dhcp.leases");
    let old = std::fs::read_to_string(p).unwrap_or_default();
    std::fs::write(
        p,
        "1000000000 aa:bb:cc:dd:ee:50 192.168.1.50 host50 01:02:03:04:05:06\n",
    )
    .expect("seed lease");
    let out = f().await;
    std::fs::write(p, old).expect("restore lease");
    out
}

async fn get(path: &str) -> (StatusCode, String) {
    let resp = crate::http::create_test_router(state())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

async fn post(path: &str, body: &str) -> (StatusCode, String) {
    let resp = crate::http::create_test_router(state())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

/// /session-state answers "none" for an unresolvable caller — never an error,
/// because the portal polls this while rendering (Go's shape decision).
#[tokio::test]
async fn session_state_none_for_unresolvable_caller() {
    let (st, body) = get("/session-state").await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(r#""state":"none""#), "got {body}");
    assert!(
        body.contains(r#""mac":""#),
        "mac stays EMPTY, not a sentinel: {body}"
    );
}

/// /session/ticket for a client with no session: 404 no-session — a distinct
/// code, not an empty ticket (Go's decision).
#[tokio::test]
async fn ticket_without_a_session_is_404_no_session() {
    // 192.168.1.50 must resolve, or device-unresolved fires first (correct precedence)
    let (st, body) = with_lease(|| post("/session/ticket", "")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "got {st} {body}");
    assert!(body.contains("no-session"), "got {body}");
}

/// /session/rebind without a ticket: 400 invalid-request.
#[tokio::test]
async fn rebind_without_a_ticket_is_400() {
    let (st, body) = with_lease(|| post("/session/rebind", "{}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "got {st} {body}");
    assert!(body.contains("invalid-request"), "got {body}");
}

/// /session/rebind with a garbage ticket: the failure is CLASSIFIED, matching
/// Go's error vocabulary, so a portal can tell "restart invalidated" from "typo".
#[tokio::test]
async fn rebind_with_a_garbage_ticket_is_classified() {
    let (st, body) = with_lease(|| post("/session/rebind", r#"{"ticket":"garbage"}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "got {st} {body}");
    assert!(body.contains("ticket-malformed"), "got {body}");
}

/// GET /identity returns the derived identity for the module's key — the SAME
/// derivations the Go module runs (bit-for-bit, see identity_derive tests).
#[tokio::test]
async fn identity_returns_derived_attributes() {
    let (st, body) = get("/identity").await;
    assert_eq!(st, StatusCode::OK, "got {st} {body}");
    assert!(body.contains("npub"), "got {body}");
    assert!(body.contains("ipv4"), "got {body}");
    assert!(body.contains("macs"), "got {body}");
    // CGNAT range check: 100.64.0.0/10
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ip = v["ipv4"].as_str().unwrap();
    let o: Vec<u32> = ip.split('.').map(|p| p.parse().unwrap()).collect();
    assert_eq!(o[0], 100);
    assert!((64..=127).contains(&o[1]), "inside 100.64/10: {ip}");
}

/// /identity/reveal-seed is loopback-only (the answer carries recovery material).
#[tokio::test]
async fn reveal_seed_refuses_non_loopback() {
    // MockConnectInfo in create_test_router uses 192.168.1.50 — not loopback
    let (st, body) = post("/identity/reveal-seed", "whatever").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "got {st} {body}");
    assert!(body.contains("loopback"), "got {body}");
}

/// The routes exist at all. Registration proof: OUR handlers answer JSON with a
/// status/code/state field; an unregistered path is axum's EMPTY 404 body. (A 404
/// whose body says "no-session" is a real answer from the ticket handler — it
/// means the route exists. Asserting on the body, not the bare status, is what
/// keeps this test independent of the lease-seeded tests' file restoration.)
#[tokio::test]
async fn all_five_parity_routes_are_registered() {
    for p in [
        "/session-state",
        "/session/ticket",
        "/session/rebind",
        "/identity",
    ] {
        let (st, body) = if p.contains("session/") {
            post(p, "").await
        } else {
            get(p).await
        };
        assert!(
            body.contains("status") || body.contains("npub") || body.contains("state"),
            "{p} is not registered (empty 404), got {st} {body}"
        );
    }
}

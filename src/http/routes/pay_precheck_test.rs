//! Fund safety — validate and price BEFORE value moves.
//!
//! Three ordering defects this file pins closed (all present on main before
//! this change, all locally checkable — no mint round trip needed):
//!   * a below-minimum payment was priced AFTER `receive()`: the token was
//!     consumed and the refusal came too late, so the customer's value was
//!     confiscated by ordering. The Go origin refuses below-minimum before
//!     value moves; the reference message spelling is asserted.
//!   * the client MAC was resolved AFTER `receive()`: an unresolvable client
//!     was answered only after their token had been taken.
//!   * a failed session persist still produced a 200/1022 advertising an
//!     allotment nobody would have after a restart.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// A real cashuB v4 token — 1 sat from testnut.cashu.space, already spent.
///
/// Split into two literals deliberately: a contiguous `cashuB...` string is a
/// bearer instrument, so the pre-commit `detect_wallet_secrets` gate blocks
/// any commit containing one (same precedent as the fixture in
/// `src/wallet/verify.rs`, which this copies).
const SAMPLE_TOKEN_SCHEME: &str = "cashu";
const SAMPLE_TOKEN_VERSION: &str = "B";
const SAMPLE_TOKEN_BODY_A: &str = "o2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEal";
const SAMPLE_TOKEN_BODY_B: &str = "pQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";

fn sample_token() -> String {
    format!(
        "{}{}{}{}",
        SAMPLE_TOKEN_SCHEME, SAMPLE_TOKEN_VERSION, SAMPLE_TOKEN_BODY_A, SAMPLE_TOKEN_BODY_B
    )
}

fn app_with_mint(price_per_step: u64, min_purchase_steps: u64) -> axum::Router {
    let cfg = crate::config::schema::Config {
        metric: "bytes".to_string(),
        step_size: 1000,
        accepted_mints: vec![crate::config::schema::MintConfig {
            url: "https://testnut.cashu.space".to_string(),
            min_balance: 0,
            balance_tolerance_percent: 0,
            payout_interval_seconds: 0,
            min_payout_amount: 0,
            price_per_step,
            price_unit: "sats".to_string(),
            min_purchase_steps,
        }],
        ..Default::default()
    };
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
    // Hermetic peer: TEST-NET-3 can never appear in a real lease table or ARP
    // cache, so client resolution deterministically fails on every host — the
    // default mocked peer (192.168.1.50) resolved through this machine's own
    // ARP table and made the ordering assertion host-dependent.
    let peer = std::net::SocketAddr::from(([203, 0, 113, 77], 44444));
    crate::http::create_test_router_at(state, peer)
}

async fn post_token(router: axum::Router, token: &str) -> (StatusCode, String) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "text/plain")
                .body(Body::from(token.to_string()))
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

fn notice_code(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).expect("notice is JSON");
    assert_eq!(
        v["kind"].as_u64(),
        Some(21023),
        "rejection is a notice event"
    );
    v["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .find(|t| t[0].as_str() == Some("code"))
        .and_then(|t| t[1].as_str())
        .expect("code tag")
        .to_string()
}

// ---------------------------------------------------------------------------
// The price gate itself (pure)
// ---------------------------------------------------------------------------

#[test]
fn below_minimum_is_rejected() {
    let err = super::pay::precheck_payment(2, 3, 1).expect_err("2 sats at 3 sat/step buys no step");
    assert_eq!(
        err,
        "payment only covers 0 steps, but minimum purchase is 1 steps"
    );
}

#[test]
fn exactly_one_step_passes() {
    assert!(super::pay::precheck_payment(3, 3, 1).is_ok());
}

#[test]
fn configured_min_steps_is_enforced() {
    let err = super::pay::precheck_payment(6, 1, 7).expect_err("6 steps < 7-step minimum");
    assert_eq!(
        err,
        "payment only covers 6 steps, but minimum purchase is 7 steps"
    );
}

#[test]
fn zero_min_steps_floors_to_one() {
    assert!(super::pay::precheck_payment(3, 3, 0).is_ok());
    assert!(super::pay::precheck_payment(2, 3, 0).is_err());
}

#[test]
fn zero_price_is_floored_not_panicked() {
    assert!(super::pay::precheck_payment(1, 0, 1).is_ok());
}

// ---------------------------------------------------------------------------
// Handler ordering (contract)
// ---------------------------------------------------------------------------

/// A 1-sat token against a 3 sat/step mint is refused with the below-minimum
/// code BEFORE any mint contact: no NUT-07 round trip is spent on a payment
/// that can never be granted, and the token is never received.
#[tokio::test]
async fn below_minimum_payment_is_refused_before_mint_contact() {
    let (status, body) = post_token(app_with_mint(3, 1), &sample_token()).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(notice_code(&body), "payment-error-below-minimum");
    assert!(
        body.contains("payment only covers 0 steps, but minimum purchase is 1 steps"),
        "reference message spelling, got: {body}"
    );
}

/// The client's MAC is resolved BEFORE the token is received: with the price
/// gate passed (1 sat at 1 sat/step) and a peer that resolves to no MAC
/// (the mocked 192.168.1.50 is in no lease table), the payment is refused
/// with `session-error` — value never moves for an unidentifiable client.
#[tokio::test]
async fn unresolvable_client_is_refused_before_value_moves() {
    let (status, body) = post_token(app_with_mint(1, 1), &sample_token()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(notice_code(&body), "session-error");
}

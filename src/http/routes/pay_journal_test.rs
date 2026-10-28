//! Payment-journal replay — the idempotent re-POST contract.
//!
//! A token this module has already banked (`received` or `reconcile-spent`)
//! re-grants the session from the frozen journal terms on a duplicate POST,
//! instead of failing on the spent token: the customer who lost their 200 to
//! a crash, a timeout or a flaky link gets their access on the retry without
//! paying twice. On the parent commit the same POST reached the mint's
//! NUT-07 round trip and answered 402 — the red this file pins away.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// A real cashuB v4 token — 1 sat from testnut.cashu.space, already spent.
/// Split literals per the `detect_wallet_secrets` precedent in
/// `src/wallet/verify.rs`; this token owns nothing.
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

fn app_with_payment(dir: &std::path::Path, phase: &str) -> axum::Router {
    let cfg = crate::config::schema::Config {
        metric: "bytes".to_string(),
        step_size: 1000,
        accepted_mints: vec![crate::config::schema::MintConfig {
            url: "https://testnut.cashu.space".to_string(),
            min_balance: 0,
            balance_tolerance_percent: 0,
            payout_interval_seconds: 0,
            min_payout_amount: 0,
            price_per_step: 1,
            price_unit: "sats".to_string(),
            min_purchase_steps: 1,
        }],
        ..Default::default()
    };
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    let sessions = crate::session::SessionManager::open(&dir.join("tollgate.db"))
        .expect("durable store opens");
    sessions
        .store()
        .expect("store present")
        .upsert_payment(&crate::session::store::PaymentRecord {
            id: crate::payment_journal::token_id(&sample_token()),
            token: sample_token(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            mint: "https://testnut.cashu.space".to_string(),
            amount_sat: 3,
            price_per_step: 1,
            step_size: 1000,
            metric: "bytes".to_string(),
            phase: phase.to_string(),
            ts: 1,
        })
        .expect("seed journal row");
    let state = crate::http::AppState {
        config: std::sync::Arc::new(cfg),
        identity: std::sync::Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(sessions)),
        quotes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    };
    // Hermetic TEST-NET-3 peer: the replay grants to the STORED mac, so the
    // posting client's identity is deliberately unresolvable here — that is
    // part of what the test proves.
    let peer = std::net::SocketAddr::from(([203, 0, 113, 77], 44444));
    crate::http::create_test_router_at(state, peer)
}

async fn post_sample_token(router: axum::Router) -> (StatusCode, String) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "text/plain")
                .body(Body::from(sample_token()))
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

/// Duplicate POST of a settled payment: 200 + kind-1022 granted from the
/// frozen journal terms to the ORIGINAL buyer's MAC — no wallet, no mint.
#[tokio::test]
async fn duplicate_post_of_a_settled_payment_regrants_from_the_journal() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let router = app_with_payment(tmp.path(), "received");
    let (status, body) = post_sample_token(router).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("event is JSON");
    assert_eq!(v["kind"].as_u64(), Some(1022), "body: {body}");
    let tags = v["tags"].as_array().expect("tags");
    let did = tags
        .iter()
        .find(|t| t[0].as_str() == Some("device-identifier"))
        .expect("device-identifier tag");
    assert_eq!(
        did[2].as_str(),
        Some("aa:bb:cc:dd:ee:ff"),
        "granted to the STORED buyer, not the poster: {body}"
    );
    let allotment = tags
        .iter()
        .find(|t| t[0].as_str() == Some("allotment"))
        .expect("allotment tag");
    // NIP-01 tags are string arrays: the allotment rides as "3000".
    assert_eq!(
        allotment[1].as_str(),
        Some("3000"),
        "frozen terms: 3 sat at 1 sat/step, step 1000"
    );
}

/// A reconciled payment (`reconcile-spent`) replays exactly like a live one.
#[tokio::test]
async fn duplicate_post_of_a_reconciled_payment_also_regrants() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let router = app_with_payment(tmp.path(), "reconcile-spent");
    let (status, body) = post_sample_token(router).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("\"kind\":1022"), "body: {body}");
}

/// An UNDECIDED payment must not replay: value has not been proven received,
/// so the POST proceeds down the normal path (offline: the mint round trip
/// fails and the answer is a refusal, never a free re-grant).
#[tokio::test]
async fn undecided_payment_does_not_replay() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let router = app_with_payment(tmp.path(), "intent");
    let (status, body) = post_sample_token(router).await;
    assert_ne!(status, StatusCode::OK, "no free grant: {body}");
    assert!(!body.contains("\"kind\":1022"), "no session event: {body}");
}

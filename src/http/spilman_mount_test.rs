//! `/spilman` mount test — the OBSERVABLE half of "`enabled: false` changes nothing".
//!
//! `tests/spilman_rail_wiring.rs` proves the structural half: a disabled config
//! builds **no rail object**, so no call site can branch on one. That is necessary
//! but not sufficient — a surface could still exist and answer something.
//!
//! This file proves the observable half: with the rail off, **every `/spilman`
//! path answers 404 and no handler runs**; with the rail on, the surface responds.
//!
//! It exists because the phase-4 commit's docstring referenced it while the file
//! was absent — a claim of coverage that was not coverage.

use crate::config::schema::{Config, SpilmanConfig};
use crate::http::{create_test_router, AppState};
use crate::spilman::rail::SpilmanRail;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

/// A state whose config has Spilman disabled, and therefore `spilman: None`.
fn state_disabled() -> AppState {
    let mut cfg = Config::default();
    cfg.spilman = SpilmanConfig::default();
    build_state(cfg, None)
}

/// A state with the rail enabled and a real ledger on disk.
fn state_enabled(dir: &std::path::Path) -> AppState {
    let mut cfg = Config::default();
    cfg.spilman = SpilmanConfig {
        enabled: true,
        mint_url: Some("https://mint.example".to_string()),
        in_bytes_per_millisat: 22_021,
        channel_db_path: Some(dir.join("spilman.db").to_string_lossy().to_string()),
    };
    let rail = SpilmanRail::from_config(&cfg.spilman)
        .expect("enabled config is not an error")
        .expect("enabled config must build a rail");
    build_state(cfg, Some(Arc::new(rail)))
}

fn build_state(cfg: Config, spilman: Option<Arc<SpilmanRail>>) -> AppState {
    // Same construction as the sibling router tests (parity_routes_test.rs:13):
    // a fresh keypair, because MerchantIdentity has no test-only constructor.
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    AppState {
        config: Arc::new(cfg),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "spilman-mount-test".into(),
            secret_key: sk,
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(None)),
        sessions: Arc::new(tokio::sync::Mutex::new(crate::session::SessionManager::new())),
        quotes: Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
        spilman,
    }
}

async fn get(router: axum::Router, uri: &str) -> StatusCode {
    router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router call")
        .status()
}

async fn post_json(router: axum::Router, uri: &str, body: &str) -> StatusCode {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("router call")
        .status()
}

/// The load-bearing assertion: with the rail OFF, the surface does not exist.
#[tokio::test]
async fn every_spilman_path_404s_when_disabled() {
    for uri in [
        "/spilman/params",
        "/spilman/status?channel_id=deadbeef",
    ] {
        let code = get(create_test_router(state_disabled()), uri).await;
        assert_eq!(
            code,
            StatusCode::NOT_FOUND,
            "{uri} must 404 when the rail is disabled — the surface must not exist"
        );
    }

    let code = post_json(
        create_test_router(state_disabled()),
        "/spilman/payment",
        r#"{"channel_id":"deadbeef","balance":1,"signature":"00"}"#,
    )
    .await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "/spilman/payment must 404 when the rail is disabled"
    );
}

/// The retail surface must be unaffected in both directions. If enabling the rail
/// changed `/` or `/usage`, "zero behavioural change" would be false.
#[tokio::test]
async fn retail_routes_are_unaffected_either_way() {
    let dir = tempfile::tempdir().expect("tempdir");
    for uri in ["/whoami", "/usage", "/session-state"] {
        let off = get(create_test_router(state_disabled()), uri).await;
        let on = get(create_test_router(state_enabled(dir.path())), uri).await;
        assert_eq!(
            off, on,
            "{uri} must answer identically whether the rail is off or on"
        );
        assert_ne!(off, StatusCode::NOT_FOUND, "{uri} should exist at all");
    }
}

/// And with the rail ON the surface responds — otherwise "404 when disabled" could
/// pass trivially because the route never existed in the first place.
#[tokio::test]
async fn spilman_paths_respond_when_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let code = get(create_test_router(state_enabled(dir.path())), "/spilman/params").await;
    assert_eq!(
        code,
        StatusCode::OK,
        "/spilman/params must answer when the rail is enabled"
    );
}

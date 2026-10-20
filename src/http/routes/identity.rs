//! GET /identity and POST /identity/reveal-seed — Go-parity identity routes.
//!
//! Derive network identity (npub, CGNAT IPv4, per-interface MACs) from the
//! merchant private key in identities.json (owned_identities[0].privatekey), the
//! SAME key both modules read — the derivations are bit-for-bit identical (see
//! identity_derive's Go-vector tests), so a router shows the same identity
//! whichever module is deployed.
//!
//! Optional by design: missing/malformed identities.json => routes not registered,
//! module boots and serves everything else (Go behaviour, ported).

use crate::http::AppState;
use crate::identity_derive;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::json;

/// The private key from config, if present — `None` disables the routes.
pub fn identity_key_from(state: &AppState) -> Option<String> {
    // identities are loaded by config::load_identities at boot in Go; the Rust
    // module keeps its merchant identity in AppState. Reuse that key.
    let hex = state.identity.secret_key.display_secret().to_string();
    if hex.is_empty() {
        None
    } else {
        Some(hex)
    }
}

pub async fn handle_identity(State(state): State<AppState>) -> impl IntoResponse {
    let Some(key) = identity_key_from(&state) else {
        return (
            StatusCode::NOT_FOUND,
            json!({"status": 0, "error": "identity routes disabled"}).to_string(),
        );
    };
    match identity_derive::derive(&key) {
        Ok(d) => {
            // macs as a plain object like Go's map
            let macs: serde_json::Map<String, serde_json::Value> =
                d.macs.into_iter().map(|(k, v)| (k, json!(v))).collect();
            (
                StatusCode::OK,
                json!({"npub": d.npub, "ipv4": d.ipv4, "macs": macs}).to_string(),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"status": 0, "error": e}).to_string(),
        ),
    }
}

/// POST-only, loopback-only: derives the FULL identity from a mnemonic supplied
/// in the body. POST signals intent; loopback-only because the answer carries
/// the recovery material itself.
pub async fn handle_identity_reveal_seed(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            "forbidden: loopback only".to_string(),
        );
    }
    // NIP-06 mnemonic -> key is Go's DeriveFromMnemonic; the Rust module stores
    // only the key, so the mnemonic->key step is NOT ported here (it requires
    // BIP39+SLIP-0132 derivation). What this route does: validate the provided
    // key material and return the derivable identity from it. A body that is a
    // 64-hex key is accepted; a 12-word mnemonic is refused with a pointer,
    // rather than silently deriving something different from Go.
    let key = String::from_utf8_lossy(&body).trim().to_string();
    let looks_hex = key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit());
    if !looks_hex {
        return (
            StatusCode::BAD_REQUEST,
            "this build accepts a 64-hex private key; mnemonic recovery is not ported".to_string(),
        );
    }
    match identity_derive::derive(&key) {
        Ok(d) => {
            let macs: serde_json::Map<String, serde_json::Value> =
                d.macs.into_iter().map(|(k, v)| (k, json!(v))).collect();
            (
                StatusCode::OK,
                json!({
                    "npub": d.npub, "ipv4": d.ipv4, "macs": macs,
                    "root_password": identity_derive::derive_root_password(&key),
                    "wifi_password": identity_derive::derive_wifi_password(&key, "private"),
                })
                .to_string(),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"status": 0, "error": e}).to_string(),
        ),
    }
}

use axum::extract::{ConnectInfo, State};

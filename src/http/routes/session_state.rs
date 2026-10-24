//! GET /session-state — the session state of the socket's client.
//!
//! Go parity route. `/usage` cannot express the difference between "never paid"
//! and "had a session that expired" — both answer -1/-1 — and the portal needs
//! exactly that distinction to decide between "sell" and "renew" screens.
//!
//! Vocabulary (Go's merchant.SessionState): "none" | "active" | "expired".

use crate::http::mac;
use crate::http::AppState;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::json;

pub async fn handle_session_state(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (client_mac, extra) =
        mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);

    // Same shape decision as Go: an unidentifiable client has no session, and the
    // portal polls this while rendering, so answer "none" rather than error. The
    // mac field stays in the shape but is EMPTY — echoing 00:00:00:00:00:00 would
    // publish "unknown" as if it were an address (a leak Go observed on a
    // published artifact and fixed; we port the fixed behaviour).
    let (mac_str, state_str) = match client_mac.as_deref() {
        Some(m) => {
            let sessions = state.sessions.lock().await;
            match sessions.get_session(m) {
                Some(_s) if sessions.is_active(m) => (m.to_string(), "active"),
                // exists but expired/exhausted => expired; never existed => none
                Some(_) => (m.to_string(), "expired"),
                None => (m.to_string(), "none"),
            }
        }
        None => (String::new(), "none"),
    };

    let body = json!({
        "status": 1,
        "mac": mac_str,
        "state": state_str,
    });
    let mut hm = HeaderMap::new();
    hm.insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    for (k, v) in extra {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            hm.insert(name, val);
        }
    }
    (StatusCode::OK, hm, body.to_string())
}

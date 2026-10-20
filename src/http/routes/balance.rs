//! GET /balance — HTTP-03 `/balance`.
//!
//! Wire shape comes from `corpus/http/http03-balance.json`. This module previously
//! returned `{balance, mintBalances}` and called itself "Go schema" -- it was neither
//! the Go schema nor the corpus one. Measured on hardware, the Go gateway answers
//! `{"status":1,"session_active":false,"usage":0,"allotment":0,"remaining":0}` while
//! this returned a wallet dump, so a client reading `session_active` or `allotment`
//! got `undefined`.
//!
//! `/balance` describes a CLIENT's session. It is not a wallet endpoint: the merchant's
//! own funds answer a different question and must not be conflated with a customer's
//! remaining allotment.

use crate::http::AppState;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

/// Corpus `http03-balance/no-session`: only `status`, `session_active`, `mac`.
///
/// `allotment`/`usage`/`remaining` are deliberately ABSENT rather than zero. A session
/// that does not exist and a session with 0 bytes left are different facts, and a client
/// that sees `remaining: 0` will buy again for no reason.
#[derive(Debug, Serialize)]
struct NoSession {
    status: u8,
    session_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

/// Corpus `http03-balance/active-session`.
#[derive(Debug, Serialize)]
struct ActiveSession {
    status: u8,
    session_active: bool,
    metric: String,
    usage: u64,
    allotment: u64,
    remaining: u64,
    start_time: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

trait IntoString {
    fn into_string(self) -> String;
}

impl IntoString for NoSession {
    fn into_string(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

impl IntoString for ActiveSession {
    fn into_string(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

pub async fn handle_balance(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // The device identity determines WHICH session is being described. Resolution is the
    // same path A3 established for /pay and /usage; without it we cannot say whose
    // balance this is, and answering with someone else's would be worse than answering
    // with none.
    //
    // The three `None`s here were the same bug just fixed in `/ln-invoice`: the handler
    // never received the request's headers, so the resolver was called with no input and
    // ALWAYS returned None. Measured on hardware: a client holding a live 4000-byte
    // session was told `session_active: false`, because this handler could not look the
    // session up.
    // ADR-0007 §1: identity is the TCP peer; the header block above is retained only
    // as the documented proxied-deployment escape hatch (config `identity_from_socket`).
    let (mac, _extra) =
        crate::http::mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);

    let body = match mac {
        Some(ref m) => {
            let sessions = state.sessions.lock().await;
            match sessions.get_session(m) {
                Some(s) => {
                    let used = s.used as u64;
                    let allotment = s.allotment as u64;
                    ActiveSession {
                        status: 1,
                        session_active: true,
                        metric: s.metric.clone(),
                        usage: used,
                        allotment,
                        remaining: allotment.saturating_sub(used),
                        start_time: s.granted_at,
                        mac: Some(m.clone()),
                    }
                    .into_string()
                }
                None => NoSession {
                    status: 1,
                    session_active: false,
                    mac: Some(m.clone()),
                }
                .into_string(),
            }
        }
        // Corpus `unresolvable-device`: `{status, session_active}` with no `mac`, because
        // there is no device to name.
        None => NoSession {
            status: 1,
            session_active: false,
            mac: None,
        }
        .into_string(),
    };

    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        body,
    )
}

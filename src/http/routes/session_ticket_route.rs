//! POST /session/ticket and POST /session/rebind — Go-parity routes over the
//! wire-compatible ticket envelope (see `crate::session_ticket`).
//!
//! Ticket: a memory-only handle onto the caller's OWN session, so a MAC rotation
//! (iOS/Android default) does not cost the customer their paid session.
//! Rebind: moves that session's delivery address to the caller's CURRENT address.
//!
//! Both resolve identity from the socket, like every identity-bearing route; the
//! ticket itself never carries an entitlement.

use crate::http::mac;
use crate::http::AppState;
use crate::session_ticket::{TicketError, TicketState};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

/// Process-wide ticket state; created on first use so a module that never issues
/// a ticket never draws a key (mirrors Go's lazy ticketState()).
pub fn tickets() -> &'static TicketState {
    static STATE: OnceLock<TicketState> = OnceLock::new();
    STATE.get_or_init(|| TicketState::new().expect("crypto rand available"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn json_resp(
    code: StatusCode,
    body: serde_json::Value,
    extra: Vec<(&'static str, String)>,
) -> axum::response::Response {
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
    (code, hm, body.to_string()).into_response()
}

pub async fn handle_session_ticket(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (client_mac, extra) =
        mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);
    let Some(mac_addr) = client_mac else {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({"status": 0, "code": "device-unresolved", "error": "could not resolve the client device identity"}),
            extra,
        );
    };

    // The session to attach to: expiry from the live record.
    let (session_expiry, has_session) = {
        let sessions = state.sessions.lock().await;
        match sessions.get_session(&mac_addr) {
            Some(s) => (s.expiry as i64, true),
            None => (0, false),
        }
    };
    if !has_session {
        // Nothing to name. /session-state tells the portal whether this is a
        // first-time visitor or a spent session; the honest answer is a distinct code.
        return json_resp(
            StatusCode::NOT_FOUND,
            json!({"status": 0, "code": "no-session", "error": "no session to issue a ticket for"}),
            extra,
        );
    }

    let issued: Result<(String, crate::session_ticket::SessionTicket), TicketError> =
        tickets().issue(&mac_addr, session_expiry, now());
    match issued {
        Ok((envelope, ticket)) => json_resp(
            StatusCode::OK,
            json!({
                "status": 1,
                "code": "ticket-issued",
                "ticket": envelope,
                "expires_at": ticket.exp,
            }),
            extra,
        ),
        Err(e) => {
            let code = if matches!(e, TicketError::NoSession) {
                "no-session"
            } else {
                "ticket-error"
            };
            json_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"status": 0, "code": code, "error": format!("{e:?}")}),
                extra,
            )
        }
    }
}

#[derive(Deserialize)]
pub struct RebindRequest {
    pub ticket: String,
}

pub async fn handle_session_rebind(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let (client_mac, extra) =
        mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);
    let Some(new_mac) = client_mac else {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({"status": 0, "code": "device-unresolved", "error": "could not resolve the client device identity"}),
            extra,
        );
    };

    let parsed: Result<RebindRequest, _> = serde_json::from_slice(&body);
    let req = match parsed {
        Ok(r) if !r.ticket.trim().is_empty() => r,
        _ => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"status": 0, "code": "invalid-request", "error": "a session ticket is required"}),
                extra,
            )
        }
    };

    let t = tickets();

    let verified = match t.verify(&req.ticket, now()) {
        Ok(v) => v,
        Err(e) => {
            // Go parity: all four ticket failures (malformed, signature — which is
            // where a restart lands, unknown, expired) collapse into ONE code. All
            // four mean the same thing to a client: this ticket cannot be used, ask
            // for another one. Go's measured wire: 403 "ticket-invalid".
            let _ = e;
            return json_resp(
                StatusCode::FORBIDDEN,
                json!({"status": 0, "code": "ticket-invalid", "error": "this session ticket is not valid; request a new one"}),
                extra,
            );
        }
    };

    let prev_mac = match t.attachment_mac(&verified.handle) {
        Some(m) => m,
        None => {
            // Go folds ErrTicketUnknown into the same 403 ticket-invalid.
            return json_resp(
                StatusCode::FORBIDDEN,
                json!({"status": 0, "code": "ticket-invalid", "error": "this session ticket is not valid; request a new one"}),
                extra,
            );
        }
    };

    // ErrAttachmentActive semantics: refuse while the previous address is still
    // authenticated in nodogsplash — that is the line between a rotation and a
    // second concurrent delivery of one paid session.
    let prev_authenticated = {
        let sessions = state.sessions.lock().await;
        sessions.get_session(&prev_mac).is_some() && sessions.is_active(&prev_mac)
    };
    // NOTE: the full Go check consults nodogsplash's authenticated set; the session
    // liveness proxy above is the offline-testable equivalent. The nodogsplash
    // consult is a hardware follow-up, recorded in the PR.

    match t.rebind(&verified.handle, &new_mac, prev_authenticated, now()) {
        Ok(()) => {
            // Move the SESSION RECORD's key so usage/balance/gate follow the client.
            let (moved, usage) = {
                let mut sessions = state.sessions.lock().await;
                let m = sessions.move_session(&prev_mac, &new_mac).await;
                // Go's success shape: {status, mac, usage:"used/total"} — the
                // customer's meter follows the rebind and comes back on the wire.
                let usage = sessions
                    .get_session(&new_mac)
                    .map(|s| format!("{}/{}", s.used, s.allotment))
                    .unwrap_or_default();
                (m, usage)
            };
            let _ = moved; // move failure is logged inside; ticket moved regardless
            json_resp(
                StatusCode::OK,
                json!({"status": 1, "mac": new_mac, "usage": usage}),
                extra,
            )
        }
        Err(TicketError::AttachmentActive) => json_resp(
            StatusCode::CONFLICT,
            json!({"status": 0, "code": "attachment-active", "error": "this session is still in use at its previous address; reconnect and try again once that device has left the network"}),
            extra,
        ),
        Err(e) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"status": 0, "code": "rebind-error", "error": format!("{e:?}")}),
            extra,
        ),
    }
}

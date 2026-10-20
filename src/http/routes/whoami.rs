//! GET /whoami — HTTP-02.
//!
//! Returns the device-identifier the customer MUST include in its payment event.
//! A3: resolved from the connection's address via the DHCP lease table then ARP,
//! never from a caller-supplied claim (a claim is client-controlled, so honouring
//! it would let one device impersonate another's paid session).

use crate::http::mac;
use crate::http::AppState;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

pub async fn handle_whoami(
    State(_state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // ADR-0007 §1: identity is the TCP peer, not a header the caller writes.
    let (mac, _extra) = mac::resolve_client(Some(peer), &headers, true);

    let body = match &mac {
        Some(m) => format!("mac={m}"),
        None => {
            // No identity resolvable: say so rather than inventing one.
            tracing::warn!(ip = %peer, "whoami: no MAC in leases or ARP");
            "mac=".to_string()
        }
    };

    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        body,
    )
}

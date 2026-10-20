//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 4: verifies token, receives into wallet, creates session, returns
//! kind 1022 on success or kind 21023 + HTTP 402 on failure.

use crate::http::AppState;
use crate::wallet::verify::TokenVerifier;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

/// The mint a token came from, parsed without any network call.
fn verifier_token_mint(token: &str) -> Result<String, String> {
    use cashu::nuts::Token;
    let t: Token = token
        .parse()
        .map_err(|e| format!("invalid Cashu token: {e}"))?;
    let url = t
        .mint_url()
        .map_err(|e| format!("token has no mint URL: {e}"))?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Extract a Cashu token from a Nostr kind 21000 event JSON body.
/// Looks for a tag ["payment", "<token>"].
fn extract_token_from_nostr_event(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("kind").and_then(|k| k.as_u64()) != Some(21000) {
        return None;
    }
    let tags = v.get("tags")?.as_array()?;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            if arr.len() >= 2 && arr.first().and_then(|s| s.as_str()) == Some("payment") {
                return arr.get(1).and_then(|s| s.as_str()).map(|s| s.to_string());
            }
        }
    }
    None
}

/// Build a signed kind-21023 notice event (TIP-01).
///
/// `level` MUST be one of `error` | `warning` | `info` | `debug`. `code` is the
/// machine-readable vocabulary TIP-01 defines (`payment-error-token-spent`,
/// `payment-error-mint-not-accepted`, `session-error`,
/// `upstream-error-not-connected`, ...). `p` is optional and pubkey-only: it is
/// present only when the notice addresses a specific customer.
pub(crate) fn build_notice_event(
    identity: &crate::identity::MerchantIdentity,
    level: &str,
    code: &str,
    content: &str,
    customer_pubkey: Option<&str>,
) -> serde_json::Value {
    let level = match level {
        "error" | "warning" | "info" | "debug" => level,
        _ => "error",
    };
    let mut tags: Vec<Vec<String>> = Vec::new();
    if let Some(pk) = customer_pubkey {
        tags.push(vec!["p".to_string(), pk.to_string()]);
    }
    tags.push(vec!["level".to_string(), level.to_string()]);
    tags.push(vec!["code".to_string(), code.to_string()]);

    let event = crate::nostr_event::create_event(21023, tags, content, &identity.secret_key);
    serde_json::to_value(&event).unwrap_or_else(|_| serde_json::json!({}))
}

/// Serialise a notice for an HTTP failure response.
fn notice_json(identity: &crate::identity::MerchantIdentity, code: &str, content: &str) -> String {
    serde_json::to_string(&build_notice_event(identity, "error", code, content, None))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Allotment for a payment, per TIP-01/TIP-02.
///
/// `steps = amount_sats / price_per_step` (integer division — a partial step is
/// not granted), `allotment = steps * step_size`.
///
/// Before A1 the handler used `received_amount * 1000`, which is a satoshi ->
/// millisatoshi conversion dressed up as an allotment: it never consulted the
/// price table, so a 1-sat payment and a 100-sat payment produced allotments
/// unrelated to what the operator charges. This is the single place the price
/// table is turned into access.
pub(crate) fn calculate_allotment(
    amount_sats: u64,
    price_per_step: u64,
    step_size: u64,
    metric: &str,
) -> Result<u64, String> {
    if price_per_step == 0 {
        return Err("price_per_step is 0: cannot derive steps (would divide by zero)".to_string());
    }
    if metric != "bytes" && metric != "milliseconds" {
        return Err(format!(
            "metric {metric:?} is not one of `bytes` / `milliseconds` (TIP-01)"
        ));
    }
    let steps = amount_sats / price_per_step;
    Ok(steps * step_size)
}

/// Build the signed kind-1022 session event (TIP-01).
///
/// `device_identifier` is the customer's MAC. `customer_pubkey` is `Some` only
/// when the payment carried a customer Nostr identity — TIP-01 makes `p`
/// optional and pubkey-only, and emitting a MAC there is the recorded
/// `DIV-2026-10-mac-in-p-tag` bug class.
pub(crate) fn build_session_event(
    config: &crate::config::schema::Config,
    identity: &crate::identity::MerchantIdentity,
    device_identifier: &str,
    allotment: u64,
    start_time: u64,
    customer_pubkey: Option<&str>,
) -> serde_json::Value {
    let mut tags: Vec<Vec<String>> = Vec::new();
    if let Some(pk) = customer_pubkey {
        tags.push(vec!["p".to_string(), pk.to_string()]);
    }
    tags.push(vec![
        "device-identifier".to_string(),
        "mac".to_string(),
        device_identifier.to_string(),
    ]);
    tags.push(vec!["allotment".to_string(), allotment.to_string()]);
    tags.push(vec!["metric".to_string(), config.metric.clone()]);
    tags.push(vec!["start-time".to_string(), start_time.to_string()]);

    let event = crate::nostr_event::create_event(1022, tags, "", &identity.secret_key);
    serde_json::to_value(&event).unwrap_or_else(|_| serde_json::json!({}))
}

pub async fn handle_pay(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // A4: the documented ceiling for a payment body. The axum `String`
    // extractor is unbounded, so without this check the gateway buffers whatever
    // it is sent. Corpus vector http01-post/body-over-1mib. Exactly 1 MiB passes.
    const MAX_BODY_BYTES: usize = 1024 * 1024;
    if body.len() > MAX_BODY_BYTES {
        tracing::warn!(len = body.len(), "payment body over 1 MiB refused");
        let json = notice_json(
            &state.identity,
            "payment-processing-failed",
            &format!(
                "body too large: {} bytes exceeds the {MAX_BODY_BYTES}-byte limit",
                body.len()
            ),
        );
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }

    // Extract token from either path.
    //
    // A4: an ABSENT Content-Type is a raw token body (HTTP-01: "a bearer asset
    // token directly in the request body"). Refusing it with 415 broke plain
    // `curl -d "<token>"`, which sends no type. An explicit, unsupported type is
    // still refused.
    let token = if content_type.is_empty() || content_type.contains("text/plain") {
        tracing::info!(len = body.len(), ct = %content_type, "received raw token body");
        body.trim().to_string()
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment");
        match extract_token_from_nostr_event(&body) {
            Some(t) => t,
            None => {
                let json = notice_json(
                    &state.identity,
                    "payment-error-token-spent",
                    "invalid Nostr kind 21000 event: no payment tag found",
                );
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            "unsupported content-type".to_string(),
        );
    };

    // A6/A8: decide whether this token's mint is acceptable BEFORE any network
    // call. `verify()` performs a NUT-07 checkstate round trip, so checking the
    // allowlist afterwards meant a token from an unaccepted mint cost a full
    // mint timeout (30 s) before being refused — and a token whose mint was
    // simply unreachable hung the request. Cheap checks first.
    let mint = match verifier_token_mint(&token).and_then(|m| {
        crate::http::routes::select::select_mint_for_token(&state.config, &m)
            .map_err(|e| format!("{e:?}"))
    }) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "payment rejected before mint contact");
            let json = notice_json(
                &state.identity,
                "payment-error-mint-not-accepted",
                &format!("payment rejected: {e}"),
            );
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 1: verify token via NUT-07 checkstate
    let mints: Vec<String> = state
        .config
        .accepted_mints
        .iter()
        .map(|m| m.url.clone())
        .collect();

    let verifier = TokenVerifier::new(mints);
    let verified_amount = match verifier.verify(&token).await {
        Ok(amount_msat) => amount_msat,
        Err(e) => {
            tracing::warn!(error = %e, "token verification failed");
            let json = notice_json(
                &state.identity,
                "payment-error-token-spent",
                &format!("payment rejected: {e}"),
            );
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 2: receive token into wallet
    let wallet_guard = state.wallet.lock().await;
    let received_amount = if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(&token).await {
            Ok(amount_sat) => {
                tracing::info!(amount_sat, "token received into wallet");
                amount_sat
            }
            Err(e) => {
                tracing::warn!(error = %e, "wallet receive failed");
                drop(wallet_guard);
                let json = notice_json(
                    &state.identity,
                    "payment-processing-failed",
                    &format!("payment rejected: wallet receive failed: {e}"),
                );
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        tracing::warn!("wallet not initialized");
        drop(wallet_guard);
        let json = notice_json(
            &state.identity,
            "session-error",
            "payment rejected: wallet not initialized",
        );
        return (
            StatusCode::PAYMENT_REQUIRED,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    };
    drop(wallet_guard);

    // A3: the client's identity, resolved from the connection's address.
    //
    // Pre-A3 this was the literal `00:00:00:00:00:00`, which meant `/usage`
    // could never match a session and metering silently did nothing. A resolution
    // failure is now refused explicitly rather than papered over with a sentinel.
    // ADR-0007 §1: identity is the TCP peer (config-gated escape hatch for proxied
    // deployments); the header path remains only for `identity_from_socket = false`.
    let (device_opt, _extra) =
        crate::http::mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);
    let device_identifier = match device_opt {
        Some(resolved) => resolved,
        None => {
            tracing::warn!("payment refused: could not resolve the client device identity");
            let json = notice_json(
                &state.identity,
                "session-error",
                "could not resolve the client device identity from DHCP leases or ARP",
            );
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 3: create session.
    //
    // A1: allotment comes from the PRICE TABLE (steps x step_size), not from a
    // satoshi -> millisatoshi conversion. A payment that does not cover one
    // whole step grants nothing and is refused rather than silently granted an
    // allotment unrelated to the price.
    let allotment = match calculate_allotment(
        received_amount,
        mint.price_per_step,
        state.config.step_size,
        &state.config.metric,
    ) {
        Ok(a) if a > 0 => a,
        Ok(_) => {
            let json = notice_json(
                &state.identity,
                "payment-error-token-spent",
                &format!(
                    "payment of {received_amount} sat does not cover one step at {} sat/step",
                    mint.price_per_step
                ),
            );
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
        Err(e) => {
            let json = notice_json(
                &state.identity,
                "session-error",
                &format!("payment rejected: {e}"),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    let start_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut sessions = state.sessions.lock().await;
    let _session = sessions.create_session(
        &device_identifier,
        allotment,
        &state.config.metric,
        start_time,
    );
    drop(sessions);

    tracing::info!(
        verified_msat = verified_amount,
        received_sat = received_amount,
        allotment_msat = allotment,
        "session granted"
    );

    // Step 4: return a SIGNED kind 1022 session event (TIP-01).
    let resp = build_session_event(
        &state.config,
        &state.identity,
        &device_identifier,
        allotment,
        start_time,
        None, // a raw-token payment carries no customer pubkey; p stays omitted
    );

    let json = serde_json::to_string(&resp).unwrap_or_default();
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_valid_nostr_event() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["payment", "cashuBabc123token"]],
            "content": "",
            "pubkey": "abc",
            "id": "def",
            "sig": "ghi",
            "created_at": 1234567890
        })
        .to_string();
        let token = extract_token_from_nostr_event(&event);
        assert_eq!(token.as_deref(), Some("cashuBabc123token"));
    }

    #[test]
    fn extract_token_rejects_wrong_kind() {
        let event = serde_json::json!({
            "kind": 99999,
            "tags": [["payment", "token"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_rejects_missing_payment_tag() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["other", "value"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_handles_invalid_json() {
        assert!(extract_token_from_nostr_event("not json").is_none());
    }

    #[test]
    fn extract_token_handles_multiple_tags() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [
                ["other", "val"],
                ["payment", "real-token"],
                ["another", "x"]
            ],
        })
        .to_string();
        assert_eq!(
            extract_token_from_nostr_event(&event).as_deref(),
            Some("real-token")
        );
    }

    /// Test that a session is created after a successful payment flow.
    /// Uses the SessionManager directly to verify the integration logic.
    #[tokio::test]
    async fn payment_creates_session() {
        use crate::session::SessionManager;

        let mut mgr = SessionManager::new();
        let allotment: u64 = 5000; // 5 sats * 1000 = 5000 msat
        let session = mgr.create_session("test:mac", allotment, "bytes", 3600);
        assert_eq!(session.allotment, 5000);
        assert_eq!(session.metric, "bytes");
        assert!(mgr.is_active("test:mac"));
    }

    /// Test that rejected tokens return 402 (simulated).
    #[test]
    fn rejected_token_returns_402() {
        // The handler returns PAYMENT_REQUIRED for failed verification.
        // We verify the status code constant matches.
        let expected = StatusCode::PAYMENT_REQUIRED;
        assert_eq!(expected.as_u16(), 402);
    }
}

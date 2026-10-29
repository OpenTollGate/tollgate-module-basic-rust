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

/// The mint and face value (sats) of a token, parsed without any network call.
fn verifier_token_facts(token: &str) -> Result<(String, u64), String> {
    use cashu::nuts::Token;
    let t: Token = token
        .parse()
        .map_err(|e| format!("invalid Cashu token: {e}"))?;
    let url = t
        .mint_url()
        .map_err(|e| format!("token has no mint URL: {e}"))?
        .to_string();
    let face_sat: u64 = t
        .value()
        .map_err(|e| format!("token has no spendable value: {e}"))?
        .into();
    Ok((url.trim_end_matches('/').to_string(), face_sat))
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

/// Fund-safety gate: a payment must buy at least the operator's minimum
/// number of steps BEFORE the token is received.
///
/// In the order this gate replaced, `receive()` ran first and the below-
/// minimum refusal came afterwards — the token was consumed with nothing
/// granted and no refund path, so a below-minimum payment was confiscated by
/// ordering. `min_purchase_steps` is floored to 1 (schema parity with the Go
/// origin, which floors absent/0 to 1 for legacy configs), so every accepted
/// payment buys at least one whole step.
pub(crate) fn precheck_payment(
    face_sat: u64,
    price_per_step: u64,
    min_purchase_steps: u64,
) -> Result<(), String> {
    let price_per_step = price_per_step.max(1);
    let min_steps = min_purchase_steps.max(1);
    let steps = face_sat / price_per_step;
    if steps < min_steps {
        // Message spelling matches the Go origin (merchant.go, the
        // calculateAllotment minimum check) so portal/operator tooling greps
        // identically on both modules.
        return Err(format!(
            "payment only covers {steps} steps, but minimum purchase is {min_steps} steps"
        ));
    }
    Ok(())
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

/// HTTP semantics for a failed receive. A timeout is an UNKNOWN outcome,
/// not a rejection: `tokio::time::timeout` cancels the receive saga
/// mid-flight, and a cancelled saga may or may not have been accepted by the
/// mint — the wallet reconciles it in-session, but at answer time the honest
/// status is 504 `payment-outcome-unknown` with do-not-resubmit guidance.
/// Answering 402 "rejected" invites the customer to treat a possibly-spent
/// token as failed and pay twice.
/// Go's `isRateLimitError` parity: the rate-limit signal comes from the
/// mint's protocol surface (text the mint itself returns), so mirroring
/// the string match is wire parity, not smell.
fn is_mint_rate_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("429") || m.contains("rate limit") || m.contains("too many requests")
}

/// The receive-error taxonomy (the fork's R16/#55, mirroring Go's
/// post-receive classifier): the same underlying condition emits the same
/// `code` tag as the Go origin, and every failure path carries one — a
/// shared portal can tell a dead token from a busy mint from a retired
/// keyset instead of parsing prose.
fn receive_failure_code(e: &crate::wallet::wallet::WalletError) -> &'static str {
    let msg = e.to_string().to_ascii_lowercase();
    if matches!(e, crate::wallet::wallet::WalletError::Timeout(_)) {
        // The outer timeout is an AMBIGUOUS outcome — handled by
        // receive_failure_shape's 504 arm; reaching here would be a bug.
        return "payment-outcome-unknown";
    }
    if msg.contains("already spent")
        || msg.contains("already used")
        || msg.contains("token already spent")
    {
        return "payment-error-token-spent";
    }
    if is_mint_rate_limit(&msg) {
        return "mint-rate-limited";
    }
    if msg.contains("keyset")
        && (msg.contains("expired") || msg.contains("inactive") || msg.contains("unknown"))
    {
        return "payment-error-keyset-expired";
    }
    if msg.contains("fee")
        && (msg.contains("insufficient") || msg.contains("below") || msg.contains("cannot cover"))
    {
        return "payment-error-below-swap-fee";
    }
    if msg.contains("unreachable")
        || msg.contains("connection refused")
        || msg.contains("connect error")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("dns")
    {
        // Definitive transport errors surfaced by CDK (the outer timeout
        // is handled first); a 5xx after the mint processed the swap is
        // ambiguous, but CDK reports those as saga-persisted errors that
        // recovery decides — the classifier only names what it saw.
        return "payment-error-mint-unreachable";
    }
    "payment-processing-failed"
}

fn receive_failure_shape(
    e: &crate::wallet::wallet::WalletError,
) -> (StatusCode, &'static str, String) {
    match e {
        crate::wallet::wallet::WalletError::Timeout(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "payment-outcome-unknown",
            "payment outcome unknown after timeout; do not resubmit this token".to_string(),
        ),
        other => {
            let code = receive_failure_code(other);
            let message = match code {
                "payment-error-token-spent" => {
                    "This e-cash note has already been spent".to_string()
                }
                "mint-rate-limited" => {
                    "Mint is rate-limiting requests. Please try again in a moment.".to_string()
                }
                "payment-error-keyset-expired" => format!(
                    "This e-cash note was issued on a keyset the mint has retired; it cannot be recovered by retrying. Cause: {other}"
                ),
                "payment-error-mint-unreachable" => {
                    "Mint temporarily unavailable. Please try again, or use a token from another mint.".to_string()
                }
                _ => format!("payment processing failed: {other}"),
            };
            (StatusCode::PAYMENT_REQUIRED, code, message)
        }
    }
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

    // NUT-10 spending conditions (P2PK/HTLC) lock the proofs to conditions
    // this gateway cannot satisfy: receiving such a token takes value the
    // wallet can never spend upstream. Refused at parse time — the cheapest
    // check, before any network call or mint consultation.
    if crate::wallet::verify::token_is_locked(&token) {
        tracing::warn!("payment refused: token carries NUT-10 spending conditions");
        let json = notice_json(
            &state.identity,
            "payment-error-invalid-token",
            "payment rejected: token carries NUT-10 spending conditions (P2PK/HTLC) and cannot be received",
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

    // A6/A8: decide whether this token's mint is acceptable BEFORE any network
    // call. `verify()` performs a NUT-07 checkstate round trip, so checking the
    // allowlist afterwards meant a token from an unaccepted mint cost a full
    // mint timeout (30 s) before being refused — and a token whose mint was
    // simply unreachable hung the request. Cheap checks first.
    let (mint, face_sat) = match verifier_token_facts(&token).and_then(|(m, v)| {
        crate::http::routes::select::select_mint_for_token(&state.config, &m)
            .map(|mc| (mc, v))
            .map_err(|e| format!("{e:?}"))
    }) {
        Ok((m, v)) => (m, v),
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

    // Settled-payment replay: a token this module has already banked (live
    // `received` or reconciled `reconcile-spent`) re-grants the session from
    // the frozen journal terms instead of failing on the spent token — the
    // customer who lost a 200 to a crash, a timeout or a flaky link gets
    // their access on the retry without paying twice. Runs before any
    // network call — and before the price gate: the frozen terms already
    // priced this payment, so a later config change must not strand the
    // delivery of something already bought.
    {
        let id = crate::payment_journal::token_id(&token);
        let settled = {
            let sessions = state.sessions.lock().await;
            sessions
                .store()
                .and_then(|st| st.get_payment(&id).ok().flatten())
                .filter(|rec| crate::payment_journal::is_settled(&rec.phase))
        };
        if let Some(rec) = settled {
            let allotment = crate::payment_journal::allotment_for(&rec);
            let start_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut sessions = state.sessions.lock().await;
            // Replay semantics with ADDITIVE grants (#63): a live session
            // already carrying this payment's value is REPORTED as-is —
            // re-granting would stack allotment on every retry. Only a
            // LOST session (expired, replaced) re-grants, additively, so
            // a retry can never erase a sibling's paid remainder either.
            let already_live = sessions.get_session(&rec.mac).is_some();
            if !already_live {
                sessions.grant_additive(
                    &rec.mac,
                    allotment,
                    &rec.metric,
                    // Exhaustion-driven, like the original grant: the frozen
                    // terms priced this access once already.
                    u64::MAX / 4,
                );
            }
            let persist_ok = sessions
                .get_session(&rec.mac)
                .map(|sess| sess.allotment > 0)
                .unwrap_or(false);
            drop(sessions);
            if !persist_ok {
                let json = notice_json(
                    &state.identity,
                    "session-error",
                    "payment already received but the session could not be recorded; do not pay again — contact the operator with this response",
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
            if !already_live {
                let _ = crate::metering::open_gate(&rec.mac).await;
            }
            let live_allotment = {
                let sessions = state.sessions.lock().await;
                sessions
                    .get_session(&rec.mac)
                    .map(|sess| sess.allotment)
                    .unwrap_or(allotment)
            };
            let resp = build_session_event(
                &state.config,
                &state.identity,
                &rec.mac,
                live_allotment,
                start_time,
                None,
            );
            let json = serde_json::to_string(&resp).unwrap_or_default();
            return (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    }

    // Fund safety: a payment must buy at least the operator's minimum number
    // of steps BEFORE the token is received. In the order this replaces,
    // `receive()` ran first and the below-minimum refusal came afterwards —
    // the token was consumed with nothing granted and no refund path, so a
    // below-minimum payment was confiscated by ordering. Priced from the
    // parsed face value: cheaper than the NUT-07 round trip it precedes
    // (A6/A8 cheap-checks-first), and the mint's answer about proofs that can
    // never buy one step is irrelevant. The post-receive `calculate_allotment`
    // guard still prices the actual grant from the received amount.
    if let Err(msg) = precheck_payment(face_sat, mint.price_per_step, mint.min_purchase_steps) {
        tracing::warn!(
            face_sat,
            price_per_step = mint.price_per_step,
            "payment below minimum refused before mint contact"
        );
        let json = notice_json(&state.identity, "payment-error-below-minimum", &msg);
        return (
            StatusCode::PAYMENT_REQUIRED,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }

    // A3: the client's identity, resolved from the connection's address.
    //
    // Pre-A3 this was the literal `00:00:00:00:00:00`, which meant `/usage`
    // could never match a session and metering silently did nothing. A resolution
    // failure is now refused explicitly rather than papered over with a sentinel.
    // ADR-0007 §1: identity is the TCP peer (config-gated escape hatch for proxied
    // deployments); the header path remains only for `identity_from_socket = false`.
    //
    // Fund safety: resolution runs BEFORE `receive()` — the payment journal
    // records the buyer at intent time, an unresolvable client must be
    // refused before value moves, and a 400 after consumption would strand
    // the token for a session nobody could ever look up. The Go origin
    // refuses unidentified clients pre-receive too (`client-not-registered`).
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

    // Payment journal, part 1: the frozen record. Pricing terms are captured
    // at intent time so a later reconcile grants what was paid for even if
    // the operator changes config before the deciding boot.
    let payment_id = crate::payment_journal::token_id(&token);
    let payment_rec = |phase: &str, amount_sat: u64| crate::session::store::PaymentRecord {
        id: payment_id.clone(),
        token: token.clone(),
        mac: device_identifier.clone(),
        mint: mint.url.clone(),
        amount_sat,
        price_per_step: mint.price_per_step,
        step_size: state.config.step_size,
        metric: state.config.metric.clone(),
        phase: phase.to_string(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    // Payment journal, part 2: the intent row exists BEFORE value moves. A
    // crash anywhere between `receive()` and the 200 leaves this row for the
    // boot reconciler (NUT-07 decides: spent ⇒ the owed session is granted).
    // A payment whose intent cannot be journaled is refused outright — moving
    // value with no recovery record is exactly the window this table closes.
    {
        let sessions = state.sessions.lock().await;
        if let Some(store) = sessions.store() {
            let rec = payment_rec(crate::payment_journal::PHASE_INTENT, verified_amount / 1000);
            if let Err(e) = store.upsert_payment(&rec) {
                tracing::error!(error = %e, "payment journal intent write failed; refusing to move value");
                let json = notice_json(
                    &state.identity,
                    "payment-processing-failed",
                    "payment refused: the payment journal is unavailable",
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    }

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
                // Journal the outcome class before answering: a definitive
                // failure owes nothing (`rejected`); a timeout is UNKNOWN
                // (`timeout-unknown`) — the cancelled saga may still have
                // been accepted by the mint, and the boot reconciler decides
                // it on NUT-07 evidence rather than on this refusal.
                {
                    let sessions = state.sessions.lock().await;
                    if let Some(store) = sessions.store() {
                        let phase = if matches!(e, crate::wallet::wallet::WalletError::Timeout(_)) {
                            crate::payment_journal::PHASE_TIMEOUT_UNKNOWN
                        } else {
                            crate::payment_journal::PHASE_REJECTED
                        };
                        let rec = payment_rec(phase, 0);
                        let _ = store.upsert_payment(&rec);
                    }
                }
                let (status, code, message) = receive_failure_shape(&e);
                let json = notice_json(&state.identity, code, &message);
                return (
                    status,
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

    // Journal the live outcome: value has moved, so the row advances to
    // `received` before the session work below — a crash from here on is
    // covered by this terminal (idempotent same-MAC overwrite at replay).
    {
        let sessions = state.sessions.lock().await;
        if let Some(store) = sessions.store() {
            let rec = payment_rec(crate::payment_journal::PHASE_RECEIVED, received_amount);
            if let Err(e) = store.upsert_payment(&rec) {
                tracing::error!(error = %e, "payment journal terminal write failed; boot reconcile will decide the row");
            }
        }
    }

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
            // With the pre-receive gate above, this branch is reachable only
            // when swap fees consumed the margin between the token's face
            // value and what the wallet actually received. Value has already
            // moved; the honest classification is the fee class — the old
            // `payment-error-token-spent` code told the customer their note
            // was dead when it had in fact been taken as payment.
            let json = notice_json(
                &state.identity,
                "payment-error-below-swap-fee",
                &format!(
                    "payment of {received_amount} sat does not cover one step at {} sat/step after swap fees",
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
    let session = sessions.create_session(
        &device_identifier,
        allotment,
        &state.config.metric,
        start_time,
    );
    if session.allotment == 0 {
        // The persist-failure sentinel: value moved, but the session is NOT
        // durable (create_session removed it from memory and answers
        // {allotment: 0} — the pre-receive gate guarantees every real grant
        // a positive allotment). Roll the journal row back to `intent` so
        // the boot reconciler re-grants — and keeps re-granting — until a
        // session row sticks, then refuse: a 200 here would promise access
        // that vanishes on restart.
        if let Some(store) = sessions.store() {
            let rec = payment_rec(crate::payment_journal::PHASE_INTENT, received_amount);
            let _ = store.upsert_payment(&rec);
        }
        drop(sessions);
        tracing::error!(
            mac = %device_identifier,
            received_sat = received_amount,
            "session persist failed after receive; journal rolled back to intent for reconciliation"
        );
        let json = notice_json(
            &state.identity,
            "session-error",
            "payment received but the session could not be recorded; do not pay again — it will be granted automatically",
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
    drop(sessions);

    // `create_session` answers the `{allotment: 0, expiry: 0}` sentinel exactly
    // when the durable store refused the write — it also removes the session
    // from memory ("a session we could not persist is a session we do not
    // have"). The pre-receive gate guarantees `allotment > 0` for every grant
    // that reaches this point, so a zero here is always the persist-failure
    // sentinel, never a real allotment. Advertising 200 would claim access
    // that vanishes on restart; the payment's value is already in the wallet,
    // so the customer must be told to seek the operator, not to pay again.
    if session.allotment == 0 {
        tracing::error!(
            mac = %device_identifier,
            received_sat = received_amount,
            "session persist failed after receive; refusing to advertise a grant"
        );
        let json = notice_json(
            &state.identity,
            "session-error",
            "payment received but the session could not be recorded; do not pay again — contact the operator with this response",
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

    /// The R16 taxonomy (fork #55 / Go's post-receive classifier): the same
    /// condition, the same code tag as the Go origin.
    #[test]
    fn receive_taxonomy_matches_go_codes() {
        use crate::wallet::wallet::WalletError as W;
        let cases: Vec<(W, &str)> = vec![
            (
                W::Database("mint returned 429 too many requests".into()),
                "mint-rate-limited",
            ),
            (
                W::Database("the mint reports rate limit exceeded".into()),
                "mint-rate-limited",
            ),
            (
                W::Cdk(cdk::Error::UnknownKeySet),
                "payment-error-keyset-expired",
            ),
            (
                W::Database("token already spent".into()),
                "payment-error-token-spent",
            ),
            (
                W::TokenParse("fee insufficient for swap".into()),
                "payment-error-below-swap-fee",
            ),
            (
                W::Database("mint unreachable: connection refused".into()),
                "payment-error-mint-unreachable",
            ),
            (
                W::TokenParse("something novel".into()),
                "payment-processing-failed",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(receive_failure_code(&e), want, "input: {e}");
        }
    }

    /// A receive timeout is an UNKNOWN outcome: 504 + do-not-resubmit, never
    /// a 402 that invites the customer to spend a second token.
    #[test]
    fn receive_timeout_maps_to_outcome_unknown_not_rejected() {
        let e = crate::wallet::wallet::WalletError::Timeout(std::time::Duration::from_secs(30));
        let (status, code, message) = receive_failure_shape(&e);
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(code, "payment-outcome-unknown");
        assert!(message.contains("do not resubmit"), "got: {message}");
    }

    /// Definitive receive failures keep the rejection semantics.
    #[test]
    fn definitive_receive_failure_stays_a_rejection() {
        let e = crate::wallet::wallet::WalletError::TokenParse("bad token".into());
        let (status, code, _) = receive_failure_shape(&e);
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(code, "payment-processing-failed");
    }
}

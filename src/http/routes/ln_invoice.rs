//! `POST /ln-invoice` and `GET /ln-invoice?quote=` — HTTP-03.
//!
//! Wire shape comes from `corpus/http/http03-ln-invoice.json`, not from invention. P3
//! shipped `{quote, request, pubkey, amount, expiry}`, which no client expects: the
//! corpus fixes `{status, quote, invoice, mint_url, amount, expiry, state,
//! access_granted}` and uses `status: 1` rather than an HTTP-only success signal.
//!
//! P5 adds the step that made this lane pointless: when a quote is PAID the merchant
//! must MINT the tokens and GRANT the session. Before this, `mint_tokens` was never
//! called from production code and `create_session` only from tests — a customer could
//! pay an invoice and receive nothing at all.

use crate::http::routes::quote::{MintQuoteRegistry, QuoteState};
use crate::http::AppState;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Fallback quote lifetime when the mint does not state one.

/// The decision a GET poll makes, extracted so it is testable without a router.
///
/// WHY THIS EXISTS: the ISSUED branch's comment claimed "fall through to the grant
/// report below" as if it were a report, but the code below MINTS AND GRANTS on
/// every poll. Measured on A04E: 12 polls of a settled quote grew the allotment
/// 1000 -> 12000 — a customer refreshing their invoice status minted themselves
/// free bandwidth. The decision of WHETHER to grant must be pure and testable.
///
/// The quote registry's `Issued` mark is the idempotency token: a quote that has
/// already granted once must NEVER grant again, regardless of what the poll says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollDecision {
    /// Quote is unpaid: report pending, no grant.
    ReportPending,
    /// Money arrived and this quote has not granted yet: mint + grant (once).
    Grant,
    /// Already granted for this quote: report the EXISTING grant. No mint, no
    /// allotment change. This is what makes GET idempotent.
    ReportExistingGrant,
}

/// Decide what a GET poll should do.
///
/// `mint_state` is the mint's own word (UNPAID/PAID/ISSUED); `already_issued` is
/// whether OUR registry has marked this quote as granted.
pub fn poll_grant_decision(mint_state: &str, already_issued: bool) -> PollDecision {
    match (mint_state, already_issued) {
        ("UNPAID", _) => PollDecision::ReportPending,
        // PAID, never granted: grant now.
        ("PAID", false) => PollDecision::Grant,
        // PAID but we already granted (e.g. mint state lagged behind our mark):
        // report the existing grant, never re-grant.
        ("PAID", true) => PollDecision::ReportExistingGrant,
        // ISSUED means the mint already minted for this quote. If we have not
        // marked it granted, this is our first observation: grant (which will
        // find existing tokens or mint idempotently). If we HAVE marked it, this
        // is a re-poll: report, never re-grant.
        ("ISSUED", false) => PollDecision::Grant,
        ("ISSUED", true) => PollDecision::ReportExistingGrant,
        // Unknown state: report pending rather than grant on a guess. Coercing an
        // unknown state to a grant would hand out bandwidth on malformed input.
        _ => PollDecision::ReportPending,
    }
}

const DEFAULT_QUOTE_TTL_SECS: u64 = 15 * 60;
/// Steps granted per sat, from the corpus's own `price_per_step = 1` default.
const DEFAULT_METRIC: &str = "bytes";

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    /// The mint to buy from. Accepted from the body because the corpus's request
    /// vector carries it; when absent a single configured mint is used.
    #[serde(default)]
    pub mint_url: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    /// Wire compatibility only: identity comes from the network context (HTTP-03).
    #[serde(default)]
    pub mac: Option<String>,
}

#[derive(Debug, Serialize)]
struct InvoiceResponse {
    status: u8,
    quote: String,
    invoice: String,
    mint_url: String,
    amount: u64,
    expiry: u64,
    state: String,
    access_granted: bool,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
    /// Wire compatibility only; the device identifier is resolved from the network.
    #[serde(default)]
    pub mac: Option<String>,
}

/// Success body for `GET` on a quote that has been applied.
#[derive(Debug, Serialize)]
struct PaidStatus {
    status: u8,
    quote: String,
    state: String,
    access_granted: bool,
    allotment: u64,
    metric: String,
}

/// Non-terminal `GET` body. `state` is the corpus's fixed vocabulary.
#[derive(Debug, Serialize)]
struct PendingStatus {
    status: u8,
    quote: String,
    state: String,
    access_granted: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    status: u8,
    error: &'a str,
}

fn registry(state: &AppState) -> Arc<Mutex<MintQuoteRegistry>> {
    state.quotes.clone()
}

fn json<T: Serialize>(
    code: StatusCode,
    body: &T,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        code,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        serde_json::to_string(body)
            .unwrap_or_else(|_| "{\"status\":0,\"error\":\"serialisation failed\"}".to_string()),
    )
}

fn err(code: StatusCode, msg: &str) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    // The corpus error envelope is `{status: 0, error}` — a bare string body is not
    // conformant, and a client parsing `status` would see nothing.
    json(
        code,
        &ErrorBody {
            status: 0,
            error: msg,
        },
    )
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    // Corpus `post-missing-fields`: zero amount is a 400, not a quote.
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be greater than zero");
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Resolve which mint to buy from: the body's, else the single configured one.
    let mint_url = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet is not initialised");
        };
        match req
            .mint_url
            .clone()
            .map(|m| m.trim_end_matches('/').to_string())
            .or_else(|| w.default_mint().map(|m| m.to_string()))
        {
            Some(m) => m,
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "no mint_url given and no single configured mint to default to",
                )
            }
        }
    };

    // The mint must be one we accept. Charging against an unaccepted mint would take
    // money we cannot bank.
    if crate::http::routes::select::select_mint_for_token(&state.config, &mint_url).is_err() {
        return err(StatusCode::BAD_REQUEST, "mint_url is not an accepted mint");
    }

    let quote = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet unavailable");
        };
        match w.request_mint_quote(&mint_url, req.amount).await {
            Ok(q) => q,
            Err(e) => {
                // A mint failure must not be dressed up as a quote.
                tracing::error!(mint = %mint_url, error = %e, "mint quote request failed");
                return err(StatusCode::BAD_GATEWAY, "the mint did not issue a quote");
            }
        }
    };

    if quote.request.is_empty() {
        return err(
            StatusCode::BAD_GATEWAY,
            "the mint returned a quote without an invoice",
        );
    }

    let expiry = if quote.expiry > now {
        quote.expiry
    } else {
        now + DEFAULT_QUOTE_TTL_SECS
    };

    registry(&state)
        .lock()
        .map(|mut r| r.insert(&quote.id, req.amount, expiry))
        .ok();

    let body = InvoiceResponse {
        status: 1,
        quote: quote.id,
        invoice: quote.request,
        mint_url,
        amount: req.amount,
        expiry,
        state: "UNPAID".to_string(),
        // Nothing has been granted yet; claiming otherwise would unlock the gate for free.
        access_granted: false,
    };
    json(StatusCode::OK, &body)
}

/// Resolve the paying device from the request's address headers.
///
/// Extracted so the header plumbing is TESTABLE. The original bug was that the handler
/// called the resolver with three literal `None`s, and no test could see it because
/// identity resolution sat after minting and was unreachable without a live mint.
///
/// `resolve` is injected so a test can prove the header VALUES arrive, without needing a
/// DHCP lease on the test machine.
pub(crate) fn resolve_device(
    forwarded: Option<&str>,
    real_ip: Option<&str>,
    socket_ip: Option<&str>,
    resolve: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let ip = crate::http::mac::client_ip_from_headers(forwarded, real_ip, socket_ip)?;
    resolve(&ip)
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Query(q): Query<InvoiceQuery>,
) -> impl IntoResponse {
    let found = registry(&state)
        .lock()
        .ok()
        .and_then(|r| r.get(&q.quote).cloned());

    // Corpus `get-unknown-quote-fullservice`: 404 with the error envelope.
    let Some(quote) = found else {
        return err(StatusCode::NOT_FOUND, "unknown quote");
    };

    let mint_url = {
        let guard = state.wallet.lock().await;
        guard
            .as_ref()
            .and_then(|w| w.default_mint().map(|m| m.to_string()))
    };

    // Ask the MINT for the truth rather than trusting our cached guess.
    let mut state_str = match quote.state {
        QuoteState::Unpaid => "UNPAID",
        QuoteState::Paid => "PAID",
        QuoteState::Issued => "ISSUED",
    }
    .to_string();

    if let Some(mint) = &mint_url {
        let guard = state.wallet.lock().await;
        if let Some(w) = guard.as_ref() {
            match w.check_mint_quote(mint, &quote.id).await {
                Ok(mint_state) => {
                    let upper = mint_state.to_uppercase();
                    state_str = if upper.contains("ISSUED") {
                        "ISSUED".to_string()
                    } else if upper.contains("PAID") {
                        "PAID".to_string()
                    } else if upper.contains("UNPAID") {
                        "UNPAID".to_string()
                    } else {
                        // Passed through verbatim: coercing an unknown state to UNPAID
                        // would tell a paying customer to pay again.
                        mint_state
                    };
                }
                Err(e) => {
                    tracing::warn!(quote = %quote.id, error = %e, "could not reach the mint");
                }
            }
        }
    }

    if state_str == "UNPAID" {
        let body = PendingStatus {
            status: 1,
            quote: quote.id.clone(),
            state: state_str,
            access_granted: false,
        };
        return json(StatusCode::OK, &body);
    }

    // The device identifier comes from the network; `mac` in the query is accepted for
    // wire compatibility and deliberately ignored (HTTP-03), because it is
    // client-controlled and honouring it would let a device claim another's paid session.
    // Same resolution A3 established for /pay: address -> lease/ARP -> MAC.
    //
    // The three `None`s here were a real bug: the handler never received the request's
    // headers, so the resolver was called with no input and could never succeed. A paid
    // quote therefore always answered "the device could not be identified" and minted
    // nothing. `/pay` had the same information and worked; this handler simply did not
    // ask for it.
    //
    // The query `mac` is deliberately NOT consulted: it is client-controlled, and
    // honouring it would let a device claim another's paid session.
    // ADR-0007 §1: identity is the TCP peer; the escape hatch (identity_from_socket
    // = false) restores header trust for proxied deployments by config, not by default.
    let (device, _extra) =
        crate::http::mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);
    let Some(mac) = device else {
        // Paid, but we cannot say who to. Refuse rather than grant to a sentinel.
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "payment received but the device could not be identified; retry from the client",
        );
    };

    // Idempotency: has OUR registry already granted for this quote?
    let already_issued = registry(&state)
        .lock()
        .map(|r| {
            r.get(&quote.id)
                .map(|q| q.state == QuoteState::Issued)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    match poll_grant_decision(&state_str, already_issued) {
        PollDecision::ReportPending => {
            let body = PendingStatus {
                status: 1,
                quote: quote.id.clone(),
                state: state_str,
                access_granted: false,
            };
            return json(StatusCode::OK, &body);
        }
        PollDecision::ReportExistingGrant => {
            // Re-poll of a settled quote: report the grant that ALREADY exists.
            // No mint, no allotment change. This is what makes GET idempotent;
            // before this, every poll re-granted (measured: 1000 -> 12000).
            let sessions = state.sessions.lock().await;
            let (allotment, granted) = match sessions.get_session(&mac) {
                Some(s) => (s.allotment, true),
                None => (0, false),
            };
            drop(sessions);
            let body = PaidStatus {
                status: 1,
                quote: quote.id.clone(),
                state: "ISSUED".to_string(),
                access_granted: granted,
                allotment,
                metric: DEFAULT_METRIC.to_string(),
            };
            return json(StatusCode::OK, &body);
        }
        PollDecision::Grant => {}
    }

    // PAID (or first ISSUED observation): the money arrived, so MINT and GRANT.
    //
    // This is the step that was missing entirely. `mint_tokens` had never been called
    // from production code, so a customer could pay an invoice and get nothing.
    let Some(mint) = mint_url else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "wallet has no mint");
    };

    let minted = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet unavailable");
        };
        if state_str == "ISSUED" {
            // Already minted in an earlier poll; do not ask the mint again.
            quote.amount
        } else {
            match w.mint_tokens(&mint, &quote.id).await {
                Ok(n) => n,
                Err(e) => {
                    // Paid but not yet applied: the corpus allows a 503 transient here, and
                    // retrying is correct. A 200 would lie about the grant.
                    tracing::warn!(quote = %quote.id, error = %e, "minting from a paid quote failed");
                    return err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("payment received, tokens not yet issued: {e}; retry"),
                    );
                }
            }
        }
    };

    // Allotment: one step per sat at the default price, which is what the corpus's
    // `price_per_step = 1` means. Using the raw amount would over-grant when an operator
    // charges more than a sat per step.
    // `step_size` is a gateway-wide setting (Config), not per-mint.
    let step_size = state.config.step_size;
    let price_per_step = state
        .config
        .accepted_mints
        .iter()
        .find(|m| m.url.trim_end_matches('/') == mint.trim_end_matches('/'))
        .map(|m| m.price_per_step.max(1))
        .unwrap_or(1);
    let allotment = (minted / price_per_step) * step_size;

    {
        let mut sessions = state.sessions.lock().await;
        // Reuse the session if one already exists for this device.
        //
        // WHY: granting while a session is live must extend it, not replace it. Replacing
        // discards the client's current usage, so a device that buys twice mid-session
        // would appear to have consumed nothing — the exact accounting divergence the
        // corpus's `spent` verdicts exist to catch.
        //
        // It also makes the grant IDEMPOTENT. A client polls GET; without this, a second
        // poll mints nothing (correct) but would still rewrite the session row.
        let secs = quote.expiry.saturating_sub(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        if sessions
            .extend_session(&mac, allotment, DEFAULT_METRIC, secs)
            .is_none()
        {
            sessions.create_session(&mac, allotment, DEFAULT_METRIC, secs);
        }
    }
    // OPEN THE GATE. Minting and recording a session does not let the customer through:
    // nodogsplash still drops their traffic until it is told to authenticate the MAC.
    // Measured 2026-10-19: the module had NO `ndsctl auth` call anywhere, so a paying
    // customer received a session record and stayed blocked -- the portal was decorative.
    //
    // A failure here must NOT report `access_granted: true`, because it would be false.
    let gate = crate::metering::open_gate(&mac).await;
    match &gate {
        crate::metering::GateOutcome::Opened => {}
        crate::metering::GateOutcome::Closed => {}
        crate::metering::GateOutcome::UnknownClient => {
            // nodogsplash has not seen this MAC yet: it has not been intercepted, so there
            // is no gate to open. The session is recorded and will apply once it is.
            tracing::warn!(
                mac = %mac,
                "paid, but nodogsplash has not seen this client yet; it must trigger the portal once"
            );
        }
        crate::metering::GateOutcome::Refused(m) => {
            tracing::error!(mac = %mac, reason = %m, "nodogsplash refused to open the gate");
        }
        crate::metering::GateOutcome::Unavailable(m) => {
            tracing::error!(mac = %mac, reason = %m, "ndsctl unavailable; gate not opened");
        }
    }
    let gate_opened = matches!(gate, crate::metering::GateOutcome::Opened);

    registry(&state)
        .lock()
        .map(|mut r| r.mark_issued(&quote.id))
        .ok();

    let body = PaidStatus {
        status: 1,
        quote: quote.id,
        state: "ISSUED".to_string(),
        // The session exists, but "granted" must reflect whether the device can actually
        // reach the network. A paid-but-still-blocked client that is told `true` will sit
        // there wondering why nothing works.
        access_granted: gate_opened,
        allotment,
        metric: DEFAULT_METRIC.to_string(),
    };
    json(StatusCode::OK, &body)
}

#[cfg(test)]
mod poll_decision_tests {
    use super::*;

    /// THE measured bug: 12 polls of a settled quote grew allotment 1000 -> 12000.
    /// A quote our registry has already granted must NEVER grant again.
    #[test]
    fn re_polling_an_issued_quote_never_regrants() {
        assert_eq!(
            poll_grant_decision("ISSUED", true),
            PollDecision::ReportExistingGrant
        );
    }

    /// First observation of ISSUED: grant (idempotently at the mint), then mark.
    #[test]
    fn first_issued_observation_grants() {
        assert_eq!(poll_grant_decision("ISSUED", false), PollDecision::Grant);
    }

    /// PAID and never granted: grant once.
    #[test]
    fn paid_and_ungranted_grants_once() {
        assert_eq!(poll_grant_decision("PAID", false), PollDecision::Grant);
    }

    /// PAID but already granted (mint state lagging behind our mark): report.
    #[test]
    fn paid_but_already_granted_reports() {
        assert_eq!(
            poll_grant_decision("PAID", true),
            PollDecision::ReportExistingGrant
        );
    }

    /// UNPAID never grants, whatever our registry thinks.
    #[test]
    fn unpaid_never_grants() {
        assert_eq!(
            poll_grant_decision("UNPAID", false),
            PollDecision::ReportPending
        );
        assert_eq!(
            poll_grant_decision("UNPAID", true),
            PollDecision::ReportPending
        );
    }

    /// Unknown state must not hand out bandwidth on malformed input.
    #[test]
    fn unknown_state_is_pending_not_grant() {
        assert_eq!(
            poll_grant_decision("GARBAGE", false),
            PollDecision::ReportPending
        );
    }
}

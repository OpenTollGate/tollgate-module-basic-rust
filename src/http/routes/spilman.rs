//! `/spilman` channel routes — the payee-side HTTP surface a channel client talks to.
//!
//! # Why this exists
//!
//! `AppState.spilman` was wired by the config work but **no route was ever
//! mounted**, so a channel client had nothing to talk to and the physical-router
//! lane's four assertions could not be wired. This module closes that gap.
//!
//! # The `enabled: false` contract, observably
//!
//! When the rail is off there is **no rail object** (`AppState.spilman == None`)
//! and every route here answers `404`. That is the observable half of "enabled:
//! false changes nothing": it is not enough that the rail abstains — the surface
//! must not exist. `src/http/spilman_mount_test.rs` pins both directions.
//!
//! # Why the routes are thin
//!
//! Every decision lives in the rail (`PaymentRail::authorize_bytes` /
//! `record_payment`), never here. A handler that re-derives credit is how a payee
//! ends up serving a byte the payer's signature does not cover.

use crate::http::AppState;
use crate::spilman::rail::SpilmanRail;
use crate::spilman::PaymentRail;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Fetch the rail, or `None` when Spilman is disabled.
///
/// Returning `None` here is what makes every handler below answer 404 rather than
/// 500: a disabled rail is not an error, it is an absent surface.
fn rail(state: &AppState) -> Option<Arc<SpilmanRail>> {
    state.spilman.clone()
}

/// GET /spilman/params — what a payer needs in order to open a channel.
///
/// Deliberately does NOT leak the channel ledger or any balance: this is public
/// configuration, and a discovery endpoint that exposes state is a privacy bug.
#[derive(Debug, Serialize)]
pub struct ParamsResponse {
    pub status: u8,
    pub mint_url: Option<String>,
    pub in_bytes_per_millisat: u64,
}

pub async fn handle_params(State(state): State<AppState>) -> impl IntoResponse {
    match rail(&state) {
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "status": 0,
            "error": "spilman rail is not enabled"
        }))).into_response(),
        Some(r) => Json(ParamsResponse {
            status: 1,
            mint_url: r.mint_url().map(str::to_string),
            in_bytes_per_millisat: r.in_bytes_per_millisat(),
        })
        .into_response(),
    }
}

/// POST /spilman/payment — a signed balance update from the payer.
#[derive(Debug, Deserialize)]
pub struct PaymentRequest {
    pub channel_id: String,
    pub balance: u64,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct PaymentResponse {
    pub status: u8,
    /// What the ledger did with it: accepted, or refused as a replay.
    pub outcome: String,
}

pub async fn handle_payment(
    State(state): State<AppState>,
    Json(req): Json<PaymentRequest>,
) -> impl IntoResponse {
    let Some(r) = rail(&state) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "status": 0,
            "error": "spilman rail is not enabled"
        }))).into_response();
    };

    // The rail decides. A refusal is a 409, not a 500: the request was well-formed
    // and the answer is "no", which is a different fact from "we broke".
    match r.record_payment(&req.channel_id, req.balance, &req.signature).await {
        Ok(outcome) => Json(PaymentResponse {
            status: 1,
            outcome: format!("{outcome:?}"),
        })
        .into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({
            "status": 0,
            "error": e.to_string()
        }))).into_response(),
    }
}

/// GET /spilman/status?channel_id=... — the payee's view of one channel.
#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub channel_id: String,
}

pub async fn handle_status(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<StatusQuery>,
) -> impl IntoResponse {
    let Some(r) = rail(&state) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "status": 0,
            "error": "spilman rail is not enabled"
        }))).into_response();
    };

    // `authorize_bytes` with zero cumulative bytes asks the only question that
    // matters for a status read: "is this channel known?" An unknown channel
    // denies, which we surface as 404 rather than a misleading zero balance.
    let decision = r.authorize_bytes(&q.channel_id, 0, 0).await;
    match decision {
        crate::spilman::RailDecision::Deny => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "status": 0,
                "error": "unknown channel"
            })),
        )
            .into_response(),
        other => Json(serde_json::json!({
            "status": 1,
            "channel_id": q.channel_id,
            "decision": format!("{other:?}")
        }))
        .into_response(),
    }
}

/// Build the `/spilman` sub-router.
///
/// Mounted by `create_router` **only when the rail exists**, so a disabled rail
/// yields 404 on every path here with no handler ever running.
pub fn spilman_router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/params", axum::routing::get(handle_params))
        .route("/payment", axum::routing::post(handle_payment))
        .route("/status", axum::routing::get(handle_status))
}

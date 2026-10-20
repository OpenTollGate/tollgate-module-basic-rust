//! GET / — Nostr kind 10021 discovery event.

use crate::http::AppState;
use crate::nostr_event;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_discovery(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    let identity = &state.identity;

    let metric = if config.metric == "bytes" {
        "bytes"
    } else {
        "milliseconds"
    };
    // TIP-02: one `price_per_step` tag PER accepted mint, in the shape
    // ["price_per_step", "cashu", <price>, <unit>, <mint_url>, <min_steps>].
    // Emitting only the first mint made a multi-mint TollGate advertise a
    // single price and look single-mint to every client. The 6-element shape
    // was also misnamed "block", so clients never parsed it at all.
    //
    // `min_steps`: TIP-02 says an absent value or `0` MUST be treated as `1`,
    // and known clients reject advertisements carrying `min_steps=0`. The
    // config field defaults to 0, so floor it here rather than advertise a
    // value that makes the TollGate unpurchasable.
    let mut tags = vec![
        vec!["metric".to_string(), metric.to_string()],
        vec!["step_size".to_string(), config.step_size.to_string()],
    ];

    for m in &config.accepted_mints {
        let min_steps = if m.min_purchase_steps == 0 {
            1
        } else {
            m.min_purchase_steps
        };
        tags.push(vec![
            "price_per_step".to_string(),
            "cashu".to_string(),
            m.price_per_step.to_string(),
            m.price_unit.clone(),
            m.url.clone(),
            min_steps.to_string(),
        ]);
    }

    // TIP-01: `tips` lists the implemented TIP numbers as values. A bare
    // ["tips"] with no values is not a valid advertisement.
    tags.push(vec!["tips".to_string(), "1".to_string(), "2".to_string()]);

    let event = nostr_event::create_event(10021, tags, "", &identity.secret_key);
    let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}

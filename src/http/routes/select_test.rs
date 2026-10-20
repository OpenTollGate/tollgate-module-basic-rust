//! A6 — multi-mint selection.
//!
//! Before A6 the payment path used `accepted_mints.first()` for two separate
//! decisions:
//!   1. which mint's `price_per_step` to charge
//!   2. whether the token's mint is acceptable
//!
//! That is wrong in both directions. A token from the SECOND configured mint was
//! priced with the FIRST mint's price (so a cheap-mint token could be charged a
//! dear-mint price, or vice versa), and a payment was refused for a mint that is
//! actually accepted. With one mint configured the bug is invisible, which is why
//! it survived to Phase 4 being called "complete".

use super::select::{select_mint_for_token, MintSelectionError};
use crate::config::schema::{Config, MintConfig};

fn mint(url: &str, price: u64, unit: &str) -> MintConfig {
    let mut m = MintConfig::default_production(url);
    m.price_per_step = price;
    m.price_unit = unit.to_string();
    m
}

fn cfg(mints: Vec<MintConfig>) -> Config {
    let mut c = Config::default();
    c.metric = "bytes".to_string();
    c.step_size = 1000;
    c.accepted_mints = mints;
    c
}

#[test]
fn selects_the_mint_the_token_actually_came_from() {
    let c = cfg(vec![
        mint("https://a.example", 1, "sats"),
        mint("https://b.example", 5, "sats"),
    ]);
    let got = select_mint_for_token(&c, "https://b.example").expect("selected");
    assert_eq!(got.url, "https://b.example");
    assert_eq!(
        got.price_per_step, 5,
        "A6: the token's own mint sets the price, not the first configured mint"
    );
}

#[test]
fn the_first_mint_is_not_a_default_for_unknown_tokens() {
    let c = cfg(vec![mint("https://a.example", 1, "sats")]);
    let err = select_mint_for_token(&c, "https://evil.example").unwrap_err();
    assert!(
        matches!(err, MintSelectionError::NotAccepted { .. }),
        "an unaccepted mint must be refused, never priced with another mint's table"
    );
}

#[test]
fn trailing_slashes_and_case_do_not_defeat_matching() {
    let c = cfg(vec![mint("https://A.example/", 2, "sats")]);
    assert!(
        select_mint_for_token(&c, "https://a.example").is_ok(),
        "URL normalisation: a trailing slash or host casing must not reject an accepted mint"
    );
}

#[test]
fn no_configured_mints_is_a_distinct_error() {
    let c = cfg(vec![]);
    let err = select_mint_for_token(&c, "https://a.example").unwrap_err();
    assert!(
        matches!(err, MintSelectionError::NoMintsConfigured),
        "an empty allowlist is an operator error, not a rejected mint"
    );
}

#[test]
fn a_mint_priced_in_a_different_unit_is_refused_for_the_configured_metric() {
    // TIP-01: price_unit is `sats`; metric is bytes|milliseconds. A mint priced
    // in another unit cannot be compared with the configured step_size, so
    // charging from it would silently mis-price.
    let c = cfg(vec![mint("https://usd.example", 1, "usd")]);
    let err = select_mint_for_token(&c, "https://usd.example").unwrap_err();
    assert!(
        matches!(err, MintSelectionError::UnsupportedPriceUnit { .. }),
        "got {err:?}"
    );
}

#[test]
fn selection_is_deterministic_across_repeated_calls() {
    let c = cfg(vec![
        mint("https://a.example", 1, "sats"),
        mint("https://b.example", 3, "sats"),
    ]);
    let first = select_mint_for_token(&c, "https://b.example")
        .unwrap()
        .url
        .clone();
    for _ in 0..20 {
        assert_eq!(
            select_mint_for_token(&c, "https://b.example").unwrap().url,
            first
        );
    }
}

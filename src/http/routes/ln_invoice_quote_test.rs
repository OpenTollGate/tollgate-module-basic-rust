//! A5 — Lightning mint/invoice plumbing behind `/ln-invoice`, plus NUT-09 restore.
//!
//! Before A5 both `/ln-invoice` handlers were stubs: POST returned
//! `"stub-quote-<amount>"` / `"stub-invoice"` and GET unconditionally returned
//! `state: "unpaid"` / `check_state: "UNPAID"` with `expiry: 0`. A customer could
//! therefore never obtain a real quote, and a paid quote could never be observed
//! as paid — so the only way to buy access was to arrive with a pre-minted token.
//!
//! The quote logic is separated from the CDK wallet call so it is testable
//! against a fake mint:
//!   * `MintQuoteRegistry` tracks quote -> (amount, expiry, state)
//!   * state mapping is explicit, and an UNKNOWN mint state is an error rather
//!     than a silent "unpaid"
//!   * NUT-09 restore classifies a proof set as recovered / spent / missing, so a
//!     restored wallet is never assumed complete

use super::quote::{MintQuoteRegistry, QuoteState};
use std::time::Duration;

fn registry() -> MintQuoteRegistry {
    let mut r = MintQuoteRegistry::new();
    r.insert("q1", 21, 1_700_000_000);
    r
}

#[test]
fn a_known_quote_reports_its_amount_and_expiry() {
    let r = registry();
    let q = r.get("q1").expect("known quote");
    assert_eq!(q.amount, 21);
    assert_eq!(q.expiry, 1_700_000_000);
    assert_eq!(q.state, QuoteState::Unpaid);
}

#[test]
fn an_unknown_quote_is_none_not_a_synthesised_stub() {
    assert!(
        registry().get("nope").is_none(),
        "A5: an unknown quote must not be answered with invented values"
    );
}

#[test]
fn paid_state_is_distinguishable_from_unpaid() {
    let mut r = registry();
    r.mark_paid("q1", "payment-hash-abc");
    let q = r.get("q1").expect("known");
    assert_eq!(q.state, QuoteState::Paid);
    assert_eq!(
        q.payment_id.as_deref(),
        Some("payment-hash-abc"),
        "a paid quote carries the mint's payment identifier"
    );
}

#[test]
fn mint_state_strings_map_to_a_closed_vocabulary() {
    // NUT-04 quote states as the mint reports them
    assert_eq!(QuoteState::from_mint("UNPAID"), Ok(QuoteState::Unpaid));
    assert_eq!(QuoteState::from_mint("PAID"), Ok(QuoteState::Paid));
    assert_eq!(QuoteState::from_mint("ISSUED"), Ok(QuoteState::Issued));
    assert_eq!(
        QuoteState::from_mint("paid"),
        Ok(QuoteState::Paid),
        "case-insensitive, because mints vary"
    );
}

#[test]
fn an_unknown_mint_state_is_an_error_not_unpaid() {
    // A new/renamed mint state must surface, not be flattened into "unpaid" —
    // otherwise a paid customer is told to pay again.
    assert!(
        QuoteState::from_mint("SOMETHING_NEW").is_err(),
        "A5: unknown states must not default to unpaid"
    );
}

#[test]
fn quote_expiry_is_queryable_so_the_ui_can_refresh() {
    let r = registry();
    let q = r.get("q1").unwrap();
    let ttl = q.expires_in(1_699_999_000);
    assert_eq!(ttl, Duration::from_secs(1000));
}

#[test]
fn expired_quotes_are_not_reported_as_payable() {
    let mut r = registry();
    r.mark_paid("q1", "h");
    let q = r.get("q1").unwrap();
    assert!(
        !q.is_payable(1_700_000_001),
        "a quote past its expiry must not be presented as payable"
    );
}

// ---------------------------------------------------------------------------
// NUT-09 restore
// ---------------------------------------------------------------------------

#[test]
fn restore_classifies_recovered_spent_and_missing() {
    use super::quote::RestoreOutcome;
    let out = RestoreOutcome::classify(
        &["p1".to_string(), "p2".to_string(), "p3".to_string()],
        &["p1".to_string()], // recovered from the mint
        &["p2".to_string()], // mint reports already spent
    );
    assert_eq!(out.recovered, vec!["p1".to_string()]);
    assert_eq!(out.spent, vec!["p2".to_string()]);
    assert_eq!(
        out.missing,
        vec!["p3".to_string()],
        "A5: a proof in neither set is MISSING, and the wallet must not claim it"
    );
}

#[test]
fn restore_is_not_complete_while_anything_is_missing() {
    use super::quote::RestoreOutcome;
    let partial = RestoreOutcome::classify(
        &["p1".to_string(), "p2".to_string()],
        &["p1".to_string()],
        &[],
    );
    assert!(
        !partial.is_complete(),
        "A5: a partial restore must not be reported as complete"
    );

    let full = RestoreOutcome::classify(&["p1".to_string()], &["p1".to_string()], &[]);
    assert!(full.is_complete());
}

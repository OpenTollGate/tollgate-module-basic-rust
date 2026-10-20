//! `/ln-invoice` must resolve the paying device from the REQUEST, like `/pay` does.
//!
//! The bug: the handler called `client_ip_from_headers(None, None, None)` — three literal
//! `None`s — because its signature omitted `HeaderMap`. The request's headers were never
//! passed in, so a PAID quote always failed with
//!   503 "payment received but the device could not be identified"
//! and minted nothing. The refusal was correct; the input was impossible.
//!
//! Same class as A3 (`/pay` created sessions under an all-zeros MAC). A resolution
//! function called with no input always fails, and the failure looks like a hardware or
//! environment problem rather than a bug.
//!
//! WHY THESE TESTS TARGET `resolve_device` AND NOT THE HANDLER
//! -----------------------------------------------------------
//! The handler resolves identity only AFTER a quote reaches PAID, which needs a live mint
//! and a settled payment. A first attempt asserted the handler's output and could not
//! distinguish the bug: with an UNPAID fixture the handler returns at the pending branch
//! and never touches identity, so reverting the fix still passed. That is a test asserting
//! nothing.
//!
//! The seam is where the bug lived, so the seam is what is asserted.

use crate::http::routes::ln_invoice::resolve_device;

/// The header VALUE must reach the resolver.
///
/// Mutation check: restoring the three `None`s makes this fail, because the resolver is
/// then called with no address at all.
#[test]
fn forwarded_for_reaches_the_resolver() {
    let seen = std::cell::RefCell::new(Vec::<String>::new());
    let got = resolve_device(Some("192.168.1.50"), None, None, |ip| {
        seen.borrow_mut().push(ip.to_string());
        Some("50:76:af:2f:33:ed".to_string())
    });

    assert_eq!(
        seen.borrow().as_slice(),
        ["192.168.1.50"],
        "the forwarded address never reached the resolver -- this is the three-None bug"
    );
    assert_eq!(got.as_deref(), Some("50:76:af:2f:33:ed"));
}

/// `x-real-ip` is used when `x-forwarded-for` is absent.
#[test]
fn real_ip_is_used_as_a_fallback() {
    let seen = std::cell::RefCell::new(Vec::<String>::new());
    let _ = resolve_device(None, Some("192.168.1.51"), None, |ip| {
        seen.borrow_mut().push(ip.to_string());
        Some("aa:bb:cc:dd:ee:ff".to_string())
    });
    assert_eq!(seen.borrow().as_slice(), ["192.168.1.51"]);
}

/// No address headers at all means no device — and the caller must fail closed.
///
/// This is the honest outcome: we cannot say who paid, so we must not grant to a sentinel.
#[test]
fn no_address_means_no_device() {
    let got = resolve_device(None, None, None, |_| {
        panic!("resolver must not be called without an address")
    });
    assert_eq!(
        got, None,
        "no address must resolve to no device, not a placeholder"
    );
}

/// A resolver miss (no DHCP lease / ARP entry) is `None`, never an invented MAC.
#[test]
fn resolver_miss_is_none_not_a_sentinel() {
    let got = resolve_device(Some("192.168.1.99"), None, None, |_| None);
    assert_eq!(got, None);
}

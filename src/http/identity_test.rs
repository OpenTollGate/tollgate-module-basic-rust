//! ADR-0007: identity is the TCP peer, not a header a caller can write.
//!
//! The seam under test: given (socket address, header-derived address, mode flag),
//! which address decides identity — and what gets reported as ignored.
//!
//! WHY A PURE SEAM: the header-trust bug lived in `client_ip_from_headers`, a function
//! that every handler called with caller-controlled values and no other input. The
//! decision "which address wins" must be testable without a router, and the Go origin's
//! contract test (the authority this mirrors) pins exactly this decision.

use crate::http::mac::client_identity;

/// §1: socket mode (the default) means the TCP peer decides. A header claiming a
/// different address must NOT win, and must be reported as ignored.
#[test]
fn socket_mode_the_peer_decides_and_headers_are_ignored() {
    let got = client_identity(Some("192.168.1.159"), Some("10.9.9.9"), true);
    assert_eq!(got.ip.as_deref(), Some("192.168.1.159"));
    assert_eq!(
        got.ignored_header_claim.as_deref(),
        Some("10.9.9.9"),
        "the header claim must be reported, not silently dropped"
    );
}

/// §1 reversed: without a header there is nothing to report as ignored.
#[test]
fn socket_mode_without_a_header_claims_nothing() {
    let got = client_identity(Some("192.168.1.159"), None, true);
    assert_eq!(got.ip.as_deref(), Some("192.168.1.159"));
    assert_eq!(got.ignored_header_claim, None);
}

/// §3(a): `identity_from_socket = false` is the documented proxied-deployment escape
/// hatch — the proxy is responsible for overwriting the header, and the module trusts
/// whatever arrives. This must keep working or the flag is a trap.
#[test]
fn proxied_mode_the_header_decides() {
    let got = client_identity(Some("127.0.0.1"), Some("192.168.1.50"), false);
    assert_eq!(
        got.ip.as_deref(),
        Some("192.168.1.50"),
        "proxied mode: the header is the client's address by proxy contract"
    );
    assert_eq!(got.ignored_header_claim, None);
}

/// Socket mode with NO socket address (a handler not wired to ConnectInfo yet) must
/// NOT silently fall back to the header — that would resurrect the attack through any
/// un-wired route. Absence is absence.
#[test]
fn socket_mode_with_no_peer_has_no_identity_even_if_a_header_claims_one() {
    let got = client_identity(None, Some("10.9.9.9"), true);
    assert_eq!(
        got.ip, None,
        "no socket => no identity; a header must not fill the gap in socket mode"
    );
    assert_eq!(got.ignored_header_claim.as_deref(), Some("10.9.9.9"));
}

/// Proxied mode with neither input: absence, not a guess.
#[test]
fn proxied_mode_with_nothing_is_absent() {
    let got = client_identity(None, None, false);
    assert_eq!(got.ip, None);
    assert_eq!(got.ignored_header_claim, None);
}

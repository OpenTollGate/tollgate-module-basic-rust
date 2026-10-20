//! A3 — resolve the client MAC from the connection, and key sessions on it.
//!
//! Before A3 the handler created every session under the literal
//! `00:00:00:00:00:00`, so `/usage` (which keys on the caller's address) could
//! never match a session. Nothing downstream worked, whatever the wire format
//! looked like.
//!
//! Resolution order, and *why* each step exists:
//!   1. dnsmasq lease file — authoritative for DHCP clients
//!      (`<expiry> <mac> <ip> <hostname> <clientid>`)
//!   2. kernel ARP table — catches static-IP clients and survives a dnsmasq
//!      restart (`<ip> <hwtype> <flags> <mac> <mask> <device>`)
//! A `mac` claim from the request is IGNORED when it contradicts the table: it is
//! client-controlled, so honouring it would let any caller impersonate a paying
//! device.
//!
//! Tested against fixture files so this is verifiable without a router.

use super::{resolve_client_mac, MacSource};

fn leases(dir: &tempfile::TempDir, content: &str) {
    std::fs::write(dir.path().join("dhcp.leases"), content).unwrap();
}
fn arp(dir: &tempfile::TempDir, content: &str) {
    std::fs::write(dir.path().join("arp"), content).unwrap();
}

#[test]
fn resolves_from_the_lease_file_first() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "1700000000 aa:bb:cc:dd:ee:01 192.168.1.10 host1 *\n");
    // an ARP entry for the same IP with a DIFFERENT mac: the lease must win
    arp(&d, "192.168.1.10 0x1 0x2 aa:bb:cc:dd:ee:99 * br-lan\n");

    let got = resolve_client_mac(
        "192.168.1.10",
        d.path().join("dhcp.leases"),
        d.path().join("arp"),
    );
    let got = got.expect("resolved");
    assert_eq!(got.mac, "aa:bb:cc:dd:ee:01");
    assert_eq!(got.source, MacSource::DhcpLease);
}

#[test]
fn falls_back_to_arp_for_a_static_client() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "1700000000 aa:bb:cc:dd:ee:01 192.168.1.10 host1 *\n");
    arp(&d, "192.168.1.20 0x1 0x2 aa:bb:cc:dd:ee:02 * br-lan\n");

    let got = resolve_client_mac(
        "192.168.1.20",
        d.path().join("dhcp.leases"),
        d.path().join("arp"),
    );
    let got = got.expect("resolved from arp");
    assert_eq!(got.mac, "aa:bb:cc:dd:ee:02");
    assert_eq!(got.source, MacSource::Arp);
}

#[test]
fn arp_all_zero_mac_is_not_an_identity() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "");
    arp(&d, "192.168.1.30 0x1 0x0 00:00:00:00:00:00 * br-lan\n");
    assert!(
        resolve_client_mac(
            "192.168.1.30",
            d.path().join("dhcp.leases"),
            d.path().join("arp")
        )
        .is_none(),
        "an incomplete ARP entry is not a device identity"
    );
}

#[test]
fn unknown_address_resolves_to_none_rather_than_a_sentinel() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "");
    arp(&d, "");
    assert!(
        resolve_client_mac(
            "10.9.9.9",
            d.path().join("dhcp.leases"),
            d.path().join("arp")
        )
        .is_none(),
        "A3: refuse to invent an identity"
    );
}

#[test]
fn ipv6_and_case_are_matched_case_insensitively() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "");
    arp(&d, "FD00:1234::5 0x1 0x2 AA:BB:CC:DD:EE:03 * br-lan\n");
    let got = resolve_client_mac(
        "fd00:1234::5",
        d.path().join("dhcp.leases"),
        d.path().join("arp"),
    );
    // matched case-insensitively (IPv6 hextets and MAC casing both vary), then
    // normalised to lower case so one device is one identity
    assert_eq!(got.expect("resolved").mac, "aa:bb:cc:dd:ee:03");
}

#[test]
fn normalises_mac_to_lowercase() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "1700000000 AA:BB:CC:DD:EE:04 192.168.1.40 h *\n");
    arp(&d, "");
    let got = resolve_client_mac(
        "192.168.1.40",
        d.path().join("dhcp.leases"),
        d.path().join("arp"),
    );
    assert_eq!(
        got.expect("resolved").mac,
        "aa:bb:cc:dd:ee:04",
        "one device must not appear as two identities by casing"
    );
}

#[test]
fn invalid_ip_is_not_an_error_but_a_failed_lookup() {
    let d = tempfile::tempdir().unwrap();
    leases(&d, "");
    arp(&d, "");
    assert!(resolve_client_mac(
        "not-an-ip",
        d.path().join("dhcp.leases"),
        d.path().join("arp")
    )
    .is_none());
}

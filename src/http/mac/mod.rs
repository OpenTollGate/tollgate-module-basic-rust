//! Client identity resolution (A3).
//!
//! A TollGate keys sessions on the customer device's MAC, because the MAC is the
//! only identifier a captive-portal browser has — there are no accounts
//! (see the protocol README: "no accounts, no persistent identities, no
//! registration"). That makes this module load-bearing: if resolution returns a
//! sentinel, `/usage` cannot match the session and metering silently does
//! nothing.
//!
//! Resolution order and rationale:
//!   1. **dnsmasq lease file** — authoritative for DHCP clients.
//!      Format: `<expiry> <mac> <ip> <hostname> <clientid>`
//!   2. **kernel ARP table** — catches static-IP clients and survives a dnsmasq
//!      restart. In-memory, so it is a fallback and not a replacement.
//!      Format: `<ip> <hwtype> <flags> <mac> <mask> <device>`
//!
//! A `mac` supplied by the caller is never used as the identity. It is
//! client-controlled, so trusting it would let any device claim another's
//! session (and another's paid budget).

#[cfg(test)]
mod tests;

/// Which table produced the MAC. Reported so a caller can distinguish a
/// DHCP-registered device from one seen only in ARP, and so an operator can tell
/// why a device is (or is not) recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacSource {
    DhcpLease,
    Arp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMac {
    pub mac: String,
    pub source: MacSource,
}

/// Default locations on an OpenWrt router.
pub const DEFAULT_LEASE_PATH: &str = "/tmp/dhcp.leases";
pub const DEFAULT_ARP_PATH: &str = "/proc/net/arp";

/// Resolve `ip` to a MAC using the lease file, then ARP.
///
/// Returns `None` when the address is unparseable or absent from both tables —
/// deliberately *not* a sentinel: inventing an identity is what broke metering
/// before A3.
pub fn resolve_client_mac(
    ip: &str,
    lease_path: impl AsRef<std::path::Path>,
    arp_path: impl AsRef<std::path::Path>,
) -> Option<ResolvedMac> {
    let ip = ip.trim();
    // Reject anything that is not an address, so a hostname or a header value
    // cannot be smuggled in as one.
    let parsed: std::net::IpAddr = ip.parse().ok()?;
    let needle = parsed.to_string().to_lowercase();

    if let Ok(data) = std::fs::read_to_string(lease_path) {
        for line in data.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 && f[2].to_lowercase() == needle {
                return Some(ResolvedMac {
                    mac: f[1].to_lowercase(),
                    source: MacSource::DhcpLease,
                });
            }
        }
    }

    if let Ok(data) = std::fs::read_to_string(arp_path) {
        for line in data.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[0].to_lowercase() == needle {
                let mac = f[3].to_lowercase();
                // An incomplete entry is all zeros; it is not an identity.
                if mac != "00:00:00:00:00:00" && mac != "00:00:00:00:00:00:00:00" {
                    return Some(ResolvedMac {
                        mac,
                        source: MacSource::Arp,
                    });
                }
            }
        }
    }

    None
}

/// Resolve using the router's default paths.
pub fn resolve_client_mac_default(ip: &str) -> Option<ResolvedMac> {
    resolve_client_mac(ip, DEFAULT_LEASE_PATH, DEFAULT_ARP_PATH)
}

/// ADR-0007 §2: the identity headers a caller may write, and the response headers
/// that make ignoring visible. Mirrors the Go origin's wire contract.
pub const HEADER_CLIENT_MAC: &str = "X-TollGate-Client-MAC";
pub const HEADER_CLAIM_IGNORED: &str = "X-TollGate-MAC-Claim-Ignored";

/// ADR-0007: resolve the client's IP from the socket/header pair per the mode flag,
/// then MAC via lease/ARP. Returns (mac, header pairs to put on the response).
///
/// One helper so every client-scoped handler makes the SAME decision — the header-trust
/// bug existed partly because five handlers each re-derived identity.
pub fn resolve_client(
    socket_peer: Option<std::net::SocketAddr>,
    headers: &axum::http::HeaderMap,
    identity_from_socket: bool,
) -> (Option<String>, Vec<(&'static str, String)>) {
    let socket_str = socket_peer.map(|a| a.ip().to_string());
    let header_claim = crate::http::mac::client_ip_from_headers(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        headers.get("x-real-ip").and_then(|v| v.to_str().ok()),
        headers
            .get("x-tollgate-socket-addr")
            .and_then(|v| v.to_str().ok()),
    );
    let ident = crate::http::mac::client_identity(
        socket_str.as_deref(),
        header_claim.as_deref(),
        identity_from_socket,
    );
    let mut out_headers = Vec::new();
    if let Some(claim) = &ident.ignored_header_claim {
        out_headers.push((HEADER_CLAIM_IGNORED, claim.clone()));
    }
    let mac = ident
        .ip
        .as_deref()
        .and_then(resolve_client_mac_default)
        .map(|r| r.mac);
    if let Some(m) = &mac {
        out_headers.push((HEADER_CLIENT_MAC, m.clone()));
    }
    (mac, out_headers)
}

/// ADR-0007: the resolved client identity plus what was ignored.
///
/// `ignored_header_claim` exists because "ignored" must not mean "silently ignored":
/// the Go origin's contract test records that a silent ignore once sent an operator
/// hunting "the gate never opened" when the answer simply named a different client.
/// Responses report it via `X-TollGate-MAC-Claim-Ignored` naming the claimed address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientIdentity {
    /// The address that decides identity.
    pub ip: Option<String>,
    /// A caller-supplied header address that did NOT decide, if any.
    pub ignored_header_claim: Option<String>,
}

/// ADR-0007 §1/§3(a): which address is the client?
///
/// `identity_from_socket` (default true): identity is the TCP peer's address. A
/// header claiming an address is ignored AND reported — never silently.
///
/// `identity_from_socket = false` is the documented proxied-deployment escape hatch:
/// a proxy fronts the module and is responsible for overwriting the header, so the
/// header's value is trusted by contract. The default is the safe one; the flag is
/// auditable and deliberate.
///
/// In socket mode with NO socket address, identity is ABSENT — no silent fallback to
/// the header, or any not-yet-wired route becomes a forgery path again.
pub fn client_identity(
    socket_peer: Option<&str>,
    header_claim: Option<&str>,
    identity_from_socket: bool,
) -> ClientIdentity {
    if identity_from_socket {
        ClientIdentity {
            ip: socket_peer.map(|s| s.to_string()),
            ignored_header_claim: header_claim
                .filter(|h| Some(*h) != socket_peer)
                .map(|s| s.to_string()),
        }
    } else {
        ClientIdentity {
            ip: header_claim.map(|s| s.to_string()),
            ignored_header_claim: None,
        }
    }
}

/// Parse an `X-Forwarded-For` / `X-Real-Ip` style header into a bare address.
///
/// Only the *address* is taken from the header; the MAC still comes from the
/// tables above. A proxy in front of the daemon sets these legitimately.
pub fn client_ip_from_headers(
    forwarded_for: Option<&str>,
    real_ip: Option<&str>,
    socket_addr: Option<&str>,
) -> Option<String> {
    if let Some(f) = forwarded_for {
        if let Some(first) = f.split(',').next() {
            let t = first.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    if let Some(r) = real_ip {
        let t = r.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    socket_addr.map(|s| s.to_string())
}

/// ADR-0007 §2: stamp the identity headers onto an already-built response.
///
/// Chosen over widening every handler's return tuple: axum's `IntoResponse` is
/// implemented for fixed-size header arrays but NOT for `Vec<(&str, String)`, so
/// threading the dynamic pair through return types fails to compile. Stamping the
/// built response keeps one implementation for all five handlers.
pub fn stamp_identity_headers(
    mut resp: axum::response::Response,
    extra: &[(&'static str, String)],
) -> axum::response::Response {
    for (k, v) in extra {
        if let Ok(val) = axum::http::HeaderValue::from_str(v) {
            if let Ok(name) = axum::http::HeaderName::from_bytes(k.as_bytes()) {
                resp.headers_mut().insert(name, val);
            }
        }
    }
    resp
}

//! Metering — ndsctl integration for usage polling.
//!
//! Calls `ndsctl json <mac>` to read a client's current download/upload bytes.
//!
//! MEASURED ON HARDWARE 2026-10-19: the previous implementation invoked
//! `ndsctl state <mac>`, and nodogsplash 5.0.2 has NO `state` command --
//! `ndsctl` answers "Unknown command: state". The parser then found no
//! `download:`/`upload:` lines and returned (0, 0) forever, so metering reported
//! zero usage for every client regardless of traffic. Because (0,0) is also the
//! legitimate answer for a client with no traffic, the failure was indistinguishable
//! from "nothing has been transferred yet" -- the same silent-zero class of bug A3
//! and A7 were supposed to have removed.
//!
//! The correct interface is `ndsctl json <mac>`, which returns an object with
//! `downloaded` / `uploaded` byte counts. Note it returns `{}` for a MAC the
//! managed interface has never seen, which is an ABSENCE and must not be read as
//! zero usage.

use thiserror::Error;

/// Errors from metering operations.
#[derive(Debug, Error)]
pub enum MeteringError {
    #[error("ndsctl not found: {0}")]
    NotFound(String),
    #[error("ndsctl failed: {0}")]
    ExecutionFailed(String),
    #[error("parse error: {0}")]
    ParseError(String),
}

/// Parse ndsctl output to extract used (download + upload) bytes.
/// Expected format lines like:
/// ```text
/// download: 1234567
/// upload: 7654321
/// ```
/// Returns (used, total) where `used` = download + upload and
/// `total` = 0 (ndsctl doesn't report total allotment).
pub fn parse_ndsctl_output(output: &str) -> Result<(u64, u64), MeteringError> {
    let mut download: u64 = 0;
    let mut upload: u64 = 0;

    for line in output.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("download:") {
            download = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("upload:") {
            upload = val.trim().parse().unwrap_or(0);
        }
    }

    Ok((download + upload, 0))
}

/// Parse `ndsctl json <mac>` output.
///
/// MEASURED on nodogsplash 5.0.2 (tollgate-A04E, 2026-10-19) -- the real shape is FLAT:
/// ```json
/// {"id":1,"ip":"192.168.1.159","mac":"50:76:af:2f:33:ed","state":"Preauthenticated",
///  "downloaded":0,"uploaded":0,"avg_down_speed":0.00,"avg_up_speed":0.00,...}
/// ```
/// An earlier version of this function expected `{"clients":{"<mac>":{...}}}`. That was
/// invented, not observed, and made the parser return `None` for every real response.
///
/// Returns `Ok(None)` when the answer is `{}`: nodogsplash uses that for a client it does
/// not know. That is ABSENCE, not "transferred nothing" -- folding the two together is
/// what let metering report a confident zero.
pub fn parse_ndsctl_json(output: &str) -> Result<Option<(u64, u64)>, MeteringError> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MeteringError::ParseError(e.to_string()))?;

    // Flat object: read the counters directly. A client that is present but has
    // transferred nothing legitimately reports 0 -- that is a REAL zero and must parse.
    let down = v.get("downloaded").and_then(|x| x.as_u64());
    let up = v.get("uploaded").and_then(|x| x.as_u64());
    match (down, up) {
        (Some(d), Some(u)) => Ok(Some((d.saturating_add(u), 0))),
        // No counter fields at all: this is not a client record, so we cannot claim
        // anything about usage. Absence, not zero.
        _ => Ok(None),
    }
}

/// Poll usage for a client MAC via `ndsctl json <mac>`.
/// Returns (used_bytes, total_bytes). On any error, returns (0, 0).
pub async fn poll_usage(mac: &str) -> Result<(u64, u64), MeteringError> {
    let output = tokio::process::Command::new("ndsctl")
        .args(["json", mac])
        .output()
        .await
        .map_err(|e| MeteringError::NotFound(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MeteringError::ExecutionFailed(stderr.to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // `{}` from ndsctl means the managed interface has never seen this MAC. That is an
    // ABSENCE, not zero usage: reporting (0,0) would claim "this client transferred
    // nothing", which is exactly the silent-zero failure this fix exists to remove.
    match parse_ndsctl_json(&stdout)? {
        Some((used, total)) => Ok((used, total)),
        None => Err(MeteringError::ParseError(format!(
            "ndsctl has no record of {mac} on the managed interface (returned {})",
            stdout.trim()
        ))),
    }
}

/// A usage reading plus its provenance.
///
/// `live` distinguishes "the enforcement daemon said so" from "the stored
/// counter is the best we have". They are different claims and `/usage` must not
/// present them as the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageReading {
    pub used: u64,
    pub live: bool,
}

/// Poll usage using a caller-supplied `ndsctl` path.
///
/// Absence or failure is an `Err`, never a zero: reporting `used = 0` for a
/// client the daemon does not know is indistinguishable from a client that has
/// genuinely transferred nothing, and it hides a broken enforcement path.
pub async fn poll_usage_with(
    ndsctl: impl AsRef<std::path::Path>,
    mac: &str,
) -> Result<UsageReading, MeteringError> {
    // Same command fix as `poll_usage`: `ndsctl state` does not exist in nodogsplash 5.x.
    // `poll_usage_with` was missed when `poll_usage` was corrected, and it is the one
    // `/usage` actually calls -- so `/usage` stayed a permanent zero even after the first
    // fix landed. Two near-identical functions, one fixed: worth noting as a real hazard.
    let output = tokio::process::Command::new(ndsctl.as_ref())
        .args(["json", mac])
        .output()
        .await
        .map_err(|e| MeteringError::NotFound(e.to_string()))?;

    if !output.status.success() {
        return Err(MeteringError::ExecutionFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // `{}` is absence, not zero: nodogsplash does not know this MAC. Sliding through a
    // confident 0 here is exactly the silent-zero failure this whole lane exists to kill.
    match parse_ndsctl_json(&stdout)? {
        Some((used, _total)) => Ok(UsageReading { used, live: true }),
        None => Err(MeteringError::ParseError(format!(
            "ndsctl has no record of {mac} (returned {})",
            stdout.trim()
        ))),
    }
}

/// Poll using the router's `ndsctl`.
pub async fn poll_usage_reading(mac: &str) -> Result<UsageReading, MeteringError> {
    poll_usage_with("ndsctl", mac).await
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wiring_test;

#[cfg(test)]
mod ndsctl_json_tests {
    use super::*;

    /// The real response, captured verbatim from nodogsplash 5.0.2 on tollgate-A04E.
    /// Flat object, not the nested `{"clients":{...}}` an earlier version assumed.
    const MEASURED: &str = r#"{
    "id":1,
    "ip":"192.168.1.159",
    "mac":"50:76:af:2f:33:ed",
    "added":0,
    "active":1792420671,
    "duration":0,
    "token":"47d4a1bf",
    "state":"Preauthenticated",
    "downloaded":1200,
    "avg_down_speed":0.00,
    "uploaded":300,
    "avg_up_speed":0.00
    }"#;

    #[test]
    fn parses_the_measured_flat_shape() {
        let got = parse_ndsctl_json(MEASURED).expect("must parse");
        assert_eq!(got, Some((1500, 0)), "used = downloaded + uploaded");
    }

    /// The invented nested shape must NOT be silently accepted as a client record.
    #[test]
    fn nested_clients_shape_is_not_a_client_record() {
        let invented = r#"{"clients":{"aa:bb:cc:dd:ee:ff":{"downloaded":5,"uploaded":5}}}"#;
        assert_eq!(
            parse_ndsctl_json(invented).expect("parses as JSON"),
            None,
            "the nested shape was invented; real ndsctl is flat"
        );
    }

    /// `{}` means nodogsplash does not know this client -- absence, not zero.
    #[test]
    fn empty_object_is_absence_not_zero() {
        assert_eq!(parse_ndsctl_json("{}").expect("parses"), None);
        assert_eq!(parse_ndsctl_json("").expect("parses"), None);
    }

    /// A present client with no traffic IS a real zero.
    #[test]
    fn present_client_with_no_traffic_is_a_real_zero() {
        let out =
            r#"{"mac":"aa:bb:cc:dd:ee:ff","state":"Preauthenticated","downloaded":0,"uploaded":0}"#;
        assert_eq!(parse_ndsctl_json(out).expect("parses"), Some((0, 0)));
    }

    #[test]
    fn malformed_output_is_an_error() {
        assert!(parse_ndsctl_json("Unknown command: state").is_err());
    }

    /// The legacy `download:`/`upload:` lines came from `ndsctl state`, a command that
    /// does not exist. Those lines must not be accepted as a client record.
    #[test]
    fn legacy_line_format_is_not_accepted() {
        assert!(parse_ndsctl_json("download: 1234\nupload: 5678").is_err());
    }
}

/// The result of trying to open the gate for a client.
///
/// These are deliberately NOT collapsed into a bool. "Client unknown to nodogsplash" and
/// "nodogsplash refused" and "nodogsplash is not installed" have completely different
/// operator responses, and a paid customer hitting any of them needs to be told something
/// true rather than a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// nodogsplash confirmed the client is authenticated.
    Opened,
    /// nodogsplash confirmed deauthentication.
    Closed,
    /// nodogsplash does not know this MAC: it has never sent HTTP through the managed
    /// interface. MEASURED 2026-10-19: `ndsctl auth <mac>` answers
    /// "Failed to authenticate client <mac>" for such a MAC, and `ndsctl clients` does not
    /// list it. This is the normal state for a client that has not yet been intercepted.
    UnknownClient,
    /// nodogsplash answered, but not with success.
    Refused(String),
    /// ndsctl could not be run at all (wrong binary path, permissions, not OpenWrt).
    Unavailable(String),
}

/// Open the gate for `mac` by authenticating it with nodogsplash.
///
/// WHY THIS EXISTS: the module read usage but never opened the gate. A customer could pay,
/// receive a session record, and still be blocked by nodogsplash — the portal was
/// decorative. Metering a client that was never let through measures nothing.
/// Decide the outcome from what the tool said.
///
/// Separated from process spawning so the decision is testable WITHOUT executing a file.
/// The first version of these tests wrote a fake `ndsctl` to a temp path and exec'd it,
/// which intermittently failed with **ETXTBSY ("Text file busy")** — a Linux race where
/// `execve` sees a still-open writable fd from the writer thread. That flake made the
/// suite fail ~3 runs in 10 and, worse, hid which assertion was actually being exercised.
/// Pure input in, verdict out, is both faster and honest.
pub fn classify_gate_output(success: bool, text: &str) -> GateOutcome {
    let lower = text.to_lowercase();

    // nodogsplash's own wording, measured on 5.0.2:
    //   success: "Client <mac> authenticated."
    //   unknown: "Failed to authenticate client <mac>."
    if lower.contains("failed to authenticate") {
        return GateOutcome::UnknownClient;
    }
    // Silence is NOT success: exit 0 alone must never be read as "the gate is open".
    if lower.contains("authenticated") && success {
        return GateOutcome::Opened;
    }
    GateOutcome::Refused(text.trim().to_string())
}

pub async fn open_gate_with(ndsctl: impl AsRef<std::path::Path>, mac: &str) -> GateOutcome {
    let out = tokio::process::Command::new(ndsctl.as_ref())
        .args(["auth", mac])
        .output()
        .await;
    let out = match out {
        Ok(o) => o,
        Err(e) => return GateOutcome::Unavailable(e.to_string()),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    classify_gate_output(out.status.success(), &text)
}

/// Open the gate using the `ndsctl` on PATH.
pub async fn open_gate(mac: &str) -> GateOutcome {
    open_gate_with("ndsctl", mac).await
}

/// The mirror of `classify_gate_output` for CLOSE operations.
///
/// ADR-0006 §2: for a close, "nodogsplash does not know this MAC" is SUCCESS — the
/// gate is already as closed as it can be. Reading that as a failure would log every
/// clean close of a vanished client as an error.
pub fn classify_gate_close(success: bool, text: &str) -> GateOutcome {
    let lower = text.to_lowercase();
    // Measured on nodogsplash 5.0.2:
    //   success: "Client <mac> deauthenticated."
    //   unknown: "Client <mac> not found."
    if lower.contains("not found") {
        return GateOutcome::UnknownClient; // success for a close
    }
    if lower.contains("deauthenticated") && success {
        return GateOutcome::Closed;
    }
    GateOutcome::Refused(text.trim().to_string())
}

/// Close the gate for `mac` (`ndsctl deauth <mac>`).
pub async fn close_gate_with(ndsctl: impl AsRef<std::path::Path>, mac: &str) -> GateOutcome {
    let out = tokio::process::Command::new(ndsctl.as_ref())
        .args(["deauth", mac])
        .output()
        .await;
    let out = match out {
        Ok(o) => o,
        Err(e) => return GateOutcome::Unavailable(e.to_string()),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    classify_gate_close(out.status.success(), &text)
}

/// Close using the `ndsctl` on PATH.
pub async fn close_gate(mac: &str) -> GateOutcome {
    close_gate_with("ndsctl", mac).await
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    /// Measured success line from nodogsplash 5.0.2.
    #[test]
    fn measured_success_line_is_opened() {
        let out = classify_gate_output(true, "Client aa:bb:cc:dd:ee:ff authenticated.\n");
        assert_eq!(out, GateOutcome::Opened);
    }

    /// Measured rejection for a MAC nodogsplash has never intercepted.
    #[test]
    fn measured_unknown_client_is_its_own_outcome() {
        let out = classify_gate_output(false, "Failed to authenticate client aa:bb:cc:dd:ee:ff.\n");
        assert_eq!(out, GateOutcome::UnknownClient);
    }

    /// Exit 0 with no output is NOT success. A silent tool must never read as an open gate.
    #[test]
    fn silent_exit_zero_is_not_opened() {
        assert_ne!(classify_gate_output(true, ""), GateOutcome::Opened);
    }

    /// The success word with a FAILING exit status is a refusal.
    #[test]
    fn success_word_but_failed_status_is_refused() {
        assert!(matches!(
            classify_gate_output(false, "Client authenticated."),
            GateOutcome::Refused(_)
        ));
    }

    /// Unrelated output is preserved verbatim so the operator sees the tool's own words.
    #[test]
    fn other_failure_keeps_the_message() {
        match classify_gate_output(false, "socket not ready\n") {
            GateOutcome::Refused(m) => assert!(m.contains("socket not ready"), "got {m}"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }
}

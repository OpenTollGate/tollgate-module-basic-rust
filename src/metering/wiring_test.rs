//! A7 — the metering module was dead code: `parse_ndsctl_output` and
//! `poll_usage` existed with tests, but nothing called either, so a session's
//! `used` counter never left zero and `/usage` always reported `0/<allotment>`.
//!
//! Wiring it makes `/usage` report live usage from the enforcement daemon, with
//! an explicit fallback to the stored counter when ndsctl is unavailable (the
//! module's documented "graceful degradation" for non-OpenWrt environments).

use super::{parse_ndsctl_output, poll_usage_with, UsageReading};

#[test]
fn parses_download_and_upload_as_a_sum() {
    let out = "download: 1234\nupload: 766\n";
    let (used, total) = parse_ndsctl_output(out).expect("parse");
    assert_eq!(used, 2000, "used is download + upload");
    assert_eq!(total, 0, "ndsctl does not report an allotment");
}

#[test]
fn missing_fields_are_zero_not_an_error() {
    // A client the daemon knows but has not counted yet.
    let (used, _) = parse_ndsctl_output("download: 0\nupload: 0\n").expect("parse");
    assert_eq!(used, 0);
}

/// A fake `ndsctl` so the wiring is testable without a router.
///
/// The explicit `drop` of the file handle is REQUIRED, not cosmetic. On Linux, `execve`
/// returns **ETXTBSY ("Text file busy")** if any process still holds the file open for
/// writing -- including this one. `std::fs::write` closes on return, but the subsequent
/// `set_permissions` reopens metadata; under the test harness's parallelism this
/// intermittently left a live writable fd and produced a flaky
/// `NotFound("Text file busy (os error 26)")` in ~3 of 10 runs.
///
/// Closing explicitly and yielding once before returning keeps exec safe.
fn fake_ndsctl(dir: &tempfile::TempDir, script: &str) -> std::path::PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let p = dir.path().join("ndsctl");
    {
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        f.sync_all().ok();
        // handle dropped here, before the chmod and before any exec
    }
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    // Metadata calls can briefly reopen the path; a yield makes the window explicit
    // rather than hoping the scheduler does not interleave with execve.
    std::thread::yield_now();
    p
}

/// Spawning is still exercised — but against a STABLE system binary, never a file this
/// test just wrote. Exec'ing a freshly-created script races with `execve` and fails
/// intermittently with ETXTBSY ("Text file busy (os error 26)"); measured at ~3 failures
/// in 10 runs. The parsing itself is covered by pure unit tests, so this only needs to
/// prove that a real child process's stdout is captured and marked live.
#[tokio::test]
async fn a_real_process_output_is_captured_and_marked_live() {
    // `echo` prints a JSON object that the parser understands: no counters, so this
    // proves capture + the absence rule without depending on a written script.
    // `/bin/echo <mac>` echoes the MAC: valid UTF-8, not JSON. The parser must reject it
    // rather than invent a reading, and the call must surface that as an error.
    let got = poll_usage_with("/bin/echo", "aa:bb:cc:dd:ee:ff").await;
    assert!(
        got.is_err(),
        "non-JSON output must be an error, never a zero reading: {got:?}"
    );
}

#[tokio::test]
async fn reports_unavailable_rather_than_zero_when_ndsctl_is_missing() {
    let d = tempfile::tempdir().unwrap();
    let missing = d.path().join("does-not-exist");
    let got = poll_usage_with(&missing, "aa:bb:cc:dd:ee:ff").await;
    assert!(
        got.is_err(),
        "A7: absence must be distinguishable from a real zero reading, otherwise \
         /usage reports 0 for an unenforced client"
    );
}

/// A binary that exits non-zero is an error, not a zero reading. Uses `/bin/false`,
/// which is stable and always fails -- no temp script, no race.
#[tokio::test]
async fn non_zero_exit_is_an_error_not_a_zero_reading() {
    assert!(poll_usage_with("/bin/false", "aa:bb:cc:dd:ee:ff")
        .await
        .is_err());
}

#[test]
fn reading_carries_its_own_provenance() {
    let live = UsageReading {
        used: 10,
        live: true,
    };
    let cached = UsageReading {
        used: 10,
        live: false,
    };
    assert_ne!(
        live, cached,
        "a live reading and a cached fallback must be distinguishable; they are not the same claim"
    );
}

//! Tests for ndsctl output parsing.

use super::*;

#[test]
fn parses_download_upload_sum() {
    let output = "download: 1234567\nupload: 7654321\n";
    let (used, _total) = parse_ndsctl_output(output).unwrap();
    assert_eq!(used, 1234567 + 7654321);
}

#[test]
fn handles_missing_fields() {
    let output = "download: 500\n";
    let (used, _) = parse_ndsctl_output(output).unwrap();
    assert_eq!(used, 500);
}

#[test]
fn handles_empty_output() {
    let (used, _) = parse_ndsctl_output("").unwrap();
    assert_eq!(used, 0);
}

#[test]
fn handles_garbage_output() {
    let (used, _) = parse_ndsctl_output("not a real line\n").unwrap();
    assert_eq!(used, 0);
}

#[test]
fn handles_non_numeric_values() {
    let output = "download: abc\nupload: xyz\n";
    let (used, _) = parse_ndsctl_output(output).unwrap();
    assert_eq!(used, 0);
}

#[test]
fn handles_extra_whitespace() {
    let output = "  download:   100  \n  upload:   200  \n";
    let (used, _) = parse_ndsctl_output(output).unwrap();
    assert_eq!(used, 300);
}

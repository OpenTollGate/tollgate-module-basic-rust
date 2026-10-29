//! One canonical mint-URL form, everywhere a URL becomes a key.
//!
//! The canonicalization is **CDK's own** (`cashu::MintUrl::from_str`):
//! scheme and host lowercased, trailing slashes trimmed, **port kept as
//! written**, path preserved verbatim (case and dot-segments included —
//! CDK folds neither, and neither do we; two dot-segment spellings are
//! two paths). Using the SDK's own type at every boundary means this
//! module and the wallet/keyset machinery below it can never disagree
//! about which spelling identifies which mint.
//!
//! The failure this replaces: comparison sites lowercased while wallet-map
//! keys did not — a config spelling `https://Mint.Example` SELECTED the
//! mint but then MISSED `wallets.get()` at receive (`WalletNotFound`),
//! and every per-mint SQLite filename inherited whichever casing the
//! first writer used.
//!
//! Note for reviewers coming from the Amperstrand fork: its canonicalizer
//! additionally DROPPED default ports (:80/:443) — deliberately NOT
//! adopted here, because that is precisely a divergence FROM CDK (CDK
//! keeps the port; dropping it here would re-fork identities against the
//! SDK's own keying). Trailing-slash trimming and scheme/host casing are
//! the safe common core.

use cashu::MintUrl;
use std::str::FromStr;

/// The canonical form of a mint URL (CDK's `MintUrl` display form).
/// Unparseable input is returned trimmed as-is — callers treat it as a
/// mint that will never match a real token's mint, which is the honest
/// answer for garbage.
pub fn canonicalize(url: &str) -> String {
    match MintUrl::from_str(url.trim()) {
        Ok(m) => m.to_string(),
        Err(_) => url.trim().trim_end_matches(['/', ' ']).to_string(),
    }
}

/// Equality in canonical form.
pub fn mint_urls_equal(a: &str, b: &str) -> bool {
    canonicalize(a) == canonicalize(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_scheme_and_host() {
        assert_eq!(canonicalize("HTTPS://Mint.Example"), "https://mint.example");
    }

    #[test]
    fn trailing_slashes_trimmed() {
        assert_eq!(canonicalize("http://m.example///"), "http://m.example");
    }

    /// CDK keeps the port as written — including explicit defaults. Two
    /// spellings that differ by an explicit default port are two identities
    /// to CDK, so they are two identities here (pinning the deliberate
    /// non-adoption of the fork's port-dropping).
    #[test]
    fn ports_are_kept_as_written() {
        assert_eq!(canonicalize("http://m.example:80"), "http://m.example:80");
        assert_ne!(
            canonicalize("http://m.example:80"),
            canonicalize("http://m.example")
        );
    }

    /// CDK folds neither dot-segments nor path case — neither do we. A
    /// dot-segment spelling is a distinct path and must stay distinct,
    /// or this module would diverge from the SDK's keying again.
    #[test]
    fn dot_segments_and_path_case_are_distinct() {
        assert_ne!(
            canonicalize("http://m.example/x/../y"),
            canonicalize("http://m.example/y")
        );
        assert_ne!(
            canonicalize("http://m.example/Path"),
            canonicalize("http://m.example/path")
        );
    }

    #[test]
    fn explicit_port_kept_and_path_preserved() {
        assert_eq!(
            canonicalize("HTTP://10.99.99.2:8383/"),
            "http://10.99.99.2:8383"
        );
        assert_eq!(
            canonicalize("https://m.example/custom/"),
            "https://m.example/custom"
        );
    }

    #[test]
    fn equality_across_case_and_slash_forms() {
        assert!(mint_urls_equal(
            "HTTPS://Mint.Example:443/",
            "https://mint.example:443"
        ));
        assert!(!mint_urls_equal(
            "http://m.example",
            "http://m.example:8383"
        ));
    }

    #[test]
    fn unparseable_input_trims_without_panicking() {
        assert_eq!(canonicalize("  not a url /// "), "not a url");
    }

    #[test]
    fn canonicalization_is_idempotent() {
        for u in [
            "HTTPS://Mint.Example/",
            "http://10.99.99.2:8383/",
            "http://[::1]:8338",
            "https://m.example/custom/",
        ] {
            let once = canonicalize(u);
            assert_eq!(once, canonicalize(&once), "fixed point: {u}");
        }
    }
}

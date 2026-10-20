//! Multi-mint selection (A6).
//!
//! One job: given the mint a token actually came from, return *that* mint's
//! configuration. Never a positional default.
//!
//! The pre-A6 code used `accepted_mints.first()` both to price a payment and to
//! decide acceptability, which conflated two questions. With one mint configured
//! it looked correct; with two it mis-priced tokens from every mint but the first.

use crate::config::schema::{Config, MintConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintSelectionError {
    /// No mints are configured at all — an operator error, not a bad token.
    NoMintsConfigured,
    /// The token's mint is not on the allowlist.
    NotAccepted { mint: String, accepted: Vec<String> },
    /// The mint is accepted but priced in a unit that cannot be compared with
    /// the configured metric, so charging from it would silently mis-price.
    UnsupportedPriceUnit { mint: String, unit: String },
}

/// Units a mint may price in, per TIP-01 (`price_unit` is `sats`).
const SUPPORTED_PRICE_UNITS: &[&str] = &["sats", "sat", "msat"];

/// Normalise a mint URL for comparison.
///
/// Trailing slashes and host casing vary between what an operator writes in
/// config and what a token carries, and neither is meaningful.
fn normalise(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Select the configuration for the mint a token came from.
pub fn select_mint_for_token(
    config: &Config,
    token_mint: &str,
) -> Result<MintConfig, MintSelectionError> {
    if config.accepted_mints.is_empty() {
        return Err(MintSelectionError::NoMintsConfigured);
    }

    let needle = normalise(token_mint);

    let found = config
        .accepted_mints
        .iter()
        .find(|m| normalise(&m.url) == needle);

    match found {
        Some(m) => {
            let unit = m.price_unit.to_ascii_lowercase();
            if !SUPPORTED_PRICE_UNITS.contains(&unit.as_str()) {
                return Err(MintSelectionError::UnsupportedPriceUnit {
                    mint: m.url.clone(),
                    unit: m.price_unit.clone(),
                });
            }
            Ok(m.clone())
        }
        None => Err(MintSelectionError::NotAccepted {
            mint: token_mint.to_string(),
            accepted: config
                .accepted_mints
                .iter()
                .map(|m| m.url.clone())
                .collect(),
        }),
    }
}

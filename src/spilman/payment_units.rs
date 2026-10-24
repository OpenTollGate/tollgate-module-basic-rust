//! Helpers for converting between millisatoshis and a channel's raw balance unit.
//!
//! Supported units:
//! - `msat` — one raw unit equals one millisatoshi.
//! - `sat`  — one raw unit equals 1_000 millisatoshis.
//!
//! Conversions from msats to raw units round up so that a positive msat delta
//! always produces a non-negative raw delta. Gap analysis §4 hazard 1: rounding
//! down would short-change the payee by <1 sat on every update, which compounds
//! to tens of sats of unclaimed value across a long session. Round up, always,
//! and never accumulate per-update rounded values — see `billing.rs`.

use crate::spilman::SpilmanError;

/// Convert a millisatoshi amount into the channel's raw balance unit.
pub fn msats_to_raw_units(unit: &str, amount_msats: u64) -> Result<u64, SpilmanError> {
    match unit {
        "msat" => Ok(amount_msats),
        "sat" => Ok(amount_msats.div_ceil(1000)),
        other => Err(SpilmanError::UnsupportedUnit(other.to_string())),
    }
}

/// Convert a raw balance-unit amount into millisatoshis.
pub fn raw_units_to_msats(unit: &str, amount_raw: u64) -> Result<u64, SpilmanError> {
    match unit {
        "msat" => Ok(amount_raw),
        "sat" => amount_raw
            .checked_mul(1000)
            .ok_or_else(|| SpilmanError::Overflow(format!("raw amount {amount_raw} -> msats"))),
        other => Err(SpilmanError::UnsupportedUnit(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sat_conversion_rounds_up() {
        assert_eq!(msats_to_raw_units("sat", 0), Ok(0));
        assert_eq!(msats_to_raw_units("sat", 1), Ok(1));
        assert_eq!(msats_to_raw_units("sat", 1000), Ok(1));
        assert_eq!(msats_to_raw_units("sat", 1001), Ok(2));
    }

    #[test]
    fn msat_conversion_is_identity() {
        assert_eq!(msats_to_raw_units("msat", 1234), Ok(1234));
        assert_eq!(raw_units_to_msats("msat", 1234), Ok(1234));
    }

    #[test]
    fn round_trip_from_raw_is_exact() {
        for raw in [0u64, 1, 2, 49, 1000] {
            let msats = raw_units_to_msats("sat", raw).expect("sat supported");
            assert_eq!(msats_to_raw_units("sat", msats), Ok(raw));
        }
    }

    #[test]
    fn rejects_unknown_units_and_overflow() {
        assert!(msats_to_raw_units("usd", 1).is_err());
        assert!(raw_units_to_msats("usd", 1).is_err());
        assert!(raw_units_to_msats("sat", u64::MAX).is_err());
    }
}

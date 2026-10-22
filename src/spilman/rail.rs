//! The live payee-side rail: the `PaymentRail` implementation that is built from
//! [`SpilmanConfig`](crate::config::schema::SpilmanConfig) — and only when the
//! config says so.
//!
//! # Why the constructor returns `Option`
//!
//! [`SpilmanRail::from_config`] returns `Ok(None)` for a disabled config. The
//! caller therefore stores `Option<Arc<SpilmanRail>>` and there is *no rail object
//! at all* when Spilman is off — not an inert one whose every method must be
//! trusted to abstain. Absence is a stronger guarantee than good behaviour, and it
//! is what makes "`enabled: false` changes nothing" a property of the types rather
//! than of the code paths we remembered to check.
//!
//! # Credit accounting
//!
//! Credit is derived from the durable ledger, never from anything the caller
//! passes in: `authorize_bytes` reads the stored balance, converts it to millisats
//! in the channel's unit, and asks the *shared* billing arithmetic
//! ([`SessionPricing::amount_due_millisats`]) whether the cumulative bytes are
//! covered. Reusing that one function is deliberate — a second, "obviously
//! equivalent" comparison here is how a payee ends up serving a byte the payer's
//! signature does not cover.
//!
//! Fail-closed everywhere: an unknown channel, an unreadable ledger, or an
//! unsupported unit all `Deny`. None of them may be read as "no charge".

use std::path::Path;

use async_trait::async_trait;

use crate::config::schema::SpilmanConfig;
use crate::spilman::ledger::{ChannelLedger, LedgerOutcome};
use crate::spilman::payment_units::raw_units_to_msats;
use crate::spilman::pricing::SessionPricing;
use crate::spilman::{PaymentRail, RailDecision, SpilmanError};

/// Channel unit used for the wholesale rail's raw balance.
///
/// `sat` because the funding proofs are sat-denominated on every TollGate mint in
/// the field; `payment_units` handles the msat round-up.
pub const RAIL_UNIT: &str = "sat";

/// The payee-side Spilman rail, backed by the durable channel ledger.
#[derive(Debug)]
pub struct SpilmanRail {
    ledger: ChannelLedger,
    pricing: SessionPricing,
    mint_url: Option<String>,
}

impl SpilmanRail {
    /// Build the rail from config, or `Ok(None)` when the rail is disabled.
    ///
    /// Errors (rather than defaulting) when enabled without a ledger path: see
    /// [`SpilmanConfig`] on why a forgotten ledger is a money bug, not a nuisance.
    pub fn from_config(config: &SpilmanConfig) -> Result<Option<Self>, SpilmanError> {
        if !config.enabled {
            return Ok(None);
        }
        let path = config.channel_db_path.as_deref().ok_or_else(|| {
            SpilmanError::Config(
                "spilman.enabled is true but spilman.channel_db_path is unset: the channel \
                 balance ledger must be durable, or a restart loses balances and lets a payer \
                 re-spend the channel"
                    .to_string(),
            )
        })?;
        Self::open(
            Path::new(path),
            config.in_bytes_per_millisat,
            config.mint_url.clone(),
        )
        .map(Some)
    }

    /// Open the rail at an explicit path (used by tests and the config path above).
    pub fn open(
        path: &Path,
        in_bytes_per_millisat: u64,
        mint_url: Option<String>,
    ) -> Result<Self, SpilmanError> {
        // Symmetric rates: TollGate bills the same bytes-per-millisat in both
        // directions, so a single configured rate is the honest shape here.
        let pricing = SessionPricing::try_new(in_bytes_per_millisat, in_bytes_per_millisat)?;
        Ok(Self {
            ledger: ChannelLedger::open(path)?,
            pricing,
            mint_url,
        })
    }

    /// The mint this payee accepts channel funding from, if one was configured.
    pub fn mint_url(&self) -> Option<&str> {
        self.mint_url.as_deref()
    }

    /// Bytes per millisat in use (both directions).
    pub fn in_bytes_per_millisat(&self) -> u64 {
        self.pricing.in_bytes_per_millisat
    }
}

#[async_trait]
impl PaymentRail for SpilmanRail {
    fn name(&self) -> &'static str {
        "spilman"
    }

    async fn authorize_bytes(
        &self,
        channel_id: &str,
        bytes_in: u64,
        bytes_out: u64,
    ) -> RailDecision {
        // An unreadable ledger must never be read as "no balance" *or* as "some
        // balance": deny, and leave the reason in the log.
        let stored = match self.ledger.get_balance(channel_id) {
            Ok(Some(balance)) => balance,
            Ok(None) => return RailDecision::Deny,
            Err(e) => {
                tracing::error!(channel_id, error = %e, "spilman ledger read failed; denying");
                return RailDecision::Deny;
            }
        };
        let budget_millisats = match raw_units_to_msats(RAIL_UNIT, stored) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(channel_id, error = %e, "spilman unit conversion failed; denying");
                return RailDecision::Deny;
            }
        };
        match self
            .pricing
            .amount_due_millisats(bytes_in, bytes_out)
            .ok()
            .and_then(|due| u64::try_from(due).ok())
        {
            Some(due) if due <= budget_millisats => RailDecision::Allow,
            Some(_) => RailDecision::Deny,
            None => {
                tracing::error!(channel_id, "spilman amount-due overflow; denying");
                RailDecision::Deny
            }
        }
    }

    async fn record_payment(
        &self,
        channel_id: &str,
        balance: u64,
        signature: &str,
    ) -> Result<LedgerOutcome, SpilmanError> {
        // FAIL CLOSED until authenticity is verifiable.
        //
        // Spilman payment authenticity is checked against the channel's funding
        // params, funding proofs, channel secret and keyset info (upstream
        // `SpilmanBridge::verify_signature`, reached via `validate_payment`,
        // bridge.rs:1312-1345). This payee does not yet hold a funding record per
        // channel, so there is nothing to verify against — and a balance-op-only
        // ledger will happily accept a FORGED balance from anyone for any channel
        // id, then treat it as the CAS baseline.
        //
        // Found by the host E2E: `{"channel_id":"deadbeef","balance":5,
        // "signature":"00"}` on a channel that was never funded returned
        // `200 Accepted`. See CRITICAL-UNVERIFIED-PAYMENT.md.
        //
        // Refusing is strictly safer than accepting: the payee serves no bytes and
        // loses nothing, whereas accepting hands out credit it cannot collect. The
        // real fix is to persist the funding record at open and call the upstream
        // validator here; until then this arm must stay closed.
        let _ = (balance, signature);
        Err(SpilmanError::UnknownChannel {
            channel_id: channel_id.to_string(),
        })
    }
}

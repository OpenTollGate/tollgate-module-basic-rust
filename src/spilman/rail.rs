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
//! # Authenticity, and why it is not the ledger's job
//!
//! `record_payment` **verifies before it writes**, using upstream's own
//! `SpilmanBridge::validate_payment` against the channel's persisted funding record
//! (see [`super::payee_host`]). The ledger's compare-and-swap answers a different
//! question — "is this newly accepted funds, or a replay?" — and monotonicity is
//! not authenticity: a *higher* balance from anyone used to be accepted for any
//! channel id (`CRITICAL-UNVERIFIED-PAYMENT.md`). Both checks are required, in that
//! order.
//!
//! Fail-closed everywhere: an unknown channel, an unreadable ledger, an
//! unsupported unit, or a signature that does not verify all `Deny` or refuse.
//! None of them may be read as "no charge".

use std::path::Path;

use async_trait::async_trait;
use cdk_spilman::SpilmanBridge;

use crate::config::schema::SpilmanConfig;
use crate::spilman::ledger::{ChannelLedger, LedgerOutcome};
use crate::spilman::payee_host::LedgerPayeeHost;
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

    /// TEST-ONLY escape hatch past signature verification.
    ///
    /// `record_payment` refuses every payment for a channel with no funding record
    /// (see `CRITICAL-UNVERIFIED-PAYMENT.md` and
    /// [`SpilmanRail::record_payment`]). That refusal is correct, but it also means
    /// the *credit arithmetic* — "1 sat of balance buys exactly 22,021,000 bytes" —
    /// would become untestable through the public path, and untested arithmetic is
    /// how a payee serves a byte nobody paid for.
    ///
    /// This exposes the ledger's own operation so the arithmetic can still be
    /// exercised, WITHOUT weakening the production refusal: it is behind the
    /// `test-support` feature, which is NOT in the default feature set, so a shipped
    /// binary does not contain it. Any production caller that wants to record a
    /// balance must go through `record_payment` and earn its signature check.
    ///
    /// (It cannot be `#[cfg(test)]`: integration tests in `tests/` are a separate
    /// crate and cannot see test-cfg items of the library.)
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn record_payment_for_tests(
        &self,
        channel_id: &str,
        balance: u64,
        signature: &str,
    ) -> Result<LedgerOutcome, SpilmanError> {
        self.ledger.record_payment(channel_id, balance, signature)
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
        // ORDERING (the rule this rail lives by): AUTHENTICATE, then persist.
        //
        // 1. Load the channel's funding record. Absent -> UnknownChannel, the same
        //    fail-closed refusal this arm returned while the record did not exist at
        //    all. An unknown channel has nothing to verify against, and "no record"
        //    must never be read as "no capacity".
        if self.ledger.get_channel_funding(channel_id)?.is_none() {
            return Err(SpilmanError::UnknownChannel {
                channel_id: channel_id.to_string(),
            });
        }

        // 2. Verify with UPSTREAM'S OWN validator — not a re-implementation.
        //
        //    `SpilmanBridge::validate_payment` (bridge.rs:1312) re-derives the
        //    channel from the persisted params, funding proofs, channel secret and
        //    keyset info, refuses a balance above capacity, and verifies the sender's
        //    Schnorr signature over `(channel_id, balance)`. The host it runs against
        //    is `LedgerPayeeHost`, i.e. this same durable ledger.
        //
        //    Refusing on ANY error is the whole fix: before it, the ledger checked
        //    monotonicity only, so `{"channel_id":"deadbeef","balance":5,
        //    "signature":"00"}` minted credit for anyone (CRITICAL-UNVERIFIED-PAYMENT.md).
        let host = LedgerPayeeHost::new(&self.ledger);
        let bridge = SpilmanBridge::new(host);
        // The context is `String` (this rail's host is `SpilmanHost<String>`); the
        // payee's own byte accounting is driven by `authorize_bytes`, not here.
        let context = String::new();
        bridge
            .validate_payment(channel_id, balance, signature, &context)
            .map_err(|e| {
                tracing::warn!(
                    channel_id,
                    balance,
                    error = %e,
                    "spilman payment failed verification; refusing to credit it"
                );
                SpilmanError::PaymentNotVerified {
                    channel_id: channel_id.to_string(),
                    balance,
                    detail: e.to_string(),
                }
            })?;

        // 3. Only now the durable write — and it stays CAS (exact previous balance),
        //    because authenticity and "not a replay" are different questions: a
        //    re-sent valid signature verifies perfectly and must still not be credited
        //    twice.
        self.ledger.record_payment(channel_id, balance, signature)
    }
}

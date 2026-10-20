//! Mint quote tracking and NUT-09 restore classification (A5).
//!
//! Split out from the HTTP handlers so the logic is testable without a mint.
//!
//! Two rules encode the failures this replaces:
//!   * an unknown quote is `None`, never synthesised — the pre-A5 stubs answered
//!     any query with `state: "unpaid"`, `expiry: 0`;
//!   * an unknown *mint* state is an error, never flattened to `Unpaid` — telling
//!     a paid customer to pay again is worse than a 500.

use std::time::Duration;

/// NUT-04 quote state, as a closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteState {
    Unpaid,
    Paid,
    Issued,
}

impl QuoteState {
    /// Map the mint's string to a state.
    ///
    /// Case-insensitive because mints vary, but *closed*: anything unrecognised
    /// is an error. A silent default here would report a paid quote as unpaid.
    pub fn from_mint(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_uppercase().as_str() {
            "UNPAID" => Ok(Self::Unpaid),
            "PAID" => Ok(Self::Paid),
            "ISSUED" => Ok(Self::Issued),
            other => Err(format!(
                "unrecognised mint quote state {other:?}; known: UNPAID, PAID, ISSUED"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unpaid => "unpaid",
            Self::Paid => "paid",
            Self::Issued => "issued",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintQuote {
    pub id: String,
    pub amount: u64,
    pub expiry: u64,
    pub state: QuoteState,
    pub payment_id: Option<String>,
}

impl MintQuote {
    /// Seconds until expiry, saturating at zero.
    pub fn expires_in(&self, now: u64) -> Duration {
        Duration::from_secs(self.expiry.saturating_sub(now))
    }

    /// Whether a customer may still pay this quote.
    pub fn is_payable(&self, now: u64) -> bool {
        self.state == QuoteState::Unpaid && now < self.expiry
    }
}

#[derive(Debug, Default)]
pub struct MintQuoteRegistry {
    quotes: std::collections::HashMap<String, MintQuote>,
}

impl MintQuoteRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: &str, amount: u64, expiry: u64) {
        self.quotes.insert(
            id.to_string(),
            MintQuote {
                id: id.to_string(),
                amount,
                expiry,
                state: QuoteState::Unpaid,
                payment_id: None,
            },
        );
    }

    pub fn get(&self, id: &str) -> Option<&MintQuote> {
        self.quotes.get(id)
    }

    pub fn mark_paid(&mut self, id: &str, payment_id: &str) {
        if let Some(q) = self.quotes.get_mut(id) {
            q.state = QuoteState::Paid;
            q.payment_id = Some(payment_id.to_string());
        }
    }

    pub fn mark_issued(&mut self, id: &str) {
        if let Some(q) = self.quotes.get_mut(id) {
            q.state = QuoteState::Issued;
        }
    }
}

/// Outcome of a NUT-09 restore.
///
/// The three sets are mutually exclusive and cover every proof asked about, so a
/// caller can never mistake "we did not find it" for "we have it".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Proofs the mint still recognises as unspent.
    pub recovered: Vec<String>,
    /// Proofs the mint reports as already spent.
    pub spent: Vec<String>,
    /// Proofs the mint said nothing about.
    pub missing: Vec<String>,
}

impl RestoreOutcome {
    pub fn classify(asked: &[String], recovered: &[String], spent: &[String]) -> Self {
        let rec: std::collections::HashSet<&String> = recovered.iter().collect();
        let spn: std::collections::HashSet<&String> = spent.iter().collect();

        let mut out = Self::default();
        for p in asked {
            if rec.contains(p) {
                out.recovered.push(p.clone());
            } else if spn.contains(p) {
                out.spent.push(p.clone());
            } else {
                out.missing.push(p.clone());
            }
        }
        out
    }

    /// A restore is complete only when nothing is unaccounted for.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

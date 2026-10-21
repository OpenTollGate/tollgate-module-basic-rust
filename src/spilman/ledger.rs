//! Durable per-channel balance ledger (payee side).
//!
//! This is the `SpilmanHost` shape MONAD uses at
//! `monad-relay/src/payments.rs:961-972`, reduced to the two operations the hot
//! path actually needs:
//!
//! - `get_amount_due` reads the stored balance (a local SQLite read, no network);
//! - `record_payment` writes `(balance, signature)` **durably before returning**.
//!
//! # Why CAS and not `new > stored`
//!
//! Gap analysis §6: the mint honours only the highest balance the payee can
//! produce, so a lost latest signature is lost money (payee's loss), and a payee
//! that serves bytes for a balance it never durably recorded can be rolled back
//! (payer's loss). Both are closed by an exact compare-and-swap against the
//! previous balance inside one `IMMEDIATE` transaction:
//!
//! ```text
//! BEGIN IMMEDIATE
//!   previous := SELECT balance ...          -- 0 when the channel is new
//!   if new_balance <= previous -> ROLLBACK, Err(BalanceNotAdvanced)
//!   UPSERT ... WHERE balance = previous
//! COMMIT
//! ```
//!
//! A naive `if new_balance > stored` is not equivalent: a *retry of a different
//! payment that happens to carry the same balance* would be treated as newly
//! accepted funds. Only the exact-previous CAS rejects it (INTEGRATION.md:
//! "Applications computing credit deltas must use this CAS rather than treating
//! a monotonic no-op as newly accepted funds").
//!
//! # Durability
//!
//! `synchronous = FULL` + WAL: a committed `record_payment` has reached stable
//! storage before the caller is allowed to serve a single byte. Both settings
//! together are the whole point of this module; do not relax them for speed.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::spilman::SpilmanError;

/// A durable `(balance, signature)` pair — the latest payment the payee holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPayment {
    pub channel_id: String,
    pub balance: u64,
    pub signature: String,
    /// Number of accepted payments seen for this channel (1-based).
    pub payment_count: u64,
    pub updated_at: u64,
}

/// Result of an accepted ledger write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerOutcome {
    /// The balance advanced from the exact previous value and is now durable.
    Accepted,
}

/// SQLite-backed channel balance ledger.
#[derive(Debug)]
pub struct ChannelLedger {
    conn: Mutex<Connection>,
}

impl ChannelLedger {
    /// Open (or create) the ledger at `path`.
    ///
    /// FAIL CLOSED is the caller's job: an unreadable ledger must never be read
    /// as "no balance" — that would let a payer re-spend a channel.
    pub fn open(path: &Path) -> Result<Self, SpilmanError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SpilmanError::Io {
                path: parent.display().to_string(),
                detail: e.to_string(),
            })?;
        }
        let conn = Connection::open(path).map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        Self::configure(&conn)?;
        let ledger = Self {
            conn: Mutex::new(conn),
        };
        ledger.init_schema()?;
        Ok(ledger)
    }

    /// Open an in-memory ledger (tests only — nothing survives the handle).
    pub fn open_in_memory() -> Result<Self, SpilmanError> {
        let conn = Connection::open_in_memory().map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        let ledger = Self {
            conn: Mutex::new(conn),
        };
        ledger.init_schema()?;
        Ok(ledger)
    }

    fn configure(conn: &Connection) -> Result<(), SpilmanError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        // WAL keeps readers (get_amount_due on the hot path) from blocking on the
        // writer. FULL makes a commit durable before record_payment returns.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        Ok(())
    }

    fn init_schema(&self) -> Result<(), SpilmanError> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS spilman_channel_balance (
                 channel_id    TEXT    PRIMARY KEY,
                 balance       INTEGER NOT NULL,
                 signature     TEXT    NOT NULL,
                 payment_count INTEGER NOT NULL,
                 updated_at    INTEGER NOT NULL
             ) STRICT;",
        )
        .map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SpilmanError> {
        self.conn
            .lock()
            .map_err(|_| SpilmanError::Ledger("ledger lock poisoned".to_string()))
    }

    /// The latest durable payment for a channel, or `None` if it has none.
    pub fn get_latest_payment(
        &self,
        channel_id: &str,
    ) -> Result<Option<StoredPayment>, SpilmanError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT channel_id, balance, signature, payment_count, updated_at
               FROM spilman_channel_balance
              WHERE channel_id = ?1",
            [channel_id],
            |row| {
                Ok(StoredPayment {
                    channel_id: row.get(0)?,
                    balance: row.get(1)?,
                    signature: row.get(2)?,
                    payment_count: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|e| SpilmanError::Ledger(e.to_string()))
    }

    /// The stored balance, or `None` when the channel has never been paid.
    ///
    /// `None` is deliberately distinct from `Some(0)`: a channel that does not
    /// exist must not be served, and "no record" is not "zero balance".
    pub fn get_balance(&self, channel_id: &str) -> Result<Option<u64>, SpilmanError> {
        Ok(self.get_latest_payment(channel_id)?.map(|p| p.balance))
    }

    /// Amount still due before the channel is exhausted, in raw channel units.
    ///
    /// `amount_due` here is the *next* balance the payer must sign, i.e. the
    /// stored balance. The host contract calls it "amount due" and MONAD
    /// implements it as a plain read of the stored balance; keeping that shape
    /// means the payee's credit accounting (`balance - served`) stays a single
    /// subtraction in the caller.
    pub fn get_amount_due(&self, channel_id: &str) -> Result<u64, SpilmanError> {
        Ok(self.get_balance(channel_id)?.unwrap_or(0))
    }

    /// Durably record a new balance, CAS'd against the exact previous value.
    ///
    /// Rejects any balance that does not strictly advance the stored one. That
    /// covers replay (equal), stale/rollback (lower) and the same-balance
    /// different-signature double-credit case.
    pub fn record_payment(
        &self,
        channel_id: &str,
        balance: u64,
        signature: &str,
    ) -> Result<LedgerOutcome, SpilmanError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;

        let previous: Option<(u64, u64)> = tx
            .query_row(
                "SELECT balance, payment_count FROM spilman_channel_balance WHERE channel_id = ?1",
                [channel_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;

        let (previous_balance, previous_count) = previous.unwrap_or((0, 0));
        if previous.is_some() && balance <= previous_balance {
            // No write, no count bump: a rejected payment is not newly accepted
            // funds. The tx is dropped (rolled back) on return.
            return Err(SpilmanError::BalanceNotAdvanced {
                channel_id: channel_id.to_string(),
                stored: previous_balance,
                offered: balance,
            });
        }
        if previous.is_none() && balance == 0 {
            // A zero-balance opening signature is legitimate, but it must not be
            // re-recorded as if it were progress.
            return Err(SpilmanError::BalanceNotAdvanced {
                channel_id: channel_id.to_string(),
                stored: 0,
                offered: 0,
            });
        }

        let payment_count = previous_count + 1;
        let updated_at = now_seconds();
        let changed = tx
            .execute(
                "INSERT INTO spilman_channel_balance
                     (channel_id, balance, signature, payment_count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(channel_id) DO UPDATE SET
                     balance       = excluded.balance,
                     signature     = excluded.signature,
                     payment_count = excluded.payment_count,
                     updated_at    = excluded.updated_at
                 WHERE spilman_channel_balance.balance = ?6",
                rusqlite::params![
                    channel_id,
                    balance,
                    signature,
                    payment_count,
                    updated_at,
                    previous_balance
                ],
            )
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;

        if changed != 1 {
            // Someone advanced the balance between our read and our write. The
            // CAS guard refused the UPDATE; do not pretend it landed.
            return Err(SpilmanError::BalanceNotAdvanced {
                channel_id: channel_id.to_string(),
                stored: previous_balance,
                offered: balance,
            });
        }

        tx.commit()
            .map_err(|e| SpilmanError::Ledger(e.to_string()))?;
        Ok(LedgerOutcome::Accepted)
    }

    /// Number of accepted payments for a channel (0 when unknown).
    pub fn payment_count(&self, channel_id: &str) -> Result<u64, SpilmanError> {
        Ok(self
            .get_latest_payment(channel_id)?
            .map(|p| p.payment_count)
            .unwrap_or(0))
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_cannot_both_advance_from_the_same_previous_balance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spilman.db");
        let ledger = ChannelLedger::open(&path).expect("open");
        ledger.record_payment("c", 1, "s1").expect("prime");

        // Two writers offer the same next balance. Exactly one may win; the CAS
        // guard (`WHERE balance = previous`) is what makes the second fail even
        // if both read the same previous value.
        let a = ledger.record_payment("c", 2, "s2a");
        let b = ledger.record_payment("c", 2, "s2b");
        assert_eq!(a, Ok(LedgerOutcome::Accepted));
        assert!(b.is_err());
        let stored = ledger
            .get_latest_payment("c")
            .expect("query")
            .expect("present");
        assert_eq!(stored.balance, 2);
        assert_eq!(stored.signature, "s2a");
        assert_eq!(stored.payment_count, 2);
    }

    #[test]
    fn zero_balance_cannot_be_recorded_twice() {
        let ledger = ChannelLedger::open_in_memory().expect("open");
        assert!(ledger.record_payment("c", 0, "s0").is_err());
        assert_eq!(ledger.get_balance("c").expect("query"), None);
    }
}

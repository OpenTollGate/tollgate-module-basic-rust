//! Durable payment journal — business-level recovery for payments.
//!
//! Wallet-level atomicity (CDK sagas) is not application-level atomicity: a
//! payment is `receive → session-create → gate → 200`, and a crash or timeout
//! anywhere in that chain can leave the customer's token consumed with no
//! session. The `payments` table in the durable store is the write-ahead log
//! that closes the window:
//!
//! | death point | journal phase | reconcile outcome |
//! |---|---|---|
//! | before intent write | (none) | nothing moved, nothing owed |
//! | after intent, before/during receive | `intent` | NUT-07: spent ⇒ owed session granted; unspent ⇒ closed |
//! | after receive, before `received` write | `intent` | spent ⇒ session granted (outputs land via saga recovery) |
//! | after `received` | `received` | none needed (same-MAC session overwrite is idempotent) |
//! | receive timeout | `timeout-unknown` | NUT-07 decides: spent ⇒ granted; unspent ⇒ closed |
//!
//! Undecided rows (`intent`, `timeout-unknown`) are retried every boot and are
//! never marked failed: the outcome is unknown, not negative. Grants are
//! journaled `reconcile-spent` only AFTER the session row is durable, so every
//! crash point converges through idempotent re-grant instead of stranding a
//! settled-but-sessionless payment.
//!
//! Pricing terms are frozen at intent time (the record carries
//! price/step_size/metric), so reconciliation grants what was paid for even if
//! the operator changes config before the deciding boot.

use crate::session::store::PaymentRecord;
use crate::session::SessionManager;
use sha2::{Digest, Sha256};

pub const PHASE_INTENT: &str = "intent";
pub const PHASE_RECEIVED: &str = "received";
pub const PHASE_REJECTED: &str = "rejected";
pub const PHASE_TIMEOUT_UNKNOWN: &str = "timeout-unknown";
pub const PHASE_RECONCILE_SPENT: &str = "reconcile-spent";
pub const PHASE_RECONCILE_UNSPENT: &str = "reconcile-unspent";
pub const PHASE_RECONCILE_ZERO_STEPS: &str = "reconcile-zero-steps";

pub const UNDECIDED_PHASES: [&str; 2] = [PHASE_INTENT, PHASE_TIMEOUT_UNKNOWN];

/// MAC marker for CLI `wallet fund` rows: a receive the OPERATOR initiated,
/// not a customer payment. Real client MACs are hex-colon strings, so this
/// can never collide. Reconciliation advances these rows exactly like
/// payments — the value lands via the same saga recovery — but owes no
/// session and opens no gate: the operator sees the balance.
pub const CLI_FUND_MAC: &str = "cli-fund";

/// sha256(token) — the journal key. A hash rather than the raw string so the
/// key is greppable in operator tooling without being a bearer instrument.
pub fn token_id(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// The allotment a record's frozen terms buy. Mirrors the live path's
/// `calculate_allotment` arithmetic against the frozen inputs.
pub fn allotment_for(rec: &PaymentRecord) -> u64 {
    let steps = rec.amount_sat / rec.price_per_step.max(1);
    steps * rec.step_size
}

/// A settled outcome is one where value provably arrived: the live path's
/// `received`, or the reconciler's `reconcile-spent`. Only these may replay.
pub fn is_settled(phase: &str) -> bool {
    phase == PHASE_RECEIVED || phase == PHASE_RECONCILE_SPENT
}

/// What the reconciler owes for one undecided payment, given the mint's word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owed {
    /// The mint says the proofs were spent: a session of this allotment is
    /// owed to the record's MAC. `spent_sat` is the mint-reported amount —
    /// what the terminal journal row records.
    Grant { allotment: u64, spent_sat: u64 },
    /// Spent, but the frozen terms price it below one step: no session is
    /// possible; recorded as credit material for the operator.
    ZeroSteps { spent_sat: u64 },
    /// The mint says unspent: the customer lost nothing — the token is still
    /// theirs to spend. Closed.
    Nothing,
    /// The mint could not be asked: still unknown, retried next boot.
    Undecided,
}

/// Pure decision, testable without a mint. `mint_word` is the NUT-07 answer:
/// `Ok(Some(amount_sat))` = the proofs are spent (the customer paid);
/// `Ok(None)` = unspent; `Err` = unreachable.
pub fn owed_decision(mint_word: Result<Option<u64>, String>, rec: &PaymentRecord) -> Owed {
    match mint_word {
        Ok(Some(spent_sat)) => {
            let mut priced = rec.clone();
            priced.amount_sat = spent_sat;
            if allotment_for(&priced) == 0 {
                Owed::ZeroSteps { spent_sat }
            } else {
                Owed::Grant {
                    allotment: allotment_for(&priced),
                    spent_sat,
                }
            }
        }
        Ok(None) => Owed::Nothing,
        Err(_) => Owed::Undecided,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub granted: u64,
    pub closed_unspent: u64,
    pub zero_steps: u64,
    pub undecided: u64,
}

/// Decide every undecided payment, granting durable sessions for what is owed.
///
/// Ordering (the crash-window core): the session row is created and persisted
/// FIRST; only then is the row advanced to `reconcile-spent`. A crash between
/// the two re-runs this pass next boot — the mint still says spent, and the
/// same-MAC session overwrite is idempotent — so every death point converges.
/// A persist failure leaves the row undecided on purpose: an undurable grant
/// must not be recorded as delivered.
///
/// Gate opening is attempted after the durable grant and is tolerated to
/// fail: ndsctl may be down at boot, and the ADR-0006 continuity sweep and
/// the client's own portal trigger both re-open a recorded session.
pub async fn settle_undecided<F, Fut>(
    sessions: &tokio::sync::Mutex<SessionManager>,
    check: F,
) -> ReconcileReport
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<u64>, String>>,
{
    let mut report = ReconcileReport::default();

    let undecided: Vec<PaymentRecord> = {
        let guard = sessions.lock().await;
        match guard.store() {
            Some(st) => st.payments_in_phase(&UNDECIDED_PHASES).unwrap_or_else(|e| {
                tracing::error!(error = %e, "payment reconcile: journal read failed");
                Vec::new()
            }),
            None => Vec::new(),
        }
    };
    if undecided.is_empty() {
        return report;
    }
    tracing::info!(
        count = undecided.len(),
        "payment reconcile: deciding undecided payments"
    );

    for rec in undecided {
        let mint_word = check(rec.token.clone()).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut terminal = rec.clone();
        terminal.ts = now;
        // Operator-initiated rows (CLI `wallet fund`) owe no session: the
        // value landing in the wallet IS the delivery. Spent ⇒ the fund
        // succeeded, terminalize; unspent ⇒ nothing was taken, close;
        // unreachable stays undecided like any payment.
        if rec.mac == CLI_FUND_MAC {
            match mint_word {
                Ok(Some(spent_sat)) => {
                    let guard = sessions.lock().await;
                    if let Some(st) = guard.store() {
                        terminal.phase = PHASE_RECONCILE_SPENT.to_string();
                        terminal.amount_sat = spent_sat;
                        let _ = st.upsert_payment(&terminal);
                    }
                    report.granted += 1;
                }
                Ok(None) => {
                    let guard = sessions.lock().await;
                    if let Some(st) = guard.store() {
                        terminal.phase = PHASE_RECONCILE_UNSPENT.to_string();
                        let _ = st.upsert_payment(&terminal);
                    }
                    report.closed_unspent += 1;
                }
                Err(_) => report.undecided += 1,
            }
            continue;
        }
        match owed_decision(mint_word, &rec) {
            Owed::Grant {
                allotment,
                spent_sat,
            } => {
                let mut guard = sessions.lock().await;
                let session = guard.create_session(
                    &rec.mac,
                    allotment,
                    &rec.metric,
                    // Exhaustion-driven, like the /pay grant path: the frozen
                    // terms already price this access; an additional wall-clock
                    // deadline would cut off a customer whose router spent the
                    // outage window rebooting.
                    u64::MAX / 4,
                );
                if session.allotment == 0 {
                    // The persist-failure sentinel: the session is NOT durable,
                    // so the payment stays undecided and this pass re-runs.
                    tracing::error!(
                        mac = %rec.mac,
                        id = %rec.id,
                        "payment reconcile: session persist failed; leaving undecided for next boot"
                    );
                    report.undecided += 1;
                    continue;
                }
                if let Some(st) = guard.store() {
                    terminal.phase = PHASE_RECONCILE_SPENT.to_string();
                    terminal.amount_sat = spent_sat;
                    if let Err(e) = st.upsert_payment(&terminal) {
                        tracing::error!(id = %rec.id, error = %e, "payment reconcile: terminal write failed; idempotent re-run next boot");
                        report.undecided += 1;
                        continue;
                    }
                }
                drop(guard);
                let _ = crate::metering::open_gate(&rec.mac).await;
                tracing::info!(mac = %rec.mac, allotment, id = %rec.id, "payment reconcile: granted the owed session");
                report.granted += 1;
            }
            Owed::ZeroSteps { spent_sat } => {
                let guard = sessions.lock().await;
                if let Some(st) = guard.store() {
                    terminal.phase = PHASE_RECONCILE_ZERO_STEPS.to_string();
                    terminal.amount_sat = spent_sat;
                    let _ = st.upsert_payment(&terminal);
                }
                report.zero_steps += 1;
            }
            Owed::Nothing => {
                let guard = sessions.lock().await;
                if let Some(st) = guard.store() {
                    terminal.phase = PHASE_RECONCILE_UNSPENT.to_string();
                    let _ = st.upsert_payment(&terminal);
                }
                report.closed_unspent += 1;
            }
            Owed::Undecided => {
                report.undecided += 1;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(amount_sat: u64, price_per_step: u64, step_size: u64) -> PaymentRecord {
        PaymentRecord {
            id: token_id("some-token"),
            token: "some-token".to_string(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            mint: "https://mint.example".to_string(),
            amount_sat,
            price_per_step,
            step_size,
            metric: "bytes".to_string(),
            phase: PHASE_INTENT.to_string(),
            ts: 1,
        }
    }

    #[test]
    fn token_id_is_sha256_hex() {
        assert_eq!(token_id("").len(), 64);
        assert_ne!(token_id("a"), token_id("b"));
        assert_eq!(token_id("a"), token_id("a"));
    }

    #[test]
    fn allotment_uses_frozen_terms() {
        assert_eq!(allotment_for(&rec(5, 1, 1000)), 5000);
        assert_eq!(
            allotment_for(&rec(7, 3, 1000)),
            2000,
            "partial step not granted"
        );
        assert_eq!(allotment_for(&rec(2, 3, 1000)), 0);
    }

    #[test]
    fn spent_above_one_step_owes_a_grant() {
        let owed = owed_decision(Ok(Some(5)), &rec(0, 1, 1000));
        assert_eq!(
            owed,
            Owed::Grant {
                allotment: 5000,
                spent_sat: 5
            }
        );
    }

    #[test]
    fn spent_below_one_step_is_credit_material() {
        let owed = owed_decision(Ok(Some(2)), &rec(0, 3, 1000));
        assert_eq!(owed, Owed::ZeroSteps { spent_sat: 2 });
    }

    #[test]
    fn unspent_owes_nothing() {
        assert_eq!(owed_decision(Ok(None), &rec(9, 1, 1000)), Owed::Nothing);
    }

    #[test]
    fn unreachable_stays_undecided() {
        assert_eq!(
            owed_decision(Err("mint down".into()), &rec(9, 1, 1000)),
            Owed::Undecided
        );
    }

    #[test]
    fn only_value_received_outcomes_are_settled() {
        assert!(is_settled(PHASE_RECEIVED));
        assert!(is_settled(PHASE_RECONCILE_SPENT));
        assert!(!is_settled(PHASE_INTENT));
        assert!(!is_settled(PHASE_TIMEOUT_UNKNOWN));
        assert!(!is_settled(PHASE_REJECTED));
        assert!(!is_settled(PHASE_RECONCILE_UNSPENT));
    }

    /// The boot reconciler, driven offline against a real store and a faked
    /// mint word. These are the crash-window proofs: an `intent` row is a
    /// process that died between value-movement and the 200.
    mod settle {
        use super::*;

        fn seed(
            dir: &std::path::Path,
            id: &str,
            token: &str,
            mac: &str,
            price_per_step: u64,
        ) -> tokio::sync::Mutex<crate::session::SessionManager> {
            let mgr = crate::session::SessionManager::open(&dir.join("tollgate.db"))
                .expect("durable store opens");
            mgr.store()
                .expect("store present")
                .upsert_payment(&PaymentRecord {
                    id: id.to_string(),
                    token: token.to_string(),
                    mac: mac.to_string(),
                    mint: "https://mint.example".to_string(),
                    amount_sat: 0,
                    price_per_step,
                    step_size: 1000,
                    metric: "bytes".to_string(),
                    phase: PHASE_INTENT.to_string(),
                    ts: 1,
                })
                .expect("seed intent row");
            tokio::sync::Mutex::new(mgr)
        }

        /// The mint says spent: the owed session is granted DURABLY and the
        /// row terminalizes only after the session row exists.
        #[tokio::test]
        async fn spent_intent_grants_the_owed_session() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-1", "tok-1", "aa:bb:cc:dd:ee:01", 1);
            let report = settle_undecided(&sessions, |t| async move {
                Ok::<Option<u64>, String>(if t == "tok-1" { Some(5) } else { None })
            })
            .await;
            assert_eq!(report.granted, 1);

            let guard = sessions.lock().await;
            let session = guard
                .get_session("aa:bb:cc:dd:ee:01")
                .expect("session granted");
            assert_eq!(session.allotment, 5000, "5 sat at 1 sat/step, step 1000");
            let rec = guard
                .store()
                .and_then(|st| st.get_payment("id-1").ok().flatten())
                .expect("row exists");
            assert_eq!(rec.phase, PHASE_RECONCILE_SPENT);
            assert_eq!(rec.amount_sat, 5, "the mint-reported amount is the record");
        }

        /// The mint says unspent: the customer lost nothing — closed, no session.
        #[tokio::test]
        async fn unspent_intent_closes_with_nothing_owed() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-2", "tok-2", "aa:bb:cc:dd:ee:02", 1);
            let report =
                settle_undecided(&sessions, |_| async { Ok::<Option<u64>, String>(None) }).await;
            assert_eq!(report.closed_unspent, 1);
            let guard = sessions.lock().await;
            assert!(guard.get_session("aa:bb:cc:dd:ee:02").is_none());
            let rec = guard
                .store()
                .and_then(|st| st.get_payment("id-2").ok().flatten())
                .expect("row exists");
            assert_eq!(rec.phase, PHASE_RECONCILE_UNSPENT);
        }

        /// The mint cannot be asked: still unknown — retried next boot,
        /// never failed.
        #[tokio::test]
        async fn unreachable_mint_leaves_the_row_undecided() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-3", "tok-3", "aa:bb:cc:dd:ee:03", 1);
            let report = settle_undecided(&sessions, |_| async {
                Err::<Option<u64>, String>("mint down".to_string())
            })
            .await;
            assert_eq!(report.undecided, 1);
            let guard = sessions.lock().await;
            assert!(guard.get_session("aa:bb:cc:dd:ee:03").is_none());
            let rec = guard
                .store()
                .and_then(|st| st.get_payment("id-3").ok().flatten())
                .expect("row exists");
            assert_eq!(rec.phase, PHASE_INTENT, "untouched for the next boot");
        }

        /// Spent but priced below one step by the frozen terms: no session is
        /// possible; the value is recorded as credit material.
        #[tokio::test]
        async fn spent_below_one_step_is_credit_material() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-4", "tok-4", "aa:bb:cc:dd:ee:04", 3);
            let report =
                settle_undecided(&sessions, |_| async { Ok::<Option<u64>, String>(Some(2)) }).await;
            assert_eq!(report.zero_steps, 1);
            let guard = sessions.lock().await;
            assert!(guard.get_session("aa:bb:cc:dd:ee:04").is_none());
            let rec = guard
                .store()
                .and_then(|st| st.get_payment("id-4").ok().flatten())
                .expect("row exists");
            assert_eq!(rec.phase, PHASE_RECONCILE_ZERO_STEPS);
        }

        /// An operator `wallet fund` row owes no session: spent ⇒ the value
        /// landed, terminalize; the reconciler must not create a session or
        /// touch the gate for a `cli-fund`-marked row.
        #[tokio::test]
        async fn cli_fund_marker_reconciles_without_a_session() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-f", "tok-f", CLI_FUND_MAC, 1);
            let report = settle_undecided(&sessions, |t| async move {
                Ok::<Option<u64>, String>(if t == "tok-f" { Some(7) } else { None })
            })
            .await;
            assert_eq!(report.granted, 1);
            let guard = sessions.lock().await;
            assert!(
                guard.get_session(CLI_FUND_MAC).is_none(),
                "an operator fund owes no session"
            );
            let rec = guard
                .store()
                .and_then(|st| st.get_payment("id-f").ok().flatten())
                .expect("row exists");
            assert_eq!(rec.phase, PHASE_RECONCILE_SPENT);
            assert_eq!(rec.amount_sat, 7);
        }

        /// Convergence: a second boot over a decided journal grants nothing
        /// new and never doubles an allotment.
        #[tokio::test]
        async fn second_pass_over_a_decided_journal_grants_nothing() {
            let tmp = tempfile::tempdir().unwrap();
            let sessions = seed(tmp.path(), "id-5", "tok-5", "aa:bb:cc:dd:ee:05", 1);
            let first = settle_undecided(&sessions, |t| async move {
                Ok::<Option<u64>, String>(if t == "tok-5" { Some(5) } else { None })
            })
            .await;
            assert_eq!(first.granted, 1);
            let second =
                settle_undecided(&sessions, |_| async { Ok::<Option<u64>, String>(Some(5)) }).await;
            assert_eq!(second.granted, 0, "nothing left to decide");
            let guard = sessions.lock().await;
            assert_eq!(
                guard.get_session("aa:bb:cc:dd:ee:05").unwrap().allotment,
                5000,
                "the grant is not doubled by re-running"
            );
        }
    }
}

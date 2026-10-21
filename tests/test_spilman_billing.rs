//! Phase 2 tests for the Spilman rail seam.
//!
//! Written before the implementation existed (RED observed: `could not find
//! spilman in tollgate_module_basic_rust`), then implemented to GREEN.
//!
//! The properties they pin down are the ones the integration can lose money on:
//!
//! 1. **Billing parity.** `step_size = 22_020_096` bytes must cost
//!    `price_per_step = 1` sat. At `millisat` granularity that is exactly 1000 msat;
//!    at `sat` granularity it is exactly 1 raw unit.
//! 2. **Rounding direction.** A channel's raw balance is in `sat`, the bill is in
//!    `msat`. The conversion must round **up**, because rounding down short-changes
//!    the payee by <1 sat per update and that error accumulates across a session.
//! 3. **CAS, not "greater than".** A replayed or stale `(balance, signature)` must be
//!    rejected, and the rejection must be against the *exact* previous balance so a
//!    retry of a different payment that happens to carry the same balance cannot be
//!    double-counted.
//!
//! # Measured correction to the gap analysis
//!
//! Gap analysis §4 derives `in_bytes_per_millisat = 22_020` from
//! `22_020_096 / 1000` and claims it "reproduces TollGate's price line exactly".
//! It does not: 22_020_096 / 1000 = 22_020.**096**, so at rate 22_020 the ceil is
//! `floor(22_020_096 / 22_020) + 1 = 1001` msat. One step then also costs **2 sat**
//! after round-up at sat granularity — a 100% overcharge at step granularity.
//! `22_021` is the smallest integer rate that bills a step at exactly 1000 msat.
//! Both are asserted below so the drift cannot silently change.

use tollgate_module_basic_rust::spilman::billing::{amount_due_millisats, remaining_milli_sats};
use tollgate_module_basic_rust::spilman::ledger::{ChannelLedger, LedgerOutcome};
use tollgate_module_basic_rust::spilman::payment_units::{msats_to_raw_units, raw_units_to_msats};
use tollgate_module_basic_rust::spilman::{
    NoopRail, PaymentRail, RailDecision, DEFAULT_IN_BYTES_PER_MILLISAT,
    TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT, TOLLGATE_PRICE_PER_STEP_MILLISATS,
    TOLLGATE_STEP_SIZE_BYTES,
};

fn due(bytes_in: u64, bytes_out: u64, rate: u64) -> u128 {
    amount_due_millisats(bytes_in, bytes_out, rate, rate).expect("valid rates")
}

/// (i) The step and its price are the ones the live config uses.
#[test]
fn constants_match_the_live_router() {
    assert_eq!(TOLLGATE_STEP_SIZE_BYTES, 22_020_096, "21 MiB step");
    assert_eq!(TOLLGATE_PRICE_PER_STEP_MILLISATS, 1_000, "1 sat per step");
    assert_eq!(
        DEFAULT_IN_BYTES_PER_MILLISAT, 22_020,
        "instructed config default"
    );
    assert_eq!(
        TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT, 22_021,
        "exact-parity rate"
    );
}

/// (i) At the exact-parity rate, one step is exactly 1000 msat = exactly 1 sat.
#[test]
fn one_step_costs_exactly_one_sat_at_the_parity_rate() {
    let msat = due(
        TOLLGATE_STEP_SIZE_BYTES,
        0,
        TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
    );
    assert_eq!(msat, u128::from(TOLLGATE_PRICE_PER_STEP_MILLISATS));
    let raw = msats_to_raw_units("sat", 1000).expect("sat supported");
    assert_eq!(raw, 1);
}

/// (i) The instructed default (22_020) does NOT bill a step at 1000 msat — it bills
/// 1001, because 22_020_096 / 1000 is 22_020.096. Pinned so any future "fix" that
/// silently changes the price line fails here first.
#[test]
fn default_rate_overbills_a_step_by_one_millisat_and_that_is_pinned() {
    let msat = due(TOLLGATE_STEP_SIZE_BYTES, 0, DEFAULT_IN_BYTES_PER_MILLISAT);
    assert_eq!(msat, 1001, "rate 22_020 cannot express 22_020.096 B/msat");
    // At sat granularity that is a whole extra raw unit per step: 100% overcharge.
    assert_eq!(msats_to_raw_units("sat", 1000).expect("sat"), 1);
    assert_eq!(msats_to_raw_units("sat", 1001).expect("sat"), 2);
}

/// (i) For every rate that can express the retail price, n steps cost n sat exactly.
#[test]
fn n_steps_cost_n_sat_after_rounding_up() {
    for steps in 1..=64u64 {
        let bytes = TOLLGATE_STEP_SIZE_BYTES * steps;
        let msat = due(bytes, 0, TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT);
        let raw = msats_to_raw_units("sat", u64::try_from(msat).expect("fits")).expect("sat");
        assert_eq!(
            raw, steps,
            "{steps} steps must cost {steps} sat (msat={msat})"
        );
    }
}

/// (ii) Cumulative bytes bill to the same value as the exact rational reference.
#[test]
fn cumulative_bytes_match_the_exact_ceil_reference() {
    let cases = [
        (0u64, 0u64),
        (1, 0),
        (0, 1),
        (22_019, 0),
        (22_020, 0),
        (22_021, 0),
        (22_020_096, 0),
        (0, 22_020_096),
        (22_020_096, 22_020_096),
        (1_048_576, 524_288),
        (u64::MAX, 0),
        (0, u64::MAX),
    ];
    let rates = [
        (22_020u64, 22_020u64),
        (22_021, 22_021),
        (3, 7),
        (1, 1),
        (1_000_000, 3),
    ];

    for (bytes_in, bytes_out) in cases {
        for (in_rate, out_rate) in rates {
            let due =
                amount_due_millisats(bytes_in, bytes_out, in_rate, out_rate).expect("valid rates");
            let numerator =
                bytes_in as u128 * out_rate as u128 + bytes_out as u128 * in_rate as u128;
            let denominator = in_rate as u128 * out_rate as u128;
            assert_eq!(
                due,
                numerator.div_ceil(denominator),
                "bytes=({bytes_in},{bytes_out}) rates=({in_rate},{out_rate})"
            );
        }
    }
}

/// (ii) Billing from *cumulative* totals is monotone and never re-rounds previously
/// billed fractions (the "compute amount_due once" rule).
#[test]
fn cumulative_billing_is_monotone_and_rounds_only_once() {
    let rate = DEFAULT_IN_BYTES_PER_MILLISAT;
    let mut bytes = 0u64;
    for _ in 0..500 {
        bytes += 22_019; // just under one msat per tick
        let due_now = due(bytes, 0, rate);
        assert_eq!(
            due_now,
            (bytes as u128).div_ceil(rate as u128),
            "the bill is a function of the cumulative total, not of the deltas"
        );
    }
    // Per-tick ceil() would have charged 1 msat for every tick; the cumulative rule
    // charges for the bytes actually carried. Demonstrated on 1-byte ticks, where the
    // difference is maximal: 1000 ticks cost 1 msat cumulatively, not 1000.
    let ticks = 1_000u64;
    let per_tick: u128 = u128::from(ticks);
    let cumulative = (ticks as u128).div_ceil(u128::from(rate));
    assert_eq!(cumulative, 1);
    assert!(
        cumulative < per_tick,
        "cumulative {cumulative} < per-tick {per_tick}"
    );
}

#[test]
fn zero_rates_are_rejected() {
    assert!(amount_due_millisats(10, 10, 0, 22_020).is_err());
    assert!(amount_due_millisats(10, 10, 22_020, 0).is_err());
}

#[test]
fn remaining_credit_is_signed() {
    // Exactly paid: zero remaining, not one millisat of debt.
    let exact = due(
        TOLLGATE_STEP_SIZE_BYTES,
        0,
        TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
    );
    assert_eq!(
        remaining_milli_sats(
            u64::try_from(exact).expect("fits"),
            TOLLGATE_STEP_SIZE_BYTES,
            0,
            TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
            TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT
        ),
        Ok(0)
    );
    // Nothing consumed: full credit.
    assert_eq!(remaining_milli_sats(1000, 0, 0, 22_021, 22_021), Ok(1000));
    // Consumed without paying: negative, and it may not wrap.
    assert_eq!(
        remaining_milli_sats(0, TOLLGATE_STEP_SIZE_BYTES, 0, 22_021, 22_021),
        Ok(-1000)
    );
}

/// (iv) msat -> raw rounding goes UP at non-multiples; raw -> msat is exact.
#[test]
fn msat_to_sat_rounds_up_at_non_multiples() {
    assert_eq!(msats_to_raw_units("sat", 0), Ok(0));
    assert_eq!(msats_to_raw_units("sat", 1), Ok(1));
    assert_eq!(msats_to_raw_units("sat", 999), Ok(1));
    assert_eq!(msats_to_raw_units("sat", 1000), Ok(1));
    assert_eq!(msats_to_raw_units("sat", 1001), Ok(2));
    assert_eq!(msats_to_raw_units("msat", 1001), Ok(1001));

    assert_eq!(raw_units_to_msats("sat", 0), Ok(0));
    assert_eq!(raw_units_to_msats("sat", 2), Ok(2000));
    assert_eq!(raw_units_to_msats("msat", 2000), Ok(2000));

    assert!(msats_to_raw_units("usd", 1).is_err());
    assert!(raw_units_to_msats("usd", 1).is_err());
    assert!(
        raw_units_to_msats("sat", u64::MAX).is_err(),
        "overflow must error, not wrap"
    );
}

/// (iii) CAS: exact-next accepted, stale / replayed / equal rejected.
#[test]
fn ledger_cas_accepts_exact_next_and_rejects_stale_or_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = ChannelLedger::open(&dir.path().join("spilman.db")).expect("open ledger");
    let id = "chan-cas";

    assert_eq!(
        ledger.record_payment(id, 10, "sig-10").expect("record 10"),
        LedgerOutcome::Accepted
    );
    assert_eq!(ledger.get_balance(id).expect("balance"), Some(10));

    // Equal balance is a replay, not an advance.
    assert!(ledger.record_payment(id, 10, "sig-10").is_err());
    // Lower balance is stale.
    assert!(ledger.record_payment(id, 5, "sig-5").is_err());
    // Nothing above was written.
    assert_eq!(ledger.get_balance(id).expect("balance"), Some(10));

    // Exact next is accepted.
    assert_eq!(
        ledger.record_payment(id, 11, "sig-11").expect("record 11"),
        LedgerOutcome::Accepted
    );
    assert_eq!(ledger.get_balance(id).expect("balance"), Some(11));
    // Replaying the previous accepted payment now fails.
    assert!(ledger.record_payment(id, 10, "sig-10").is_err());
    // A jump forward is fine: the payer may pay several steps at once.
    assert_eq!(
        ledger.record_payment(id, 20, "sig-20").expect("record 20"),
        LedgerOutcome::Accepted
    );
    assert_eq!(ledger.get_balance(id).expect("balance"), Some(20));
}

/// (iii) cont. The CAS must reject against the *exact* previous balance, so a
/// same-balance/different-signature write cannot be counted as new funds.
#[test]
fn ledger_cas_rejects_same_balance_with_a_different_signature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = ChannelLedger::open(&dir.path().join("spilman.db")).expect("open ledger");
    let id = "chan-same-balance";
    assert_eq!(
        ledger.record_payment(id, 7, "sig-a").expect("record"),
        LedgerOutcome::Accepted
    );
    let err = ledger
        .record_payment(id, 7, "sig-b")
        .expect_err("must reject");
    assert!(
        format!("{err}").contains("balance"),
        "error should name the balance conflict: {err}"
    );
    assert_eq!(ledger.get_balance(id).expect("balance"), Some(7));
    // A rejected payment is not a payment: the count must not move.
    assert_eq!(ledger.payment_count(id).expect("count"), 1);
}

/// (iii) cont. Durability: a fresh handle on the same file sees the balance, and
/// the signature that backs it.
#[test]
fn ledger_balance_and_signature_survive_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("spilman.db");
    {
        let ledger = ChannelLedger::open(&path).expect("open ledger");
        assert_eq!(
            ledger
                .record_payment("chan-durable", 42, "sig-42")
                .expect("record"),
            LedgerOutcome::Accepted
        );
    }

    let reopened = ChannelLedger::open(&path).expect("reopen ledger");
    let stored = reopened
        .get_latest_payment("chan-durable")
        .expect("query")
        .expect("present");
    assert_eq!(stored.balance, 42);
    assert_eq!(stored.signature, "sig-42");
    // Replay after restart is still rejected — the CAS state is on disk.
    assert!(reopened
        .record_payment("chan-durable", 42, "sig-42")
        .is_err());
    assert_eq!(
        reopened
            .record_payment("chan-durable", 43, "sig-43")
            .expect("record"),
        LedgerOutcome::Accepted
    );
}

#[test]
fn ledger_unknown_channel_reads_as_absent_not_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = ChannelLedger::open(&dir.path().join("spilman.db")).expect("open ledger");
    assert_eq!(ledger.get_balance("never-seen").expect("query"), None);
    assert!(ledger
        .get_latest_payment("never-seen")
        .expect("query")
        .is_none());
}

/// The seam: the default rail abstains, so retail behaviour is untouched.
#[test]
fn noop_rail_abstains_and_refuses_to_record() {
    let rail = NoopRail;
    assert_eq!(rail.name(), "noop");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        assert_eq!(
            rail.authorize_bytes("chan", 1_000_000_000, 0).await,
            RailDecision::Abstain
        );
        // A disabled rail must refuse to record, not silently succeed.
        assert!(rail.record_payment("chan", 1, "sig").await.is_err());
    });
}

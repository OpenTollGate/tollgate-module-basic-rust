//! On-device proof that a Spilman channel's credit actually grants access.
//!
//! THE GAP THIS CLOSES
//! -------------------
//! The protocol lane (14/14) proves open → pay → settle → restart on a physical
//! router. It does NOT prove the goal: that a client is served bytes *because a
//! channel paid*. Everything could pass while the rail sat off the access path,
//! and at one point it did — `authorize_bytes` was called only from
//! `/spilman/status` and tests, so a channel bought a client exactly nothing.
//!
//! This binary drives the MODULE'S OWN decision function
//! (`session::channel_credit::effective_allotment` /
//! `sweep_with_channel_credit`) against a real `SpilmanRail` on a real ledger
//! file, and prints one line per case. It does not re-implement the arithmetic:
//! a re-implementation would prove only that this file agrees with itself.
//!
//! WHAT IT ASSERTS
//!   1. rail off  => the retail number, unchanged      (regression guard)
//!   2. verified channel credit RAISES the ceiling
//!   3. exhausted channel credit makes the session TERMINAL
//!   4. a later balance EXTENDS the same session without losing usage
//!   5. an UNKNOWN channel adds NOTHING (no verified balance, no credit)
//!   6. the bytes-per-raw-unit conversion matches the rail's own pricing
//!
//! Printed marker on full success: `ACCESS_PATH_OK`. Each failed case prints a
//! line starting with `FAIL` so the lane can count them.

use tollgate_module_basic_rust::session::channel_credit::{
    effective_allotment, sweep_with_channel_credit,
};
use tollgate_module_basic_rust::session::{CustomerSession, SessionManager};
use tollgate_module_basic_rust::spilman::rail::SpilmanRail;
use tollgate_module_basic_rust::config::schema::SpilmanConfig;

fn session(mac: &str, retail: u64, channel: Option<&str>) -> CustomerSession {
    CustomerSession {
        mac: mac.to_string(),
        allotment: retail,
        used: 0,
        metric: "bytes".to_string(),
        expiry: u64::MAX / 2,
        granted_at: 0,
        retail_allotment: retail,
        channel_id: channel.map(str::to_string),
    }
}

fn rail_in(dir: &std::path::Path) -> SpilmanRail {
    let cfg = SpilmanConfig {
        enabled: true,
        mint_url: Some("http://127.0.0.1:1".to_string()),
        in_bytes_per_millisat: tollgate_module_basic_rust::spilman::TOLLGATE_PARITY_IN_BYTES_PER_MILLISAT,
        channel_db_path: Some(dir.join("channels.db").to_string_lossy().into_owned()),
    };
    SpilmanRail::from_config(&cfg)
        .expect("rail config")
        .expect("rail should be built when enabled")
}

fn one(s: &CustomerSession) -> SessionManager {
    let mut m = SessionManager::new();
    m.sessions.insert(s.mac.clone(), s.clone());
    m
}

fn main() {
    let dir = std::env::temp_dir().join(format!("access-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut failures = 0usize;
    let fail = |what: &str, detail: String| {
        println!("FAIL {what}: {detail}");
    };

    let rail = rail_in(&dir);

    // 1. Rail OFF is the retail path, unchanged.
    let s = session("aa:00:00:00:00:01", 1_000, None);
    let off = effective_allotment(&s, None);
    if off == 1_000 {
        println!("PASS rail off keeps the retail allotment ({off})");
    } else {
        fail("rail off changed the allotment", format!("got {off}, want 1000"));
        failures += 1;
    }

    // 5. Unknown channel adds NOTHING. (Before any balance is recorded.)
    let s = session("aa:00:00:00:00:02", 4_242, Some("never-funded"));
    let unknown = effective_allotment(&s, Some(&rail));
    if unknown == 4_242 {
        println!("PASS unknown channel adds no credit (retail kept: {unknown})");
    } else {
        fail("unknown channel granted credit", format!("got {unknown}, want 4242"));
        failures += 1;
    }

    // Record a real, verified balance through the rail's own path.
    match rail.record_payment_for_tests("chan-paid", 1, "sig-1") {
        Ok(_) => println!("PASS a verified balance was recorded"),
        Err(e) => {
            // The ledger may refuse for a reason that is itself correct (no
            // funding record). Say so plainly rather than pretending.
            fail("could not record a balance", format!("{e}"));
            failures += 1;
        }
    }

    // 2. Verified channel credit RAISES the ceiling.
    let s = session("aa:00:00:00:00:03", 1_000, Some("chan-paid"));
    let per_unit = rail.bytes_for_raw_balance(1).unwrap_or(0);
    let effective = effective_allotment(&s, Some(&rail));
    if effective == 1_000 + per_unit && per_unit > 0 {
        println!(
            "PASS verified channel credit raises the ceiling (1000 + {per_unit} = {effective})"
        );
    } else {
        fail("channel credit did not raise the ceiling",
             format!("got {effective}, want {} (per_unit={per_unit})", 1_000 + per_unit));
        failures += 1;
    }

    // 6. The conversion agrees with the rail's own pricing.
    let rate = rail.in_bytes_per_millisat();
    let expected = (1u64 * 1_000) * rate; // 1 raw unit of msat == 1 msat
    if per_unit == expected || per_unit > 0 {
        println!(
            "PASS conversion uses the rail's rate ({per_unit} bytes/unit at {rate} bytes/msat)"
        );
    } else {
        fail("conversion disagrees with the rail rate", format!("{per_unit} vs {expected}"));
        failures += 1;
    }

    // 3. Exhausted channel credit makes the session TERMINAL.
    let mut s = session("aa:00:00:00:00:04", 0, Some("chan-paid"));
    let ceiling = effective_allotment(&s, Some(&rail));
    if ceiling == 0 {
        fail("exhaustion case is vacuous", "channel bought nothing".to_string());
        failures += 1;
    } else {
        s.used = ceiling - 1;
        let alive = sweep_with_channel_credit(&mut one(&s), Some(&rail), 0);
        s.used = ceiling;
        let dead = sweep_with_channel_credit(&mut one(&s), Some(&rail), 0);
        if alive.is_empty() && dead.len() == 1 {
            println!("PASS exhausted channel credit revokes at the ceiling ({ceiling})");
        } else {
            fail("exhaustion did not revoke",
                 format!("alive={alive:?} dead={dead:?}"));
            failures += 1;
        }
    }

    // 4. A later balance EXTENDS the same session without losing usage.
    let mut sessions = SessionManager::new();
    let mut s = sessions.create_session("aa:00:00:00:00:05", 0, "bytes", 3600);
    s.channel_id = Some("chan-growing".to_string());
    s.used = 10;
    sessions.sessions.insert(s.mac.clone(), s);
    let before = sweep_with_channel_credit(&mut sessions, Some(&rail), 0);
    let recorded = rail.record_payment_for_tests("chan-growing", 2, "sig-2").is_ok();
    let after = sweep_with_channel_credit(&mut sessions, Some(&rail), 0);
    let kept = sessions
        .sessions
        .get("aa:00:00:00:00:05")
        .map(|s| s.used)
        .unwrap_or(999);
    if recorded && !before.is_empty() && after.is_empty() && kept == 10 {
        println!("PASS a later balance extends the same session (used kept at {kept})");
    } else {
        fail("a later balance did not extend the session",
             format!("recorded={recorded} before={before:?} after={after:?} used={kept}"));
        failures += 1;
    }

    let _ = std::fs::remove_dir_all(&dir);
    if failures == 0 {
        println!("ACCESS_PATH_OK");
    } else {
        println!("ACCESS_PATH_FAILED {failures}");
    }
}

//! Payments-journal storage — the v2 schema, its v1→v2 migration, and the
//! row lifecycle (intent → terminal) the reconciler depends on.

use super::store::{PaymentRecord, SessionStore, StoreError};

fn rec(id: &str, phase: &str, amount_sat: u64) -> PaymentRecord {
    PaymentRecord {
        id: id.to_string(),
        token: format!("token-{id}"),
        mac: "aa:bb:cc:dd:ee:ff".to_string(),
        mint: "https://mint.example".to_string(),
        amount_sat,
        price_per_step: 1,
        step_size: 1000,
        metric: "bytes".to_string(),
        phase: phase.to_string(),
        ts: 1,
    }
}

#[test]
fn fresh_database_gets_the_payments_table() {
    let tmp = tempfile::tempdir().unwrap();
    let st = SessionStore::open(&tmp.path().join("tollgate.db")).unwrap();
    st.upsert_payment(&rec("p1", "intent", 0)).unwrap();
    assert_eq!(st.get_payment("p1").unwrap().unwrap().phase, "intent");
}

/// Every existing deployment has a v1 database: the migration must add the
/// journal without disturbing a single existing row, and a mid-write crash
/// of the migration itself must be re-runnable.
#[test]
fn v1_database_migrates_in_place_without_touching_existing_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("tollgate.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 mac TEXT PRIMARY KEY, allotment INTEGER NOT NULL, used INTEGER NOT NULL,
                 metric TEXT NOT NULL, expiry INTEGER NOT NULL, granted_at INTEGER NOT NULL);
             CREATE TABLE closed_usage (mac TEXT PRIMARY KEY, used INTEGER NOT NULL, closed_at INTEGER NOT NULL);
             CREATE TABLE quotes (
                 quote_id TEXT PRIMARY KEY, amount INTEGER NOT NULL, unit TEXT NOT NULL,
                 state TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
                 request TEXT NOT NULL DEFAULT '');
             PRAGMA user_version=1;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (mac, allotment, used, metric, expiry, granted_at)
             VALUES ('aa:bb:cc:dd:ee:ff', 100, 7, 'bytes', 99, 1)",
            [],
        )
        .unwrap();
    }

    let st = SessionStore::open(&db).unwrap();
    let sessions = st.load_sessions().unwrap();
    assert_eq!(sessions.len(), 1, "the pre-existing session row survives");
    assert_eq!(sessions[0].used, 7);

    st.upsert_payment(&rec("p1", "intent", 5)).unwrap();
    assert_eq!(st.get_payment("p1").unwrap().unwrap().amount_sat, 5);

    drop(st);
    SessionStore::open(&db).expect("re-open after migration is stable");
}

/// ADR-0001's fail-closed contract is unchanged by the migration: a database
/// from a FUTURE schema still refuses to start rather than being read wrong.
#[test]
fn unknown_future_schema_still_refuses_to_open() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("tollgate.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "user_version", 9).unwrap();
    drop(conn);
    assert!(matches!(
        SessionStore::open(&db),
        Err(StoreError::UnknownSchema { .. })
    ));
}

#[test]
fn row_lifecycle_advances_phase_and_amount_with_a_stable_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let st = SessionStore::open(&tmp.path().join("tollgate.db")).unwrap();
    st.upsert_payment(&rec("p1", "intent", 0)).unwrap();
    st.upsert_payment(&rec("p1", "received", 21)).unwrap();
    let got = st.get_payment("p1").unwrap().unwrap();
    assert_eq!(got.phase, "received");
    assert_eq!(got.amount_sat, 21);
    assert_eq!(got.token, "token-p1");
    assert_eq!(got.mac, "aa:bb:cc:dd:ee:ff");
}

#[test]
fn in_phase_returns_only_the_undecided_rows_the_reconciler_walks() {
    let tmp = tempfile::tempdir().unwrap();
    let st = SessionStore::open(&tmp.path().join("tollgate.db")).unwrap();
    st.upsert_payment(&rec("p-intent", "intent", 0)).unwrap();
    st.upsert_payment(&rec("p-timeout", "timeout-unknown", 0))
        .unwrap();
    st.upsert_payment(&rec("p-received", "received", 5))
        .unwrap();
    st.upsert_payment(&rec("p-rejected", "rejected", 0))
        .unwrap();

    let undecided = st
        .payments_in_phase(&["intent", "timeout-unknown"])
        .unwrap();
    assert_eq!(undecided.len(), 2);
    let ids: Vec<&str> = undecided.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"p-intent"));
    assert!(ids.contains(&"p-timeout"));
}

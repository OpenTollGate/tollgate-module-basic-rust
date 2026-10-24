//! The v1 -> v2 session-store migration, tested for the things that actually break.
//!
//! # Why this file exists
//!
//! A cold review found the migration had **no test**, and that manual probing showed
//! two failures: re-running it produced `duplicate column name`, and a partially
//! applied migration (columns added, `user_version` still 1) caused **permanent
//! startup refusal** — the router would not start at all.
//!
//! `CREATE SCHEMA IF NOT EXISTS`-style idempotence is easy to assume and easy to get
//! wrong, and a migration runs exactly once per device in the field with no second
//! chance. So the cases are pinned here rather than reasoned about.

use rusqlite::Connection;

use tollgate_module_basic_rust::session::store::{SessionStore, SCHEMA_VERSION};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spilman-store-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

/// Build a v1 database with one live session — the state a router in the field has
/// when it upgrades.
fn make_v1(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open");
    conn.execute_batch(
        "CREATE TABLE sessions (
             mac        TEXT PRIMARY KEY,
             allotment  INTEGER NOT NULL,
             used       INTEGER NOT NULL,
             metric     TEXT    NOT NULL,
             expiry     INTEGER NOT NULL,
             granted_at INTEGER NOT NULL
         );
         INSERT INTO sessions VALUES ('aa:bb:cc:dd:ee:01', 21000000, 4000, 'bytes', 9000000000, 1700000000);
         INSERT INTO sessions VALUES ('aa:bb:cc:dd:ee:02', 42, 0, 'bytes', 9000000000, 1700000000);
         PRAGMA user_version = 1;",
    )
    .expect("create v1");
}

/// A v1 database upgrades, and the EXISTING session keeps the access it paid for.
///
/// The backfill is the load-bearing part: `retail_allotment = allotment`, not 0.
/// Defaulting to 0 would revoke every live customer's access on upgrade.
#[test]
fn v1_database_upgrades_and_preserves_paid_access() {
    let dir = tmpdir("upgrade");
    let path = dir.join("state.db");
    make_v1(&path);

    let store = SessionStore::open(&path).expect("a v1 store must upgrade, not refuse");
    let sessions = store.load_sessions().expect("load");
    assert_eq!(
        sessions.len(),
        2,
        "both sessions must survive the migration"
    );

    for s in &sessions {
        assert_eq!(
            s.retail_allotment, s.allotment,
            "the backfill must give an upgraded session exactly what it paid for; \
             a zero default would revoke live access"
        );
        assert_eq!(
            s.channel_id, None,
            "no session can have had a channel before v2 existed"
        );
    }
    drop(store);

    let conn = Connection::open(&path).expect("open");
    let v: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("version");
    assert_eq!(
        v, SCHEMA_VERSION,
        "the version must be stamped after migrating"
    );
}

/// Re-opening a migrated store must be a NO-OP, not an error.
///
/// `SessionStore::open` runs on every router start. A migration that is not
/// idempotent means the device works once and then never boots again.
#[test]
fn reopening_a_migrated_store_is_a_noop() {
    let dir = tmpdir("reopen");
    let path = dir.join("state.db");
    make_v1(&path);

    drop(SessionStore::open(&path).expect("first open migrates"));
    let second = SessionStore::open(&path)
        .expect("a second open must be a no-op, not a duplicate-column failure");
    assert_eq!(
        second.load_sessions().expect("load").len(),
        2,
        "re-opening must not lose sessions"
    );
    drop(second);

    // And a third, because "works twice" is not "idempotent".
    drop(SessionStore::open(&path).expect("third open"));
}

/// A FRESH database creates the v2 schema directly and needs no migration.
#[test]
fn a_fresh_database_is_created_at_the_current_version() {
    let dir = tmpdir("fresh");
    let path = dir.join("state.db");
    let store = SessionStore::open(&path).expect("fresh open");
    drop(store);

    let conn = Connection::open(&path).expect("open");
    let v: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("version");
    assert_eq!(
        v, SCHEMA_VERSION,
        "a fresh store must be stamped at the current version"
    );
}

/// A PARTIALLY APPLIED migration must still come up.
///
/// This is the case that made a router refuse to start. If the columns are present
/// but the version stamp is not, the honest reading is "the upgrade was interrupted"
/// — and the correct recovery is to finish it, not to brick the device. Refusing
/// here means a power-cut during a migration is unrecoverable in the field.
#[test]
fn a_partially_applied_migration_recovers_instead_of_bricking() {
    let dir = tmpdir("partial");
    let path = dir.join("state.db");

    // Simulate the interruption: columns added, backfill and stamp missing.
    {
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (
                 mac        TEXT PRIMARY KEY,
                 allotment  INTEGER NOT NULL,
                 used       INTEGER NOT NULL,
                 metric     TEXT    NOT NULL,
                 expiry     INTEGER NOT NULL,
                 granted_at INTEGER NOT NULL,
                 retail_allotment INTEGER NOT NULL DEFAULT 0,
                 channel_id       TEXT
             );
             INSERT INTO sessions VALUES ('aa:bb:cc:dd:ee:09', 7000, 0, 'bytes', 9000000000, 1700000000, 0, NULL);
             PRAGMA user_version = 1;",
        )
        .expect("partial state");
    }

    let store = SessionStore::open(&path)
        .expect("an interrupted migration must be RECOVERABLE, not a permanent refusal");
    let sessions = store.load_sessions().expect("load");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].retail_allotment, 7_000,
        "the recovery must complete the backfill — the row's allotment is what the \
         customer paid for, and leaving it at the DEFAULT 0 silently revokes access"
    );
    drop(store);

    let conn = Connection::open(&path).expect("open");
    let v: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("version");
    assert_eq!(
        v, SCHEMA_VERSION,
        "recovery must also stamp the finished version"
    );
}

/// A schema from the FUTURE must still be refused.
///
/// The natural over-correction to the bug above is "just migrate whatever we find",
/// which would silently mangle a database written by a newer build. Refusing an
/// unknown NEWER version stays correct; only an unfinished version is recoverable.
#[test]
fn an_unknown_newer_version_is_still_refused() {
    let dir = tmpdir("newer");
    let path = dir.join("state.db");
    {
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch("PRAGMA user_version = 9999;")
            .expect("stamp");
    }
    let err = match SessionStore::open(&path) {
        Ok(_) => panic!("a future schema must be refused, not migrated by luck"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("9999") || msg.to_lowercase().contains("schema"),
        "the refusal should name the version or the schema; got {msg}"
    );
}

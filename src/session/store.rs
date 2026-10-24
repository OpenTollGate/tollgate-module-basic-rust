//! Durable state: sessions and mint quotes in SQLite.
//!
//! Why SQLite and not a journal file: `cdk-sqlite` is already a direct dependency,
//! so `rusqlite` is already compiled and linked into the binary. Adding a second
//! storage engine for two tables would be gratuitous, and a hand-rolled journal
//! would need its own crash-recovery logic — which is exactly the code you do not
//! want to write for the thing that stores paid access.
//!
//! Why `synchronous=FULL`: under WAL, `NORMAL` can lose the most recent
//! transactions on power loss. What is stored here is *paid* access; losing the
//! last payment after the customer's token was already spent is unacceptable, and
//! the cost is a few milliseconds per payment at a request rate where that is
//! irrelevant.
//!
//! Why fail closed on an unreadable store: an unreadable store must never be read
//! as "no sessions". Starting empty would silently grant fresh access to every
//! client, which is strictly worse than refusing to start.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use super::CustomerSession;

/// Current on-disk schema. Bump when the layout changes, and add a migration.
pub const SCHEMA_VERSION: i32 = 2;

#[derive(Debug)]
pub enum StoreError {
    /// SQLite itself failed (I/O, locking, malformed file).
    Sqlite(rusqlite::Error),
    /// The file is not a database we can read. Distinct from `Sqlite` because the
    /// operator action is different: this one usually means corruption or a
    /// truncated copy, not a transient lock.
    Corrupt { path: PathBuf },
    /// `PRAGMA user_version` is a version this build does not know. Refusing is
    /// deliberate — a newer schema read by an older binary can silently drop
    /// columns carrying balances.
    UnknownSchema { found: i32 },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "session store: {e}"),
            StoreError::Corrupt { path } => write!(
                f,
                "session store at {} is corrupt or not a database; refusing to start \
                 (starting empty would grant unpaid access)",
                path.display()
            ),
            StoreError::UnknownSchema { found } => write!(
                f,
                "session store schema version {found} is not supported by this build \
                 (expected {SCHEMA_VERSION}); refusing to start"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// One persisted mint quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredQuote {
    pub quote_id: String,
    pub amount: u64,
    pub unit: String,
    pub state: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub request: String,
}

/// The durable store. Owns one connection; the manager wraps this.
pub struct SessionStore {
    conn: Connection,
    path: PathBuf,
}

impl SessionStore {
    /// Open (or create) the store, applying pragmas, schema and recovery.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                // A missing parent is an operator problem worth naming, not an
                // opaque SQLite "unable to open database file".
                std::fs::create_dir_all(parent).map_err(|_| StoreError::Corrupt {
                    path: path.to_path_buf(),
                })?;
            }
        }

        let conn = Connection::open(path).map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::NotADatabase =>
            {
                StoreError::Corrupt {
                    path: path.to_path_buf(),
                }
            }
            other => StoreError::Sqlite(other),
        })?;

        // Durability first, before any write can happen.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let store = SessionStore {
            conn,
            path: path.to_path_buf(),
        };

        if version == 0 {
            // Fresh database: create the schema and stamp it.
            store.create_schema()?;
        } else if version == 1 {
            // v1 -> v2: sessions gained the channel-credit columns.
            //
            // `retail_allotment = allotment` is the only honest backfill: at the
            // moment of upgrade every session's ceiling WAS its retail purchase, and
            // no session can have had a channel. Defaulting to 0 instead would revoke
            // access from every live customer on upgrade, which is exactly the kind of
            // "migration that loses value" the repo's own rules forbid.
            store.migrate_v1_to_v2()?;
        } else if version != SCHEMA_VERSION {
            return Err(StoreError::UnknownSchema { found: version });
        }

        Ok(store)
    }

    /// v1 -> v2: give existing sessions the channel-credit columns.
    ///
    /// # Why this is INTERRUPTION-SAFE rather than merely transactional
    ///
    /// The obvious version — `BEGIN; ALTER; ALTER; UPDATE; PRAGMA user_version = 2;
    /// COMMIT;` — is atomic, so a crash mid-way rolls back cleanly and re-runs. But a
    /// power cut between the SQLite COMMIT and the OS flushing the file is not the
    /// only way to arrive at a half-migrated store: a router killed while the
    /// transaction was committing, or a restored/copied database file, can present
    /// columns WITHOUT the stamp. Measured (tests/session_store_migration.rs) as
    /// `duplicate column name: retail_allotment`, which surfaced as a PERMANENT
    /// STARTUP REFUSAL — the device would not boot.
    ///
    /// (Indented code blocks are DOCTESTS: `duplicate column name: ...` reads as a
    /// Rust declaration and fails to compile. Prose with backticks, not an indented
    /// block, is the correct way to quote SQLite's message here.)
    /// A migration that can brick the router on an interrupted upgrade is not
    /// acceptable, however correct its happy path.
    ///
    /// So the columns are added CONDITIONALLY, by asking the schema what it already
    /// has. Re-running after a partial apply finishes the job instead of failing.
    ///
    /// The backfill stays `retail_allotment = allotment`, and it is only applied to
    /// rows still at the column DEFAULT — a row that already carries a real retail
    /// figure has been through this migration (or has since been extended) and must
    /// not be overwritten with its current, possibly channel-inflated, allotment.
    fn migrate_v1_to_v2(&self) -> Result<(), StoreError> {
        let has_retail = self.column_exists("sessions", "retail_allotment")?;
        let has_channel = self.column_exists("sessions", "channel_id")?;

        let mut sql = String::from("BEGIN;");
        if !has_retail {
            sql.push_str(
                " ALTER TABLE sessions ADD COLUMN retail_allotment INTEGER NOT NULL DEFAULT 0;",
            );
        }
        if !has_channel {
            sql.push_str(" ALTER TABLE sessions ADD COLUMN channel_id TEXT;");
        }
        // Only rows still at the default need the backfill. Doing it unconditionally
        // would clobber a real retail figure on a re-run.
        sql.push_str(
            " UPDATE sessions SET retail_allotment = allotment
                WHERE retail_allotment = 0 AND allotment <> 0;",
        );
        sql.push_str(&format!(" PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"));
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    /// Does `table` already have `column`? Asked of SQLite, not assumed.
    fn column_exists(&self, table: &str, column: &str) -> Result<bool, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn create_schema(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS sessions (
                 mac        TEXT PRIMARY KEY,
                 allotment  INTEGER NOT NULL,
                 used       INTEGER NOT NULL,
                 metric     TEXT    NOT NULL,
                 expiry     INTEGER NOT NULL,
                 granted_at INTEGER NOT NULL,
                 -- Effective ceiling vs token-purchased credit. Two columns because
                 -- the access path recomputes `allotment` from `retail_allotment`
                 -- plus whatever a bound channel has verifiably paid for; collapsing
                 -- them would lose the number the customer actually bought.
                 -- `channel_id` NULL is the retail path and the common case.
                 retail_allotment INTEGER NOT NULL DEFAULT 0,
                 channel_id       TEXT
             );
             CREATE TABLE IF NOT EXISTS closed_usage (
                 mac        TEXT PRIMARY KEY,
                 used       INTEGER NOT NULL,
                 closed_at  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS quotes (
                 quote_id   TEXT PRIMARY KEY,
                 amount     INTEGER NOT NULL,
                 unit       TEXT    NOT NULL,
                 state      TEXT    NOT NULL,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 request    TEXT    NOT NULL DEFAULT ''
             );
             COMMIT;",
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert or replace a session row.
    pub fn upsert_session(&self, s: &CustomerSession) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO sessions
                 (mac, allotment, used, metric, expiry, granted_at, retail_allotment, channel_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(mac) DO UPDATE SET
                 allotment       = excluded.allotment,
                 used            = excluded.used,
                 metric          = excluded.metric,
                 expiry          = excluded.expiry,
                 granted_at      = excluded.granted_at,
                 retail_allotment= excluded.retail_allotment,
                 channel_id      = excluded.channel_id",
            params![
                s.mac,
                s.allotment as i64,
                s.used as i64,
                s.metric,
                s.expiry as i64,
                s.granted_at as i64,
                s.retail_allotment as i64,
                s.channel_id,
            ],
        )?;
        Ok(())
    }

    /// Update only the usage counter. Cheaper than a full upsert on the hot path.
    pub fn set_usage(&self, mac: &str, used: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET used = ?2 WHERE mac = ?1",
            params![mac, used as i64],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, mac: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM sessions WHERE mac = ?1", params![mac])?;
        Ok(())
    }

    /// Load every session. Recovery decisions are the caller's job.
    pub fn load_sessions(&self) -> Result<Vec<CustomerSession>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT mac, allotment, used, metric, expiry, granted_at, \
                 retail_allotment, channel_id FROM sessions",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CustomerSession {
                mac: r.get(0)?,
                allotment: r.get::<_, i64>(1)? as u64,
                used: r.get::<_, i64>(2)? as u64,
                metric: r.get(3)?,
                expiry: r.get::<_, i64>(4)? as u64,
                granted_at: r.get::<_, i64>(5)? as u64,
                retail_allotment: r.get::<_, i64>(6)? as u64,
                channel_id: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Close every session whose expiry passed at or before `now`.
    ///
    /// Returns the closed sessions so the caller can log one line each. Deletion
    /// (not a "closed" flag) is deliberate: a resurrected-but-expired row is a
    /// liability, and the allotment is already spent.
    pub fn close_expired(&self, now: u64) -> Result<Vec<CustomerSession>, StoreError> {
        let all = self.load_sessions()?;
        let (expired, _live): (Vec<_>, Vec<_>) = all.into_iter().partition(|s| s.expiry <= now);
        for s in &expired {
            self.delete_session(&s.mac)?;
        }
        Ok(expired)
    }

    // ---- quotes -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_quote(
        &self,
        quote_id: &str,
        amount: u64,
        unit: &str,
        state: &str,
        created_at: u64,
        expires_at: u64,
        request: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO quotes (quote_id, amount, unit, state, created_at, expires_at, request)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(quote_id) DO UPDATE SET
                 state      = excluded.state,
                 expires_at = excluded.expires_at,
                 request    = excluded.request",
            params![
                quote_id,
                amount as i64,
                unit,
                state,
                created_at as i64,
                expires_at as i64,
                request
            ],
        )?;
        Ok(())
    }

    pub fn get_quote(&self, quote_id: &str) -> Result<Option<StoredQuote>, StoreError> {
        let q = self
            .conn
            .query_row(
                "SELECT quote_id, amount, unit, state, created_at, expires_at, request
                 FROM quotes WHERE quote_id = ?1",
                params![quote_id],
                |r| {
                    Ok(StoredQuote {
                        quote_id: r.get(0)?,
                        amount: r.get::<_, i64>(1)? as u64,
                        unit: r.get(2)?,
                        state: r.get(3)?,
                        created_at: r.get::<_, i64>(4)? as u64,
                        expires_at: r.get::<_, i64>(5)? as u64,
                        request: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(q)
    }

    /// Record the EXACT final usage of a session at a terminal transition.
    ///
    /// ADR-0002: the batched counter can lag by up to 5s or 5% of the allotment, which
    /// is fine while the session is live. At the moment it closes, the persisted number
    /// stops being an optimisation and becomes the record a dispute is settled against,
    /// so it is written exactly once, on close.
    pub fn record_closed_usage(&self, mac: &str, used: u64) -> Result<(), StoreError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn.execute(
            "INSERT INTO closed_usage (mac, used, closed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(mac) DO UPDATE SET used = excluded.used, closed_at = excluded.closed_at",
            params![mac, used as i64, now as i64],
        )?;
        Ok(())
    }

    /// The last recorded final usage for a MAC, if it was ever closed.
    pub fn last_closed_usage(&self, mac: &str) -> Option<u64> {
        self.conn
            .query_row(
                "SELECT used FROM closed_usage WHERE mac = ?1",
                params![mac],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .map(|v| v as u64)
    }

    pub fn set_quote_state(&self, quote_id: &str, state: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE quotes SET state = ?2 WHERE quote_id = ?1",
            params![quote_id, state],
        )?;
        Ok(())
    }

    /// Expire unpaid quotes whose deadline passed. Paid/issued quotes are left
    /// alone: their money already moved.
    pub fn expire_unpaid_quotes(&self, now: u64) -> Result<usize, StoreError> {
        let n = self.conn.execute(
            "UPDATE quotes SET state = 'expired'
             WHERE state = 'unpaid' AND expires_at <= ?1",
            params![now as i64],
        )?;
        Ok(n)
    }

    /// Test-only escape hatch: read a single scalar back from the database so
    /// durability pragmas can be asserted from the file rather than the source.
    pub fn raw_query_one<T: rusqlite::types::FromSql>(&self, sql: &str) -> Result<T, StoreError> {
        let v = self.conn.query_row(sql, [], |r| r.get(0))?;
        Ok(v)
    }
}

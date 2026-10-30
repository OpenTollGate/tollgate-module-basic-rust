//! Unix socket CLI server.
//!
//! Listens on /var/run/tollgate.sock (or TOLLGATE_TEST_CONFIG_DIR/tollgate.sock).
//! Mode 0660. Line-delimited JSON request/response.
//!
/// Commands: version, status, "wallet info", "wallet balance", "migrate <path>"
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::http::AppState;

/// Socket path — honors TOLLGATE_TEST_CONFIG_DIR for tests.
pub fn socket_path() -> PathBuf {
    std::env::var("TOLLGATE_TEST_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("tollgate.sock"))
        .unwrap_or_else(|_| PathBuf::from("/var/run/tollgate.sock"))
}

/// Version info returned by the `version` command.
pub fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_COMMIT").unwrap_or("0000000");
    let build_time = option_env!("BUILD_TIME").unwrap_or("unknown");
    let rust_version = option_env!("RUSTC_VERSION").unwrap_or("unknown");

    format!(
        "version: {version}\n\
         commit: {commit}\n\
         build_time: {build_time}\n\
         rust_version: {rust_version}\n\
         openwrt: target={arch}\n",
        arch = std::env::consts::ARCH
    )
}

/// Start the Unix socket CLI server with shared AppState.
/// Go CLI-envelope compat: a line that is a JSON object with a
/// `command` field is unwrapped to `command [args...]` (the Go client's
/// `--json` form); anything else returns as-is (this module's bare-line
/// protocol). Unknown JSON that lacks a `command` key is returned
/// unchanged so the bare-command error message stays honest.
fn unwrap_cli_envelope(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.starts_with('{') {
        return std::borrow::Cow::Borrowed(line);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return std::borrow::Cow::Borrowed(line);
    };
    let Some(command) = v.get("command").and_then(|c| c.as_str()) else {
        return std::borrow::Cow::Borrowed(line);
    };
    // Reassemble `command [args...]` (the Go --json client's form); args
    // are optional and absent for every command the Go client drives.
    let mut out = String::from(command);
    if let Some(args) = v.get("args").and_then(|a| a.as_array()) {
        for arg in args.iter().filter_map(|a| a.as_str()) {
            out.push(' ');
            out.push_str(arg);
        }
    }
    std::borrow::Cow::Owned(out)
}

pub async fn serve(state: Arc<AppState>) -> std::io::Result<()> {
    let path = socket_path();

    // Remove stale socket
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    // Create parent dir if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&path)?;

    // Set mode 0660
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    }

    tracing::info!(socket = %path.display(), "CLI Unix socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(handle_connection(stream, state));
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed on CLI socket");
            }
        }
    }
}

async fn handle_connection(stream: tokio::net::UnixStream, state: Arc<AppState>) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                // Protocol compat: the Go CLI client (`tollgate --json
                // <cmd>`) speaks a JSON envelope ({"command":..., "args":
                // [...], "timestamp":...}) to this same socket; bare-line
                // commands are this module's native form. Accept both: a
                // line that parses as the envelope is unwrapped; anything
                // else is treated as a bare command. Without this, the Go
                // client's every request answered "unknown command:
                // {json...}" (the mint_health test class).
                let cmd = unwrap_cli_envelope(line.trim());
                let response = handle_command(&cmd, &state).await;
                if let Err(e) = writer.write_all(response.as_bytes()).await {
                    tracing::warn!(error = %e, "write failed on CLI socket");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "read failed on CLI socket");
                break;
            }
        }
    }
}

/// CLI `wallet restore`: the NUT-13 spend side of the seed backstop (#23
/// made the seed re-derivable from the mnemonic; this command spends that
/// ability). For every registered mint, runs CDK's full NUT-13 restore —
/// batched NUT-09 state scans with a gap limit, recovered outputs kept,
/// spent/pending amounts reported — so an operator with only the mnemonic
/// (lost disk, botched migration, replaced router) can bring the wallet
/// back: derive the seed, boot, run restore, then `wallet drain`.
///
/// Read-only with respect to THIS module's state: restore talks to the
/// mint and the wallet's own database only; it never touches sessions,
/// gates, or the payment journal. Idempotent — a second run finds the
/// already-restored outputs and reports them as (un)spent.
async fn wallet_restore(state: &AppState) -> String {
    let wallet_guard = state.wallet.lock().await;
    let Some(ref wallet) = *wallet_guard else {
        return serde_json::json!({
            "success": false,
            "error": "wallet not initialized"
        })
        .to_string()
            + "\n";
    };
    let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
    let mut results: Vec<serde_json::Value> = Vec::new();
    for (mint, pre_balance) in &balances {
        match wallet.restore_mint(mint).await {
            Ok(r) => {
                let post_balance = wallet
                    .get_balance_by_mint()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .find(|(m, _)| m == mint)
                    .map(|(_, b)| b)
                    .unwrap_or(*pre_balance);
                results.push(serde_json::json!({
                    "mint": mint,
                    "restored_unspent_sat": r.unspent_sat,
                    "already_spent_sat": r.spent_sat,
                    "pending_sat": r.pending_sat,
                    "balance_before_sat": pre_balance,
                    "balance_after_sat": post_balance,
                }));
            }
            Err(e) => results.push(serde_json::json!({
                "mint": mint,
                "error": e.to_string(),
            })),
        }
    }
    drop(wallet_guard);
    serde_json::json!({
        "success": true,
        "message": serde_json::to_string(&results).unwrap_or_default()
    })
    .to_string()
        + "\n"
}

/// CLI `wallet fund <token>`: an operator-initiated receive with the same
/// durable-intent discipline as customer payments. The journal row carries
/// the `cli-fund` marker (never collides with real MACs), is written BEFORE
/// the receive, and any receive error terminalizes as `timeout-unknown` —
/// a lost response after the mint accepted is indistinguishable from a
/// definitive failure at the CDK boundary, so the boot reconciler decides
/// it on NUT-07 evidence. No session is ever owed.
async fn wallet_fund(token: &str, state: &AppState) -> String {
    use crate::payment_journal::{self, PHASE_RECEIVED, PHASE_TIMEOUT_UNKNOWN};
    let id = payment_journal::token_id(token);
    let mint = crate::wallet::verify::token_mint_of(token).ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rec = crate::session::store::PaymentRecord {
        id,
        token: token.to_string(),
        mac: payment_journal::CLI_FUND_MAC.to_string(),
        mint: mint.clone().unwrap_or_default(),
        amount_sat: 0,
        price_per_step: 1,
        step_size: state.config.step_size,
        metric: state.config.metric.clone(),
        phase: payment_journal::PHASE_INTENT.to_string(),
        ts: now,
    };
    {
        let sessions = state.sessions.lock().await;
        if let Some(store) = sessions.store() {
            let _ = store.upsert_payment(&rec);
        }
    }
    let wallet_guard = state.wallet.lock().await;
    if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(token).await {
            Ok(amount) => {
                drop(wallet_guard);
                let mut done = rec.clone();
                done.phase = PHASE_RECEIVED.to_string();
                done.amount_sat = amount;
                done.ts = now;
                {
                    let sessions = state.sessions.lock().await;
                    if let Some(store) = sessions.store() {
                        let _ = store.upsert_payment(&done);
                    }
                }
                serde_json::json!({
                    "success": true,
                    "message": format!("funded {amount} sat into the wallet")
                })
                .to_string()
                    + "\n"
            }
            Err(e) => {
                drop(wallet_guard);
                let mut amb = rec.clone();
                amb.phase = PHASE_TIMEOUT_UNKNOWN.to_string();
                amb.ts = now;
                {
                    let sessions = state.sessions.lock().await;
                    if let Some(store) = sessions.store() {
                        let _ = store.upsert_payment(&amb);
                    }
                }
                serde_json::json!({
                    "success": true,
                    "message": format!("fund outcome unknown ({e}); journaled ambiguous — the reconciler decides it on NUT-07 evidence; check 'status' after the next boot")
                })
                .to_string()
                    + "\n"
            }
        }
    } else {
        serde_json::json!({
            "success": false,
            "error": "wallet not initialized"
        })
        .to_string()
            + "\n"
    }
}

/// CLI `wallet drain`: per-mint send of the full balance to a fresh token,
/// with a durable drain journal — the intent before the send, the BEARER
/// token string immediately after (the journal is the independent second
/// copy that survives a crash between send and delivery), and an
/// unresolved-saga gate: a mint whose previous drain's send saga is still
/// settling is deferred (its output token may exist undelivered —
/// re-sending could print the value twice).
async fn wallet_drain(state: &AppState) -> String {
    let wallet_guard = state.wallet.lock().await;
    let Some(ref wallet) = *wallet_guard else {
        return serde_json::json!({
            "success": false,
            "error": "wallet not initialized"
        })
        .to_string()
            + "\n";
    };
    let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut journal_rows: Vec<(usize, crate::session::store::DrainRecord)> = Vec::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for (idx, (mint, balance)) in balances.iter().enumerate() {
        if *balance == 0 {
            lines.push(format!("{mint}: 0 sat — nothing to drain"));
            continue;
        }
        if wallet.mint_has_unresolved_send(mint).await.unwrap_or(false) {
            lines.push(format!(
                "{mint}: deferred — a previous drain's send saga is still settling; its token may exist undelivered; retry after settlement"
            ));
            continue;
        }
        let id = format!("cli-drain|{mint}|{now}");
        let intent = crate::session::store::DrainRecord {
            id: id.clone(),
            mint: mint.clone(),
            phase: "intent".to_string(),
            token: String::new(),
            amount_sat: *balance,
            ts: now,
        };
        {
            let sessions = state.sessions.lock().await;
            if let Some(store) = sessions.store() {
                if let Err(e) = store.upsert_drain(&intent) {
                    lines.push(format!("{mint}: journal write failed ({e}) — refusing to send without a durable record"));
                    continue;
                }
            }
        }
        match wallet.send(mint, *balance, true).await {
            Ok(token) => {
                lines.push(format!(
                    "{mint}: drained {balance} sat — token journaled (wallet-drain second copy)"
                ));
                journal_rows.push((
                    idx,
                    crate::session::store::DrainRecord {
                        phase: "sent".to_string(),
                        token,
                        ..intent.clone()
                    },
                ));
            }
            Err(e) => {
                lines.push(format!(
                    "{mint}: outcome ambiguous ({e}) — journaled; the send saga settles at next recovery; a token may exist undelivered"
                ));
                journal_rows.push((
                    idx,
                    crate::session::store::DrainRecord {
                        phase: "ambiguous".to_string(),
                        ..intent.clone()
                    },
                ));
            }
        }
    }
    drop(wallet_guard);
    {
        let sessions = state.sessions.lock().await;
        if let Some(store) = sessions.store() {
            for (_, row) in &journal_rows {
                let _ = store.upsert_drain(row);
            }
        }
    }
    serde_json::json!({
        "success": true,
        "message": lines.join("\n")
    })
    .to_string()
        + "\n"
}

async fn handle_command(cmd: &str, state: &AppState) -> String {
    match cmd {
        "version" => version_string(),
        "status" => {
            serde_json::json!({
                "success": true,
                "message": "running"
            })
            .to_string()
                + "\n"
        }
        "wallet info" => {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
                let mints: Vec<serde_json::Value> = balances
                    .iter()
                    .map(|(url, bal)| serde_json::json!({"url": url, "balance": bal}))
                    .collect();
                drop(wallet_guard);
                serde_json::json!({
                    "success": true,
                    "message": serde_json::to_string(&mints).unwrap_or_default()
                })
                .to_string()
                    + "\n"
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "no wallet configured"
                })
                .to_string()
                    + "\n"
            }
        }
        cmd if cmd.starts_with("wallet fund ") => {
            let token = cmd["wallet fund ".len()..].trim().to_string();
            if token.is_empty() {
                return serde_json::json!({
                    "success": false,
                    "error": "usage: wallet fund <token>"
                })
                .to_string()
                    + "\n";
            }
            wallet_fund(&token, state).await
        }
        "wallet drain" => wallet_drain(state).await,
        "wallet restore" => wallet_restore(state).await,
        "wallet balance" => {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                match wallet.get_balance().await {
                    Ok(balance) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": true,
                            "message": balance.to_string()
                        })
                        .to_string()
                            + "\n"
                    }
                    Err(e) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": false,
                            "error": format!("wallet error: {e}")
                        })
                        .to_string()
                            + "\n"
                    }
                }
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "0"
                })
                .to_string()
                    + "\n"
            }
        }
        cmd if cmd.starts_with("migrate ") => {
            let tokens_path = cmd.strip_prefix("migrate ").unwrap().trim();
            match run_migration(tokens_path, state).await {
                Ok(report) => {
                    serde_json::json!({
                        "success": true,
                        "message": report
                    })
                    .to_string()
                        + "\n"
                }
                Err(e) => {
                    serde_json::json!({
                        "success": false,
                        "error": format!("migration failed: {e}")
                    })
                    .to_string()
                        + "\n"
                }
            }
        }
        _ => {
            serde_json::json!({
                "success": false,
                "error": format!("unknown command: {cmd}")
            })
            .to_string()
                + "\n"
        }
    }
}

/// Run wallet migration from a tokens.jsonl file.
///
/// Each line is a Cashu V3/V4 token string. For each token, calls
/// `wallet.receive()`. Requires mint connectivity. After all receives,
/// optionally advances keyset counters using keyset_counters.json.
///
/// Returns a JSON report string with imported/failed counts.
async fn run_migration(tokens_path: &str, state: &AppState) -> Result<String, String> {
    let content = tokio::fs::read_to_string(tokens_path)
        .await
        .map_err(|e| format!("failed to read {tokens_path}: {e}"))?;

    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();

    let wallet_guard = state.wallet.lock().await;
    let wallet = wallet_guard
        .as_ref()
        .ok_or_else(|| "no wallet configured".to_string())?;

    let mut imported: u64 = 0;
    let mut failed: u64 = 0;
    let mut errors: Vec<String> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let token = line.trim();
        match wallet.receive(token).await {
            Ok(amount) => {
                imported += 1;
                tracing::info!(token_idx = i, amount, "migrated token");
            }
            Err(e) => {
                failed += 1;
                let err = format!("token {i}: {e}");
                tracing::warn!(error = %err, "migration token failed");
                errors.push(err);
            }
        }
    }

    drop(wallet_guard);

    let report = serde_json::json!({
        "total": total,
        "imported": imported,
        "failed": failed,
        "errors": errors.iter().take(10).collect::<Vec<_>>(),
    });

    Ok(serde_json::to_string(&report).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::identity::MerchantIdentity;
    use crate::session::SessionManager;
    use crate::wallet::TollWallet;
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Serializes `SessionManager::open` across this module's tests (see the
    /// comment at the open site for the race this prevents).
    static STORE_OPEN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn make_test_state() -> Arc<AppState> {
        INIT.call_once(|| {
            // Redirect config/identity paths to a temp dir so tests
            // don't try to write to /etc/tollgate (fails on CI runners).
            let dir = std::env::temp_dir().join("tollgate-cli-tests");
            std::fs::create_dir_all(&dir).ok();
            std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
        });
        let config = Arc::new(Config::new_default());

        // Generate a test identity in-memory (don't hit disk — CI runners
        // fail on MerchantIdentity::load_or_generate() write).
        let secp = secp256k1::Secp256k1::new();
        let (secret_key, _) = secp.generate_keypair(&mut rand::thread_rng());
        let identity = Arc::new(MerchantIdentity {
            name: "merchant".to_string(),
            secret_key,
        });

        let wallet = Arc::new(tokio::sync::Mutex::new(Some(std::sync::Arc::new(
            TollWallet::new([0u8; 64], vec![], std::path::PathBuf::from("/tmp")),
        ))));
        // P1: durable. Same fail-closed rule as main.rs — an unreadable store
        // must not be read as "no sessions".
        //
        // Hermetic BY CONSTRUCTION: the state db path is derived directly
        // from the test temp dir, NOT from `config_dir()`/`TOLLGATE_TEST_CONFIG_DIR`.
        // The env var is process-global and OTHER modules' tests legitimately
        // remove it mid-suite (config tests set/remove per test); a CLI test
        // whose `config_dir()` read lands in such a window computed
        // `/etc/tollgate` — opening, fail-closing `exit(78)` on, and even
        // SCHEMA-MIGRATING the host's real state (observed twice on the
        // bench host). No env read means no window. `TOLLGATE_STATE_DB` is
        // likewise never consulted: a stray environment variable must not
        // point tests at operator state.
        //
        // The OPEN is serialized: every test in this module opens the same
        // shared store file, and two concurrent `SessionManager::open`s race
        // on the WAL pragma + schema-create — the loser fails closed and
        // `exit(78)`s the whole test binary (observed as a nondeterministic
        // full-suite kill). Connections remain concurrently usable after
        // open; only the open itself needs the lock.
        let state_db = std::env::temp_dir()
            .join("tollgate-cli-tests")
            .join("tollgate.db");
        let sessions = Arc::new(tokio::sync::Mutex::new({
            let _open_guard = STORE_OPEN_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            SessionManager::open(&state_db).unwrap_or_else(|e| {
                eprintln!(
                    "FATAL: cannot open session store at {}: {e}",
                    state_db.display()
                );
                std::process::exit(78);
            })
        }));
        Arc::new(AppState {
            config,
            identity,
            wallet,
            sessions,
            quotes: Arc::new(std::sync::Mutex::new(
                crate::http::routes::quote::MintQuoteRegistry::new(),
            )),
        })
    }

    #[tokio::test]
    async fn version_contains_required_fields() {
        let v = version_string();
        assert!(v.contains("version:"));
        assert!(v.contains("commit:"));
        assert!(v.contains("build_time:"));
        assert!(v.contains("rust_version:"));
        assert!(v.contains("openwrt"));
    }

    #[tokio::test]
    async fn status_returns_running() {
        let state = make_test_state();
        let resp = handle_command("status", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "running");
    }

    #[tokio::test]
    async fn wallet_balance_returns_zero_for_empty_wallet() {
        let state = make_test_state();
        let resp = handle_command("wallet balance", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        // Balance will be "0" since the wallet has no mints registered
        assert_eq!(json["message"], "0");
    }

    #[tokio::test]
    async fn wallet_info_returns_json() {
        let state = make_test_state();
        let resp = handle_command("wallet info", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].is_string());
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let state = make_test_state();
        let resp = handle_command("foobar", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("unknown command"));
    }

    #[tokio::test]
    async fn migrate_nonexistent_file_returns_error() {
        let state = make_test_state();
        let resp = handle_command("migrate /nonexistent/tokens.jsonl", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("failed to read"));
    }

    #[tokio::test]
    async fn migrate_empty_file_returns_zero_totals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tokens_path = tmp.path().join("tokens.jsonl");
        std::fs::write(&tokens_path, "").unwrap();

        let state = make_test_state();
        let path_str = tokens_path.to_str().unwrap();
        let resp = handle_command(&format!("migrate {path_str}"), &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        let report: serde_json::Value =
            serde_json::from_str(json["message"].as_str().unwrap()).unwrap();
        assert_eq!(report["total"], 0);
        assert_eq!(report["imported"], 0);
        assert_eq!(report["failed"], 0);
    }

    #[tokio::test]
    async fn migrate_invalid_tokens_counted_as_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tokens_path = tmp.path().join("tokens.jsonl");
        // Two invalid token strings — wallet has no mints registered so receive will fail
        std::fs::write(&tokens_path, "not-a-token\ndefinitely-not-a-token\n").unwrap();

        let state = make_test_state();
        let path_str = tokens_path.to_str().unwrap();
        let resp = handle_command(&format!("migrate {path_str}"), &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        let report: serde_json::Value =
            serde_json::from_str(json["message"].as_str().unwrap()).unwrap();
        assert_eq!(report["total"], 2);
        assert_eq!(report["failed"], 2);
    }

    #[test]
    fn go_json_envelope_unwraps_to_bare_command() {
        let line = r#"{"command":"status","timestamp":"2026-10-28T15:04:24Z"}"#;
        assert_eq!(unwrap_cli_envelope(line), "status");
    }

    #[test]
    fn go_json_envelope_with_args_reassembles() {
        let line = r#"{"command":"wallet","args":["balance"],"timestamp":"x"}"#;
        assert_eq!(unwrap_cli_envelope(line), "wallet balance");
    }

    #[test]
    fn bare_line_passes_through() {
        assert_eq!(unwrap_cli_envelope("status"), "status");
        assert_eq!(unwrap_cli_envelope("wallet balance"), "wallet balance");
    }

    #[test]
    fn non_command_json_passes_through_unchanged() {
        let line = r#"{"not":"a command"}"#;
        assert_eq!(unwrap_cli_envelope(line), line);
    }
}

//! TollWallet — CDK wallet wrapper for tollgate-module-basic-rust.
//!
//! Replaces gonuts `wallet.Wallet` with CDK `cdk::Wallet`. CDK's saga pattern
//! makes operations atomic, eliminating the swap-counter race.
//!
//! # Mapping (13 gonuts call sites → CDK)
//!
//! | gonuts method            | CDK equivalent                                |
//! |-------------------------|-----------------------------------------------|
//! | `wallet.LoadWallet`     | `Wallet::new(mint_url, unit, localstore, seed)` |
//! | `wallet.AddMint`        | `Wallet::new` for that mint (multi-mint map)   |
//! | `wallet.Shutdown`       | drop `Wallet` (closes DB)                     |
//! | `wallet.Receive`        | `wallet.receive(token_str, ReceiveOptions)`    |
//! | `wallet.Send`           | `wallet.prepare_send(amount, opts).confirm()` |
//! | `wallet.SendWithOptions`| `prepare_send` with `SendKind::OnlineTolerance` |
//! | `wallet.RequestMint`    | `wallet.mint_quote(BOLT11, amount, ...)`      |
//! | `wallet.MintQuoteState` | `wallet.check_mint_quote_status(&id)`          |
//! | `wallet.MintTokens`     | `wallet.mint(&id, SplitTarget, None)`          |
//! | `wallet.GetBalance`     | `wallet.total_balance()`                       |
//! | `wallet.GetBalanceByMints` | per-wallet `total_balance()`                |
//! | `wallet.RequestMeltQuote`| `wallet.melt_quote(BOLT11, invoice, ...)`     |
//! | `wallet.Melt`           | `wallet.prepare_melt(quote_id, meta).confirm()`|

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cdk::amount::SplitTarget;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;
use rand::Rng;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

/// Default receive/send/melt timeout (matches Go's 30s).
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors returned by TollWallet operations.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("CDK error: {0}")]
    Cdk(#[from] cdk::Error),
    #[error("database error: {0}")]
    Database(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("mint {0} not in accepted mints list")]
    MintNotAccepted(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet not initialized for mint {0}")]
    WalletNotFound(String),
    #[error("token parse error: {0}")]
    TokenParse(String),
}

/// TollWallet wraps multiple CDK Wallet instances (one per mint URL) behind
/// a tokio Mutex for thread-safe serialized access. CDK's saga pattern
/// ensures operations are atomic — no swap-counter race.
/// Read-only keyset hygiene report for one mint: unspent-proof census,
/// how many proofs sit on keysets the mint has retired, and which HELD
/// keysets expire inside the warn window (a keyset we hold nothing on is
/// mint history, not a fund risk).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeysetHygiene {
    pub unspent_proofs: usize,
    pub proofs_on_inactive_keysets: usize,
    pub soonest_expiry_keysets: Vec<String>,
}

pub struct TollWallet {
    wallets: HashMap<String, Arc<Mutex<Wallet>>>,
    seed: [u8; 64],
    accepted_mints: Vec<String>,
    db_dir: PathBuf,
}

/// A `tokio::time::timeout` CANCELS the future it wraps — and cancelling a
/// CDK saga mid-flight does not roll it back: the saga record persists in
/// SQLite, half-executed. CDK itself only resumes such sagas when
/// `recover_incomplete_sagas` runs, which this module did solely inside
/// `ensure_mint` — i.e. at boot. A customer whose receive timed out at
/// 14:00 therefore had their saga sit unfinished until the NEXT RESTART,
/// and the HTTP layer answered "payment failed" for a payment whose swap
/// the mint may well have accepted.
///
/// Called from every op's timeout arm: reconcile the cancelled saga
/// in-session, detached, so the caller's error path stays fast. Budgeted at
/// 120s; failures log and defer to the next boot's `ensure_mint` — the same
/// ladder the Go origin runs (receive-intent reconcile on NUT-07 evidence).
fn spawn_post_timeout_recovery(wallet: &Arc<Mutex<cdk::Wallet>>, mint: String) {
    let wallet = wallet.clone();
    tokio::spawn(async move {
        let recovery = timeout(Duration::from_secs(120), async {
            wallet.lock().await.recover_incomplete_sagas().await
        })
        .await;
        match recovery {
            Ok(Ok(report)) => {
                if report.recovered > 0 || report.compensated > 0 {
                    tracing::info!(
                        mint = %mint,
                        recovered = report.recovered,
                        compensated = report.compensated,
                        "post-timeout saga recovery settled a cancelled operation"
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(mint = %mint, error = %e, "post-timeout saga recovery failed; retried at next boot")
            }
            Err(_) => {
                tracing::warn!(mint = %mint, "post-timeout saga recovery timed out; retried at next boot")
            }
        }
    });
}

impl TollWallet {
    /// Create a new TollWallet. Does NOT open any wallets — call `ensure_mint`.
    pub fn new(seed: [u8; 64], accepted_mints: Vec<String>, db_dir: PathBuf) -> Self {
        Self {
            wallets: HashMap::new(),
            seed,
            accepted_mints,
            db_dir,
        }
    }

    /// Whether a wallet is registered for this spelling (canonicalized) —
    /// the probe the case-divergence regression pins.
    #[cfg(test)]
    fn mint_registered(&self, mint_url: &str) -> Result<bool, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        Ok(self.wallets.contains_key(normalized.as_str()))
    }

    fn is_mint_accepted(&self, mint_url: &str) -> bool {
        self.accepted_mints.is_empty()
            || self
                .accepted_mints
                .iter()
                .any(|m| crate::mint_url::mint_urls_equal(m, mint_url))
    }

    /// Register a mint and open a CDK wallet for it.
    /// Maps gonuts `AddMint(mintURL)` + `LoadWallet`.
    pub async fn ensure_mint(&mut self, mint_url: &str) -> Result<(), WalletError> {
        if !self.is_mint_accepted(mint_url) {
            return Err(WalletError::MintNotAccepted(mint_url.to_string()));
        }

        let normalized = crate::mint_url::canonicalize(mint_url);
        if self.wallets.contains_key(normalized.as_str()) {
            return Ok(());
        }

        let db_path = self.db_path_for_mint(normalized.as_str());
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let localstore = WalletSqliteDatabase::new(db_path.to_str().unwrap_or(":memory:"))
            .await
            .map_err(|e| WalletError::Database(e.to_string()))?;

        let wallet = Wallet::new(
            mint_url,
            CurrencyUnit::Sat,
            Arc::new(localstore),
            self.seed,
            None,
        )?;

        let recovery = wallet.recover_incomplete_sagas().await?;
        if !recovery.is_empty() {
            tracing::info!(
                recovered = recovery.recovered,
                compensated = recovery.compensated,
                skipped = recovery.skipped,
                failed = recovery.failed,
                "recovered incomplete sagas for {}",
                normalized
            );
        }

        self.wallets
            .insert(normalized.to_string(), Arc::new(Mutex::new(wallet)));
        Ok(())
    }

    fn db_path_for_mint(&self, mint_url: &str) -> PathBuf {
        let sanitized: String = mint_url
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.db_dir.join(format!("{sanitized}.sqlite"))
    }

    /// Receive a Cashu token (maps gonuts `Receive`).
    ///
    /// CDK's receive is atomic — no counter race. Wrapped in 30s timeout.
    pub async fn receive(&self, token_str: &str) -> Result<u64, WalletError> {
        let token: cashu::nuts::Token = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = crate::mint_url::canonicalize(&mint_url);

        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.receive(token_str, ReceiveOptions::default()).await
        })
        .await;

        match result {
            Ok(Ok(amount)) => {
                let sat: u64 = amount.into();
                Ok(sat)
            }
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Send tokens (maps gonuts `Send`).
    /// Returns the serialized Cashu V4 token string.
    pub async fn send(
        &self,
        mint_url: &str,
        amount_sat: u64,
        include_fee: bool,
    ) -> Result<String, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let opts = SendOptions {
            include_fee,
            ..Default::default()
        };

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let prepared = w.prepare_send(Amount::from(amount_sat), opts).await?;
            let token = prepared.confirm(None).await?;
            Ok::<_, cdk::Error>(token.to_string())
        })
        .await;

        match result {
            Ok(Ok(token_str)) => Ok(token_str),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// True when this mint's wallet has an incomplete SEND saga — a drain or
    /// payout whose swap the mint may have accepted without our receiving
    /// the output token. Re-issuing such a send could print the value twice
    /// (the output token may exist undelivered), so the drain command defers
    /// these mints until `recover_incomplete_sagas` settles them.
    pub async fn mint_has_unresolved_send(&self, mint_url: &str) -> Result<bool, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(false),
        };
        let w = wallet.lock().await;
        let sagas = w
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(|e| WalletError::Database(e.to_string()))?;
        Ok(sagas
            .into_iter()
            .any(|s| matches!(s.state, cdk::wallet::types::WalletSagaState::Send(_))))
    }

    /// Read-only keyset hygiene (per mint): refreshes the keyset list from
    /// the mint, counts unspent proofs parked on keysets the mint reports
    /// INACTIVE — value that can still be spent but will never be issued
    /// to again and should be rotated — and lists HELD keysets whose
    /// final_expiry falls inside the warn window. Never moves value.
    pub async fn keyset_hygiene(
        &self,
        mint_url: &str,
        expiry_warn_days: u64,
    ) -> Result<KeysetHygiene, WalletError> {
        use cdk::wallet::types::KeysetLoadPolicy;

        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(KeysetHygiene::default()),
        };

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let keysets = w.keysets(KeysetLoadPolicy::CacheThenNetwork).await?;
            let unspent = w.get_unspent_proofs().await?;

            let inactive: std::collections::HashSet<cdk::nuts::nut02::Id> = keysets
                .iter()
                .filter(|ks| ks.active == Some(false))
                .map(|ks| ks.id)
                .collect();
            let on_inactive = unspent
                .iter()
                .filter(|p| inactive.contains(&p.keyset_id))
                .count();

            // Only keysets we actually HOLD unspent proofs on belong in the
            // expiry warning: mints advertise historical keysets we never
            // used, and warning about those every sweep is permanent noise
            // that obscures the fund-risk signal.
            let held: std::collections::HashSet<cdk::nuts::nut02::Id> =
                unspent.iter().map(|p| p.keyset_id).collect();
            let warn_cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + expiry_warn_days * 24 * 60 * 60;
            let soonest: Vec<String> = keysets
                .iter()
                .filter_map(|ks| {
                    ks.final_expiry
                        .filter(|_| held.contains(&ks.id))
                        .filter(|e| *e < warn_cutoff)
                        .map(|_| ks.id.to_string())
                })
                .collect();

            Ok::<_, cdk::Error>(KeysetHygiene {
                unspent_proofs: unspent.len(),
                proofs_on_inactive_keysets: on_inactive,
                soonest_expiry_keysets: soonest,
            })
        })
        .await;

        match result {
            Ok(Ok(h)) => Ok(h),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Proactively rotate proofs off retired keysets: swaps every unspent
    /// proof on an INACTIVE keyset to the mint's active keyset via CDK's
    /// public self-swap — same-mint, value never leaves. Returns the
    /// number of proofs rotated. Errors and timeouts trigger the same
    /// detached saga recovery as every money-moving wrapper: a cancelled
    /// swap leaves a persisted half-executed saga that must not wait for
    /// the next boot.
    pub async fn rotate_inactive_keyset_proofs(&self, mint_url: &str) -> Result<u32, WalletError> {
        use cdk::amount::SplitTarget;
        use cdk::wallet::types::KeysetLoadPolicy;

        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(0),
        };
        let normalized_owned = normalized.to_string();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let keysets = w.keysets(KeysetLoadPolicy::CacheThenNetwork).await?;
            let inactive: std::collections::HashSet<cdk::nuts::nut02::Id> = keysets
                .iter()
                .filter(|ks| ks.active == Some(false))
                .map(|ks| ks.id)
                .collect();
            if inactive.is_empty() {
                return Ok::<_, cdk::Error>(0u64);
            }
            let unspent = w.get_unspent_proofs().await?;
            let stale: Vec<cdk::nuts::nut00::Proof> = unspent
                .into_iter()
                .filter(|p| inactive.contains(&p.keyset_id))
                .collect();
            if stale.is_empty() {
                return Ok(0u64);
            }
            let count = stale.len() as u64;
            w.swap(None, SplitTarget::default(), stale, None, true, false)
                .await?;
            Ok(count)
        })
        .await;

        match result {
            Ok(Ok(n)) => Ok(n as u32),
            Ok(Err(e)) => {
                let e = WalletError::Cdk(e);
                spawn_post_timeout_recovery(&wallet, normalized_owned);
                Err(e)
            }
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized_owned);
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Get total balance across all mints (maps gonuts `GetBalance`).
    pub async fn get_balance(&self) -> Result<u64, WalletError> {
        let mut total: u64 = 0;
        for wallet in self.wallets.values() {
            let w = wallet.lock().await;
            let bal: u64 = w.total_balance().await?.into();
            total += bal;
        }
        Ok(total)
    }

    /// Get per-mint balances (maps gonuts `GetBalanceByMints`).
    pub async fn get_balance_by_mint(&self) -> Result<Vec<(String, u64)>, WalletError> {
        let mut result = Vec::new();
        for (mint_url, wallet) in &self.wallets {
            let w = wallet.lock().await;
            let bal: u64 = w.total_balance().await?.into();
            result.push((mint_url.clone(), bal));
        }
        Ok(result)
    }

    /// Request a mint quote (NUT-04, maps gonuts `RequestMintQuote`).
    pub async fn request_mint_quote(
        &self,
        mint_url: &str,
        amount_sat: u64,
    ) -> Result<MintQuoteInfo, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.mint_quote(
                PaymentMethod::BOLT11,
                Some(Amount::from(amount_sat)),
                None,
                None,
            )
            .await
        })
        .await;

        match result {
            Ok(Ok(quote)) => Ok(MintQuoteInfo {
                id: quote.id,
                request: quote.request,
                amount: quote.amount.map(|a| -> u64 { a.into() }).unwrap_or(0),
                expiry: quote.expiry,
            }),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// The mint a request should be issued against when the caller does not name one.
    ///
    /// Single-mint deployments are the normal case, so this returns the sole configured
    /// mint. Rather than pick arbitrarily when several are configured (which would make
    /// a customer's invoice come from a mint they did not expect), it returns `None` and
    /// the caller must name a mint explicitly.
    pub fn default_mint(&self) -> Option<&str> {
        if self.wallets.len() == 1 {
            self.wallets.keys().next().map(|s| s.as_str())
        } else {
            None
        }
    }

    /// Every configured mint URL, sorted for deterministic output.
    pub fn mint_urls(&self) -> Vec<String> {
        let mut v: Vec<String> = self.wallets.keys().cloned().collect();
        v.sort();
        v
    }

    /// Check mint quote status (maps gonuts `MintQuoteState`).
    pub async fn check_mint_quote(
        &self,
        mint_url: &str,
        quote_id: &str,
    ) -> Result<String, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.check_mint_quote_status(quote_id).await
        })
        .await;

        match result {
            Ok(Ok(quote)) => Ok(format!("{:?}", quote.state)),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Mint tokens from a paid quote (NUT-04, maps gonuts `MintTokens`).
    /// CDK API: `wallet.mint(quote_id, SplitTarget, Option<SpendingConditions>)`.
    pub async fn mint_tokens(&self, mint_url: &str, quote_id: &str) -> Result<u64, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let proofs = w.mint(quote_id, SplitTarget::default(), None).await?;
            let total: u64 = proofs.iter().map(|p| -> u64 { p.amount.into() }).sum();
            Ok::<_, cdk::Error>(total)
        })
        .await;

        match result {
            Ok(Ok(total)) => Ok(total),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Request a melt quote + prepare melt (NUT-05, maps gonuts `RequestMeltQuote` + `Melt`).
    /// CDK flow: `melt_quote(BOLT11, invoice)` → `prepare_melt(quote_id, meta)` → `confirm()`.
    pub async fn melt(&self, mint_url: &str, invoice: &str) -> Result<MeltQuoteInfo, WalletError> {
        let normalized = crate::mint_url::canonicalize(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let invoice_owned = invoice.to_string();
        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            // Step 1: create melt quote
            let quote = w
                .melt_quote(PaymentMethod::BOLT11, invoice_owned, None, None)
                .await?;
            // Step 2: prepare melt with the quote ID
            let prepared = w.prepare_melt(&quote.id, HashMap::new()).await?;
            // Step 3: confirm
            let finalized = prepared.confirm().await?;
            Ok::<_, cdk::Error>((quote, finalized))
        })
        .await;

        match result {
            Ok(Ok((quote, _finalized))) => Ok(MeltQuoteInfo {
                quote_id: quote.id,
                amount: quote.amount.into(),
                fee: quote.fee_reserve.into(),
            }),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => {
                spawn_post_timeout_recovery(&wallet, normalized.to_string());
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Shutdown — drop all wallets (closes sqlite DB handles).
    /// Maps gonuts `Shutdown`.
    pub async fn shutdown(self) {
        // Dropping self drops wallets, which closes sqlite connections.
        drop(self);
    }

    /// Load or generate a wallet seed (64 bytes) from the given path.
    /// Load or create the 64-byte wallet seed, with a BIP39 mnemonic
    /// backstop written alongside (`wallet_mnemonic.txt`, same directory,
    /// 0600). The ecosystem durability ladder: local durable files, then
    /// NUT-19 replay, then NUT-09 restore — and when the seed FILE itself
    /// is lost (disk failure, botched migration), a mnemonic is the only
    /// thing that re-derives it (NUT-13).
    ///
    /// Write ordering makes every crash point self-heal:
    ///   neither file  → generate mnemonic FIRST, fsync, then derive+write
    ///                    the seed (a crash between leaves mnemonic-only,
    ///                    which the next boot re-derives from);
    ///   mnemonic only → re-derive the seed (the crash case above);
    ///   seed only     → pre-backstop wallet: loads exactly as before (a
    ///                    mnemonic cannot be derived from a seed — the
    ///                    operator can backfill by writing the file);
    ///   both          → load, but a MISMATCH is a hard error: one of the
    ///                    two files was replaced independently, and
    ///                    silently preferring either would fork the wallet
    ///                    identity, orphaning every deterministic
    ///                    derivation;
    ///   wrong size    → hard error (was: silent regeneration — a fresh
    ///                    seed would orphan every derivation too).
    pub async fn load_or_create_seed(path: &Path) -> Result<[u8; 64], WalletError> {
        let mnemonic_path = path.with_file_name("wallet_mnemonic.txt");

        let seed_on_disk: Option<Vec<u8>> = if path.exists() {
            let data = tokio::fs::read(path).await?;
            if data.len() != 64 {
                tracing::error!(
                    len = data.len(),
                    "seed file has the wrong size — refusing to regenerate: a fresh seed would orphan every deterministic derivation. Restore the original 64-byte file from backup."
                );
                return Err(WalletError::Io(std::io::Error::other(format!(
                    "seed file {} is {} bytes, expected 64 — refusing to regenerate (restore from backup)",
                    path.display(),
                    data.len()
                ))));
            }
            Some(data)
        } else {
            None
        };

        let mnemonic_on_disk: Option<String> = if mnemonic_path.exists() {
            let m = tokio::fs::read_to_string(&mnemonic_path).await?;
            let m = m.trim().to_string();
            if m.is_empty() {
                tracing::error!(
                    "mnemonic file exists but is empty — refusing to proceed: an empty backstop cannot be distinguished from a torn write. Restore or delete the file deliberately."
                );
                return Err(WalletError::Io(std::io::Error::other(format!(
                    "mnemonic file {} is empty — restore it or remove it deliberately",
                    mnemonic_path.display()
                ))));
            }
            Some(m)
        } else {
            None
        };

        match (mnemonic_on_disk, seed_on_disk) {
            (Some(m), Some(seed_bytes)) => {
                // Consistency check: the mnemonic must re-derive the seed.
                let derived = crate::identity_derive::mnemonic_to_bip39_seed(&m)
                    .map_err(|e| WalletError::Io(std::io::Error::other(e)))?;
                let mut on_disk = [0u8; 64];
                on_disk.copy_from_slice(&seed_bytes);
                if derived != on_disk {
                    tracing::error!(
                        "wallet seed and mnemonic disagree — one file was replaced independently. Refusing to fork the wallet identity: restore the correct pair from backup."
                    );
                    return Err(WalletError::Io(std::io::Error::other(
                        "seed/mnemonic mismatch: refusing to load — one of the two files was replaced; restore the matching pair from backup",
                    )));
                }
                Ok(on_disk)
            }
            (Some(m), None) => {
                // Crash between the mnemonic write and the seed write:
                // re-derive the seed from the mnemonic and finish the job.
                let seed = crate::identity_derive::mnemonic_to_bip39_seed(&m)
                    .map_err(|e| WalletError::Io(std::io::Error::other(e)))?;
                write_secret(path, &seed).await?;
                tracing::info!("re-derived the wallet seed from the mnemonic backstop");
                Ok(seed)
            }
            (None, Some(seed_bytes)) => {
                // Pre-backstop wallet: no mnemonic exists to check against.
                let mut seed = [0u8; 64];
                seed.copy_from_slice(&seed_bytes);
                Ok(seed)
            }
            (None, None) => {
                // First boot: mnemonic first (crash-safe), then the seed.
                let mnemonic = generate_mnemonic_24()?;
                write_secret(&mnemonic_path, mnemonic.as_bytes()).await?;
                let seed = crate::identity_derive::mnemonic_to_bip39_seed(&mnemonic)
                    .map_err(|e| WalletError::Io(std::io::Error::other(e)))?;
                write_secret(path, &seed).await?;
                tracing::info!(
                    "generated a new wallet with a 24-word mnemonic backstop at {}",
                    mnemonic_path.display()
                );
                Ok(seed)
            }
        }
    }
}

/// Mint quote info.
#[derive(Debug, Clone)]
pub struct MintQuoteInfo {
    pub id: String,
    pub request: String,
    pub amount: u64,
    pub expiry: u64,
}

/// Melt quote info.
#[derive(Debug, Clone)]
pub struct MeltQuoteInfo {
    pub quote_id: String,
    pub amount: u64,
    pub fee: u64,
}

/// Write a secret file 0600 and fsync it, plus its directory entry — the
/// first-creation durability rule the payment journal follows too.
async fn write_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    tokio::fs::write(path, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    if let Some(dir) = path.parent() {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
    Ok(())
}

/// Generate a fresh 24-word BIP39 mnemonic: 256 random bits of entropy,
/// an 8-bit (first checksum byte) SHA-256 checksum, 24 11-bit indices over
/// the embedded English wordlist (the same list identity_derive pins).
fn generate_mnemonic_24() -> Result<String, WalletError> {
    use rand::RngCore;
    use sha2::{Digest, Sha256};

    let mut entropy = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut entropy);
    let checksum = Sha256::digest(&entropy);
    let mut bits: Vec<bool> = Vec::with_capacity(264);
    for b in entropy.iter() {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1 == 1);
        }
    }
    for i in (0..8).rev() {
        bits.push((checksum[0] >> i) & 1 == 1);
    }
    let wl = crate::identity_derive::words();
    let words: Vec<&str> = bits
        .chunks(11)
        .map(|c| {
            let idx = c.iter().fold(0usize, |acc, b| (acc << 1) | (*b as usize));
            wl[idx]
        })
        .collect();
    Ok(words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_wallet(dir: &Path, accepted_mints: Vec<String>) -> TollWallet {
        let mut seed = [0u8; 64];
        rand::thread_rng().fill(&mut seed);
        TollWallet::new(seed, accepted_mints, dir.to_path_buf())
    }

    #[tokio::test]
    async fn open_close_cycle_releases_file_lock() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut wallet = make_test_wallet(dir, vec![]);
        wallet
            .ensure_mint("https://test-mint.example")
            .await
            .unwrap();
        assert!(wallet.wallets.contains_key("https://test-mint.example"));

        wallet.shutdown().await;

        let mut wallet2 = make_test_wallet(dir, vec![]);
        wallet2
            .ensure_mint("https://test-mint.example")
            .await
            .expect("should reopen DB after shutdown");
    }

    #[tokio::test]
    async fn rejects_unlisted_mint() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec!["https://allowed.example".into()]);
        let result = wallet.ensure_mint("https://evil.example").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            WalletError::MintNotAccepted(url) => assert_eq!(url, "https://evil.example"),
            other => panic!("expected MintNotAccepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_accepted_mints_accepts_all() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://any-mint.example")
            .await
            .expect("empty accepted_mints should accept all");
    }

    #[tokio::test]
    async fn seed_load_or_create_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("seed.bin");

        let seed1 = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed1.len(), 64);

        let seed2 = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed1, seed2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&seed_path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn get_balance_returns_zero_for_new_wallet() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet.ensure_mint("https://mint.example").await.unwrap();
        let bal = wallet.get_balance().await.unwrap();
        assert_eq!(bal, 0);
    }

    #[tokio::test]
    async fn get_balance_by_mint_returns_per_mint() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet.ensure_mint("https://mint1.example").await.unwrap();
        wallet.ensure_mint("https://mint2.example").await.unwrap();
        let balances = wallet.get_balance_by_mint().await.unwrap();
        assert_eq!(balances.len(), 2);
        for (_, bal) in &balances {
            assert_eq!(*bal, 0);
        }
    }

    #[tokio::test]
    async fn db_path_sanitizes_url() {
        let tmp = TempDir::new().unwrap();
        let wallet = make_test_wallet(tmp.path(), vec![]);
        let path = wallet.db_path_for_mint("https://mint.coinos.io");
        assert!(path.starts_with(tmp.path()));
        assert!(path.extension().is_some_and(|e| e == "sqlite"));
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(!fname.contains('/'));
        assert!(!fname.contains(':'));
    }

    #[tokio::test]
    async fn receive_with_nonexistent_mint_errors() {
        let tmp = TempDir::new().unwrap();
        let wallet = make_test_wallet(tmp.path(), vec![]);
        let token = "cashuBo2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEalpQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";
        let result = wallet.receive(token).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn concurrent_operations_dont_panic() {
        // TDD Task 3.3: concurrent operations should be serialized by the mutex
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://testnut.cashu.space")
            .await
            .unwrap();

        let wallet_arc = Arc::new(wallet);
        let mut handles = Vec::new();

        for _ in 0..3 {
            let w = wallet_arc.clone();
            handles.push(tokio::spawn(async move {
                let _ = w.get_balance().await;
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn timeout_protection_on_receive() {
        // Receive should not hang forever — timeout wrapper exists
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://nonexistent.localhost.invalid")
            .await
            .unwrap();

        let token = "cashuBo2FteBtodHRwczovL25vbmV4aXN0ZW50LmxvY2FsaG9zdC5pbnZhbGlkYXVjc2F0YQ==";
        let result = tokio::time::timeout(Duration::from_secs(35), wallet.receive(token)).await;

        assert!(result.is_ok(), "receive should not hang forever");
    }

    #[test]
    fn keyset_hygiene_on_unregistered_mint_is_default() {
        // Read-only check: an unknown mint reports the empty census rather
        // than an error — the boot sweep treats it as "nothing to do".
        let tmp = TempDir::new().unwrap();
        let wallet = make_test_wallet(tmp.path(), vec![]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let h = rt
            .block_on(wallet.keyset_hygiene("https://never-registered.example", 30))
            .unwrap();
        assert_eq!(h, KeysetHygiene::default());
        let n = rt
            .block_on(wallet.rotate_inactive_keyset_proofs("https://never-registered.example"))
            .unwrap();
        assert_eq!(n, 0, "nothing to rotate on an unknown mint");
    }

    #[test]
    fn first_boot_writes_mnemonic_that_rederives_the_seed() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let seed = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap();
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");
        let m = std::fs::read_to_string(&mnemonic_path).unwrap();
        assert_eq!(m.split_whitespace().count(), 24, "24-word backstop");
        let rederived = crate::identity_derive::mnemonic_to_bip39_seed(m.trim()).unwrap();
        assert_eq!(
            rederived.as_slice(),
            seed.as_slice(),
            "the mnemonic must re-derive exactly the on-disk seed"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&mnemonic_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mnemonic is 0600");
    }

    #[test]
    fn mnemonic_only_boot_rederives_the_seed() {
        // The crash window between the two writes self-heals.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let seed1 = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap();
        std::fs::remove_file(&seed_path).unwrap();
        let seed2 = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap();
        assert_eq!(seed1, seed2, "re-derived from the mnemonic backstop");
    }

    #[test]
    fn seed_mnemonic_mismatch_fails_loudly() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap();
        // Independently replace the seed: the next boot must refuse.
        std::fs::write(&seed_path, [7u8; 64]).unwrap();
        let err = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap_err();
        assert!(err.to_string().contains("mismatch"), "got: {err}");
    }

    #[test]
    fn corrupt_seed_file_fails_instead_of_regenerating() {
        // The old behavior silently regenerated — orphaning every
        // deterministic derivation. Fund-safety: refuse and tell the
        // operator to restore from backup.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        std::fs::write(&seed_path, [0u8; 32]).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap_err();
        assert!(err.to_string().contains("refusing"), "got: {err}");
    }

    #[test]
    fn seed_only_wallet_loads_unchanged() {
        // Pre-backstop wallets keep working; no mnemonic can be derived
        // from a seed, so none is written behind the operator's back.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");
        let mut seed = [9u8; 64];
        std::fs::write(&seed_path, seed).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let loaded = rt
            .block_on(TollWallet::load_or_create_seed(&seed_path))
            .unwrap();
        seed.copy_from_slice(&loaded);
        assert!(!mnemonic_path.exists(), "no backfill of a mnemonic");
    }

    /// The §3 divergence: comparison lowercased while map keys did not — a
    /// config spelling `https://Mint.Example` selected the mint but the
    /// receive lookup missed the map (WalletNotFound). One canonical form
    /// (CDK's MintUrl) at both sites closes it.
    #[tokio::test]
    async fn case_variant_config_and_token_mint_are_one_wallet() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec!["https://Mint.Example".to_string()]);
        wallet
            .ensure_mint("https://Mint.Example")
            .await
            .expect("config spelling registers");
        assert!(
            wallet
                .mint_registered("https://mint.example")
                .expect("registered query"),
            "the token's lowercase spelling must find the wallet the config spelling registered"
        );
    }
}

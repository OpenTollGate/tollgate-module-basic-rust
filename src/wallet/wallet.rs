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
pub struct TollWallet {
    wallets: HashMap<String, Arc<Mutex<Wallet>>>,
    seed: [u8; 64],
    accepted_mints: Vec<String>,
    db_dir: PathBuf,
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

    fn is_mint_accepted(&self, mint_url: &str) -> bool {
        self.accepted_mints.is_empty()
            || self
                .accepted_mints
                .iter()
                .any(|m| m.trim_end_matches('/') == mint_url.trim_end_matches('/'))
    }

    /// Register a mint and open a CDK wallet for it.
    /// Maps gonuts `AddMint(mintURL)` + `LoadWallet`.
    pub async fn ensure_mint(&mut self, mint_url: &str) -> Result<(), WalletError> {
        if !self.is_mint_accepted(mint_url) {
            return Err(WalletError::MintNotAccepted(mint_url.to_string()));
        }

        let normalized = mint_url.trim_end_matches('/');
        if self.wallets.contains_key(normalized) {
            return Ok(());
        }

        let db_path = self.db_path_for_mint(normalized);
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
        let normalized = mint_url.trim_end_matches('/');

        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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
        let normalized = mint_url.trim_end_matches('/');
        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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
        let normalized = mint_url.trim_end_matches('/');
        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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
        let normalized = mint_url.trim_end_matches('/');
        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Mint tokens from a paid quote (NUT-04, maps gonuts `MintTokens`).
    /// CDK API: `wallet.mint(quote_id, SplitTarget, Option<SpendingConditions>)`.
    pub async fn mint_tokens(&self, mint_url: &str, quote_id: &str) -> Result<u64, WalletError> {
        let normalized = mint_url.trim_end_matches('/');
        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Request a melt quote + prepare melt (NUT-05, maps gonuts `RequestMeltQuote` + `Melt`).
    /// CDK flow: `melt_quote(BOLT11, invoice)` → `prepare_melt(quote_id, meta)` → `confirm()`.
    pub async fn melt(&self, mint_url: &str, invoice: &str) -> Result<MeltQuoteInfo, WalletError> {
        let normalized = mint_url.trim_end_matches('/');
        let wallet = self
            .wallets
            .get(normalized)
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
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Shutdown — drop all wallets (closes sqlite DB handles).
    /// Maps gonuts `Shutdown`.
    pub async fn shutdown(self) {
        // Dropping self drops wallets, which closes sqlite connections.
        drop(self);
    }

    /// Load or generate a wallet seed (64 bytes) from the given path.
    pub async fn load_or_create_seed(path: &Path) -> Result<[u8; 64], WalletError> {
        if path.exists() {
            let data = tokio::fs::read(path).await?;
            if data.len() == 64 {
                let mut seed = [0u8; 64];
                seed.copy_from_slice(&data);
                return Ok(seed);
            }
            tracing::warn!("seed file wrong size, regenerating");
        }

        let mut seed = [0u8; 64];
        rand::thread_rng().fill(&mut seed);
        tokio::fs::write(path, seed).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }

        Ok(seed)
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
}

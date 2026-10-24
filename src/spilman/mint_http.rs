//! Fund a channel at a **remote** mint, over HTTP.
//!
//! # Why this is not in-process, and why it is here rather than in the driver
//!
//! The driver used to spawn `cdk-spilman-test-mint` in its own process and hand
//! the resulting proofs straight to the payer bridge. That is invisible to
//! anything standing between the payer and the mint — and the bench lane's
//! assertion F ("ZERO mint calls between open and settle") works by putting a
//! **counting proxy** in front of the mint and advertising *the proxy's* URL
//! through the payee's `/spilman/params`. A driver that mints at an ephemeral
//! port skips the proxy, so assertion F could never fail: it was measuring
//! nothing.
//!
//! So the funding leg is real HTTP against the URL the caller passes in. Two
//! consequences worth stating:
//!
//! - **It must not exist in a normal build.** The path asks a mint for free
//!   money (the test mint's `FakeWallet` auto-pays every quote). A router binary
//!   carrying it is a router binary with a "print sats" routine. Hence
//!   `#[cfg(feature = "payer-driver")]` — the same non-default feature that opts
//!   the example into the build (`Cargo.toml`), so neither the module nor the
//!   driver is compiled by `cargo build`, `cargo test`, or a release image.
//! - **It belongs in the library, not the example.** The example should read as
//!   protocol orchestration (open → register → pay → read back). Keeping the NUT
//!   plumbing here means a test can call the mint leg directly instead of
//!   re-implementing it, and a re-implementation is how a green test stops
//!   proving anything about the binary that ships.
//!
//! # What it does, in NUT order
//!
//! ```text
//! GET  /v1/keysets          pick the ACTIVE sat keyset
//! GET  /v1/keys/{id}        its public keys, input fee and expiry
//! POST /v1/mint/quote/bolt11   ask for `amount` sats
//! GET  /v1/mint/quote/bolt11/{quote}   poll until the invoice is paid
//! POST /v1/mint/bolt11      redeem the quote for blinded signatures
//! (local)                   unblind into proofs
//! ```
//!
//! Every step is a `Result`. There is no retry loop around the minting itself
//! (only around the quote *status* poll, which is a read): a driver that retried
//! a `POST /v1/mint/bolt11` after an ambiguous failure could double-spend a
//! quote or burn it, and the honest behaviour for a test driver is to report.
//!
//! # The keyset blob is derived, not copied
//!
//! `ChannelOpening::prepare` needs `keyset_info_json` — the shape
//! `cdk_spilman::parse_keyset_info_from_json` accepts. It is built **from the
//! mint's own `/v1/keys` answer**, so the keys the channel is opened against are
//! the keys that answered, not a string the driver was handed.

use cdk::amount::SplitTarget;
use cdk::dhke::construct_proofs;
use cdk::mint_url::MintUrl;
use cdk::nuts::{
    CurrencyUnit, Id, KeysetResponse, MintQuoteBolt11Request, MintQuoteState, MintRequest,
    PaymentMethod, PreMintSecrets, Proof,
};
use cdk::wallet::{HttpClient, MintConnector};
use cdk::{Amount, MintQuoteRequest, MintQuoteResponse};

use crate::spilman::SpilmanError;

/// How long to wait for the mint to report a quote as paid.
///
/// The bench lane's mint is a `FakeWallet` mint that pays after ~1 s, but a real
/// mint's invoice never pays itself — so this is a *bound on waiting for a
/// report*, not a promise that funding completes. A driver that hung forever
/// could not emit `PAYER_EXIT`, which the lane treats as a truncated run.
pub const DEFAULT_QUOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the quote status is re-read while waiting.
pub const QUOTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The mint's active keyset, plus everything the channel open needs from it.
#[derive(Debug, Clone)]
pub struct ActiveKeyset {
    /// The keyset id the funding outputs must be blinded to.
    pub id: Id,
    /// The keyset's public keys, keyed by denomination.
    pub keys: cdk::nuts::Keys,
    /// Input fee in parts per thousand, straight from `/v1/keys/{id}`.
    pub input_fee_ppk: u64,
    /// Final expiry, when the mint declares one.
    pub final_expiry: Option<u64>,
}

impl ActiveKeyset {
    /// The keyset blob `cdk-spilman` parses: `keysetId` / `unit` / `keys` /
    /// `inputFeePpk` / `finalExpiry`.
    ///
    /// Serialised through `serde_json` rather than `format!` so a denomination
    /// map with an unusual shape is a typed build, not a string that silently
    /// fails to parse three calls later.
    pub fn to_keyset_info_json(&self) -> Result<String, SpilmanError> {
        let keys: std::collections::BTreeMap<String, String> = self
            .keys
            .iter()
            .map(|(amount, pubkey)| (amount.to_string(), pubkey.to_hex()))
            .collect();
        let mut info = serde_json::json!({
            "keysetId": self.id.to_string(),
            "unit": "sat",
            "keys": keys,
            "inputFeePpk": self.input_fee_ppk,
        });
        if let Some(expiry) = self.final_expiry {
            info["finalExpiry"] = serde_json::Value::from(expiry);
        }
        serde_json::to_string(&info)
            .map_err(|e| SpilmanError::Mint(format!("serialize keyset info: {e}")))
    }

    /// Denominations as `Amount`-free `u64`s, which is what `PreMintSecrets`
    /// wants for its split target.
    pub fn denominations(&self) -> Vec<u64> {
        self.keys
            .iter()
            .map(|(amount, _)| amount.to_u64())
            .collect()
    }
}

/// Proofs minted at the remote mint, in the two shapes the caller needs.
#[derive(Debug, Clone)]
pub struct FundedProofs {
    /// The proofs as JSON — restore material, and what the channel open consumes
    /// through a Cashu-B token.
    pub proofs_json: String,
    /// The `keyset_info_json` blob for the keyset these proofs belong to.
    pub keyset_info_json: String,
    /// The keyset id, for logs.
    pub keyset_id: String,
    /// The amount actually requested from the mint, in sats.
    pub amount: u64,
    /// The proofs themselves, for callers that want to assert on them.
    pub proofs: Vec<Proof>,
}

/// A client for one mint's NUT endpoints, over real HTTP.
///
/// `HttpClient` is `cdk`'s public HTTP wallet connector; the module it lives in
/// (`cdk::wallet::mint_connector`) is private, which is why this type is reached
/// through the `cdk::wallet` re-export. It is unauthenticated (`None`), because a
/// mint that demands blind auth is out of scope for a funding test driver — and
/// saying so here beats a mysterious 401 later.
#[derive(Debug, Clone)]
pub struct RemoteMint {
    mint_url: String,
    client: HttpClient,
}

impl RemoteMint {
    /// Point a client at `mint_url`, rejecting anything that is not a URL.
    pub fn new(mint_url: &str) -> Result<Self, SpilmanError> {
        let trimmed = mint_url.trim();
        if trimmed.is_empty() {
            return Err(SpilmanError::Config("mint URL is empty".to_string()));
        }
        let parsed: MintUrl = trimmed
            .parse()
            .map_err(|e| SpilmanError::Config(format!("mint URL {trimmed:?} is invalid: {e}")))?;
        Ok(Self {
            // Keep the caller's spelling: the token embeds it, and the payee
            // compares it against what it was told. Rewriting it would mean the
            // driver funds at one URL and registers another.
            mint_url: trimmed.trim_end_matches('/').to_string(),
            client: HttpClient::new(parsed, None),
        })
    }

    /// The URL this client was built for, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.mint_url
    }

    /// `GET /v1/info` — liveness, so a driver fails at "the mint is not there"
    /// rather than at "the quote is malformed".
    pub async fn info(&self) -> Result<String, SpilmanError> {
        let url = format!("{}/v1/info", self.mint_url);
        let resp = reqwest::get(&url)
            .await
            .map_err(|e| SpilmanError::Mint(format!("GET {url} failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(SpilmanError::Mint(format!(
                "GET {url} answered {}",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SpilmanError::Mint(format!("GET {url}: read body: {e}")))?;
        Ok(body)
    }

    /// `GET /v1/keysets` + `GET /v1/keys/{id}` for the ACTIVE sat keyset.
    pub async fn active_sat_keyset(&self) -> Result<ActiveKeyset, SpilmanError> {
        let keysets: KeysetResponse = self
            .client
            .get_mint_keysets()
            .await
            .map_err(|e| SpilmanError::Mint(format!("GET /v1/keysets: {e}")))?;

        let active = keysets
            .keysets
            .iter()
            .find(|k| k.active && k.unit == CurrencyUnit::Sat)
            .ok_or_else(|| {
                SpilmanError::Mint(format!(
                    "{} advertises no ACTIVE sat keyset ({} keysets seen)",
                    self.mint_url,
                    keysets.keysets.len()
                ))
            })?;
        let id = active.id;

        let keyset = self
            .client
            .get_mint_keyset(id)
            .await
            .map_err(|e| SpilmanError::Mint(format!("GET /v1/keys/{id}: {e}")))?;

        Ok(ActiveKeyset {
            id: keyset.id,
            keys: keyset.keys.clone(),
            input_fee_ppk: keyset.input_fee_ppk,
            final_expiry: keyset.final_expiry,
        })
    }

    /// Mint `amount` sats: quote → wait for payment → redeem → unblind.
    ///
    /// Returns proofs **and** the keyset blob, because a channel opened against
    /// one keyset and verified against another cannot settle.
    pub async fn mint(
        &self,
        amount: u64,
        quote_timeout: std::time::Duration,
    ) -> Result<FundedProofs, SpilmanError> {
        if amount == 0 {
            return Err(SpilmanError::Config(
                "refusing to mint 0 sats: a channel funded with nothing proves nothing".to_string(),
            ));
        }
        let keyset = self.active_sat_keyset().await?;
        let keyset_info_json = keyset.to_keyset_info_json()?;

        // One blinded output per denomination the split produces; the mint's own
        // denomination list is the split target's alphabet.
        let fees: cdk::amount::FeeAndAmounts =
            (keyset.input_fee_ppk, keyset.denominations()).into();
        let premint =
            PreMintSecrets::random(keyset.id, Amount::from(amount), &SplitTarget::None, &fees)
                .map_err(|e| SpilmanError::Mint(format!("split {amount} sat into outputs: {e}")))?;

        let quote_request = MintQuoteRequest::Bolt11(MintQuoteBolt11Request {
            amount: Amount::from(amount),
            unit: CurrencyUnit::Sat,
            description: Some("spilman-payer channel funding".to_string()),
            pubkey: None,
        });
        let quote: MintQuoteResponse<String> = self
            .client
            .post_mint_quote(quote_request)
            .await
            .map_err(|e| SpilmanError::Mint(format!("POST /v1/mint/quote/bolt11: {e}")))?;
        let quote_id = quote.quote().clone();

        self.await_paid(&quote_id, quote_timeout).await?;

        let mint_request = MintRequest {
            quote: quote_id.clone(),
            outputs: premint.blinded_messages(),
            signature: None,
        };
        let minted = self
            .client
            .post_mint(&PaymentMethod::BOLT11, mint_request)
            .await
            .map_err(|e| SpilmanError::Mint(format!("POST /v1/mint/bolt11: {e}")))?;

        // `construct_proofs` is cdk's own unblinding helper: hand-rolling the
        // unblind loop is how a proof ends up with the wrong key or a bad DLEQ.
        let proofs = construct_proofs(
            minted.signatures,
            premint.rs(),
            premint.secrets(),
            &keyset.keys,
        )
        .map_err(|e| SpilmanError::Mint(format!("unblind the mint's signatures: {e}")))?;

        let proofs_json = serde_json::to_string(&proofs)
            .map_err(|e| SpilmanError::Mint(format!("serialize minted proofs: {e}")))?;

        Ok(FundedProofs {
            proofs_json,
            keyset_info_json,
            keyset_id: keyset.id.to_string(),
            amount,
            proofs,
        })
    }

    /// Poll the quote until the mint reports it paid, or the bound expires.
    ///
    /// `Issued` counts as paid-or-beyond: cdk's quote accounting derives it from
    /// the paid/issued counters, and a re-run against a quote that was already
    /// redeemed must not be reported as unpaid.
    async fn await_paid(
        &self,
        quote_id: &str,
        timeout: std::time::Duration,
    ) -> Result<(), SpilmanError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let status: MintQuoteResponse<String> = self
                .client
                .get_mint_quote_status(PaymentMethod::BOLT11, quote_id)
                .await
                .map_err(|e| {
                    SpilmanError::Mint(format!("GET /v1/mint/quote/bolt11/{quote_id}: {e}"))
                })?;
            let state = status.state();
            if matches!(
                state,
                Some(MintQuoteState::Paid) | Some(MintQuoteState::Issued)
            ) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(SpilmanError::Mint(format!(
                    "quote {quote_id} was still {state:?} after {timeout:?}; the invoice was \
                     never paid (a real mint needs a lightning payment — only the bench/test \
                     mint auto-pays)"
                )));
            }
            tokio::time::sleep(QUOTE_POLL_INTERVAL).await;
        }
    }
}

/// Build the Cashu-B token the channel open consumes, for a mint URL.
///
/// Separate from [`RemoteMint::mint`] so the token's mint field is visibly the
/// caller's URL: a token naming a different mint than the one that signed the
/// proofs is exactly the mismatch the payee then refuses.
pub fn funding_token(mint_url: &str, proofs_json: &str) -> Result<String, SpilmanError> {
    cdk_spilman::build_cashu_b_token(mint_url, "sat", proofs_json)
        .map_err(|e| SpilmanError::Mint(format!("build the funding token: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mint_url_is_rejected_when_it_is_not_a_url() {
        let err = RemoteMint::new("not a url").expect_err("must refuse");
        assert!(matches!(err, SpilmanError::Config(_)), "{err}");
        let err = RemoteMint::new("   ").expect_err("must refuse");
        assert!(matches!(err, SpilmanError::Config(_)), "{err}");
    }

    #[test]
    fn the_caller_url_is_preserved_apart_from_a_trailing_slash() {
        // The token embeds this string and the payee compares it against what it
        // advertises, so normalising beyond the trailing slash would make the
        // driver fund at one URL and register another.
        let mint = RemoteMint::new("http://127.0.0.1:3499/").expect("valid url");
        assert_eq!(mint.url(), "http://127.0.0.1:3499");
    }

    #[test]
    fn keyset_info_json_has_the_shape_cdk_spilman_parses() {
        use std::collections::BTreeMap;

        let mut map = BTreeMap::new();
        map.insert(
            Amount::from(1),
            cdk::nuts::SecretKey::generate().public_key(),
        );
        map.insert(
            Amount::from(2),
            cdk::nuts::SecretKey::generate().public_key(),
        );
        let keyset = ActiveKeyset {
            id: "00ad268c4d1f5826".parse().expect("keyset id"),
            keys: cdk::nuts::Keys::new(map),
            input_fee_ppk: 400,
            final_expiry: Some(1_900_000_000),
        };

        let json = keyset.to_keyset_info_json().expect("serialize");
        let parsed = cdk_spilman::parse_keyset_info_from_json(&json)
            .expect("cdk-spilman must accept the blob this driver builds");
        assert_eq!(parsed.keyset_id, keyset.id);
        assert_eq!(parsed.input_fee_ppk, 400);
        assert_eq!(parsed.unit, CurrencyUnit::Sat);
        assert_eq!(parsed.active_keys.iter().count(), 2);
        assert_eq!(keyset.denominations(), vec![1, 2]);
    }

    #[test]
    fn minting_zero_sats_is_refused_before_any_http() {
        let mint = RemoteMint::new("http://127.0.0.1:3499").expect("valid url");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = rt
            .block_on(mint.mint(0, std::time::Duration::from_millis(1)))
            .expect_err("must refuse");
        assert!(matches!(err, SpilmanError::Config(_)), "{err}");
    }
}

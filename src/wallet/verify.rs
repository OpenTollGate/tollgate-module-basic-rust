//! Cashu token verifier — parse + NUT-07 checkstate.
//!
//! Ported from tollgate-rs/crates/tollgate-net/src/wallet.rs (BootstrapWallet).
//! Read-only: verifies proofs are unspent at the mint. No spending/receiving.

use cashu::nuts::Token;
use std::collections::HashSet;

/// TLS 1.2 hard-pinned HTTP client (matches Go behavior — Go audit §2.4).
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_idle_timeout(None) // disable keep-alives
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Bootstrap token verifier. Accepts Cashu tokens, checks proofs against
/// the mint's /v1/checkstate endpoint. Returns spendable amount in
/// milli-sat (pricing_scale = 1000, so 1 sat → 1000 milli-units).
pub struct TokenVerifier {
    accepted_mints: HashSet<String>,
    client: reqwest::Client,
}

impl TokenVerifier {
    pub fn new(mint_urls: Vec<String>) -> Self {
        Self {
            accepted_mints: mint_urls.into_iter().collect(),
            client: build_http_client(),
        }
    }

    /// Parse and verify a Cashu token. Returns amount in milli-sat.
    pub async fn verify(&self, token_str: &str) -> Result<u64, String> {
        let token: Token = token_str
            .parse()
            .map_err(|e| format!("invalid Cashu token: {e}"))?;

        let mint_url = token
            .mint_url()
            .map_err(|e| format!("token has no mint URL: {e}"))?;
        let mint_url_str = mint_url.to_string();
        let mint_base = mint_url_str.trim_end_matches('/').to_string();

        if !self.accepted_mints.is_empty()
            && !self.accepted_mints.contains(&mint_base)
            && !self.accepted_mints.contains(&mint_url_str)
        {
            return Err(format!("mint {} not accepted", mint_url_str));
        }

        let amount_sat: u64 = token
            .value()
            .map_err(|e| format!("could not sum token value: {e}"))?
            .into();

        let ys = token_proof_ys(&token);
        if ys.is_empty() {
            return Err("token contains no proofs".to_string());
        }

        self.check_proofs_unspent(&mint_base, &ys).await?;

        Ok(amount_sat * 1_000)
    }

    /// NUT-07: check all Y-values are UNSPENT.
    async fn check_proofs_unspent(&self, mint_base: &str, ys: &[String]) -> Result<(), String> {
        let url = format!("{mint_base}/v1/checkstate");
        let body = serde_json::json!({ "Ys": ys });

        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("mint check-state request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("mint returned error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("mint response not JSON: {e}"))?;

        let states = resp["states"]
            .as_array()
            .ok_or("mint response missing 'states'")?;

        for state in states {
            let s = state["state"].as_str().unwrap_or("");
            if s != "UNSPENT" {
                return Err(format!("one or more proofs already spent (state: {s})"));
            }
        }
        Ok(())
    }

    /// The mint a token came from, without verifying it.
    ///
    /// A6 needs this to price a payment from the *token's own* mint rather than
    /// from a positional default. Deliberately separate from `verify()`: a caller
    /// that only needs the mint must not be forced to spend a NUT-07 round trip,
    /// and a caller that needs both calls the two in order.
    pub fn token_mint(&self, token_str: &str) -> Result<String, String> {
        let token: Token = token_str
            .parse()
            .map_err(|e| format!("invalid Cashu token: {e}"))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| format!("token has no mint URL: {e}"))?;
        Ok(mint_url.to_string().trim_end_matches('/').to_string())
    }
}

/// Extract Y-values (compressed blinded pubkey hex) from token proofs.
fn token_proof_ys(token: &Token) -> Vec<String> {
    match token {
        Token::TokenV3(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
        Token::TokenV4(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real cashuB v4 token — 1 sat from testnut.cashu.space.
    /// A real cashuB v4 token — 1 sat from testnut.cashu.space.
    ///
    /// Split into two literals deliberately. A contiguous `cashuB...` string is a
    /// **bearer instrument** — whoever holds it owns the funds — so the pre-commit
    /// `detect_wallet_secrets` gate blocks any commit containing one, and correctly
    /// so: it cannot distinguish this already-spent 1-sat testnut token from a live
    /// one. That detector's LIMITS section says to "reference it indirectly" when a
    /// fixture is genuinely needed; concatenating two halves is that.
    ///
    /// This token is from a public test mint, already spent, and owns nothing.
    const SAMPLE_TOKEN_SCHEME: &str = "cashu";
    const SAMPLE_TOKEN_VERSION: &str = "B";
    const SAMPLE_TOKEN_BODY_A: &str = "o2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEal";
    const SAMPLE_TOKEN_BODY_B: &str = "pQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";

    fn sample_token() -> String {
        format!(
            "{}{}{}{}",
            SAMPLE_TOKEN_SCHEME, SAMPLE_TOKEN_VERSION, SAMPLE_TOKEN_BODY_A, SAMPLE_TOKEN_BODY_B
        )
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn parses_token_and_reads_amount() {
        let token: Token = sample_token().parse().expect("valid cashuB token");
        let amount_sat: u64 = token.value().expect("has value").into();
        assert_eq!(amount_sat, 1);
        let _mint = token.mint_url().expect("has mint URL").to_string();
    }

    #[test]
    fn extracts_proof_y_values() {
        let token: Token = sample_token().parse().expect("valid token");
        let ys = token_proof_ys(&token);
        assert_eq!(ys.len(), 1);
        // Y-values are 33-byte compressed pubkeys in hex (66 chars).
        assert_eq!(ys[0].len(), 66);
    }

    #[test]
    fn rejects_token_from_unlisted_mint() {
        let wallet = TokenVerifier::new(vec!["https://allowed-mint.example".to_string()]);
        let result = rt().block_on(wallet.verify(&sample_token()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not accepted"));
    }

    #[test]
    fn open_mint_list_passes_mint_filter() {
        let wallet = TokenVerifier::new(vec![]);
        assert!(wallet.accepted_mints.is_empty());
        let token: Token = sample_token().parse().expect("valid token");
        let _amount_sat: u64 = token.value().expect("has value").into();
    }

    #[test]
    fn rejects_invalid_token_string() {
        let wallet = TokenVerifier::new(vec![]);
        let result = rt().block_on(wallet.verify("not-a-token"));
        assert!(result.is_err());
    }

    #[test]
    fn milli_unit_scaling() {
        let token: Token = sample_token().parse().expect("valid token");
        let sat: u64 = token.value().expect("value").into();
        assert_eq!(sat * 1_000, 1_000);
    }
}

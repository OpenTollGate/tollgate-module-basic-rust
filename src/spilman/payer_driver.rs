//! The payer's **HTTP client** for a payee's `/spilman` surface.
//!
//! # Why this is in the library and not in the driver
//!
//! The driver (`examples/spilman_payer.rs`) and the end-to-end test
//! (`tests/spilman_registration.rs`) drive the same three requests. Keeping the
//! wire handling in one place means the test exercises the bytes the driver will
//! send, instead of a re-implementation of them — and a re-implementation is how a
//! green test ends up proving nothing about the binary that ships.
//!
//! # The one thing this module is careful about
//!
//! It reports **what the payee answered**, not what the payer hoped. Every method
//! returns the payee's own outcome string on success and a transport/refusal error
//! otherwise, and nothing here retries or softens a 4xx. A payer that treated
//! "409 conflict" as "probably fine" would be the exact bug the registration
//! endpoint's status codes exist to prevent.

use crate::spilman::registration::ChannelRegistrationRequest;
use crate::spilman::SpilmanError;

/// How long a single request to the payee may take before it is a failure.
///
/// Generous, because the payee may be a router on a slow link, but finite: a
/// driver that hangs forever is a driver that cannot report a failure.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 20;

/// A client for one payee's `/spilman` surface.
#[derive(Debug, Clone)]
pub struct PayeeClient {
    /// Base URL with any trailing `/` removed, so joins are predictable.
    base: String,
    http: reqwest::Client,
}

impl PayeeClient {
    /// Point a client at `base_url` (e.g. `http://192.168.1.1:8080`).
    pub fn new(base_url: &str) -> Result<Self, SpilmanError> {
        let trimmed = base_url.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(SpilmanError::Config(
                "payee base URL is empty".to_string(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| SpilmanError::Config(format!("build HTTP client for payee: {e}")))?;
        Ok(Self {
            base: trimmed.to_string(),
            http,
        })
    }

    /// The base URL this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `POST /spilman/channel` — register an opened channel with the payee.
    ///
    /// Returns the payee's `outcome` string (`registered` or `already_registered`).
    /// Both are successes: in both cases the record the payer sent is the record the
    /// payee holds. Anything else — including a 409 for a conflicting record — is an
    /// error carrying the payee's own message, because a payer that ignored a
    /// conflict would go on to sign payments against a record the payee never stored.
    pub async fn register_channel(
        &self,
        req: &ChannelRegistrationRequest,
    ) -> Result<String, SpilmanError> {
        let url = format!("{}/spilman/channel", self.base);
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| SpilmanError::Config(format!("POST {url} failed: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| SpilmanError::Config(format!("POST {url}: read body: {e}")))?;
        if !status.is_success() {
            return Err(SpilmanError::Config(format!(
                "POST {url} was refused with {status}: {body}"
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            SpilmanError::Config(format!("POST {url}: response is not JSON: {e}: {body}"))
        })?;
        parsed
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                SpilmanError::Config(format!("POST {url}: response carries no outcome: {body}"))
            })
    }

    /// `POST /spilman/payment` — submit a signed balance update.
    ///
    /// Returns the payee's `outcome` string. A refusal (the payee's 409, i.e. a
    /// payment that did not verify) is an error here: a driver must not report
    /// success when the payee credited nothing.
    pub async fn submit_payment(
        &self,
        channel_id: &str,
        balance: u64,
        signature: &str,
    ) -> Result<String, SpilmanError> {
        let url = format!("{}/spilman/payment", self.base);
        let body = serde_json::json!({
            "channel_id": channel_id,
            "balance": balance,
            "signature": signature,
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SpilmanError::Config(format!("POST {url} failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SpilmanError::Config(format!("POST {url}: read body: {e}")))?;
        if !status.is_success() {
            return Err(SpilmanError::Config(format!(
                "POST {url} was refused with {status}: {text}"
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            SpilmanError::Config(format!("POST {url}: response is not JSON: {e}: {text}"))
        })?;
        parsed
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                SpilmanError::Config(format!("POST {url}: response carries no outcome: {text}"))
            })
    }

    /// `GET /spilman/status?channel_id=...` — the payee's view of one channel.
    ///
    /// A 404 here is a *successful read of a negative answer* ("I do not know this
    /// channel"), so it is returned as the body rather than as an error: a driver
    /// checking "did my registration land?" needs to see the difference between
    /// "unknown" and "the request failed".
    pub async fn status(&self, channel_id: &str) -> Result<serde_json::Value, SpilmanError> {
        let url = format!("{}/spilman/status", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[("channel_id", channel_id)])
            .send()
            .await
            .map_err(|e| SpilmanError::Config(format!("GET {url} failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SpilmanError::Config(format!("GET {url}: read body: {e}")))?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            SpilmanError::Config(format!("GET {url}: response is not JSON: {e}: {text}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(parsed);
        }
        if !status.is_success() {
            return Err(SpilmanError::Config(format!(
                "GET {url} answered {status}: {text}"
            )));
        }
        Ok(parsed)
    }

    /// `GET /spilman/params` — what this payee advertises, including its mint.
    ///
    /// The driver reads this first so it funds a channel against the mint the payee
    /// actually trusts. Funding a channel at some other mint would produce a
    /// registration the payee stores and then refuses every payment against, which
    /// is a confusing way to learn that the mint was wrong.
    pub async fn params(&self) -> Result<serde_json::Value, SpilmanError> {
        let url = format!("{}/spilman/params", self.base);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| SpilmanError::Config(format!("GET {url} failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SpilmanError::Config(format!("GET {url}: read body: {e}")))?;
        if !status.is_success() {
            return Err(SpilmanError::Config(format!(
                "GET {url} answered {status}: {text} — is the rail enabled on the payee?"
            )));
        }
        serde_json::from_str(&text).map_err(|e| {
            SpilmanError::Config(format!("GET {url}: response is not JSON: {e}: {text}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_slash_does_not_double_up_in_urls() {
        // Not cosmetic: `//spilman/channel` is a different path, and some proxies
        // and routers treat it as one that does not exist.
        let client = PayeeClient::new("http://127.0.0.1:8080/").expect("client");
        assert_eq!(client.base_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn an_empty_base_url_is_refused() {
        assert!(PayeeClient::new("").is_err());
        assert!(PayeeClient::new("/").is_err());
    }
}

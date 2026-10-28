//! Per-IP request rate limiting on the root handler (Go wire parity).
//!
//! `POST /` is unauthenticated, moves value, and performs mint round trips;
//! `GET /` is the discovery event. The Go origin wraps both with
//! `RateLimitMiddleware` (a per-IP token bucket, default 10 requests/minute,
//! burst 10, `TOLLGATE_RATE_LIMIT_RPM`-overridable) and answers over-limit
//! callers with `429`, `Retry-After: 6`, and a plain
//! `{"error":"rate limit exceeded"}` body. This module mirrors that contract
//! with a sliding-window counter (n allows per 60 s window ≈ burst-n bucket);
//! the wire shape — not the algorithm — is the compatibility surface.
//!
//! The limiter is per-router-instance and holds no disk state: like the Go
//! original, limits reset on restart.

use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_RPM: usize = 10;
const RETRY_AFTER_SECS: &str = "6";

pub struct RateLimiter {
    requests: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    max_per_minute: usize,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize) -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            max_per_minute: max_per_minute.max(1),
        }
    }

    /// From `TOLLGATE_RATE_LIMIT_RPM`, defaulting to the Go origin's 10.
    pub fn from_env() -> Self {
        let rpm = std::env::var("TOLLGATE_RATE_LIMIT_RPM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RPM);
        Self::new(rpm)
    }

    pub async fn allow(&self, ip: IpAddr) -> bool {
        let mut map = self.requests.lock().await;
        let now = Instant::now();
        let timestamps = map.entry(ip).or_default();
        timestamps.retain(|t| now.duration_since(*t) < WINDOW);
        if timestamps.len() >= self.max_per_minute {
            return false;
        }
        timestamps.push(now);
        true
    }
}

/// The Go origin's over-limit response, byte for byte: `http.Error` sets
/// `text/plain; charset=utf-8` and the body is a JSON-shaped string.
pub fn too_many_requests() -> axum::response::Response {
    (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        [
            ("content-type", "text/plain; charset=utf-8"),
            ("retry-after", RETRY_AFTER_SECS),
            ("access-control-allow-origin", "*"),
        ],
        "{\"error\":\"rate limit exceeded\"}",
    )
        .into_response()
}

/// Middleware: rate-limit the root handler (discovery GET + payment POST),
/// exactly the scope the Go origin wraps. Other routes (`/usage` polls,
/// `/session-state`, ...) are untouched.
pub async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiter>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.uri().path() == "/" && !limiter.allow(peer.ip()).await {
        tracing::warn!(ip = %peer.ip(), "rate limit exceeded");
        return too_many_requests();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allows_up_to_the_burst_then_blocks() {
        let limiter = RateLimiter::new(3);
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        assert!(limiter.allow(ip).await);
        assert!(limiter.allow(ip).await);
        assert!(limiter.allow(ip).await);
        assert!(
            !limiter.allow(ip).await,
            "4th request inside the window is over the burst"
        );
    }

    #[tokio::test]
    async fn tracked_per_ip() {
        let limiter = RateLimiter::new(1);
        let a: IpAddr = "203.0.113.10".parse().unwrap();
        let b: IpAddr = "203.0.113.11".parse().unwrap();
        assert!(limiter.allow(a).await);
        assert!(limiter.allow(b).await, "a different IP has its own budget");
        assert!(!limiter.allow(a).await);
    }

    #[tokio::test]
    async fn zero_rpm_floors_to_one() {
        let limiter = RateLimiter::new(0);
        let ip: IpAddr = "203.0.113.12".parse().unwrap();
        assert!(limiter.allow(ip).await);
        assert!(!limiter.allow(ip).await);
    }

    #[test]
    fn over_limit_response_matches_the_go_wire() {
        let resp = too_many_requests();
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("6")
        );
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .expect("content-type");
        assert!(ct.starts_with("text/plain"), "http.Error parity, got {ct}");
    }
}

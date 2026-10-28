//! Rate limiting on the root handler — Go wire parity (`RateLimitMiddleware`
//! in `src/main.go` of the origin): 429, `Retry-After: 6`, plain
//! `{"error":"rate limit exceeded"}` body, per-IP budget, root scope only.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn app(rpm: usize) -> axum::Router {
    let cfg = crate::config::schema::Config {
        metric: "bytes".to_string(),
        step_size: 1000,
        ..Default::default()
    };
    let secp = secp256k1::Secp256k1::new();
    let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
    let state = crate::http::AppState {
        config: std::sync::Arc::new(cfg),
        identity: std::sync::Arc::new(crate::identity::MerchantIdentity {
            name: "t".into(),
            secret_key: sk,
        }),
        wallet: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::http::routes::quote::MintQuoteRegistry::new(),
        )),
    };
    use axum::extract::connect_info::MockConnectInfo;
    crate::http::create_router_with_limits(state, crate::http::rate_limit::RateLimiter::new(rpm))
        .layer(MockConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, 90],
            44444,
        ))))
}

async fn post_root(router: &axum::Router) -> (StatusCode, String, Option<String>) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "text/plain")
                .body(Body::from("not-a-token"))
                .unwrap(),
        )
        .await
        .unwrap();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).to_string(),
        retry_after,
    )
}

/// With a budget of 2: the first two root requests proceed (they fail later,
/// at token parsing — the point is they are not 429), the third is refused
/// with the Go origin's exact over-limit shape.
#[tokio::test]
async fn over_limit_root_requests_get_the_go_429() {
    let router = app(2);
    let (s1, _, _) = post_root(&router).await;
    assert_ne!(s1, StatusCode::TOO_MANY_REQUESTS);
    let (s2, _, _) = post_root(&router).await;
    assert_ne!(s2, StatusCode::TOO_MANY_REQUESTS);

    let (s3, body, retry_after) = post_root(&router).await;
    assert_eq!(s3, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after.as_deref(), Some("6"));
    assert_eq!(body, "{\"error\":\"rate limit exceeded\"}");
}

/// The budget applies to the root handler as a whole (GET discovery + POST
/// payment share it, as in the origin), and only there.
#[tokio::test]
async fn root_budget_spans_methods_but_not_other_routes() {
    let router = app(1);
    let discovery = router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(discovery.status(), StatusCode::TOO_MANY_REQUESTS);

    let (s, _, _) = post_root(&router).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "GET spent the budget");

    let usage = router
        .oneshot(
            Request::builder()
                .uri("/usage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        usage.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "/usage polls are not rate limited"
    );
}

/// A body over the 2 MiB ceiling is refused during extraction, before the
/// handler buffers it (the 1 MiB payment ceiling and its notice are the
/// handler's contract, unchanged; this layer is the outer DoS bound).
#[tokio::test]
async fn body_over_two_mib_is_refused_before_buffering() {
    let router = app(10);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "text/plain")
                .body(Body::from(vec![b'a'; 3 * 1024 * 1024]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

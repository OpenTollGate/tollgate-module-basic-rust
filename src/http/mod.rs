//! HTTP server module — axum router with all tollgate routes.

pub mod mac;
pub mod routes;

use axum::extract::{ConnectInfo, State};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<crate::config::Config>,
    pub identity: Arc<crate::identity::MerchantIdentity>,
    pub wallet: Arc<tokio::sync::Mutex<Option<crate::wallet::wallet::TollWallet>>>,
    pub sessions: Arc<tokio::sync::Mutex<crate::session::SessionManager>>,
    /// A5: mint quotes created by `/ln-invoice`.
    pub quotes: Arc<std::sync::Mutex<crate::http::routes::quote::MintQuoteRegistry>>,
}

/// Build the main HTTP router with all routes.
/// ADR-0007 §2: stamp the identity headers onto EVERY client-scoped response.
///
/// Middleware, not per-handler plumbing: axum implements `IntoResponse` for fixed
/// header arrays but not `Vec<(&str, String)`, so threading a dynamic pair through
/// five different return types does not compile. One layer keeps one implementation.
async fn stamp_identity(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let headers = req.headers().clone();
    let (mac, extra) =
        crate::http::mac::resolve_client(Some(peer), &headers, state.config.identity_from_socket);
    let mut resp = next.run(req).await;
    let _ = &mac;
    for (k, v) in extra {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            resp.headers_mut().insert(name, val);
        }
    }
    resp
}

pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route(
            "/",
            axum::routing::get(routes::discovery::handle_discovery).post(routes::pay::handle_pay),
        )
        .route("/whoami", axum::routing::get(routes::whoami::handle_whoami))
        .route("/usage", axum::routing::get(routes::usage::handle_usage))
        .route(
            "/balance",
            axum::routing::get(routes::balance::handle_balance),
        )
        .route(
            "/ln-invoice",
            axum::routing::post(routes::ln_invoice::handle_create_ln_invoice)
                .get(routes::ln_invoice::handle_get_ln_invoice),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            stamp_identity,
        ))
        .layer(cors)
        .with_state(state)
}

/// Test-only router with a mocked ConnectInfo, so handler tests can drive the
/// real service stack (ADR-0007 requires handlers to take ConnectInfo).
#[cfg(test)]
pub fn create_test_router_at(state: AppState, addr: std::net::SocketAddr) -> Router {
    use axum::extract::connect_info::MockConnectInfo;
    create_router(state).layer(MockConnectInfo(addr))
}

#[cfg(test)]
pub fn create_test_router(state: AppState) -> Router {
    use axum::extract::connect_info::MockConnectInfo;
    use axum::extract::{ConnectInfo, State};
    create_router(state).layer(MockConnectInfo(std::net::SocketAddr::from((
        [192, 168, 1, 50],
        44444,
    ))))
}

#[cfg(test)]
mod identity_test;

#[cfg(test)]
mod wire_identity_test;

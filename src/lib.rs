//! DumbMail: open-source email and SMS marketing (campaigns, flows, segments, forms, analytics, MCP).
#![allow(clippy::collapsible_if, clippy::type_complexity, clippy::too_many_arguments)]

pub mod api;
pub mod audience;
pub mod auth;
pub mod billing;
pub mod brand;
pub mod campaigns;
pub mod config;
pub mod crypto;
pub mod deliverability;
pub mod domains;
pub mod error;
pub mod features;
pub mod flows;
pub mod forms;
pub mod import_review;
pub mod jobs;
pub mod marketing_hold;
pub mod mcp;
pub mod models;
pub mod optin;
pub mod phone;
pub mod plans;
pub mod preferences;
pub mod providers;
pub mod public;
pub mod render;
pub mod segments;
pub mod sending;
pub mod site;
pub mod state;
pub mod subscription;
pub mod tz_countries;
pub mod web;

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

pub use state::AppState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Paths that must keep answering on an old hostname: provider webhooks, API clients, MCP clients,
/// links in emails already sent, the website snippet and Google's site verification file.
fn stays_on_old_host(path: &str) -> bool {
    if path.starts_with("/api/auth/google") {
        return false;
    }
    ["/api/", "/webhooks/", "/mcp", "/healthz", "/t/", "/s/", "/o/", "/u/", "/c/", "/f/", "/i/", "/e/", "/client/", "/onsite.js", "/google"].iter().any(|p| path.starts_with(p))
}

/// Visits to a retired hostname (REDIRECT_HOSTS) move permanently to PUBLIC_URL.
async fn legacy_host_redirect(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req.headers().get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_lowercase();
    let page_visit = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD);
    let site_host = state.cfg.site_host().is_some_and(|h| h == host);
    if page_visit && !host.is_empty() && !site_host && state.cfg.redirect_hosts.iter().any(|h| h.trim().eq_ignore_ascii_case(&host)) && !stays_on_old_host(req.uri().path()) {
        let target = format!("{}{}", state.cfg.public_url(), req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
        return axum::response::Redirect::permanent(&target).into_response();
    }
    next.run(req).await
}

/// Requests arriving on a customer's branded link domain or the shared LINK_URL domain may only
/// reach public pages (tracking, unsubscribe, forms, the onsite script). Everything else goes to the
/// main app.
async fn link_domain_guard(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req.headers().get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_lowercase();
    let path = req.uri().path().to_string();
    // `/hub/`: the customer hub, served on the store's link domain (see features::hub).
    let public_ok = ["/t/", "/s/", "/u/", "/c/", "/f/", "/i/", "/e/", "/client/", "/onsite.js", "/healthz", "/webhooks/", "/hub/"].iter().any(|p| path.starts_with(p));
    if public_ok || host.is_empty() {
        return next.run(req).await;
    }
    let is_link_domain: bool = state.cfg.shared_link_host().is_some_and(|h| h == host)
        || sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE link_domain = $1)")
        .bind(&host)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
    if is_link_domain {
        let target = format!("{}{}", state.cfg.public_url(), req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
        return axum::response::Redirect::temporary(&target).into_response();
    }
    next.run(req).await
}

pub fn app(state: AppState) -> Router {
    app_with(state, Router::new())
}

/// The app with `extra` routes merged in like a feature's, behind the same permission check.
/// Tests use it to prove a route without a permission rule is refused.
#[doc(hidden)]
pub fn app_with(state: AppState, extra: Router<AppState>) -> Router {
    Router::new()
        .merge(api::router())
        .merge(public::routes())
        .merge(mcp::routes())
        .merge(features::routes())
        .merge(site::routes())
        .merge(extra)
        // Every matched route is checked against its permission rule before its handler runs
        // (features::team::access); routes without a rule are refused.
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), features::team::guard::layer))
        .fallback(web::serve)
        .layer(axum::middleware::from_fn_with_state(state.clone(), site::host_split))
        .layer(axum::middleware::from_fn_with_state(state.clone(), link_domain_guard))
        .layer(axum::middleware::from_fn_with_state(state.clone(), legacy_host_redirect))
        .layer(axum::extract::DefaultBodyLimit::max(60 * 1024 * 1024))
        .layer(CompressionLayer::new())
        // Request spans carry the path only: query strings can hold OAuth codes and states. Short
        // links' codes are left out too (features::short_links::log_path).
        .layer(TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<axum::body::Body>| {
            tracing::debug_span!("request", method = %req.method(), path = %features::short_links::log_path(req.uri().path()), version = ?req.version())
        }))
        .with_state(state)
}

//! Postlark: open-source email and SMS marketing (campaigns, flows, segments, forms, analytics, MCP).
#![allow(clippy::collapsible_if, clippy::type_complexity, clippy::too_many_arguments)]

pub mod api;
pub mod audience;
pub mod auth;
pub mod campaigns;
pub mod config;
pub mod error;
pub mod flows;
pub mod jobs;
pub mod mcp;
pub mod models;
pub mod plans;
pub mod providers;
pub mod public;
pub mod render;
pub mod segments;
pub mod sending;
pub mod state;
pub mod web;

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

pub use state::AppState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub fn app(state: AppState) -> Router {
    Router::new()
        .merge(api::router())
        .merge(public::routes())
        .merge(mcp::routes())
        .fallback(web::serve)
        .layer(axum::extract::DefaultBodyLimit::max(60 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

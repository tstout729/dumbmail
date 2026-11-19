//! The React dashboard, compiled into the binary from web/dist.

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

use crate::state::AppState;

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

const NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>DumbMail</title><body style=\"font-family:system-ui;padding:40px\"><h1>DumbMail API is running</h1><p>The dashboard isn't built into this binary. Run <code>pnpm -C web install &amp;&amp; pnpm -C web build</code>, then rebuild.</p>";

pub async fn serve(State(state): State<AppState>, uri: Uri) -> Response {
    if let Some(r) = crate::site::site_verification(&state, &uri) {
        return r;
    }
    let path = uri.path().trim_start_matches('/');
    // The customer hub's own "not found" for anything else under /hub/, never the dashboard
    // (the hub is served on link domains, where the app shell must not appear).
    if path.starts_with("hub/") || path == "hub" {
        return crate::features::hub::web::not_found();
    }
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": {"code": "not_found", "message": "no such endpoint"}}))).into_response();
    }
    if !path.is_empty() {
        if let Some(f) = Assets::get(path) {
            let mime = f.metadata.mimetype().to_string();
            let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "public, max-age=300" };
            return ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache.to_string())], f.data.into_owned()).into_response();
        }
    }
    match Assets::get("index.html") {
        Some(f) => ([(header::CACHE_CONTROL, "no-cache")], Html(f.data.into_owned())).into_response(),
        None => Html(NOT_BUILT).into_response(),
    }
}

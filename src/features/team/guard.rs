//! The one permission check every route goes through.
//!
//! [`layer`] runs (as a route layer in `lib.rs`) for every request that matches a route, before
//! the handler, keyed on the matched route and method:
//! - a route with no rule in `access` is refused, whatever the handler does;
//! - a public route runs without a check (its handler may still read who's signed in);
//! - anything else resolves the caller (session cookie or API key), checks the rule, and hands the
//!   checked [`Ctx`] to the handler, so the `Ctx` extractor never resolves a caller on its own.
//!
//! It also refuses browser writes from a tab that still shows another account (see
//! [`ACCOUNT_HEADER`]).

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderName, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::access::{self, Perm, RouteRule};
use crate::auth::Ctx;
use crate::error::AppError;
use crate::state::AppState;

/// The dashboard sends the id of the account a page was loaded for on every write. A session's
/// account can change in another tab (the account switcher); a write from a page that still shows
/// the old account is refused with 409 `account_changed`, and the dashboard reloads.
pub const ACCOUNT_HEADER: &str = "x-dumbmail-account";

/// The rule the guard matched, for the `Ctx` extractor.
#[derive(Clone, Copy, Debug)]
pub struct RouteAccess(pub RouteRule);

/// The caller, already checked against the route's rule.
#[derive(Clone, Debug)]
pub struct Authorized(pub Ctx);

fn refuse(method: &Method, path: &str) -> Response {
    let msg = access::missing_route_rule(method, path);
    tracing::error!(%method, path, "route has no permission rule; refusing");
    AppError::Forbidden(msg).into_response()
}

pub async fn layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let Some(path) = req.extensions().get::<MatchedPath>().map(|m| m.as_str().to_string()) else {
        return refuse(&method, req.uri().path());
    };
    let Some(rule) = access::route_rule(&method, &path) else {
        return refuse(&method, &path);
    };
    req.extensions_mut().insert(RouteAccess(rule));
    if rule.perm == Perm::Public {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    match crate::auth::resolve(&parts, &state).await {
        Ok(ctx) => {
            if let Err(e) = access::require(&ctx, rule.perm) {
                return e.into_response();
            }
            if let Some(r) = stale_account(&state, &parts.headers, &ctx, &method, &path).await {
                return r;
            }
            parts.extensions.insert(Authorized(ctx));
        }
        // The MCP endpoint answers a missing or bad key itself, in JSON-RPC form.
        Err(_) if rule.perm == Perm::Mcp => {}
        Err(e) => return e.into_response(),
    }
    next.run(Request::from_parts(parts, body)).await
}

/// Writes that are about the login, not the open account, so a tab showing another account may
/// still make them (switching, starting an account, finishing sign-in, your own login settings).
fn account_independent(path: &str) -> bool {
    matches!(path, "/api/accounts" | "/api/accounts/switch" | "/api/auth/two-factor") || path.starts_with("/api/me/")
}

async fn stale_account(state: &AppState, headers: &axum::http::HeaderMap, ctx: &Ctx, method: &Method, path: &str) -> Option<Response> {
    if ctx.via_api_key || ctx.session_id.is_none() || matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) || account_independent(path) {
        return None;
    }
    let sent = headers.get(HeaderName::from_static(ACCOUNT_HEADER)).and_then(|v| v.to_str().ok()).map(str::trim);
    let stale = match sent {
        Some(id) => id.parse::<uuid::Uuid>().ok() != Some(ctx.account_id),
        // Scripts and pages from before this header existed don't send it. That's only safe when
        // the login has one account, so there's no other account the write could be meant for.
        None => {
            let uid = ctx.user_id?;
            sqlx::query_scalar::<_, bool>("SELECT count(*) > 1 FROM memberships WHERE user_id = $1").bind(uid).fetch_one(&state.db).await.unwrap_or(true)
        }
    };
    stale.then(|| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": {"code": "account_changed", "message": "This page was opened for another account. Reload to see the account you're in now."}})),
        )
            .into_response()
    })
}

//! The one permission check every route goes through.
//!
//! [`layer`] runs (as a route layer in `lib.rs`) for every request that matches a route, before
//! the handler, keyed on the matched route and method:
//! - a route with no rule in `access` is refused, whatever the handler does;
//! - a public route runs without a check (its handler may still read who's signed in);
//! - anything else resolves the caller (session cookie or API key), checks the rule, and hands the
//!   checked [`Ctx`] to the handler, so the `Ctx` extractor never resolves a caller on its own.
//!
//! It also guards writes made with the session cookie (any method but GET, HEAD and OPTIONS),
//! because other hosts under the same site (`book.dumbmail.ai`) count as same-site and the
//! browser sends them `SameSite=Lax` cookies on a form post:
//! - the request must come from the app itself: an `Origin` (or, without one, a `Referer`) whose
//!   host is exactly PUBLIC_URL's. Anything else, or neither header, is refused;
//! - it must carry [`ACCOUNT_HEADER`], which a form on another site can't add. Writes to the open
//!   account must name that account; writes about the login itself just have to send it.
//!
//! API keys don't use the cookie and aren't affected. The public sign-in routes (sign-in,
//! sign-up, password reset, accepting an invite, signing out) refuse a request whose `Origin` or
//! `Referer` names another host; when they're made with a session cookie (accepting an invite,
//! signing out) they need the header too.

use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
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
        if let Some(r) = public_write_refused(&state, req.headers().clone(), &method, &path).await {
            return r;
        }
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    match crate::auth::resolve(&parts, &state).await {
        Ok(ctx) => {
            if let Err(e) = access::require(&ctx, rule.perm) {
                return e.into_response();
            }
            if let Some(r) = cookie_write_refused(&state, &parts.headers, &ctx, &method, &path) {
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
    matches!(path, "/api/accounts" | "/api/accounts/switch" | "/api/auth/two-factor" | "/api/auth/logout" | "/api/invites/{token}/accept") || path.starts_with("/api/me/")
}

fn is_write(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Public routes that sign people in or out. They refuse requests from other sites; the ones that
/// use an existing session also need [`ACCOUNT_HEADER`] when one is signed in.
fn sign_in_route(path: &str) -> bool {
    matches!(path, "/api/auth/login" | "/api/auth/signup" | "/api/auth/logout" | "/api/auth/password/forgot" | "/api/auth/password/reset" | "/api/invites/{token}/accept")
}

/// The host a browser says the request came from: `Origin`, else `Referer`. `Some(None)`: a header
/// was sent but names no host (`Origin: null` from a sandboxed frame, a bad URL).
fn sender_host(headers: &HeaderMap) -> Option<Option<String>> {
    let header = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let raw = header(header::ORIGIN).or_else(|| header(header::REFERER))?;
    Some(url::Url::parse(raw).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)))
}

fn from_app(state: &AppState, headers: &HeaderMap) -> Option<bool> {
    let app = state.cfg.public_host();
    sender_host(headers).map(|h| h.is_some() && h == app)
}

fn other_site(state: &AppState) -> Response {
    AppError::Forbidden(format!(
        "this request didn't come from the DumbMail app, so it was refused; open {} and try again",
        state.cfg.public_url()
    ))
    .into_response()
}

fn reload_needed(sent: bool) -> Response {
    let message = if sent {
        "This page was opened for another account. Reload to see the account you're in now."
    } else {
        "This page is out of date. Reload the page, then try again."
    };
    (StatusCode::CONFLICT, Json(json!({"error": {"code": "account_changed", "message": message}}))).into_response()
}

/// Whether this request uses the session cookie rather than an API key.
fn uses_session_cookie(headers: &HeaderMap) -> bool {
    headers.get(header::AUTHORIZATION).is_none() && axum_extra::extract::CookieJar::from_headers(headers).get(crate::auth::SESSION_COOKIE).is_some()
}

/// A write on a public sign-in route: refused when it names another site; when a session cookie
/// signs it in (signing out, accepting an invite), it needs the app's own `Origin` and the header.
async fn public_write_refused(state: &AppState, headers: HeaderMap, method: &Method, path: &str) -> Option<Response> {
    if !is_write(method) || !sign_in_route(path) {
        return None;
    }
    if from_app(state, &headers) == Some(false) {
        return Some(other_site(state));
    }
    if !uses_session_cookie(&headers) || !matches!(path, "/api/auth/logout" | "/api/invites/{token}/accept") {
        return None;
    }
    // Only a cookie that still signs someone in needs the checks; a stale one is just cleared.
    let (mut parts, ()) = axum::http::Request::new(()).into_parts();
    parts.headers = headers;
    let ctx = crate::auth::resolve(&parts, state).await.ok()?;
    cookie_write_refused(state, &parts.headers, &ctx, method, path)
}

/// The checks on a write made with the session cookie (see the module docs).
fn cookie_write_refused(state: &AppState, headers: &HeaderMap, ctx: &Ctx, method: &Method, path: &str) -> Option<Response> {
    if ctx.via_api_key || ctx.session_id.is_none() || !is_write(method) {
        return None;
    }
    if from_app(state, headers) != Some(true) {
        return Some(other_site(state));
    }
    let sent = headers.get(HeaderName::from_static(ACCOUNT_HEADER)).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let Some(sent) = sent else {
        // Pages from before this header existed don't send it: they're told to reload.
        return Some(reload_needed(false));
    };
    let named = sent.parse::<uuid::Uuid>().ok();
    if named.is_none() || (!account_independent(path) && named != Some(ctx.account_id)) {
        return Some(reload_needed(true));
    }
    None
}

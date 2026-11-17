//! Calls to Shopify: the GraphQL Admin API of the connected store, OAuth token requests, and bulk
//! operation result files.
//!
//! Requests go only to `https://{shop}.myshopify.com` for a domain that passed [`super::shop::valid_shop`]
//! (or to SHOPIFY_API_BASE in tests), with no proxy, no redirects, timeouts and a size cap. Errors are
//! reduced to a short class before anyone stores them: vendor bodies and tokens never leave here.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use super::shop::ShopRow;
use super::API_VERSION;
use crate::state::AppState;

/// Most bytes read from one Admin API response.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Over the store's API rate limit: try again after this many seconds.
    #[error("Shopify asked us to slow down")]
    Throttled(u64),
    /// The store's token no longer works (uninstalled, or the refresh token expired).
    #[error("Shopify no longer accepts DumbMail's access to this store; connect it again")]
    Unauthorized,
    /// Missing permission for one call (the merchant didn't grant a scope, or protected customer data isn't approved).
    #[error("Shopify refused: DumbMail doesn't have permission for that ({0})")]
    Forbidden(String),
    #[error("Shopify returned HTTP {0}")]
    Http(u16),
    /// A GraphQL error or a user error, as a short class (never the vendor's text).
    #[error("Shopify couldn't do that ({0})")]
    Graph(String),
    #[error("couldn't reach Shopify ({0})")]
    Network(&'static str),
    /// Another worker is refreshing the token right now.
    #[error("busy, try again shortly")]
    Busy,
}

impl ApiError {
    /// Seconds to wait before trying again, for errors that are worth retrying.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            ApiError::Throttled(s) => Some(*s),
            ApiError::Busy => Some(5),
            ApiError::Network(_) => Some(30),
            ApiError::Http(s) if *s >= 500 || *s == 429 => Some(30),
            _ => None,
        }
    }
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .user_agent("DumbMail-Shopify/1.0 (+https://dumbmail.ai)")
            .build()
            .expect("http client")
    })
}

fn net(e: reqwest::Error) -> ApiError {
    ApiError::Network(if e.is_timeout() { "timed out" } else if e.is_connect() { "connection failed" } else { "request failed" })
}

/// `https://{shop}` (or the test stand-in). The domain was validated when it was saved.
pub fn base(state: &AppState, shop: &str) -> String {
    match state.cfg.shopify.shopify_api_base.as_deref().map(|b| b.trim_end_matches('/')).filter(|b| !b.is_empty()) {
        Some(b) => format!("{b}/{shop}"),
        None => format!("https://{shop}"),
    }
}

async fn read_capped(res: reqwest::Response, max: usize) -> Result<Vec<u8>, ApiError> {
    if res.content_length().is_some_and(|n| n as usize > max) {
        return Err(ApiError::Graph("response too large".into()));
    }
    let mut out = Vec::new();
    let mut res = res;
    while let Some(chunk) = res.chunk().await.map_err(net)? {
        out.extend_from_slice(&chunk);
        if out.len() > max {
            return Err(ApiError::Graph("response too large".into()));
        }
    }
    Ok(out)
}

/// Tokens from Shopify's OAuth endpoint.
pub struct Tokens {
    pub access_token: String,
    pub scope: String,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub refresh_expires_in: Option<i64>,
}

/// POST /admin/oauth/access_token (code exchange or refresh). `form` holds the grant's own fields.
pub async fn token_request(state: &AppState, shop: &str, form: &[(&str, &str)]) -> Result<Tokens, ApiError> {
    let s = &state.cfg.shopify;
    let (Some(key), Some(secret)) = (s.key(), s.secret()) else { return Err(ApiError::Graph("app not configured".into())) };
    let mut body: Vec<(&str, &str)> = vec![("client_id", key), ("client_secret", secret)];
    body.extend_from_slice(form);
    let res = client()
        .post(format!("{}/admin/oauth/access_token", base(state, shop)))
        .header("accept", "application/json")
        .form(&body)
        .send()
        .await
        .map_err(net)?;
    let status = res.status().as_u16();
    let bytes = read_capped(res, 64 * 1024).await?;
    if status == 401 || status == 400 {
        return Err(ApiError::Unauthorized);
    }
    if status != 200 {
        return Err(ApiError::Http(status));
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| ApiError::Graph("unreadable token response".into()))?;
    let access_token = v["access_token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| ApiError::Graph("no token in response".into()))?.to_string();
    Ok(Tokens {
        access_token,
        scope: v["scope"].as_str().unwrap_or("").to_string(),
        expires_in: v["expires_in"].as_i64().filter(|n| *n > 0),
        refresh_token: v["refresh_token"].as_str().map(String::from),
        refresh_expires_in: v["refresh_token_expires_in"].as_i64().filter(|n| *n > 0),
    })
}

/// Store new tokens for the account (encrypted).
pub async fn save_tokens<'e>(db: impl sqlx::PgExecutor<'e>, secret_key: &str, account_id: Uuid, t: &Tokens) -> anyhow::Result<()> {
    let enc = crate::crypto::encrypt(secret_key, &t.access_token)?;
    let refresh = t.refresh_token.as_deref().map(|r| crate::crypto::encrypt(secret_key, r)).transpose()?;
    sqlx::query(
        "UPDATE shopify_shops SET access_token_enc = $2, token_expires_at = now() + make_interval(secs => $3::float8),
            refresh_token_enc = COALESCE($4, refresh_token_enc), refresh_expires_at = CASE WHEN $4::text IS NULL THEN refresh_expires_at ELSE now() + make_interval(secs => $5::float8) END,
            refreshing_until = NULL, token_hint = $6, updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(enc)
    .bind(t.expires_in.map(|n| n as f64))
    .bind(refresh)
    .bind(t.refresh_expires_in.map(|n| n as f64))
    .bind(super::shop::hint(&t.access_token))
    .execute(db)
    .await?;
    Ok(())
}

/// The store's access token, refreshed first when it's about to expire. One refresh at a time per
/// store: the claim is a conditional update, and the network call happens with no lock held.
pub async fn access_token(state: &AppState, shop: &ShopRow, force_refresh: bool) -> Result<String, ApiError> {
    let key = &state.cfg.secret_key;
    let current = shop.access_token_enc.as_deref().ok_or(ApiError::Unauthorized)?;
    let expiring = shop.token_expires_at.is_some_and(|t| t < Utc::now() + chrono::Duration::seconds(60));
    if !(expiring || force_refresh) || shop.refresh_token_enc.is_none() {
        return crate::crypto::decrypt(key, current).map_err(|_| ApiError::Unauthorized);
    }
    let claimed: Option<(Option<String>, Option<String>, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
        "UPDATE shopify_shops SET refreshing_until = now() + interval '30 seconds'
         WHERE account_id = $1 AND status = 'connected' AND (refreshing_until IS NULL OR refreshing_until < now())
         RETURNING refresh_token_enc, access_token_enc, token_expires_at",
    )
    .bind(shop.account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| ApiError::Network("database"))?;
    let Some((Some(refresh_enc), Some(latest_enc), latest_exp)) = claimed else { return Err(ApiError::Busy) };
    // Someone else refreshed between our read and our claim: use theirs.
    if !force_refresh && latest_exp.is_some_and(|t| t > Utc::now() + chrono::Duration::seconds(60)) {
        let _ = sqlx::query("UPDATE shopify_shops SET refreshing_until = NULL WHERE account_id = $1").bind(shop.account_id).execute(&state.db).await;
        return crate::crypto::decrypt(key, &latest_enc).map_err(|_| ApiError::Unauthorized);
    }
    let refresh = crate::crypto::decrypt(key, &refresh_enc).map_err(|_| ApiError::Unauthorized)?;
    match token_request(state, &shop.shop_domain, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)]).await {
        Ok(t) => {
            save_tokens(&state.db, key, shop.account_id, &t).await.map_err(|_| ApiError::Network("database"))?;
            Ok(t.access_token)
        }
        Err(ApiError::Unauthorized) => {
            sqlx::query("UPDATE shopify_shops SET refreshing_until = NULL, last_error = $2, last_error_at = now() WHERE account_id = $1")
                .bind(shop.account_id)
                .bind("Shopify stopped accepting DumbMail's access to your store. Connect it again in Settings → Shopify.")
                .execute(&state.db)
                .await
                .map_err(|_| ApiError::Network("database"))?;
            Err(ApiError::Unauthorized)
        }
        Err(e) => {
            let _ = sqlx::query("UPDATE shopify_shops SET refreshing_until = NULL WHERE account_id = $1").bind(shop.account_id).execute(&state.db).await;
            Err(e)
        }
    }
}

/// Run a GraphQL Admin API query or mutation. Returns `data`. User errors are left for the caller.
pub async fn graphql(state: &AppState, shop: &ShopRow, query: &str, variables: Value) -> Result<Value, ApiError> {
    let mut refreshed = false;
    loop {
        let token = access_token(state, shop, refreshed).await?;
        let res = client()
            .post(format!("{}/admin/api/{API_VERSION}/graphql.json", base(state, &shop.shop_domain)))
            .header("x-shopify-access-token", token)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(json!({"query": query, "variables": variables}).to_string())
            .send()
            .await
            .map_err(net)?;
        let status = res.status().as_u16();
        let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        let bytes = read_capped(res, MAX_RESPONSE).await?;
        match status {
            200 => {}
            401 if !refreshed => {
                refreshed = true;
                continue;
            }
            401 => return Err(ApiError::Unauthorized),
            402 => return Err(ApiError::Forbidden("the store is frozen or unpaid".into())),
            403 => return Err(ApiError::Forbidden("access".into())),
            429 => return Err(ApiError::Throttled(retry_after.map(|s| s.ceil() as u64).unwrap_or(2).clamp(1, 60))),
            s => return Err(ApiError::Http(s)),
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ApiError::Graph("unreadable response".into()))?;
        if let Some(errs) = v["errors"].as_array().filter(|e| !e.is_empty()) {
            let code = errs[0]["extensions"]["code"].as_str().unwrap_or("");
            if code == "THROTTLED" {
                return Err(ApiError::Throttled(throttle_wait(&v)));
            }
            if code == "ACCESS_DENIED" {
                return Err(ApiError::Forbidden("access denied".into()));
            }
            return Err(ApiError::Graph(if code.is_empty() { "query error".into() } else { code.to_ascii_lowercase() }));
        }
        return Ok(v["data"].clone());
    }
}

/// Seconds until the bucket holds what the query asked for.
fn throttle_wait(v: &Value) -> u64 {
    let cost = &v["extensions"]["cost"];
    let need = cost["requestedQueryCost"].as_f64().unwrap_or(100.0);
    let have = cost["throttleStatus"]["currentlyAvailable"].as_f64().unwrap_or(0.0);
    let rate = cost["throttleStatus"]["restoreRate"].as_f64().filter(|r| *r > 0.0).unwrap_or(50.0);
    (((need - have).max(0.0) / rate).ceil() as u64).clamp(1, 60)
}

/// The first user error's code (or a generic class), for mutations.
pub fn user_error(payload: &Value) -> Option<String> {
    let errs = payload["userErrors"].as_array()?;
    let e = errs.first()?;
    Some(e["code"].as_str().map(|c| c.to_ascii_lowercase()).unwrap_or_else(|| "invalid".into()))
}

// ---------- Bulk operation result files ----------

/// Hosts Shopify serves bulk operation results from (signed Google Cloud Storage links).
const RESULT_HOSTS: &[&str] = &["storage.googleapis.com"];

/// Is this a result link we may fetch? https on Shopify's storage host only (or the test stand-in).
pub fn result_url_ok(state: &AppState, raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else { return false };
    if !u.username().is_empty() || u.password().is_some() || u.port().is_some_and(|p| p != 443) && u.scheme() == "https" {
        return false;
    }
    if let Some(b) = state.cfg.shopify.shopify_api_base.as_deref().filter(|b| !b.is_empty()) {
        if let Ok(bu) = url::Url::parse(b) {
            if bu.origin() == u.origin() {
                return true;
            }
        }
    }
    u.scheme() == "https" && u.host_str().is_some_and(|h| RESULT_HOSTS.contains(&h))
}

/// One read of a result file from byte `offset`, at most `max` bytes (the caller keeps whole lines
/// and asks again from where it stopped). `Ok(None)`: the link expired or vanished.
pub async fn result_chunk(state: &AppState, url: &str, offset: u64, max: usize) -> Result<Option<(Vec<u8>, bool)>, ApiError> {
    if !result_url_ok(state, url) {
        return Err(ApiError::Graph("unexpected result host".into()));
    }
    let res = client()
        .get(url)
        .header("range", format!("bytes={offset}-{}", offset + max as u64 - 1))
        .send()
        .await
        .map_err(net)?;
    let status = res.status().as_u16();
    match status {
        200 | 206 => {}
        416 => return Ok(Some((Vec::new(), true))),
        403 | 404 | 410 => return Ok(None),
        s => return Err(ApiError::Http(s)),
    }
    // A server that ignores Range sends the whole file: skip to the offset ourselves, still capped.
    let full = status == 200;
    let total = res
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|n| n.parse::<u64>().ok());
    let mut out = Vec::new();
    let mut seen: u64 = 0;
    let mut res = res;
    while let Some(chunk) = res.chunk().await.map_err(net)? {
        let mut c: &[u8] = &chunk;
        if full && seen < offset {
            let skip = ((offset - seen) as usize).min(c.len());
            seen += skip as u64;
            c = &c[skip..];
        }
        let room = max - out.len();
        out.extend_from_slice(&c[..c.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    let end = match total {
        Some(t) => offset + out.len() as u64 >= t,
        None => out.len() < max,
    };
    Ok(Some((out, end)))
}

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::AppError;
use crate::features::team::access::{Perm, Restriction, Role};
use crate::features::team::guard::{Authorized, RouteAccess};
use crate::features::team::login::ReqMeta;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "pl_session";
pub const API_KEY_PREFIX: &str = "pl_sk_";

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash failed: {e}"))
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// The signed-in account (and user, for browser sessions).
///
/// Permissions are checked before any handler runs, by `features::team::guard::layer`, for every
/// route. Extracting `Ctx` hands over the caller the guard already checked; on a public route it
/// says who's signed in (if anyone) without granting anything.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub account_id: Uuid,
    pub user_id: Option<Uuid>,
    pub via_api_key: bool,
    /// The member's role (for an API key: the role of the member who made it).
    pub role: Role,
    pub api_key_id: Option<Uuid>,
    /// The API key's scopes (None for browser sessions).
    pub scopes: Option<Vec<String>>,
    pub session_id: Option<Uuid>,
    /// Signed in through this account's single sign-on (trusted for this account only).
    pub sso_session: bool,
    /// Set while the session still has to finish two-factor or single sign-on.
    pub restriction: Option<Restriction>,
    /// When this browser session last re-entered its password or a two-factor code.
    pub reauth_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl Ctx {
    /// Whether this caller holds `perm`.
    pub fn can(&self, perm: Perm) -> bool {
        crate::features::team::access::allows(self, perm)
    }

    /// Refuse unless this caller holds `perm` (for checks inside a handler or tool).
    pub fn require(&self, perm: Perm) -> Result<(), AppError> {
        crate::features::team::access::require(self, perm)
    }

    /// Full access to one account with no person behind it, for background work.
    pub fn system(account_id: Uuid) -> Ctx {
        Ctx {
            account_id,
            user_id: None,
            via_api_key: false,
            role: Role::Owner,
            api_key_id: None,
            scopes: None,
            session_id: None,
            sso_session: false,
            restriction: None,
            reauth_at: None,
            ip: None,
            user_agent: None,
        }
    }
}

/// Who is calling, before any permission check.
pub async fn resolve(parts: &Parts, state: &AppState) -> Result<Ctx, AppError> {
    let meta = ReqMeta::from_headers(&parts.headers);
    if let Some(h) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        let v = h.to_str().map_err(|_| AppError::Unauthorized)?;
        let key = v.strip_prefix("Bearer ").ok_or(AppError::Unauthorized)?.trim();
        return crate::features::team::login::key_ctx(state, key, &meta).await;
    }
    let jar = axum_extra::extract::CookieJar::from_headers(&parts.headers);
    let token = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()).ok_or(AppError::Unauthorized)?;
    crate::features::team::login::session_ctx(state, &token, &meta).await
}

impl FromRequestParts<AppState> for Ctx {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(Authorized(ctx)) = parts.extensions.get::<Authorized>() {
            return Ok(ctx.clone());
        }
        // Only a public route runs without the guard checking a caller; there the caller is
        // looked up but given nothing extra. Anything else that got here unchecked is refused.
        match parts.extensions.get::<RouteAccess>() {
            Some(RouteAccess(rule)) if rule.perm == Perm::Public => resolve(parts, state).await,
            _ => Err(AppError::Unauthorized),
        }
    }
}

pub async fn ctx_from_api_key(state: &AppState, key: &str) -> Result<Ctx, AppError> {
    crate::features::team::login::key_ctx(state, key, &ReqMeta::default()).await
}

/// Short HMAC-signed tokens for tracking pixels, click redirects and unsubscribe links.
pub fn sign(secret: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(payload.as_bytes());
    let sig = mac.finalize().into_bytes();
    format!("{payload}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sig[..12]))
}

pub fn verify(secret: &str, token: &str) -> Option<String> {
    let (payload, _) = token.rsplit_once('.')?;
    let expected = sign(secret, payload);
    // constant-time compare
    if expected.len() == token.len()
        && expected.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    {
        Some(payload.to_string())
    } else {
        None
    }
}

/// Sign a URL for click tracking: payload is message id + url hash.
pub fn link_sig(secret: &str, message_id: Uuid, url: &str) -> String {
    let s = sign(secret, &format!("{}:{}", message_id.simple(), url));
    s.rsplit_once('.').map(|(_, sig)| sig.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies() {
        let t = sign("k", "abc");
        assert_eq!(verify("k", &t).as_deref(), Some("abc"));
        assert!(verify("other", &t).is_none());
        assert!(verify("k", "abc.zzzz").is_none());
    }

    #[test]
    fn passwords() {
        let h = hash_password("hunter22").unwrap();
        assert!(verify_password("hunter22", &h));
        assert!(!verify_password("nope", &h));
    }
}

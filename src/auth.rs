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
#[derive(Debug, Clone)]
pub struct Ctx {
    pub account_id: Uuid,
    pub user_id: Option<Uuid>,
    pub via_api_key: bool,
}

impl FromRequestParts<AppState> for Ctx {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(h) = parts.headers.get(axum::http::header::AUTHORIZATION) {
            let v = h.to_str().map_err(|_| AppError::Unauthorized)?;
            let key = v.strip_prefix("Bearer ").ok_or(AppError::Unauthorized)?.trim();
            return ctx_from_api_key(state, key).await;
        }
        let jar = axum_extra::extract::CookieJar::from_headers(&parts.headers);
        let token = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()).ok_or(AppError::Unauthorized)?;
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT user_id, account_id FROM sessions WHERE token_hash = $1 AND expires_at > now()",
        )
        .bind(sha256_hex(&token))
        .fetch_optional(&state.db)
        .await?;
        let (user_id, account_id) = row.ok_or(AppError::Unauthorized)?;
        Ok(Ctx { account_id, user_id: Some(user_id), via_api_key: false })
    }
}

pub async fn ctx_from_api_key(state: &AppState, key: &str) -> Result<Ctx, AppError> {
    if !key.starts_with(API_KEY_PREFIX) {
        return Err(AppError::Unauthorized);
    }
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE api_keys SET last_used_at = now() WHERE key_hash = $1 AND revoked_at IS NULL RETURNING account_id",
    )
    .bind(sha256_hex(key))
    .fetch_optional(&state.db)
    .await?;
    let (account_id,) = row.ok_or(AppError::Unauthorized)?;
    Ok(Ctx { account_id, user_id: None, via_api_key: true })
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

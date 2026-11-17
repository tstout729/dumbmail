//! Checking that an inbound webhook really came from the provider.
//!
//! - Telnyx signs `{telnyx-timestamp}|{raw body}` with Ed25519 (`telnyx-signature-ed25519`, base64).
//!   The public key is in the Telnyx portal under Keys & Credentials → Public Key. We reject
//!   timestamps more than 5 minutes from now, as Telnyx recommends.
//! - Twilio signs the full URL it called followed by every POST parameter, sorted by name, as
//!   name+value with no separators, with HMAC-SHA1 keyed by the account's auth token
//!   (`X-Twilio-Signature`, base64).
//!
//! Where a key is configured, a webhook that fails verification is rejected. Where none is
//! (existing installs), the text is kept and an opt-out is honored, but it is marked unverified.

use axum::http::HeaderMap;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, Mac};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::phone::ByoCredentials;
use crate::state::AppState;

/// Telnyx's recommended limit on webhook age.
pub const TELNYX_MAX_AGE_SECS: i64 = 300;

/// What a webhook proved about where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// The signature checked out.
    Verified,
    /// No key is configured, so it couldn't be checked.
    NoKey,
}

impl Proof {
    pub fn verified(self) -> bool {
        self == Proof::Verified
    }
}

fn reject() -> AppError {
    AppError::Forbidden("the webhook signature didn't check out".into())
}

/// The account's own provider credentials, when it connected that provider itself.
async fn byo(state: &AppState, account_id: Uuid) -> AppResult<Option<ByoCredentials>> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT sms_mode, sms_byo_credentials FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await?;
    let Some((mode, Some(enc))) = row else { return Ok(None) };
    if mode != "byo" {
        return Ok(None);
    }
    Ok(serde_json::from_str(&crate::crypto::decrypt(&state.cfg.secret_key, &enc)?).ok())
}

/// A webhook on one provider's route for a connection that uses the other provider: refused, never
/// treated as "no key configured".
fn wrong_provider(route: &str, scope: Option<Uuid>) -> AppError {
    tracing::warn!(account = ?scope, route, "rejected a webhook on the route of a provider this connection doesn't use");
    AppError::Forbidden(format!("this texting connection doesn't use {route}"))
}

/// The platform's own texting provider is `route` (or the log provider, which stands in for either).
fn platform_uses(state: &AppState, route: &str) -> bool {
    matches!(state.sms.name(), "log") || state.sms.name() == route
}

/// The Telnyx public key for a webhook: the account's own (when it connected its own Telnyx and
/// the webhook used its URL) or the platform's `TELNYX_PUBLIC_KEY`. Refused when that connection
/// doesn't use Telnyx.
pub async fn telnyx_key(state: &AppState, scope: Option<Uuid>) -> AppResult<Option<String>> {
    if let Some(account_id) = scope {
        match byo(state, account_id).await? {
            Some(ByoCredentials::Telnyx { public_key, .. }) => return Ok(public_key.filter(|k| !k.trim().is_empty())),
            Some(ByoCredentials::Twilio { .. }) => return Err(wrong_provider("telnyx", scope)),
            None => {}
        }
    }
    if !platform_uses(state, "telnyx") {
        return Err(wrong_provider("telnyx", scope));
    }
    Ok(state.cfg.telnyx_public_key.clone().filter(|k| !k.trim().is_empty()))
}

/// The Twilio auth token for a webhook: the account's own (connected Twilio) or the platform's
/// `TWILIO_AUTH_TOKEN`. Refused when that connection doesn't use Twilio.
pub async fn twilio_token(state: &AppState, scope: Option<Uuid>) -> AppResult<Option<String>> {
    if let Some(account_id) = scope {
        match byo(state, account_id).await? {
            Some(ByoCredentials::Twilio { auth_token, .. }) => return Ok(Some(auth_token)),
            Some(ByoCredentials::Telnyx { .. }) => return Err(wrong_provider("twilio", scope)),
            None => {}
        }
    }
    if !platform_uses(state, "twilio") {
        return Err(wrong_provider("twilio", scope));
    }
    Ok(state.cfg.twilio_auth_token.clone().filter(|k| !k.trim().is_empty()))
}

/// Check a Telnyx signature. `key` is the base64 public key from the portal.
pub fn telnyx_signature_ok(key: &str, timestamp: &str, signature: &str, body: &[u8], now: i64) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    let Ok(ts) = timestamp.trim().parse::<i64>() else { return false };
    if (now - ts).abs() > TELNYX_MAX_AGE_SECS {
        return false;
    }
    let Some(key) = b64.decode(key.trim()).ok().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key) else { return false };
    let Some(sig) = b64.decode(signature.trim()).ok().and_then(|s| <[u8; 64]>::try_from(s).ok()) else { return false };
    let mut signed = Vec::with_capacity(timestamp.len() + 1 + body.len());
    signed.extend_from_slice(timestamp.trim().as_bytes());
    signed.push(b'|');
    signed.extend_from_slice(body);
    key.verify_strict(&signed, &Signature::from_bytes(&sig)).is_ok()
}

pub async fn telnyx(state: &AppState, scope: Option<Uuid>, headers: &HeaderMap, body: &[u8]) -> AppResult<Proof> {
    let Some(key) = telnyx_key(state, scope).await? else { return Ok(Proof::NoKey) };
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if telnyx_signature_ok(&key, h("telnyx-timestamp"), h("telnyx-signature-ed25519"), body, chrono::Utc::now().timestamp()) {
        Ok(Proof::Verified)
    } else {
        tracing::warn!(account = ?scope, "rejected a Telnyx webhook with a bad or old signature");
        Err(reject())
    }
}

/// Twilio's signature over one exact URL and the POST parameters.
pub fn twilio_signature_ok(token: &str, url: &str, params: &[(String, String)], signature: &str) -> bool {
    let Ok(expected) = base64::engine::general_purpose::STANDARD.decode(signature.trim()) else { return false };
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort();
    let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(token.as_bytes()) else { return false };
    mac.update(url.as_bytes());
    for (k, v) in sorted {
        mac.update(k.as_bytes());
        mac.update(v.as_bytes());
    }
    // Constant-time comparison.
    mac.verify_slice(&expected).is_ok()
}

/// The URLs Twilio may have called for this request: the host it reached (as the proxy reports it)
/// and the configured public addresses, each with the exact path and query string.
pub fn twilio_candidate_urls(state: &AppState, headers: &HeaderMap, path_and_query: &str) -> Vec<String> {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty());
    let mut bases = Vec::new();
    if let Some(host) = h("x-forwarded-host").or_else(|| h("host")) {
        match h("x-forwarded-proto") {
            Some(proto) => bases.push(format!("{proto}://{host}")),
            None => {
                bases.push(format!("https://{host}"));
                bases.push(format!("http://{host}"));
            }
        }
    }
    for b in [Some(state.cfg.public_url()), state.cfg.site_url()].into_iter().flatten() {
        bases.push(b.trim_end_matches('/').to_string());
    }
    let mut out: Vec<String> = Vec::new();
    for b in bases {
        let u = format!("{b}{path_and_query}");
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

pub async fn twilio(state: &AppState, scope: Option<Uuid>, headers: &HeaderMap, path_and_query: &str, params: &[(String, String)]) -> AppResult<Proof> {
    let Some(token) = twilio_token(state, scope).await? else { return Ok(Proof::NoKey) };
    let sig = headers.get("x-twilio-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !sig.is_empty() && twilio_candidate_urls(state, headers, path_and_query).iter().any(|u| twilio_signature_ok(&token, u, params, sig)) {
        Ok(Proof::Verified)
    } else {
        tracing::warn!(account = ?scope, "rejected a Twilio webhook with a bad or missing signature");
        Err(reject())
    }
}

/// Whether inbound texts for this account can be verified, and where the key comes from.
pub async fn status(state: &AppState, account_id: Uuid) -> AppResult<serde_json::Value> {
    let (configured, provider, how) = match byo(state, account_id).await? {
        Some(ByoCredentials::Telnyx { public_key, .. }) => (
            public_key.is_some_and(|k| !k.trim().is_empty()),
            "telnyx",
            "Paste your Telnyx public key below. Find it in the Telnyx portal under Keys & Credentials → Public Key.",
        ),
        Some(ByoCredentials::Twilio { .. }) => (true, "twilio", "Checked with the auth token you connected."),
        None => match state.sms.name() {
            "twilio" => (state.cfg.twilio_auth_token.is_some(), "twilio", "The server operator sets TWILIO_AUTH_TOKEN (Twilio Console → Account Info → Auth Token)."),
            "telnyx" => (
                state.cfg.telnyx_public_key.is_some(),
                "telnyx",
                "The server operator sets TELNYX_PUBLIC_KEY (Telnyx portal → Keys & Credentials → Public Key).",
            ),
            other => (false, if other == "log" { "log" } else { "telnyx" }, "This server isn't connected to a texting provider, so inbound texts can't be verified."),
        },
    };
    Ok(serde_json::json!({"configured": configured, "provider": provider, "how": how}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn telnyx_signatures() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let key = b64.encode(sk.verifying_key().to_bytes());
        let body = br#"{"data":{"event_type":"message.received"}}"#;
        let now = 1_800_000_000;
        let sign = |ts: i64, body: &[u8]| {
            let mut m = format!("{ts}|").into_bytes();
            m.extend_from_slice(body);
            b64.encode(sk.sign(&m).to_bytes())
        };
        assert!(telnyx_signature_ok(&key, &now.to_string(), &sign(now, body), body, now));
        assert!(telnyx_signature_ok(&key, &(now - 299).to_string(), &sign(now - 299, body), body, now));
        assert!(!telnyx_signature_ok(&key, &(now - 301).to_string(), &sign(now - 301, body), body, now), "too old");
        assert!(!telnyx_signature_ok(&key, &now.to_string(), &sign(now, body), br#"{"data":{"event_type":"message.received","x":1}}"#, now), "body changed");
        assert!(!telnyx_signature_ok(&key, &(now + 1).to_string(), &sign(now, body), body, now), "timestamp changed");
        let other = b64.encode(SigningKey::from_bytes(&[8u8; 32]).verifying_key().to_bytes());
        assert!(!telnyx_signature_ok(&other, &now.to_string(), &sign(now, body), body, now), "wrong key");
        assert!(!telnyx_signature_ok(&key, &now.to_string(), "", body, now));
        assert!(!telnyx_signature_ok("not base64", &now.to_string(), &sign(now, body), body, now));
    }

    #[test]
    fn twilio_signatures_match_the_documented_example() {
        // Twilio's documented example (docs/usage/security): token 12345, this URL and these
        // parameters sign to 0/KCTR6DLpKmkAf8muzZqo1nDgQ=.
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let params: Vec<(String, String)> = [("CallSid", "CA1234567890ABCDE"), ("Caller", "+12349013030"), ("Digits", "1234"), ("From", "+12349013030"), ("To", "+18005551212")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(twilio_signature_ok("12345", url, &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!twilio_signature_ok("12346", url, &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!twilio_signature_ok("12345", "https://mycompany.com/myapp.php?foo=1&bar=3", &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        let mut changed = params.clone();
        changed[2].1 = "9999".into();
        assert!(!twilio_signature_ok("12345", url, &changed, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!twilio_signature_ok("12345", url, &params, ""));
    }
}

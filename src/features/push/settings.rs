//! An account's push setup: web push (VAPID keys, the service worker's path, the in-page prompt),
//! APNs and FCM credentials, and the enrollment secret. Secrets are encrypted with `crate::crypto`
//! and never returned; the API shows only whether each is set and a short hint.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::crypto;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The one scope DumbMail's worker is registered with: a folder of its own, so it controls no
/// pages and never touches a site's own worker.
pub const SW_SCOPE: &str = "/dumbmail-push/";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub account_id: Uuid,
    pub web_enabled: bool,
    pub vapid_public_key: Option<String>,
    pub vapid_private_enc: Option<String>,
    pub sw_path: String,
    pub prompt: Value,
    pub apns_key_id: Option<String>,
    pub apns_team_id: Option<String>,
    pub apns_bundle_id: Option<String>,
    pub apns_key_enc: Option<String>,
    pub apns_production: bool,
    pub apns_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub credentials_version: i64,
    pub fcm_project_id: Option<String>,
    pub fcm_client_email: Option<String>,
    pub fcm_credentials_enc: Option<String>,
    pub identity_secret_enc: Option<String>,
    pub identity_secret_hint: Option<String>,
    pub apns_error: Option<String>,
    pub fcm_error: Option<String>,
}

impl Row {
    pub fn web_ready(&self) -> bool {
        self.web_enabled && self.vapid_public_key.is_some() && self.vapid_private_enc.is_some()
    }
    pub fn apns_ready(&self) -> bool {
        self.apns_key_enc.is_some() && self.apns_key_id.is_some() && self.apns_team_id.is_some() && self.apns_bundle_id.is_some()
    }
    pub fn fcm_ready(&self) -> bool {
        self.fcm_credentials_enc.is_some() && self.fcm_project_id.is_some()
    }
    /// The device kinds this account can send to now.
    pub fn ready_kinds(&self) -> Vec<&'static str> {
        let mut k = Vec::new();
        if self.web_ready() {
            k.push("web");
        }
        if self.apns_ready() {
            k.push("ios");
        }
        if self.fcm_ready() {
            k.push("android");
        }
        k
    }
    pub fn prompt(&self) -> Prompt {
        serde_json::from_value(self.prompt.clone()).unwrap_or_default()
    }
}

pub async fn load(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<Row>> {
    sqlx::query_as("SELECT * FROM push_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await
}

/// The in-page card shown before the browser's own permission prompt. The browser prompt only ever
/// opens from a click on "Allow" here (or the site's own button calling `dumbmail.notifications.prompt()`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Prompt {
    /// Show the card automatically by the rules below. Off: only the site's own button asks.
    pub enabled: bool,
    pub title: String,
    pub body: String,
    pub allow_text: String,
    pub deny_text: String,
    /// Pages seen in this visit before it shows.
    pub page_views: u32,
    /// Seconds on the page before it shows.
    pub delay_seconds: u32,
    /// Only on pages whose address contains one of these (empty: every page).
    pub url_contains: Vec<String>,
    /// Never on pages whose address contains one of these.
    pub url_excludes: Vec<String>,
    /// After "Not now", wait this many days before showing it again.
    pub frequency_days: u32,
    pub position: String,
}

impl Default for Prompt {
    fn default() -> Self {
        Prompt {
            enabled: true,
            title: "Get our news first".into(),
            body: "Allow notifications for new arrivals and offers. You can turn them off anytime.".into(),
            allow_text: "Allow".into(),
            deny_text: "Not now".into(),
            page_views: 2,
            delay_seconds: 5,
            url_contains: Vec::new(),
            url_excludes: Vec::new(),
            frequency_days: 7,
            position: "bottom".into(),
        }
    }
}

fn check_prompt(p: &Prompt) -> AppResult<()> {
    let text = |name: &str, v: &str, max: usize| -> AppResult<()> {
        if v.trim().is_empty() || v.chars().count() > max {
            return Err(AppError::bad(format!("the prompt's {name} must be 1 to {max} characters")));
        }
        Ok(())
    };
    text("title", &p.title, 80)?;
    text("message", &p.body, 300)?;
    text("allow button", &p.allow_text, 30)?;
    text("dismiss button", &p.deny_text, 30)?;
    if !(1..=50).contains(&p.page_views) {
        return Err(AppError::bad("page_views must be 1 to 50"));
    }
    if p.delay_seconds > 600 {
        return Err(AppError::bad("delay_seconds must be 0 to 600"));
    }
    if p.frequency_days > 365 {
        return Err(AppError::bad("frequency_days must be 0 to 365"));
    }
    for list in [&p.url_contains, &p.url_excludes] {
        if list.len() > 20 || list.iter().any(|u| u.trim().is_empty() || u.chars().count() > 200) {
            return Err(AppError::bad("page rules can have up to 20 entries of 1 to 200 characters"));
        }
    }
    if !matches!(p.position.as_str(), "top" | "bottom") {
        return Err(AppError::bad("position must be top or bottom"));
    }
    Ok(())
}

fn check_sw_path(p: &str) -> AppResult<()> {
    let ok = p.starts_with('/')
        && !p.starts_with("//")
        && p.len() <= 200
        && p.ends_with(".js")
        && !p.contains("..")
        && p.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c));
    if ok { Ok(()) } else { Err(AppError::bad("the service worker path must be a path on your site ending in .js, like /dumbmail-sw.js")) }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApnsBody {
    pub key_id: String,
    pub team_id: String,
    pub bundle_id: String,
    #[serde(default = "yes")]
    pub production: bool,
    /// The .p8 file's contents. Leave out to keep the saved key (e.g. to switch to sandbox).
    #[serde(default)]
    pub key_p8: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FcmBody {
    /// The Firebase service account key file (JSON), as text.
    pub service_account_json: String,
}

/// Missing: leave as is. `null`: remove. A value: set it.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub web_enabled: Option<bool>,
    pub sw_path: Option<String>,
    pub prompt: Option<Prompt>,
    #[serde(default, deserialize_with = "present")]
    pub apns: Option<Option<ApnsBody>>,
    #[serde(default, deserialize_with = "present")]
    pub fcm: Option<Option<FcmBody>>,
}

/// The pieces of a service account key we use. Anything else in the file is ignored.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServiceAccount {
    #[serde(rename = "type", default)]
    pub kind: String,
    pub project_id: String,
    pub private_key_id: String,
    pub private_key: String,
    pub client_email: String,
    #[serde(default)]
    pub token_uri: Option<String>,
}

pub const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

fn parse_service_account(raw: &str) -> AppResult<ServiceAccount> {
    if raw.len() > 20_000 {
        return Err(AppError::bad("that service account file is too large"));
    }
    let sa: ServiceAccount = serde_json::from_str(raw.trim()).map_err(|_| AppError::bad("paste the whole service account key file (JSON) from Firebase → Project settings → Service accounts"))?;
    if sa.kind != "service_account" {
        return Err(AppError::bad("that file isn't a service account key (its type should be service_account)"));
    }
    let id_ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c));
    if !id_ok(&sa.project_id, 100) || !id_ok(&sa.client_email, 200) || !sa.client_email.contains('@') || !id_ok(&sa.private_key_id, 100) {
        return Err(AppError::bad("the service account file is missing its project_id, client_email or private_key_id"));
    }
    // Credentials only ever go to Google's own token endpoint.
    if sa.token_uri.as_deref().is_some_and(|u| u != GOOGLE_TOKEN_URI) {
        return Err(AppError::bad("the service account's token_uri must be Google's (https://oauth2.googleapis.com/token)"));
    }
    if !crypto::check_rsa_pem(&sa.private_key) {
        return Err(AppError::bad("the service account's private_key couldn't be read"));
    }
    Ok(sa)
}

fn check_apns(b: &ApnsBody) -> AppResult<()> {
    let ten = |s: &str| s.len() == 10 && s.chars().all(|c| c.is_ascii_alphanumeric());
    if !ten(b.key_id.trim()) {
        return Err(AppError::bad("the APNs key ID is the 10-character ID shown next to the key in your Apple developer account"));
    }
    if !ten(b.team_id.trim()) {
        return Err(AppError::bad("the team ID is the 10-character ID in your Apple developer account's membership details"));
    }
    let bundle = b.bundle_id.trim();
    if bundle.is_empty() || bundle.len() > 155 || !bundle.contains('.') || !bundle.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return Err(AppError::bad("the bundle ID looks like com.yourbrand.app"));
    }
    if let Some(k) = &b.key_p8 {
        if k.len() > 5000 || !crypto::check_p8(k) {
            return Err(AppError::bad("that isn't an APNs auth key: paste the whole .p8 file, including the BEGIN and END lines"));
        }
    }
    Ok(())
}

fn subject(state: &AppState) -> String {
    match state.cfg.support_email.as_deref() {
        Some(e) if e.contains('@') => format!("mailto:{e}"),
        _ => state.cfg.public_url().to_string(),
    }
}

/// The VAPID subject push services can contact about our traffic.
pub fn vapid_subject(state: &AppState) -> String {
    subject(state)
}

/// Make sure the account has a settings row and VAPID keys (generated once, kept for good: new keys
/// would cut off every browser that already subscribed).
pub async fn ensure(state: &AppState, account_id: Uuid) -> AppResult<Row> {
    sqlx::query("INSERT INTO push_settings (account_id) VALUES ($1) ON CONFLICT DO NOTHING").bind(account_id).execute(&state.db).await?;
    let (public, private) = crypto::new_vapid_keys();
    let enc = crate::crypto::encrypt(&state.cfg.secret_key, &private).map_err(AppError::Internal)?;
    sqlx::query("UPDATE push_settings SET vapid_public_key = $2, vapid_private_enc = $3 WHERE account_id = $1 AND vapid_public_key IS NULL")
        .bind(account_id)
        .bind(&public)
        .bind(&enc)
        .execute(&state.db)
        .await?;
    load(&state.db, account_id).await?.ok_or_else(|| AppError::not_found("push settings"))
}

/// What changed, for the audit log (names of settings, never values).
pub async fn update(state: &AppState, account_id: Uuid, b: SettingsBody) -> AppResult<Vec<&'static str>> {
    if let Some(p) = &b.sw_path {
        check_sw_path(p)?;
    }
    if let Some(p) = &b.prompt {
        check_prompt(p)?;
    }
    let mut changed = Vec::new();
    let current = ensure(state, account_id).await?;
    let mut tx = state.db.begin().await?;
    if b.web_enabled.is_some() || b.sw_path.is_some() || b.prompt.is_some() {
        sqlx::query("UPDATE push_settings SET web_enabled = COALESCE($2, web_enabled), sw_path = COALESCE($3, sw_path), prompt = COALESCE($4, prompt), updated_at = now() WHERE account_id = $1")
            .bind(account_id)
            .bind(b.web_enabled)
            .bind(&b.sw_path)
            .bind(b.prompt.as_ref().map(serde_json::to_value).transpose()?)
            .execute(&mut *tx)
            .await?;
        changed.push("web");
    }
    match b.apns {
        None => {}
        Some(None) => {
            sqlx::query("UPDATE push_settings SET apns_key_id = NULL, apns_team_id = NULL, apns_bundle_id = NULL, apns_key_enc = NULL, apns_error = NULL, apns_verified_at = NULL, credentials_version = credentials_version + 1, updated_at = now() WHERE account_id = $1")
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
            changed.push("apns_removed");
        }
        Some(Some(a)) => {
            check_apns(&a)?;
            if a.key_p8.is_none() && current.apns_key_enc.is_none() {
                return Err(AppError::bad("add the .p8 auth key file"));
            }
            let enc = a.key_p8.as_deref().map(|k| crate::crypto::encrypt(&state.cfg.secret_key, k.trim())).transpose().map_err(AppError::Internal)?;
            sqlx::query(
                "UPDATE push_settings SET apns_key_id = $2, apns_team_id = $3, apns_bundle_id = $4, apns_production = $5, apns_key_enc = COALESCE($6, apns_key_enc), apns_error = NULL, apns_verified_at = NULL, credentials_version = credentials_version + 1, updated_at = now()
                 WHERE account_id = $1",
            )
            .bind(account_id)
            .bind(a.key_id.trim())
            .bind(a.team_id.trim())
            .bind(a.bundle_id.trim())
            .bind(a.production)
            .bind(enc)
            .execute(&mut *tx)
            .await?;
            changed.push("apns");
        }
    }
    match b.fcm {
        None => {}
        Some(None) => {
            sqlx::query("UPDATE push_settings SET fcm_project_id = NULL, fcm_client_email = NULL, fcm_credentials_enc = NULL, fcm_error = NULL, credentials_version = credentials_version + 1, updated_at = now() WHERE account_id = $1")
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
            changed.push("fcm_removed");
        }
        Some(Some(f)) => {
            let sa = parse_service_account(&f.service_account_json)?;
            let enc = crate::crypto::encrypt(&state.cfg.secret_key, &serde_json::to_string(&sa)?).map_err(AppError::Internal)?;
            sqlx::query("UPDATE push_settings SET fcm_project_id = $2, fcm_client_email = $3, fcm_credentials_enc = $4, fcm_error = NULL, credentials_version = credentials_version + 1, updated_at = now() WHERE account_id = $1")
                .bind(account_id)
                .bind(&sa.project_id)
                .bind(&sa.client_email)
                .bind(enc)
                .execute(&mut *tx)
                .await?;
            changed.push("fcm");
        }
    }
    tx.commit().await?;
    // New credentials: new connections and tokens (Apple binds a connection to one team's key).
    if changed.iter().any(|c| c.starts_with("apns") || c.starts_with("fcm")) {
        super::providers::forget_account(account_id).await;
    }
    Ok(changed)
}

/// A new enrollment secret, returned once (only its hint is kept in view). Signs enrollment grants.
pub async fn rotate_identity_secret(state: &AppState, account_id: Uuid) -> AppResult<String> {
    ensure(state, account_id).await?;
    let mut raw = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut raw);
    let secret = format!("dmps_{}", hex::encode(raw));
    let enc = crate::crypto::encrypt(&state.cfg.secret_key, &secret).map_err(AppError::Internal)?;
    let hint = format!("…{}", &secret[secret.len() - 4..]);
    sqlx::query("UPDATE push_settings SET identity_secret_enc = $2, identity_secret_hint = $3, updated_at = now() WHERE account_id = $1")
        .bind(account_id)
        .bind(enc)
        .bind(hint)
        .execute(&state.db)
        .await?;
    Ok(secret)
}

pub fn identity_secret(state: &AppState, row: &Row) -> Option<String> {
    row.identity_secret_enc.as_deref().and_then(|e| crate::crypto::decrypt(&state.cfg.secret_key, e).ok())
}

/// The settings as the API shows them (no secrets).
pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let row = ensure(state, account_id).await?;
    let key: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let base = state.cfg.public_url();
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE kind = 'web' AND profile_id IS NOT NULL), count(*) FILTER (WHERE kind = 'ios' AND profile_id IS NOT NULL),
                count(*) FILTER (WHERE kind = 'android' AND profile_id IS NOT NULL), count(*) FILTER (WHERE anonymous),
                count(*) FILTER (WHERE profile_id IS NOT NULL AND permission = 'denied')
         FROM push_devices WHERE account_id = $1 AND status = 'active'",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let reachable = super::send::reachable_count(&state.db, account_id).await?;
    let opted_out: i64 = sqlx::query_scalar("SELECT count(*) FROM push_opt_outs WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    Ok(json!({
        "web": {
            "enabled": row.web_enabled,
            "ready": row.web_ready(),
            "vapid_public_key": row.vapid_public_key,
            "sw_path": row.sw_path,
            "sw_scope": SW_SCOPE,
            "prompt": row.prompt(),
            "service_worker_url": format!("{base}/push/sw.js?k={key}"),
        },
        "apns": {
            "configured": row.apns_ready(),
            "key_id": row.apns_key_id,
            "team_id": row.apns_team_id,
            "bundle_id": row.apns_bundle_id,
            "production": row.apns_production,
            "verified": row.apns_verified_at.is_some(),
            "error": row.apns_error,
        },
        "fcm": {
            "configured": row.fcm_ready(),
            "project_id": row.fcm_project_id,
            "client_email": row.fcm_client_email,
            "error": row.fcm_error,
        },
        "identity": {"configured": row.identity_secret_enc.is_some(), "hint": row.identity_secret_hint},
        "account_id": account_id,
        "public_key": key,
        "register_url": format!("{base}/client/push/devices?k={key}"),
        "devices": {"web": counts.0, "ios": counts.1, "android": counts.2, "anonymous": counts.3, "permission_denied": counts.4},
        "reachable_people": reachable,
        "opted_out": opted_out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_prompts() {
        assert!(check_sw_path("/dumbmail-sw.js").is_ok());
        assert!(check_sw_path("//evil.test/x.js").is_err());
        assert!(check_sw_path("/../x.js").is_err());
        assert!(check_sw_path("/sw.php").is_err());
        assert!(check_prompt(&Prompt::default()).is_ok());
        assert!(check_prompt(&Prompt { page_views: 0, ..Default::default() }).is_err());
        assert!(check_prompt(&Prompt { title: "".into(), ..Default::default() }).is_err());
    }

    #[test]
    fn service_account_token_uri_is_google_only() {
        let bad = json!({"type": "service_account", "project_id": "p", "private_key_id": "k", "private_key": "x", "client_email": "a@p.iam.gserviceaccount.com", "token_uri": "https://evil.test/token"});
        assert!(parse_service_account(&bad.to_string()).unwrap_err().public_message().contains("token_uri"));
    }
}

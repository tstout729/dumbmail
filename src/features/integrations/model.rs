//! Stored integrations: creating, updating and showing them (secrets are write-only).

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::FromRow;
use uuid::Uuid;

use super::pull::PullConfig;
use super::sandbox::SCRIPT_MAX_BYTES;
use super::signature::{self, Signature};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Most integrations one account may have.
pub const MAX_PER_ACCOUNT: i64 = 50;
const MAX_CREDENTIALS: usize = 20;

#[derive(Debug, Clone, FromRow)]
pub struct Integration {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub recipe: Option<String>,
    pub status: String,
    pub token_enc: String,
    pub signature: Value,
    pub signature_secret_enc: Option<String>,
    pub signature_secret_hint: Option<String>,
    pub script: String,
    pub settings: Value,
    pub pull: Option<Value>,
    pub pull_next_at: Option<DateTime<Utc>>,
    pub credentials_enc: Option<String>,
    pub credential_hints: Value,
    pub oauth_token_expires_at: Option<DateTime<Utc>>,
    pub auth_version: i64,
    pub contacts_opted_in: bool,
    pub message_id: Option<Value>,
    pub metrics: Vec<String>,
    pub redact: Vec<String>,
    pub state: Value,
    pub backfill: Option<Value>,
    pub deliveries_ok: i64,
    pub deliveries_failed: i64,
    pub last_delivery_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const COLS: &str = "id, account_id, name, recipe, status, token_enc, signature, signature_secret_enc, signature_secret_hint, script, settings, pull, pull_next_at, credentials_enc, credential_hints, oauth_token_expires_at, auth_version, contacts_opted_in, message_id, metrics, redact, state, backfill, deliveries_ok, deliveries_failed, last_delivery_at, last_error, last_error_at, created_at, updated_at";

impl Integration {
    /// The saved signature check. Settings that can't be read are an error, never "no check":
    /// every webhook is refused until they're fixed.
    pub fn signature(&self) -> Result<Signature, String> {
        Signature::parse(&self.signature).map_err(|e| e.public_message())
    }

    /// Whether webhooks can be accepted: the check's settings read, and its secret is saved.
    fn signature_ready(&self) -> bool {
        self.signature().is_ok_and(|s| !s.needs_secret() || self.signature_secret_enc.is_some())
    }

    pub fn pull_config(&self) -> Option<PullConfig> {
        self.pull.as_ref().and_then(|p| PullConfig::parse(p).ok())
    }

    pub fn settings_map(&self) -> Map<String, Value> {
        self.settings.as_object().cloned().unwrap_or_default()
    }

    pub fn state_map(&self) -> Map<String, Value> {
        self.state.as_object().cloned().unwrap_or_default()
    }

    pub fn secret(&self, state: &AppState) -> AppResult<Option<String>> {
        self.signature_secret_enc.as_deref().map(|c| crate::crypto::decrypt(&state.cfg.secret_key, c).map_err(AppError::Internal)).transpose()
    }

    pub fn credentials(&self, state: &AppState) -> AppResult<Map<String, Value>> {
        match &self.credentials_enc {
            Some(c) => {
                let text = crate::crypto::decrypt(&state.cfg.secret_key, c)?;
                Ok(serde_json::from_str::<Value>(&text).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default())
            }
            None => Ok(Map::new()),
        }
    }

    /// Every secret value this integration holds (credentials, signing secret, cached access
    /// token), so stored and shown messages can be scrubbed of them.
    pub async fn secret_values(&self, state: &AppState) -> Vec<String> {
        let mut out: Vec<String> = self.credentials(state).map(|c| c.values().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        if let Ok(Some(s)) = self.secret(state) {
            out.push(s);
        }
        let token: Option<Option<String>> = sqlx::query_scalar("SELECT oauth_token_enc FROM integrations WHERE id = $1").bind(self.id).fetch_optional(&state.db).await.ok();
        if let Some(Some(enc)) = token {
            if let Ok(t) = crate::crypto::decrypt(&state.cfg.secret_key, &enc) {
                out.push(t);
            }
        }
        out
    }

    /// Scrub this integration's secret values from a message before it is stored or shown.
    pub async fn redact(&self, state: &AppState, text: &str) -> String {
        super::pull::redact(text, &self.secret_values(state).await)
    }

    /// How long a resend of the same message id counts as one, when the id is limited to a window.
    pub fn dedupe_minutes(&self) -> Option<i64> {
        self.message_id.as_ref().and_then(|m| m["within_minutes"].as_i64())
    }

    /// Where the app's own message id is in a webhook, if set.
    pub fn message_id_of(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> Option<String> {
        let cfg = self.message_id.as_ref()?;
        let v = if let Some(h) = cfg["header"].as_str() {
            headers.get(h).and_then(|v| v.to_str().ok()).map(String::from)
        } else if let Some(f) = cfg["field"].as_str() {
            let body: Value = serde_json::from_slice(body).ok()?;
            let mut cur = &body;
            for part in f.split('.') {
                cur = cur.get(part)?;
            }
            match cur {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        } else {
            None
        };
        v.map(|s| s.trim().chars().take(200).collect::<String>()).filter(|s| !s.is_empty())
    }

    pub fn inbound_url(&self, state: &AppState) -> String {
        let token = crate::crypto::decrypt(&state.cfg.secret_key, &self.token_enc).unwrap_or_default();
        format!("{}/webhooks/in/{token}", state.cfg.public_url())
    }

    /// Credentials the pull needs that aren't saved.
    pub fn missing_credentials(&self) -> Vec<String> {
        let hints = self.credential_hints.as_object();
        self.pull_config()
            .map(|p| p.secret_names().into_iter().filter(|n| !hints.is_some_and(|h| h.contains_key(n))).collect())
            .unwrap_or_default()
    }

    /// The API's view: no secrets, only hints that they're saved.
    pub fn view(&self, state: &AppState) -> Value {
        json!({
            "id": self.id, "name": self.name, "recipe": self.recipe, "status": self.status,
            "inbound_url": self.inbound_url(state),
            "signature": self.signature,
            "signature_secret": self.signature_secret_hint.as_ref().map(|h| json!({"saved": true, "hint": h})).unwrap_or(json!({"saved": false})),
            "signature_ready": self.signature_ready(),
            "signature_error": self.signature().err(),
            "script": self.script, "settings": self.settings, "pull": self.pull,
            "contacts_opted_in": self.contacts_opted_in, "message_id": self.message_id,
            "metrics": self.metrics, "redact": self.redact,
            "pull_next_at": self.pull_next_at,
            "credentials": self.credential_hints,
            "missing_credentials": self.missing_credentials(),
            "oauth_token_cached_until": self.oauth_token_expires_at.filter(|t| *t > Utc::now()),
            "state": self.state, "backfill": self.backfill.as_ref().map(|b| {
                let mut b = b.clone();
                b.as_object_mut().map(|o| o.remove("next_enc"));
                b
            }),
            "deliveries_ok": self.deliveries_ok, "deliveries_failed": self.deliveries_failed,
            "last_delivery_at": self.last_delivery_at, "last_error": self.last_error, "last_error_at": self.last_error_at,
            "created_at": self.created_at, "updated_at": self.updated_at,
        })
    }

    /// The view for people who may see integrations but not change them: no inbound URL (it is a
    /// credential), no saved state or backfill cursor (they can hold people's details).
    pub fn view_limited(&self) -> Value {
        let backfill = self.backfill.as_ref().map(|b| {
            let mut b = b.clone();
            if let Some(o) = b.as_object_mut() {
                o.remove("state");
                o.remove("next_enc");
            }
            b
        });
        json!({
            "id": self.id, "name": self.name, "recipe": self.recipe, "status": self.status,
            "inbound_url": null, "limited": true,
            "signature": self.signature,
            "signature_secret": json!({"saved": self.signature_secret_hint.is_some()}),
            "signature_ready": self.signature_ready(),
            "signature_error": self.signature().err(),
            "script": self.script, "settings": self.settings, "pull": self.pull,
            "contacts_opted_in": self.contacts_opted_in, "message_id": self.message_id, "metrics": self.metrics, "redact": self.redact,
            "pull_next_at": self.pull_next_at,
            "missing_credentials": self.missing_credentials(),
            "backfill": backfill,
            "deliveries_ok": self.deliveries_ok, "deliveries_failed": self.deliveries_failed,
            "last_delivery_at": self.last_delivery_at, "last_error": self.last_error, "last_error_at": self.last_error_at,
            "created_at": self.created_at, "updated_at": self.updated_at,
        })
    }

    /// A short view for lists.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "recipe": self.recipe, "status": self.status,
            "has_pull": self.pull.is_some(), "signature_ready": self.signature_ready(), "deliveries_ok": self.deliveries_ok, "deliveries_failed": self.deliveries_failed,
            "last_delivery_at": self.last_delivery_at, "last_error": self.last_error, "last_error_at": self.last_error_at,
            "created_at": self.created_at,
        })
    }
}

pub async fn load(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Integration> {
    let q = format!("SELECT {COLS} FROM integrations WHERE id = $1 AND account_id = $2");
    sqlx::query_as::<_, Integration>(&q).bind(id).bind(account_id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("integration"))
}

pub async fn load_any(state: &AppState, id: Uuid) -> AppResult<Option<Integration>> {
    let q = format!("SELECT {COLS} FROM integrations WHERE id = $1");
    Ok(sqlx::query_as::<_, Integration>(&q).bind(id).fetch_optional(&state.db).await?)
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Vec<Integration>> {
    let q = format!("SELECT {COLS} FROM integrations WHERE account_id = $1 ORDER BY created_at");
    Ok(sqlx::query_as::<_, Integration>(&q).bind(account_id).fetch_all(&state.db).await?)
}

/// Fields that can be set when creating or updating. Secrets go in, never come out.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changes {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub signature: Option<Value>,
    /// The signing secret (write-only). Empty string removes it.
    #[serde(default)]
    pub signature_secret: Option<String>,
    #[serde(default)]
    pub settings: Option<Value>,
    /// Pull settings; `null` removes the pull.
    #[serde(default, deserialize_with = "some_value")]
    pub pull: Option<Value>,
    /// Credentials by name (write-only). A `null` value removes one; others are kept.
    #[serde(default)]
    pub credentials: Option<Map<String, Value>>,
    /// Saved state for the script (e.g. reset a cursor). Replaces the whole state.
    #[serde(default)]
    pub state: Option<Value>,
    /// "Everyone this integration adds has agreed to get marketing email from us." Default false:
    /// people it creates without an explicit opt-in are held back from marketing.
    #[serde(default)]
    pub contacts_opted_in: Option<bool>,
    /// Where a webhook carries the app's own message id, so resends are ignored:
    /// `{"header": "svix-id"}` or `{"field": "id"}` (dots reach nested keys). `null` removes it.
    #[serde(default, deserialize_with = "some_value")]
    pub message_id: Option<Value>,
    /// The metrics the script may record (replaces the list). They become server-only when saved.
    #[serde(default)]
    pub metrics: Option<Vec<String>>,
    /// Paths removed from payloads before they are stored or shown (replaces the list).
    #[serde(default)]
    pub redact: Option<Vec<String>>,
}

/// Most metrics one integration may declare.
pub const MAX_METRICS: usize = 50;
/// Most redact paths one integration may have.
pub const MAX_REDACT: usize = 50;

fn check_metrics(v: &[String]) -> AppResult<Vec<String>> {
    if v.len() > MAX_METRICS {
        return Err(AppError::bad(format!("an integration can declare up to {MAX_METRICS} metrics")));
    }
    let mut out: Vec<String> = Vec::new();
    for m in v {
        let m = m.trim();
        if m.is_empty() || m.len() > 128 {
            return Err(AppError::bad("each metric name must be 1-128 characters"));
        }
        if crate::audience::is_server_only_metric(m) {
            return Err(AppError::bad(format!("{m:?} is recorded by DumbMail itself; an integration can't record it")));
        }
        if !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    }
    Ok(out)
}

fn check_redact(v: &[String]) -> AppResult<Vec<String>> {
    if v.len() > MAX_REDACT {
        return Err(AppError::bad(format!("an integration can have up to {MAX_REDACT} redact paths")));
    }
    v.iter()
        .map(|p| {
            let p = p.trim();
            super::redact::Path::parse(p).map_err(AppError::BadRequest)?;
            Ok(p.to_string())
        })
        .collect()
}

fn check_message_id(v: &Value) -> AppResult<Option<Value>> {
    if v.is_null() {
        return Ok(None);
    }
    let o = v.as_object().ok_or_else(|| AppError::bad("message_id must be {\"header\": name} or {\"field\": path}"))?;
    let window = o.get("within_minutes");
    if window.is_some_and(|w| w.as_i64().is_none_or(|m| !(1..=1440).contains(&m))) {
        return Err(AppError::bad("message_id within_minutes must be 1 to 1440"));
    }
    let ok = o.len() == 1 + usize::from(window.is_some())
        && match (o.get("header").and_then(|h| h.as_str()), o.get("field").and_then(|f| f.as_str())) {
            (Some(h), None) => axum::http::HeaderName::from_bytes(h.as_bytes()).is_ok() && h.len() <= 100,
            (None, Some(f)) => !f.is_empty() && f.len() <= 100,
            _ => false,
        };
    if !ok {
        return Err(AppError::bad("message_id must be {\"header\": name} or {\"field\": path}"));
    }
    Ok(Some(v.clone()))
}

/// Keep an explicit `null` as `Some(Value::Null)` so it can mean "remove".
fn some_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Ok(Some(Value::deserialize(d)?))
}

fn check_name(n: &str) -> AppResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 100 {
        return Err(AppError::bad("give the integration a name (up to 100 characters)"));
    }
    Ok(n.to_string())
}

fn check_script(s: &str) -> AppResult<()> {
    if s.len() > SCRIPT_MAX_BYTES {
        return Err(AppError::bad(format!("the script is over {} KB", SCRIPT_MAX_BYTES / 1024)));
    }
    Ok(())
}

fn check_object(v: &Value, what: &str, max: usize) -> AppResult<()> {
    if !v.is_object() {
        return Err(AppError::bad(format!("{what} must be an object")));
    }
    if serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) > max {
        return Err(AppError::bad(format!("{what} is over {} KB", max / 1024)));
    }
    Ok(())
}

fn new_token() -> (String, String) {
    let t = format!("in_{}", crate::auth::random_token(32));
    let h = crate::auth::sha256_hex(&t);
    (t, h)
}

pub async fn create(state: &AppState, account_id: Uuid, recipe: Option<&str>, c: Changes) -> AppResult<Integration> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM integrations WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    if count >= MAX_PER_ACCOUNT {
        return Err(AppError::LimitReached(format!("an account can have up to {MAX_PER_ACCOUNT} integrations")));
    }
    let name = check_name(c.name.as_deref().unwrap_or(""))?;
    let (token, hash) = new_token();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO integrations (id, account_id, name, recipe, token_hash, token_enc) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(account_id)
        .bind(&name)
        .bind(recipe)
        .bind(&hash)
        .bind(crate::crypto::encrypt(&state.cfg.secret_key, &token)?)
        .execute(&state.db)
        .await?;
    let rest = Changes { name: None, ..c };
    match update(state, account_id, id, rest).await {
        Ok(i) => Ok(i),
        Err(e) => {
            sqlx::query("DELETE FROM integrations WHERE id = $1").bind(id).execute(&state.db).await?;
            Err(e)
        }
    }
}

pub async fn update(state: &AppState, account_id: Uuid, id: Uuid, c: Changes) -> AppResult<Integration> {
    let cur = load(state, account_id, id).await?;
    let name = c.name.as_deref().map(check_name).transpose()?;
    if let Some(s) = &c.script {
        check_script(s)?;
    }
    let signature = match &c.signature {
        Some(v) => Some(serde_json::to_value(Signature::parse(v)?)?),
        None => None,
    };
    let (secret_enc, secret_hint, set_secret) = match c.signature_secret.as_deref() {
        Some("") => (None, None, true),
        Some(s) if s.len() > 1000 => return Err(AppError::bad("the signing secret is too long")),
        Some(s) => (Some(crate::crypto::encrypt(&state.cfg.secret_key, s.trim())?), Some(signature::hint(s.trim())), true),
        None => (None, None, false),
    };
    if let Some(s) = &c.settings {
        check_object(s, "settings", 32 * 1024)?;
    }
    if let Some(s) = &c.state {
        check_object(s, "state", super::actions::MAX_STATE_BYTES)?;
    }
    let (message_id, set_message_id) = match &c.message_id {
        Some(v) => (check_message_id(v)?, true),
        None => (None, false),
    };
    let metrics = c.metrics.as_deref().map(check_metrics).transpose()?;
    let redact = c.redact.as_deref().map(check_redact).transpose()?;
    let (pull, set_pull) = match &c.pull {
        Some(Value::Null) => (None, true),
        Some(v) => (Some(serde_json::to_value(PullConfig::parse(v)?)?), true),
        None => (None, false),
    };
    let mut creds_changed = false;
    let (creds_enc, hints) = match &c.credentials {
        Some(changes) => {
            let mut creds = cur.credentials(state)?;
            let mut hints = cur.credential_hints.as_object().cloned().unwrap_or_default();
            for (k, v) in changes {
                if k.is_empty() || k.len() > 64 || !k.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
                    return Err(AppError::bad(format!("credential name {k:?} must be 1-64 letters, digits, - or _")));
                }
                // Only a real change counts: re-saving the same value keeps a cached OAuth token.
                match v {
                    Value::Null => {
                        creds_changed |= creds.remove(k).is_some();
                        hints.remove(k);
                    }
                    Value::String(s) if !s.trim().is_empty() && s.len() <= 4000 => {
                        creds_changed |= creds.insert(k.clone(), json!(s.trim())).as_ref() != Some(&json!(s.trim()));
                        hints.insert(k.clone(), json!(signature::hint(s.trim())));
                    }
                    _ => return Err(AppError::bad(format!("credential {k:?} must be a non-empty string (or null to remove it)"))),
                }
            }
            if creds.len() > MAX_CREDENTIALS {
                return Err(AppError::bad(format!("at most {MAX_CREDENTIALS} credentials")));
            }
            let enc = if creds.is_empty() { None } else { Some(crate::crypto::encrypt(&state.cfg.secret_key, &Value::Object(creds).to_string())?) };
            (enc, Some(Value::Object(hints)))
        }
        None => (None, None),
    };
    // The next scheduled pull: soon after pull settings change; none without an interval.
    let next_pull = if set_pull {
        pull.as_ref().and_then(|p| p["interval_minutes"].as_u64()).map(|_| Utc::now() + chrono::Duration::minutes(1))
    } else {
        cur.pull_next_at
    };
    let q = format!(
        "UPDATE integrations SET
            name = COALESCE($3, name), script = COALESCE($4, script), signature = COALESCE($5, signature),
            signature_secret_enc = CASE WHEN $6 THEN $7 ELSE signature_secret_enc END,
            signature_secret_hint = CASE WHEN $6 THEN $8 ELSE signature_secret_hint END,
            settings = COALESCE($9, settings),
            pull = CASE WHEN $10 THEN $11 ELSE pull END, pull_next_at = $12,
            credentials_enc = CASE WHEN $13 THEN $14 ELSE credentials_enc END,
            credential_hints = COALESCE($15, credential_hints),
            oauth_token_enc = CASE WHEN $17 THEN NULL ELSE oauth_token_enc END,
            oauth_token_expires_at = CASE WHEN $17 THEN NULL ELSE oauth_token_expires_at END,
            state = COALESCE($16, state),
            contacts_opted_in = COALESCE($18, contacts_opted_in),
            message_id = CASE WHEN $19 THEN $20 ELSE message_id END,
            auth_version = auth_version + CASE WHEN $21 THEN 1 ELSE 0 END,
            oauth_token_key = CASE WHEN $17 THEN NULL ELSE oauth_token_key END,
            metrics = COALESCE($22, metrics), redact = COALESCE($23, redact),
            updated_at = now()
         WHERE id = $1 AND account_id = $2 RETURNING {COLS}"
    );
    // Changing credentials or the pull (its site or auth) drops the cached OAuth token, and bumps the
    // auth version so a run that started on the old settings can't write its token back.
    let drop_token = creds_changed || (set_pull && cur.pull != pull);
    let bump = drop_token || c.settings.is_some();
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as::<_, Integration>(&q)
        .bind(id)
        .bind(account_id)
        .bind(name)
        .bind(&c.script)
        .bind(signature)
        .bind(set_secret)
        .bind(secret_enc)
        .bind(secret_hint)
        .bind(&c.settings)
        .bind(set_pull)
        .bind(&pull)
        .bind(next_pull)
        .bind(creds_changed)
        .bind(creds_enc)
        .bind(hints)
        .bind(&c.state)
        .bind(drop_token)
        .bind(c.contacts_opted_in)
        .bind(set_message_id)
        .bind(&message_id)
        .bind(bump)
        .bind(&metrics)
        .bind(&redact)
        .fetch_one(&mut *tx)
        .await?;
    // The declared metrics are trusted by flows from the moment they are saved: mark them
    // server-only in the same transaction, so a browser can never send one first.
    mark_server_only(&mut tx, account_id, &row.metrics).await?;
    tx.commit().await?;
    Ok(row)
}

/// Mark metrics server-only (creating them if needed): the public browser endpoint refuses them.
pub async fn mark_server_only(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, account_id: Uuid, names: &[String]) -> sqlx::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = names.iter().map(|_| Uuid::now_v7()).collect();
    sqlx::query(
        "INSERT INTO metrics (id, account_id, name, server_only) SELECT i, $1, n, true FROM unnest($2::uuid[], $3::text[]) AS t(i, n)
         ON CONFLICT (account_id, name) DO UPDATE SET server_only = true",
    )
    .bind(account_id)
    .bind(&ids)
    .bind(names)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn rotate_url(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Integration> {
    load(state, account_id, id).await?;
    let (token, hash) = new_token();
    let q = format!("UPDATE integrations SET token_hash = $3, token_enc = $4, updated_at = now() WHERE id = $1 AND account_id = $2 RETURNING {COLS}");
    Ok(sqlx::query_as::<_, Integration>(&q)
        .bind(id)
        .bind(account_id)
        .bind(hash)
        .bind(crate::crypto::encrypt(&state.cfg.secret_key, &token)?)
        .fetch_one(&state.db)
        .await?)
}

pub async fn set_status(state: &AppState, account_id: Uuid, id: Uuid, status: &str) -> AppResult<Integration> {
    if !matches!(status, "enabled" | "disabled") {
        return Err(AppError::bad("status must be enabled or disabled"));
    }
    let cur = load(state, account_id, id).await?;
    let next = if status == "enabled" && cur.pull_config().and_then(|p| p.interval_minutes).is_some() { Some(Utc::now() + chrono::Duration::minutes(1)) } else { None };
    let q = format!("UPDATE integrations SET status = $3, pull_next_at = $4, updated_at = now() WHERE id = $1 AND account_id = $2 RETURNING {COLS}");
    Ok(sqlx::query_as::<_, Integration>(&q).bind(id).bind(account_id).bind(status).bind(next).fetch_one(&state.db).await?)
}

pub async fn delete(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM integrations WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("integration"));
    }
    Ok(())
}

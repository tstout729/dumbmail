//! The account's RCS agent: which Telnyx credentials RCS uses, the agent id, the Check that
//! confirms the agent with Telnyx, and switching RCS on and off.
//!
//! RCS always goes through Telnyx: the platform's Telnyx account (DumbMail's numbers) or the
//! account's own connected Telnyx account. An agent is confirmed only when Telnyx shows it enabled
//! with its webhook URL set to this account's private RCS webhook address, which only someone
//! who can configure that agent in Telnyx can do; so no account can send from another's agent.

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::telnyx;
use crate::error::{AppError, AppResult};
use crate::phone::ByoCredentials;
use crate::state::AppState;

/// The Telnyx credentials RCS uses for an account.
#[derive(Clone)]
pub struct Creds {
    pub api_key: String,
    /// The account's own Telnyx account (its texts aren't billed by DumbMail).
    pub own: bool,
    /// The messaging profile configured with the connection, if any.
    pub messaging_profile_id: Option<String>,
}

impl Creds {
    /// A short fingerprint of the key, to notice a switch of credentials (never the key).
    pub fn fingerprint(&self) -> String {
        let d = Sha256::digest(format!("dumbmail-rcs:{}", self.api_key).as_bytes());
        d.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

/// The credentials RCS would use, or why RCS isn't available for this account's texting.
pub async fn creds(state: &AppState, account_id: Uuid) -> AppResult<Result<Creds, &'static str>> {
    let (mode, enc): (String, Option<String>) = sqlx::query_as("SELECT sms_mode, sms_byo_credentials FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    if mode == "byo" {
        let creds: Option<ByoCredentials> = match enc {
            Some(e) => serde_json::from_str(&crate::crypto::decrypt(&state.cfg.secret_key, &e)?).ok(),
            None => None,
        };
        return Ok(match creds {
            Some(ByoCredentials::Telnyx { api_key, messaging_profile_id, .. }) => Ok(Creds { api_key, own: true, messaging_profile_id }),
            Some(ByoCredentials::Twilio { .. }) => Err("RCS goes through Telnyx, and your texts go through your own Twilio account"),
            None => Err("reconnect your texting provider in Settings → SMS"),
        });
    }
    let platform_telnyx = state.sms.name() == "telnyx" || (state.sms.name() == "log" && telnyx::stand_in(&state.cfg));
    match state.cfg.telnyx_api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        Some(key) if platform_telnyx => Ok(Ok(Creds { api_key: key.to_string(), own: false, messaging_profile_id: state.cfg.telnyx_messaging_profile_id.clone() })),
        _ => Ok(Err("this server doesn't text through Telnyx, so RCS isn't available on it")),
    }
}

/// Where Telnyx sends this account's RCS events (the agent's webhook URL, and each message's).
pub async fn webhook_url(state: &AppState, account_id: Uuid) -> AppResult<String> {
    let token: Option<String> = sqlx::query_scalar("SELECT webhook_token FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let token = token.ok_or_else(|| AppError::Internal(anyhow::anyhow!("account has no webhook token")))?;
    Ok(format!("{}/webhooks/rcs/{token}", state.cfg.public_url()))
}

/// Whether Telnyx's webhook signing key is known for this account (needed: RCS events are only
/// believed when signed, and opt-outs arrive as events).
pub async fn signing_key_ready(state: &AppState, account_id: Uuid) -> AppResult<bool> {
    Ok(matches!(crate::features::inbox::verify::telnyx_key(state, Some(account_id)).await, Ok(Some(_))))
}

#[derive(Debug, Clone, sqlx::FromRow, Default)]
pub struct Row {
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub messaging_profile_id: Option<String>,
    pub checked_agent_id: Option<String>,
    pub checked_key: Option<String>,
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub check_problem: Option<String>,
    pub enabled: bool,
}

pub async fn row(db: &sqlx::PgPool, account_id: Uuid) -> AppResult<Row> {
    Ok(sqlx::query_as("SELECT agent_id, agent_name, messaging_profile_id, checked_agent_id, checked_key, checked_at, check_problem, enabled FROM rcs_settings WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .unwrap_or_default())
}

/// The agent RCS sends from right now, when it's on and confirmed for the current credentials.
pub struct Active {
    pub agent_id: String,
    pub messaging_profile_id: String,
    pub creds: Creds,
}

/// One cheap lookup when RCS is off (the common case): None.
pub async fn active(state: &AppState, account_id: Uuid) -> AppResult<Option<Active>> {
    let r: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT checked_agent_id, messaging_profile_id, checked_key FROM rcs_settings WHERE account_id = $1 AND enabled AND checked_agent_id IS NOT NULL AND checked_agent_id = agent_id",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((agent_id, profile, key)) = r else { return Ok(None) };
    let Ok(creds) = creds(state, account_id).await? else { return Ok(None) };
    if key.as_deref() != Some(creds.fingerprint().as_str()) {
        return Ok(None);
    }
    let Some(messaging_profile_id) = profile.or_else(|| creds.messaging_profile_id.clone()) else { return Ok(None) };
    Ok(Some(Active { agent_id, messaging_profile_id, creds }))
}

/// DumbMail's price for one RCS message (0 when not billed: self-hosted, or the account's own Telnyx).
pub fn price(state: &AppState, own: bool) -> i64 {
    if !state.cfg.hosted || own {
        return 0;
    }
    let usd = state.cfg.rcs.price_rcs.unwrap_or(state.cfg.price_sms_segment).max(0.0);
    crate::billing::to_micros(usd)
}

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let r = row(&state.db, account_id).await?;
    let creds = creds(state, account_id).await?;
    let confirmed = r.checked_agent_id.is_some() && r.checked_agent_id == r.agent_id;
    let current_key = creds.as_ref().ok().map(Creds::fingerprint);
    let own = creds.as_ref().is_ok_and(|c| c.own);
    let sending = r.enabled && confirmed && current_key.is_some() && current_key == r.checked_key;
    Ok(json!({
        "available": creds.is_ok(),
        "unavailable_reason": creds.as_ref().err(),
        "provider": if own { "own_telnyx" } else { "platform" },
        "agent_id": r.agent_id,
        "agent_name": r.agent_name.filter(|_| confirmed),
        "confirmed": confirmed,
        "checked_at": r.checked_at.filter(|_| confirmed),
        "check_problem": r.check_problem,
        "enabled": r.enabled,
        "sending": sending,
        "needs_check": r.enabled && confirmed && current_key != r.checked_key,
        "signing_key_ready": signing_key_ready(state, account_id).await?,
        "webhook_url": webhook_url(state, account_id).await?,
        "price_per_message": crate::billing::to_usd(price(state, own)),
        "price_per_sms_segment": crate::billing::to_usd(if state.cfg.hosted && !own { crate::billing::prices(state).sms_segment } else { 0 }),
    }))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    /// Missing: unchanged. `null` or "": forget the agent (RCS turns off).
    #[serde(default, deserialize_with = "present")]
    pub agent_id: Option<Value>,
    pub enabled: Option<bool>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Apply a change; returns the names of what changed (for the audit log).
pub async fn update(state: &AppState, account_id: Uuid, b: SettingsBody) -> AppResult<Vec<&'static str>> {
    let mut changed = Vec::new();
    if let Some(v) = b.agent_id {
        let agent = match &v {
            Value::Null => None,
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) if telnyx::valid_agent_id(s.trim()) => Some(s.trim().to_string()),
            Value::String(_) => return Err(AppError::bad("that isn't a Telnyx RCS agent id: copy it from the agent in the Telnyx portal (letters, digits, - and _)")),
            _ => return Err(AppError::bad("agent_id must be text or null")),
        };
        // A different agent needs its own Check; RCS stays off until then.
        let n = sqlx::query(
            "INSERT INTO rcs_settings (account_id, agent_id) VALUES ($1, $2)
             ON CONFLICT (account_id) DO UPDATE SET agent_id = $2, enabled = false, checked_agent_id = NULL, checked_key = NULL, checked_at = NULL,
                 agent_name = NULL, messaging_profile_id = NULL, check_problem = NULL, updated_at = now()
             WHERE rcs_settings.agent_id IS DISTINCT FROM $2",
        )
        .bind(account_id)
        .bind(&agent)
        .execute(&state.db)
        .await?
        .rows_affected();
        if n > 0 {
            changed.push("agent_id");
        }
    }
    match b.enabled {
        Some(true) => {
            let creds = creds(state, account_id).await?.map_err(|why| AppError::bad(why.to_string()))?;
            if !signing_key_ready(state, account_id).await? {
                return Err(AppError::bad(
                    "add Telnyx's webhook signing key first (Settings → SMS → inbox settings, or the server's TELNYX_PUBLIC_KEY): without it DumbMail can't trust RCS replies and opt-outs",
                ));
            }
            // Only an agent the last Check confirmed, with the credentials it used.
            let on: Option<Uuid> = sqlx::query_scalar(
                "UPDATE rcs_settings SET enabled = true, updated_at = now()
                 WHERE account_id = $1 AND agent_id IS NOT NULL AND checked_agent_id = agent_id AND checked_key = $2 RETURNING account_id",
            )
            .bind(account_id)
            .bind(creds.fingerprint())
            .fetch_optional(&state.db)
            .await?;
            if on.is_none() {
                return Err(AppError::bad("save your RCS agent id and run Check first: RCS turns on only for an agent Telnyx confirmed"));
            }
            changed.push("enabled");
        }
        Some(false) => {
            let n = sqlx::query("UPDATE rcs_settings SET enabled = false, updated_at = now() WHERE account_id = $1 AND enabled").bind(account_id).execute(&state.db).await?.rows_affected();
            if n > 0 {
                changed.push("enabled");
            }
        }
        None => {}
    }
    Ok(changed)
}

/// Ask Telnyx about the saved agent and record what it said. Confirmed when the agent exists for
/// these credentials, is enabled, and sends its events to this account's RCS webhook address.
/// A definite "no" also turns RCS off; a network problem changes nothing.
pub async fn check(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let r = row(&state.db, account_id).await?;
    let agent_id = r.agent_id.clone().ok_or_else(|| AppError::bad("save your RCS agent id first"))?;
    let creds = creds(state, account_id).await?.map_err(|why| AppError::bad(why.to_string()))?;
    let ours = webhook_url(state, account_id).await?;
    let problem = match telnyx::agent(&state.cfg, &creds.api_key, &agent_id).await {
        Err(e) if e.status == 0 || e.status >= 500 || e.status == 429 => {
            return Err(AppError::Conflict(format!("couldn't check the agent: {}. Try again shortly.", e.describe())));
        }
        Err(e) if e.status == 404 => Err(format!("Telnyx has no RCS agent {agent_id} for this Telnyx account")),
        Err(e) => Err(format!("Telnyx refused the check: {}", e.describe())),
        Ok(a) => {
            let id_ok = a["agent_id"].as_str().is_none_or(|id| id == agent_id);
            let hook = a["webhook_url"].as_str().unwrap_or_default().trim().trim_end_matches('/');
            if !id_ok {
                Err("Telnyx answered for a different agent".to_string())
            } else if a["enabled"].as_bool() == Some(false) {
                Err("the agent isn't enabled at Telnyx yet (it goes live once the carriers approve it)".to_string())
            } else if hook != ours {
                Err(format!("set the agent's webhook URL in Telnyx to {ours} (it's how DumbMail gets replies, opt-outs and read receipts, and how it knows the agent is yours)"))
            } else if a["profile_id"].as_str().filter(|p| !p.is_empty()).is_none() && creds.messaging_profile_id.is_none() {
                Err("put the agent on a messaging profile in Telnyx (sending needs one)".to_string())
            } else {
                Ok(a)
            }
        }
    };
    match problem {
        Ok(a) => {
            let name: Option<String> = a["agent_name"].as_str().map(|s| s.chars().take(100).collect());
            let profile: Option<String> = a["profile_id"].as_str().filter(|p| !p.is_empty() && p.len() <= 100).map(str::to_string);
            let res = sqlx::query(
                "UPDATE rcs_settings SET checked_agent_id = agent_id, checked_key = $3, checked_at = now(), agent_name = $4, messaging_profile_id = $5, check_problem = NULL, updated_at = now()
                 WHERE account_id = $1 AND agent_id = $2",
            )
            .bind(account_id)
            .bind(&agent_id)
            .bind(creds.fingerprint())
            .bind(&name)
            .bind(&profile)
            .execute(&state.db)
            .await;
            match res {
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                    record_problem(state, account_id, &agent_id, "another DumbMail account already uses this RCS agent").await?;
                }
                other => {
                    other?;
                }
            }
        }
        Err(why) => record_problem(state, account_id, &agent_id, &why).await?,
    }
    settings_json(state, account_id).await
}

async fn record_problem(state: &AppState, account_id: Uuid, agent_id: &str, why: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE rcs_settings SET check_problem = $3, enabled = false, checked_agent_id = NULL, checked_key = NULL, checked_at = NULL, updated_at = now()
         WHERE account_id = $1 AND agent_id = $2",
    )
    .bind(account_id)
    .bind(agent_id)
    .bind(why.chars().take(300).collect::<String>())
    .execute(&state.db)
    .await?;
    Ok(())
}

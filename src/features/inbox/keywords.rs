//! Texting keywords: "text JOIN to subscribe", list sign-ups, automatic answers and flow triggers.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::replies;
use super::settings::{self, Settings};
use super::store::{self, Received, HELP_WORDS};
use crate::audience::{self, NewEvent};
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

pub const MAX_KEYWORDS: i64 = 100;
pub const MAX_ALIASES: usize = 10;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Keyword {
    pub id: Uuid,
    pub keyword: String,
    pub aliases: Vec<String>,
    pub match_type: String,
    pub subscribe: bool,
    pub double_opt_in: bool,
    pub list_id: Option<Uuid>,
    pub reply: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, keyword, aliases, match_type, subscribe, double_opt_in, list_id, reply, enabled, created_at, updated_at";

/// Carrier words that keep their standard meaning (opt out, opt back in, help) and the "Y" that
/// confirms a double opt-in. No keyword or alias may use them.
pub fn reserved(word: &str) -> bool {
    crate::public::STOP_WORDS.contains(&word) || crate::public::START_WORDS.contains(&word) || HELP_WORDS.contains(&word) || word == "y"
}

/// Keywords are one word of letters and digits, stored lowercase (matching ignores case).
pub fn normalize(raw: &str) -> AppResult<String> {
    let w = raw.trim();
    if w.is_empty() || w.chars().count() > 20 || !w.chars().all(char::is_alphanumeric) {
        return Err(AppError::bad(format!("{raw:?} isn't a keyword: use one word of letters and digits, up to 20 characters")));
    }
    let w = w.to_lowercase();
    if reserved(&w) {
        return Err(AppError::bad(format!(
            "{} is reserved: carriers use STOP, START, HELP and similar words for opting out and help, and Y confirms sign-ups",
            w.to_uppercase()
        )));
    }
    Ok(w)
}

pub async fn list(db: &PgPool, account_id: Uuid) -> AppResult<Vec<Keyword>> {
    let q = format!("SELECT {COLS} FROM sms_keywords WHERE account_id = $1 ORDER BY keyword");
    Ok(sqlx::query_as::<_, Keyword>(&q).bind(account_id).fetch_all(db).await?)
}

async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Keyword> {
    let q = format!("SELECT {COLS} FROM sms_keywords WHERE id = $1 AND account_id = $2");
    sqlx::query_as::<_, Keyword>(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("keyword"))
}

/// The enabled keyword a text matches: exact matches on the keyword or an alias first, then
/// "starts with" keywords (longest first).
pub async fn matching(db: &PgPool, account_id: Uuid, word: &str) -> AppResult<Option<Keyword>> {
    if word.is_empty() {
        return Ok(None);
    }
    let q = format!(
        "SELECT {COLS} FROM sms_keywords WHERE account_id = $1 AND enabled AND (
            keyword = $2 OR $2 = ANY(aliases)
            OR (match_type = 'starts_with' AND (starts_with($2, keyword) OR EXISTS (SELECT 1 FROM unnest(aliases) a WHERE starts_with($2, a))))
         ) ORDER BY (keyword = $2 OR $2 = ANY(aliases)) DESC, length(keyword) DESC LIMIT 1"
    );
    Ok(sqlx::query_as::<_, Keyword>(&q).bind(account_id).bind(word).fetch_optional(db).await?)
}

fn opt_str(v: &Value, k: &str) -> AppResult<Option<String>> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(AppError::bad(format!("{k} must be text"))),
    }
}

fn opt_bool(v: &Value, k: &str) -> AppResult<Option<bool>> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(AppError::bad(format!("{k} must be true or false"))),
    }
}

/// Create (no `id`) or update a keyword. Fields left out keep their value; `list_id: null` clears the list.
pub async fn upsert(db: &PgPool, account: &Account, id: Option<Uuid>, v: &Value) -> AppResult<Keyword> {
    const KEYS: &[&str] = &["id", "keyword", "aliases", "match_type", "subscribe", "double_opt_in", "list_id", "reply", "enabled"];
    let obj = v.as_object().ok_or_else(|| AppError::bad("send the keyword as an object"))?;
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(AppError::bad(format!("unknown keyword field {k:?}")));
    }
    let current = match id {
        Some(id) => Some(get(db, account.id, id).await?),
        None => None,
    };
    let keyword = match opt_str(v, "keyword")? {
        Some(k) => normalize(&k)?,
        None => current.as_ref().map(|c| c.keyword.clone()).ok_or_else(|| AppError::bad("a keyword needs `keyword`, e.g. JOIN"))?,
    };
    let aliases = match v.get("aliases").filter(|a| !a.is_null()) {
        Some(a) => {
            let raw: Vec<String> = serde_json::from_value(a.clone()).map_err(|_| AppError::bad("aliases must be a list of words"))?;
            if raw.len() > MAX_ALIASES {
                return Err(AppError::bad(format!("a keyword can have at most {MAX_ALIASES} aliases")));
            }
            let mut out = Vec::new();
            for r in raw {
                let w = normalize(&r)?;
                if w != keyword && !out.contains(&w) {
                    out.push(w);
                }
            }
            out
        }
        None => current.as_ref().map(|c| c.aliases.clone()).unwrap_or_default(),
    };
    let match_type = match opt_str(v, "match_type")? {
        Some(m) if m == "exact" || m == "starts_with" => m,
        Some(_) => return Err(AppError::bad("match_type must be exact or starts_with")),
        None => current.as_ref().map_or("exact".into(), |c| c.match_type.clone()),
    };
    let subscribe = opt_bool(v, "subscribe")?.or(current.as_ref().map(|c| c.subscribe)).unwrap_or(false);
    let double_opt_in = opt_bool(v, "double_opt_in")?.or(current.as_ref().map(|c| c.double_opt_in)).unwrap_or(false) && subscribe;
    let enabled = opt_bool(v, "enabled")?.or(current.as_ref().map(|c| c.enabled)).unwrap_or(true);
    let reply = match opt_str(v, "reply")? {
        Some(r) => {
            let r = r.trim().to_string();
            if r.chars().count() > settings::MAX_REPLY_CHARS {
                return Err(AppError::bad(format!("the reply can be at most {} characters", settings::MAX_REPLY_CHARS)));
            }
            settings::check_template(account, &r, "reply")?;
            r
        }
        None => current.as_ref().map(|c| c.reply.clone()).unwrap_or_default(),
    };
    let list_id = match v.get("list_id") {
        None => current.as_ref().and_then(|c| c.list_id),
        Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => {
            let id: Uuid = s.parse().map_err(|_| AppError::bad("list_id isn't a valid id"))?;
            audience::list_belongs(db, account.id, id).await?;
            Some(id)
        }
        Some(_) => return Err(AppError::bad("list_id must be a list id or null")),
    };
    if subscribe {
        let s = settings::load(db, account.id).await?;
        if s.program_name.is_empty() || s.message_frequency.is_empty() {
            return Err(AppError::bad(
                "set your program name and message frequency in the inbox settings first: sign-up confirmations must include them",
            ));
        }
    }
    if !subscribe && list_id.is_none() && reply.is_empty() {
        return Err(AppError::bad("a keyword needs something to do: subscribe the sender, add them to a list, or send a reply"));
    }
    // Every word must belong to one keyword only.
    let mut words = aliases.clone();
    words.push(keyword.clone());
    let clash: Option<String> = sqlx::query_scalar(
        "SELECT upper(keyword) FROM sms_keywords WHERE account_id = $1 AND id IS DISTINCT FROM $2 AND (keyword = ANY($3) OR aliases && $3) LIMIT 1",
    )
    .bind(account.id)
    .bind(id)
    .bind(&words)
    .fetch_optional(db)
    .await?;
    if let Some(other) = clash {
        return Err(AppError::Conflict(format!("the keyword {other} already uses one of these words")));
    }

    let q = match id {
        Some(_) => format!(
            "UPDATE sms_keywords SET keyword = $3, aliases = $4, match_type = $5, subscribe = $6, double_opt_in = $7, list_id = $8, reply = $9, enabled = $10, updated_at = now()
             WHERE id = $1 AND account_id = $2 RETURNING {COLS}"
        ),
        None => format!(
            "INSERT INTO sms_keywords (id, account_id, keyword, aliases, match_type, subscribe, double_opt_in, list_id, reply, enabled)
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10 WHERE (SELECT count(*) FROM sms_keywords WHERE account_id = $2) < {MAX_KEYWORDS}
             RETURNING {COLS}"
        ),
    };
    let saved: Option<Keyword> = sqlx::query_as::<_, Keyword>(&q)
        .bind(id.unwrap_or_else(Uuid::now_v7))
        .bind(account.id)
        .bind(&keyword)
        .bind(&aliases)
        .bind(&match_type)
        .bind(subscribe)
        .bind(double_opt_in)
        .bind(list_id)
        .bind(&reply)
        .bind(enabled)
        .fetch_optional(db)
        .await?;
    saved.ok_or_else(|| AppError::Conflict(format!("an account can have at most {MAX_KEYWORDS} keywords")))
}

pub async fn delete(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM sms_keywords WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("keyword"));
    }
    Ok(())
}

/// The confirmation a keyword sign-up gets: program name, the keyword's own words, then the
/// required disclosures (message frequency, "Msg & data rates may apply", HELP and STOP).
pub fn confirmation(s: &Settings, k: &Keyword) -> String {
    let lead = if k.reply.is_empty() { "Thanks! You're signed up for texts.".to_string() } else { k.reply.clone() };
    format!("{}: {lead} {}", s.program_name, s.disclosure())
}

/// The double opt-in question, sent before anything else.
pub fn opt_in_request(s: &Settings) -> String {
    format!(
        "{}: Reply Y to confirm you want recurring automated marketing texts from us. {} Consent is not a condition of purchase.",
        s.program_name,
        s.disclosure()
    )
}

async fn subscribe_now(state: &AppState, rx: &Received, k: &Keyword, method: &str) -> AppResult<()> {
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed', updated_at = now() WHERE id = $1").bind(rx.profile_id).execute(&state.db).await?;
    // How and when they agreed, kept as the consent record.
    audience::record_event(
        &state.db,
        NewEvent {
            account_id: rx.account_id,
            profile_id: rx.profile_id,
            metric: "Subscribed to SMS",
            properties: json!({"method": method, "keyword": k.keyword.to_uppercase(), "text": rx.text, "to": rx.to, "inbound_message_id": rx.id}),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("sms-optin:{}", rx.id)),
            message_id: None,
        },
    )
    .await?;
    Ok(())
}

async fn add_to_list(state: &AppState, rx: &Received, k: &Keyword) -> AppResult<()> {
    if let Some(list_id) = k.list_id {
        audience::add_to_list(&state.db, rx.account_id, list_id, &[rx.profile_id]).await?;
    }
    Ok(())
}

/// Do what a matched keyword says.
pub async fn run(state: &AppState, rx: &Received, k: &Keyword) -> AppResult<()> {
    store::set_kind(&state.db, rx.id, "keyword", Some(&k.keyword)).await?;
    if !k.subscribe {
        add_to_list(state, rx, k).await?;
        if !k.reply.is_empty() {
            replies::queue_auto(state, rx, &k.reply, false).await?;
        }
        return Ok(());
    }
    let s = settings::load(&state.db, rx.account_id).await?;
    if rx.prior_consent != "subscribed" && k.double_opt_in {
        // Nothing marketing goes out until they reply Y ('pending' gets no marketing texts).
        sqlx::query("UPDATE profiles SET sms_consent = 'pending', updated_at = now() WHERE id = $1").bind(rx.profile_id).execute(&state.db).await?;
        sqlx::query(
            "INSERT INTO sms_pending_optins (profile_id, account_id, keyword_id, inbound_id) VALUES ($1, $2, $3, $4)
             ON CONFLICT (profile_id) DO UPDATE SET keyword_id = $3, inbound_id = $4, created_at = now()",
        )
        .bind(rx.profile_id)
        .bind(rx.account_id)
        .bind(k.id)
        .bind(rx.id)
        .execute(&state.db)
        .await?;
        return replies::queue_auto(state, rx, &opt_in_request(&s), false).await;
    }
    if rx.prior_consent != "subscribed" {
        subscribe_now(state, rx, k, "keyword").await?;
    }
    add_to_list(state, rx, k).await?;
    replies::queue_auto(state, rx, &confirmation(&s, k), false).await
}

/// A "Y" (or START/YES) after a double opt-in request: subscribe, add to the list, confirm.
/// `already_subscribed`: START already set consent and recorded it. Returns false when nothing was pending.
pub async fn confirm_pending(state: &AppState, rx: &Received, already_subscribed: bool) -> AppResult<bool> {
    let keyword_id: Option<Option<Uuid>> =
        sqlx::query_scalar("DELETE FROM sms_pending_optins WHERE profile_id = $1 AND account_id = $2 AND created_at > now() - interval '7 days' RETURNING keyword_id")
            .bind(rx.profile_id)
            .bind(rx.account_id)
            .fetch_optional(&state.db)
            .await?;
    let Some(Some(keyword_id)) = keyword_id else { return Ok(false) };
    let Ok(k) = get(&state.db, rx.account_id, keyword_id).await else { return Ok(false) };
    store::set_kind(&state.db, rx.id, "confirm", Some(&k.keyword)).await?;
    if !already_subscribed {
        subscribe_now(state, rx, &k, "double_opt_in").await?;
    }
    add_to_list(state, rx, &k).await?;
    let s = settings::load(&state.db, rx.account_id).await?;
    replies::queue_auto(state, rx, &confirmation(&s, &k), false).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_one_word_and_not_reserved() {
        assert_eq!(normalize(" Join ").unwrap(), "join");
        assert_eq!(normalize("VIP2026").unwrap(), "vip2026");
        for bad in ["STOP", "help", "Info", "unstop", "y", "yes", "join now", "", "a-b"] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn confirmations_carry_the_disclosures() {
        let s = Settings { program_name: "Acme VIP".into(), message_frequency: "Up to 4 msgs/month".into(), ..Default::default() };
        let k = Keyword {
            id: Uuid::nil(),
            keyword: "join".into(),
            aliases: vec![],
            match_type: "exact".into(),
            subscribe: true,
            double_opt_in: false,
            list_id: None,
            reply: "Welcome! Here's 10% off: SAVE10".into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let c = confirmation(&s, &k);
        for part in ["Acme VIP:", "SAVE10", "Up to 4 msgs/month.", "Msg & data rates may apply", "HELP", "STOP"] {
            assert!(c.contains(part), "{part} missing from {c}");
        }
        let r = opt_in_request(&s);
        assert!(r.contains("Reply Y") && r.contains("Msg & data rates may apply") && r.contains("STOP"), "{r}");
    }
}

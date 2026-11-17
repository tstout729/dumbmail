//! Texting keywords: "text JOIN to subscribe", list sign-ups, automatic answers and flow triggers.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::optout;
use super::replies;
use super::settings::{self, Settings};
use super::store::{self, Received};
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
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, keyword, aliases, match_type, subscribe, double_opt_in, list_id, reply, enabled, version, created_at, updated_at";

/// Carrier words that keep their standard meaning (opt out, opt back in, help) and the "Y" that
/// confirms a double opt-in. No keyword or alias may use them.
pub fn reserved(word: &str) -> bool {
    optout::STANDALONE_STOP.contains(&word) || optout::START_WORDS.contains(&word) || optout::HELP_WORDS.contains(&word) || word == "y" || optout::is_opt_out(word)
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

/// A keyword by its stored word (enabled or not).
pub async fn by_word(db: &PgPool, account_id: Uuid, word: &str) -> AppResult<Option<Keyword>> {
    let q = format!("SELECT {COLS} FROM sms_keywords WHERE account_id = $1 AND keyword = $2");
    Ok(sqlx::query_as::<_, Keyword>(&q).bind(account_id).bind(word).fetch_optional(db).await?)
}

async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Keyword> {
    let q = format!("SELECT {COLS} FROM sms_keywords WHERE id = $1 AND account_id = $2");
    sqlx::query_as::<_, Keyword>(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("keyword"))
}

/// The enabled keyword a text matches: exact matches on the keyword or an alias first, then
/// "starts with" keywords (longest first). Sign-up keywords only ever match exactly.
pub async fn matching(db: &PgPool, account_id: Uuid, word: &str) -> AppResult<Option<Keyword>> {
    if word.is_empty() {
        return Ok(None);
    }
    let q = format!(
        "SELECT {COLS} FROM sms_keywords WHERE account_id = $1 AND enabled AND (
            keyword = $2 OR $2 = ANY(aliases)
            OR (match_type = 'starts_with' AND NOT subscribe AND (starts_with($2, keyword) OR EXISTS (SELECT 1 FROM unnest(aliases) a WHERE starts_with($2, a))))
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
    if subscribe && match_type != "exact" {
        return Err(AppError::bad("a keyword that subscribes people must match exactly, so ordinary texts that start with it don't sign anyone up"));
    }
    let s = settings::load(db, account.id).await?;
    if subscribe && (s.program_name.is_empty() || s.message_frequency.is_empty()) {
        return Err(AppError::bad(
            "set your program name and message frequency in the inbox settings first: sign-up confirmations must include them",
        ));
    }
    if enabled && s.support_contact.is_empty() {
        return Err(AppError::bad("set how people reach support (an email or phone) in the inbox settings first: the answer to HELP must include it"));
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
            "UPDATE sms_keywords SET keyword = $3, aliases = $4, match_type = $5, subscribe = $6, double_opt_in = $7, list_id = $8, reply = $9, enabled = $10, version = version + 1, updated_at = now()
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

/// A double opt-in waiting for this number's "Y": the prompt was accepted by the provider within
/// the last 7 days, it went from `ours`, and its keyword still subscribes people and is on.
pub async fn pending_optin(conn: &mut sqlx::PgConnection, account_id: Uuid, theirs: &str, ours: &str) -> AppResult<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT p.id FROM sms_pending_optins p JOIN sms_keywords k ON k.id = p.keyword_id
         WHERE p.account_id = $1 AND p.phone_number = $2 AND p.our_number = $3
           AND p.prompt_sent_at IS NOT NULL AND p.prompt_sent_at > now() - interval '7 days'
           AND k.enabled AND k.subscribe AND k.double_opt_in",
    )
    .bind(account_id)
    .bind(theirs)
    .bind(ours)
    .fetch_optional(conn)
    .await?)
}

async fn add_to_list(state: &AppState, account_id: Uuid, profile_id: Uuid, k: &Keyword) -> AppResult<()> {
    if let Some(list_id) = k.list_id {
        audience::add_to_list(&state.db, account_id, list_id, &[profile_id]).await?;
    }
    Ok(())
}

/// Do what a matched keyword says. Sign-ups subscribe nobody here: that happens only once the
/// confirmation, with the full disclosure, has been accepted by the provider ([`subscribed`]).
pub async fn run(state: &AppState, rx: &Received, k: &Keyword) -> AppResult<()> {
    if !k.subscribe {
        add_to_list(state, rx.account_id, rx.profile_id, k).await?;
        if !k.reply.is_empty() {
            replies::queue_auto(state, rx, "keyword_reply", &k.reply, Some(k), json!({})).await?;
        }
        return Ok(());
    }
    let s = settings::load(&state.db, rx.account_id).await?;
    let consent: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE id = $1").bind(rx.profile_id).fetch_one(&state.db).await?;
    if consent == "subscribed" {
        // Already signed up: answer as a sign-up would, without a new consent record.
        add_to_list(state, rx.account_id, rx.profile_id, k).await?;
        return replies::queue_auto(state, rx, "keyword_reply", &confirmation(&s, k), Some(k), json!({})).await.map(|_| ());
    }
    let signup = json!({"signup_inbound_id": rx.id, "signup_provider_message_id": rx.provider_message_id});
    if k.double_opt_in {
        // Written first so the prompt's job always finds it; it counts once the prompt is accepted.
        sqlx::query(
            "INSERT INTO sms_pending_optins (id, account_id, profile_id, phone_number, our_number, keyword_id, keyword_version, inbound_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (account_id, phone_number) DO UPDATE SET profile_id = $3, our_number = $5, keyword_id = $6, keyword_version = $7, inbound_id = $8,
                 prompt_id = NULL, prompt_sent_at = NULL, created_at = now()",
        )
        .bind(Uuid::now_v7())
        .bind(rx.account_id)
        .bind(rx.profile_id)
        .bind(&rx.from)
        .bind(&rx.to)
        .bind(k.id)
        .bind(k.version)
        .bind(rx.id)
        .execute(&state.db)
        .await?;
        if replies::queue_auto(state, rx, "optin_request", &opt_in_request(&s), Some(k), signup).await?.is_none() {
            sqlx::query("DELETE FROM sms_pending_optins WHERE account_id = $1 AND phone_number = $2 AND inbound_id = $3 AND prompt_sent_at IS NULL")
                .bind(rx.account_id)
                .bind(&rx.from)
                .bind(rx.id)
                .execute(&state.db)
                .await?;
        }
        return Ok(());
    }
    replies::queue_auto(state, rx, "confirmation", &confirmation(&s, k), Some(k), signup).await.map(|_| ())
}

/// A "Y" or "YES" answering a double opt-in prompt that went out: queue the confirmation (with the
/// disclosure); they're subscribed once it's accepted. Returns false when nothing was waiting.
pub async fn confirm_pending(state: &AppState, rx: &Received) -> AppResult<bool> {
    let row: Option<(Uuid, i32, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "DELETE FROM sms_pending_optins p USING sms_keywords k
         WHERE k.id = p.keyword_id AND p.account_id = $1 AND p.phone_number = $2 AND p.our_number = $3
           AND p.prompt_sent_at IS NOT NULL AND p.prompt_sent_at > now() - interval '7 days' AND k.enabled AND k.subscribe AND k.double_opt_in
         RETURNING p.keyword_id, p.keyword_version, p.inbound_id, p.prompt_id",
    )
    .bind(rx.account_id)
    .bind(&rx.from)
    .bind(&rx.to)
    .fetch_optional(&state.db)
    .await?;
    let Some((keyword_id, version, signup_inbound, prompt_id)) = row else { return Ok(false) };
    let Ok(k) = get(&state.db, rx.account_id, keyword_id).await else { return Ok(false) };
    store::set_kind(&state.db, rx.id, "confirm", Some(&k.keyword)).await?;
    let signup_pmid: Option<String> = match signup_inbound {
        Some(id) => sqlx::query_scalar("SELECT provider_message_id FROM sms_inbound WHERE id = $1").bind(id).fetch_optional(&state.db).await?,
        None => None,
    };
    let prompt_pmid: Option<String> = match prompt_id {
        Some(id) => sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE id = $1").bind(id).fetch_optional(&state.db).await?.flatten(),
        None => None,
    };
    let s = settings::load(&state.db, rx.account_id).await?;
    let context = json!({
        "double_opt_in": true, "keyword_version": version,
        "signup_inbound_id": signup_inbound, "signup_provider_message_id": signup_pmid,
        "confirm_inbound_id": rx.id, "confirm_provider_message_id": rx.provider_message_id,
        "prompt_message_id": prompt_id, "prompt_provider_message_id": prompt_pmid,
    });
    replies::queue_auto(state, rx, "confirmation", &confirmation(&s, &k), Some(&k), context).await?;
    Ok(true)
}

/// The provider accepted a sign-up confirmation (`message_id`, with the disclosure text as sent):
/// now they're subscribed, with the evidence kept. Unless they opted out in the meantime.
pub async fn subscribed(state: &AppState, auto: &replies::AutoSend, message_id: Uuid) -> AppResult<()> {
    let Some(keyword_id) = auto.keyword_id else { return Ok(()) };
    let Some(k) = by_id(&state.db, auto.account_id, keyword_id).await? else { return Ok(()) };
    let (disclosure, provider_id): (String, Option<String>) = sqlx::query_as(
        "SELECT t.body, m.provider_message_id FROM sms_sent_texts t JOIN messages m ON m.id = t.message_id WHERE t.message_id = $1",
    )
    .bind(message_id)
    .fetch_one(&state.db)
    .await?;
    let c = &auto.context;
    let double = c["double_opt_in"].as_bool() == Some(true);
    let uuid = |key: &str| c[key].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || ':' || $2, 64))").bind(auto.account_id).bind(&auto.to_number).execute(&mut *tx).await?;
    if store::is_opted_out(&mut tx, auto.account_id, &auto.to_number).await? {
        return Ok(());
    }
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed', updated_at = now() WHERE id = $1").bind(auto.profile_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO sms_consents (id, account_id, profile_id, method, phone_number, our_number, keyword_id, keyword, keyword_version, disclosure,
             signup_inbound_id, signup_provider_message_id, confirm_inbound_id, confirm_provider_message_id, prompt_message_id, prompt_provider_message_id,
             confirmation_message_id, confirmation_provider_message_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
         ON CONFLICT (confirmation_message_id) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(auto.account_id)
    .bind(auto.profile_id)
    .bind(if double { "double_opt_in" } else { "keyword" })
    .bind(&auto.to_number)
    .bind(&auto.from_number)
    .bind(k.id)
    .bind(&k.keyword)
    .bind(c["keyword_version"].as_i64().map(|v| v as i32).or(auto.keyword_version).unwrap_or(k.version))
    .bind(&disclosure)
    .bind(uuid("signup_inbound_id"))
    .bind(c["signup_provider_message_id"].as_str())
    .bind(uuid("confirm_inbound_id"))
    .bind(c["confirm_provider_message_id"].as_str())
    .bind(uuid("prompt_message_id"))
    .bind(c["prompt_provider_message_id"].as_str())
    .bind(message_id)
    .bind(&provider_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let signup_text: Option<String> = match uuid("signup_inbound_id") {
        Some(id) => sqlx::query_scalar("SELECT body FROM sms_inbound WHERE id = $1").bind(id).fetch_optional(&state.db).await?,
        None => None,
    };
    // How and when they agreed, also as an event for segments and flows.
    audience::record_event(
        &state.db,
        NewEvent {
            account_id: auto.account_id,
            profile_id: auto.profile_id,
            metric: "Subscribed to SMS",
            properties: json!({
                "method": if double { "double_opt_in" } else { "keyword" }, "keyword": k.keyword.to_uppercase(), "keyword_version": k.version,
                "text": signup_text, "to": auto.from_number, "disclosure": disclosure, "inbound_message_id": uuid("signup_inbound_id"),
                "confirmation_message_id": message_id,
            }),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("sms-optin:{}", auto.id)),
            message_id: Some(message_id),
        },
    )
    .await?;
    add_to_list(state, auto.account_id, auto.profile_id, &k).await
}

/// The provider accepted a double opt-in prompt: from now, for 7 days, a "Y" counts.
pub async fn prompt_sent(state: &AppState, auto: &replies::AutoSend) -> AppResult<()> {
    sqlx::query("UPDATE sms_pending_optins SET prompt_sent_at = now(), prompt_id = $1 WHERE account_id = $2 AND phone_number = $3 AND our_number = $4 AND inbound_id = $5")
        .bind(auto.id)
        .bind(auto.account_id)
        .bind(&auto.to_number)
        .bind(&auto.from_number)
        .bind(auto.inbound_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn by_id(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Option<Keyword>> {
    let q = format!("SELECT {COLS} FROM sms_keywords WHERE id = $1 AND account_id = $2");
    Ok(sqlx::query_as::<_, Keyword>(&q).bind(id).bind(account_id).fetch_optional(db).await?)
}

/// Whether any keyword subscribes people (then the disclosure settings can't be cleared).
pub async fn any_subscribing(db: &PgPool, account_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_keywords WHERE account_id = $1 AND subscribe)").bind(account_id).fetch_one(db).await?)
}

/// Whether any keyword is on (then the support contact can't be cleared).
pub async fn any_enabled(db: &PgPool, account_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_keywords WHERE account_id = $1 AND enabled)").bind(account_id).fetch_one(db).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_one_word_and_not_reserved() {
        assert_eq!(normalize(" Join ").unwrap(), "join");
        assert_eq!(normalize("VIP2026").unwrap(), "vip2026");
        for bad in ["STOP", "help", "Info", "unstop", "y", "yes", "join now", "", "a-b", "Revoke", "stopall", "optout", "subscribe", "start"] {
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
            version: 1,
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

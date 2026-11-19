//! What the store turns on and tells the agent, checked; the settings API and its MCP tools.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

pub const MAX_NAME: usize = 40;
pub const MAX_TONE: usize = 300;
pub const MAX_ANSWERS: usize = 100;
pub const MAX_QUESTION: usize = 300;
pub const MAX_ANSWER: usize = 2000;
pub const MAX_PAGES: usize = 20;
pub const MAX_URL: usize = 500;
pub const DEFAULT_MONTHLY_CAP: i32 = 500;
pub const MAX_MONTHLY_CAP: i32 = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// The chat panel in the customer hub (signed-in shoppers).
    pub hub: bool,
    /// The chat bubble on the store's website (anyone, never anything personal).
    pub site: bool,
    /// First answers to inbound texts.
    pub sms: bool,
    /// First answers to inbound WhatsApp messages.
    pub whatsapp: bool,
    /// What the agent calls itself.
    pub name: String,
    /// A short note on how it should sound. It can't change the rules.
    pub tone: String,
    /// Questions and answers in the store's own words.
    pub answers: Vec<Answer>,
    /// Most conversations a month (the account's time zone).
    pub monthly_cap: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { hub: false, site: false, sms: false, whatsapp: false, name: String::new(), tone: String::new(), answers: Vec::new(), monthly_cap: DEFAULT_MONTHLY_CAP }
    }
}

impl Settings {
    /// The name shown to shoppers.
    pub fn display_name(&self, account: &Account) -> String {
        if self.name.trim().is_empty() {
            format!("{} assistant", account.name)
        } else {
            self.name.trim().to_string()
        }
    }

    pub fn place_on(&self, place: &str) -> bool {
        match place {
            "hub" => self.hub,
            "site" => self.site,
            "sms" => self.sms,
            "whatsapp" => self.whatsapp,
            "test" => true,
            _ => false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub hub: Option<bool>,
    pub site: Option<bool>,
    pub sms: Option<bool>,
    pub whatsapp: Option<bool>,
    pub name: Option<String>,
    pub tone: Option<String>,
    pub answers: Option<Vec<Answer>>,
    pub monthly_cap: Option<i32>,
    /// The full list of website pages to answer from (added ones are read soon, missing ones removed).
    pub pages: Option<Vec<String>>,
}

pub fn patch_schema() -> Value {
    json!({
        "hub": {"type": "boolean", "description": "Chat panel in the customer hub, for signed-in shoppers"},
        "site": {"type": "boolean", "description": "Chat bubble on your website, for anyone (it never shows anything personal)"},
        "sms": {"type": "boolean", "description": "Answer inbound texts first (a person takes over when it hands off)"},
        "whatsapp": {"type": "boolean", "description": "Answer inbound WhatsApp messages first (a person takes over when it hands off)"},
        "name": {"type": "string", "maxLength": MAX_NAME, "description": "What the agent calls itself (empty: \"<store> assistant\")"},
        "tone": {"type": "string", "maxLength": MAX_TONE, "description": "A short note on how it should sound. It can't change the agent's rules."},
        "answers": {"type": "array", "maxItems": MAX_ANSWERS, "description": "Questions and answers in your own words (replaces the list)",
            "items": {"type": "object", "properties": {"question": {"type": "string", "maxLength": MAX_QUESTION}, "answer": {"type": "string", "maxLength": MAX_ANSWER}}, "required": ["question", "answer"], "additionalProperties": false}},
        "monthly_cap": {"type": "integer", "minimum": 1, "maximum": MAX_MONTHLY_CAP, "description": "Most conversations a month"},
        "pages": {"type": "array", "maxItems": MAX_PAGES, "items": {"type": "string", "maxLength": MAX_URL}, "description": "Pages from your own website to answer from (replaces the list; read now and weekly)"},
    })
}

fn clean_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n').collect::<String>().trim().to_string()
}

pub fn check(mut s: Settings) -> AppResult<Settings> {
    s.name = clean_text(&s.name).replace('\n', " ");
    if s.name.chars().count() > MAX_NAME {
        return Err(AppError::bad(format!("The name can be at most {MAX_NAME} characters")));
    }
    s.tone = clean_text(&s.tone);
    if s.tone.chars().count() > MAX_TONE {
        return Err(AppError::bad(format!("The tone note can be at most {MAX_TONE} characters")));
    }
    if s.answers.len() > MAX_ANSWERS {
        return Err(AppError::bad(format!("At most {MAX_ANSWERS} questions and answers")));
    }
    let mut answers = Vec::new();
    for a in s.answers {
        let q = clean_text(&a.question);
        let ans = clean_text(&a.answer);
        if q.is_empty() && ans.is_empty() {
            continue;
        }
        if q.is_empty() || ans.is_empty() {
            return Err(AppError::bad("Each question needs an answer"));
        }
        if q.chars().count() > MAX_QUESTION || ans.chars().count() > MAX_ANSWER {
            return Err(AppError::bad(format!("A question can be at most {MAX_QUESTION} characters and an answer at most {MAX_ANSWER}")));
        }
        answers.push(Answer { question: q, answer: ans });
    }
    s.answers = answers;
    if !(1..=MAX_MONTHLY_CAP).contains(&s.monthly_cap) {
        return Err(AppError::bad(format!("The monthly cap must be between 1 and {MAX_MONTHLY_CAP}")));
    }
    Ok(s)
}

/// A page address the agent may read: http(s) on the store's own website (or a subdomain of it),
/// no credentials. Returns it normalized (no fragment).
pub fn page_url(account: &Account, raw: &str) -> AppResult<String> {
    let raw = raw.trim();
    if raw.len() > MAX_URL {
        return Err(AppError::bad(format!("A page address can be at most {MAX_URL} characters")));
    }
    let mut u = url::Url::parse(raw).map_err(|_| AppError::bad(format!("{raw:?} isn't a web address (https://...)")))?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() || !matches!(u.port(), None | Some(80 | 443)) {
        return Err(AppError::bad(format!("{raw:?} isn't a web address (https://...)")));
    }
    let hosts = crate::features::catalog::learn::hosts_of(account.website.as_deref(), &[]);
    if hosts.is_empty() {
        return Err(AppError::bad("Add your website in Settings → Account first: the agent reads pages only from your own site"));
    }
    if !crate::features::catalog::learn::on_site(&hosts, u.as_str()) {
        return Err(AppError::bad(format!("{raw:?} isn't on your website ({}). The agent reads pages only from your own site.", hosts.join(", "))));
    }
    u.set_fragment(None);
    Ok(u.to_string())
}

/// The account's settings (defaults when it never saved any).
pub async fn load(db: &PgPool, account_id: Uuid) -> AppResult<Settings> {
    let row: Option<Value> = sqlx::query_scalar("SELECT config FROM agent_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(row.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
}

async fn save(db: &PgPool, account_id: Uuid, s: &Settings) -> AppResult<()> {
    let v = serde_json::to_value(s).map_err(anyhow::Error::from)?;
    sqlx::query("INSERT INTO agent_settings (account_id, config) VALUES ($1, $2) ON CONFLICT (account_id) DO UPDATE SET config = $2, updated_at = now()")
        .bind(account_id)
        .bind(v)
        .execute(db)
        .await?;
    Ok(())
}

/// Replace the page list: new addresses are added (and read by the next fetch run), missing ones
/// removed. Returns (added, removed).
async fn set_pages(state: &AppState, account: &Account, urls: &[String]) -> AppResult<(usize, usize)> {
    if urls.len() > MAX_PAGES {
        return Err(AppError::bad(format!("At most {MAX_PAGES} pages")));
    }
    let mut clean: Vec<String> = Vec::new();
    for u in urls {
        let c = page_url(account, u)?;
        if !clean.contains(&c) {
            clean.push(c);
        }
    }
    let mut tx = state.db.begin().await?;
    let removed = sqlx::query("DELETE FROM agent_pages WHERE account_id = $1 AND NOT (url = ANY($2))").bind(account.id).bind(&clean).execute(&mut *tx).await?.rows_affected() as usize;
    let mut added = 0;
    for u in &clean {
        let id: Option<Uuid> = sqlx::query_scalar("INSERT INTO agent_pages (id, account_id, url) VALUES ($1, $2, $3) ON CONFLICT (account_id, url) DO NOTHING RETURNING id")
            .bind(Uuid::now_v7())
            .bind(account.id)
            .bind(u)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(id) = id {
            added += 1;
            crate::jobs::enqueue_background(&mut *tx, super::knowledge::FETCH_JOB, json!({"id": id}), None).await?;
        }
    }
    tx.commit().await?;
    Ok((added, removed))
}

/// Apply a change and save it, with an audit entry.
pub async fn update(state: &AppState, ctx: &Ctx, body: Value) -> AppResult<Value> {
    let p: Patch = serde_json::from_value(body).map_err(|e| AppError::bad(format!("invalid settings: {e}")))?;
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let mut s = load(&state.db, ctx.account_id).await?;
    let before = s.clone();
    if let Some(v) = p.hub {
        s.hub = v;
    }
    if let Some(v) = p.site {
        s.site = v;
    }
    if let Some(v) = p.sms {
        s.sms = v;
    }
    if let Some(v) = p.whatsapp {
        s.whatsapp = v;
    }
    if let Some(v) = p.name {
        s.name = v;
    }
    if let Some(v) = p.tone {
        s.tone = v;
    }
    if let Some(v) = p.answers {
        s.answers = v;
    }
    if let Some(v) = p.monthly_cap {
        s.monthly_cap = v;
    }
    let s = check(s)?;
    let pages = match &p.pages {
        Some(urls) => Some(set_pages(state, &account, urls).await?),
        None => None,
    };
    save(&state.db, ctx.account_id, &s).await?;
    crate::features::team::audit(
        state,
        ctx,
        "agent.settings_change",
        None,
        json!({
            "hub": s.hub, "site": s.site, "sms": s.sms, "whatsapp": s.whatsapp,
            "was": {"hub": before.hub, "site": before.site, "sms": before.sms, "whatsapp": before.whatsapp},
            "answers": s.answers.len(), "monthly_cap": s.monthly_cap,
            "pages_added": pages.map(|p| p.0), "pages_removed": pages.map(|p| p.1),
        }),
    )
    .await;
    settings_json(state, ctx.account_id).await
}

/// Settings plus what the dashboard needs: the pages and how they were read, whether the agent
/// can run on this server, this month's use and the price.
pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = load(&state.db, account_id).await?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let pages: Vec<(Uuid, String, Option<String>, String, Option<String>, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, url, title, status, error, fetched_at FROM agent_pages WHERE account_id = $1 ORDER BY created_at, id").bind(account_id).fetch_all(&state.db).await?;
    let used = super::chat::conversations_this_month(&state.db, &account).await?;
    let hub = crate::features::hub::settings::load(&state.db, account_id).await?;
    let mut out = serde_json::to_value(&s).map_err(anyhow::Error::from)?;
    out["display_name"] = json!(s.display_name(&account));
    out["pages"] = json!(pages
        .into_iter()
        .map(|(id, url, title, status, error, fetched_at)| json!({"id": id, "url": url, "title": title, "status": status, "error": error, "fetched_at": fetched_at}))
        .collect::<Vec<_>>());
    out["available"] = json!(state.cfg.anthropic_api_key.as_deref().is_some_and(|k| !k.is_empty()));
    out["model"] = json!(super::claude::MODEL);
    out["conversations_this_month"] = json!(used);
    out["price_per_conversation_usd"] = json!(if state.cfg.hosted { state.cfg.price_agent_conversation } else { 0.0 });
    out["hub_enabled"] = json!(hub.enabled);
    out["site_origins"] = json!(super::web::site_origins(&account, &hub));
    out["website"] = json!(account.website);
    Ok(out)
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/agent/settings", get(get_h).patch(patch_h))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, b).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_limits_and_drops_blank_rows() {
        let s = check(Settings { answers: vec![Answer { question: " ".into(), answer: "".into() }, Answer { question: "Ship to Canada?".into(), answer: "Yes.".into() }], ..Default::default() }).unwrap();
        assert_eq!(s.answers.len(), 1);
        assert!(check(Settings { answers: vec![Answer { question: "Q".into(), answer: "".into() }], ..Default::default() }).is_err());
        assert!(check(Settings { monthly_cap: 0, ..Default::default() }).is_err());
        assert!(check(Settings { name: "x".repeat(41), ..Default::default() }).is_err());
        assert!(serde_json::from_value::<Patch>(json!({"hubb": true})).is_err());
    }
}

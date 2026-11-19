//! Message templates: synced from the account's WhatsApp Business Account (on demand, and when
//! Meta's template status webhook arrives), created from DumbMail (submitted for Meta's review),
//! and read for their variables.
//!
//! A URL button DumbMail tracks points at DumbMail's click address with one variable, the
//! per-message token (`/wa/<token>`); the real destination is kept here (`tracked_buttons`) and the
//! click is recorded before the redirect.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{graph, settings};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Pages of templates read in one sync (Meta allows 250 templates per business account).
const MAX_PAGES: usize = 5;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Template {
    pub id: Uuid,
    pub meta_id: String,
    pub name: String,
    pub language: String,
    pub category: String,
    pub status: String,
    pub parameter_format: String,
    pub components: Value,
    pub tracked_buttons: Value,
    pub reason: Option<String>,
    pub quality: Option<String>,
    pub created_by_dumbmail: bool,
    pub synced_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const COLS: &str = "id, meta_id, name, language, category, status, parameter_format, components, tracked_buttons, reason, quality, created_by_dumbmail, synced_at, updated_at";

impl Template {
    pub fn approved(&self) -> bool {
        self.status == "APPROVED"
    }

    /// Marketing templates need a WhatsApp opt-in; utility and authentication ones are service messages.
    pub fn is_marketing(&self) -> bool {
        self.category != "UTILITY" && self.category != "AUTHENTICATION"
    }

    pub fn component(&self, kind: &str) -> Option<&Value> {
        self.components.as_array()?.iter().find(|c| c["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(kind)))
    }

    /// What a send has to fill in.
    pub fn vars(&self) -> Vars {
        let mut v = Vars::default();
        if let Some(h) = self.component("HEADER") {
            match h["format"].as_str().unwrap_or("TEXT").to_ascii_uppercase().as_str() {
                "TEXT" => v.header = placeholders(h["text"].as_str().unwrap_or_default()).into_iter().next(),
                "IMAGE" | "VIDEO" | "DOCUMENT" => v.header_media = h["format"].as_str().map(|s| s.to_ascii_uppercase()),
                _ => v.header_unsupported = h["format"].as_str().map(str::to_string),
            }
        }
        if let Some(b) = self.component("BODY") {
            v.body = placeholders(b["text"].as_str().unwrap_or_default());
        }
        if let Some(bs) = self.component("BUTTONS").and_then(|b| b["buttons"].as_array()) {
            for (i, b) in bs.iter().enumerate() {
                let kind = b["type"].as_str().unwrap_or_default().to_ascii_uppercase();
                if kind == "URL" && b["url"].as_str().is_some_and(|u| u.contains("{{")) {
                    v.url_buttons.push((i, self.tracked_buttons.get(i.to_string()).and_then(Value::as_str).is_some()));
                } else if !matches!(kind.as_str(), "URL" | "QUICK_REPLY" | "PHONE_NUMBER") {
                    v.unsupported_buttons.push(kind);
                }
            }
        }
        v
    }

    /// The template's text with its variables shown as `{{name}}` (for previews and the inbox).
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        for k in ["HEADER", "BODY", "FOOTER"] {
            if let Some(t) = self.component(k).and_then(|c| c["text"].as_str()) {
                parts.push(t.to_string());
            }
        }
        parts.join("\n\n")
    }

    pub fn json(&self) -> Value {
        let v = self.vars();
        json!({
            "id": self.id, "meta_id": self.meta_id, "name": self.name, "language": self.language, "category": self.category, "status": self.status,
            "parameter_format": self.parameter_format, "components": self.components, "reason": self.reason, "quality": self.quality,
            "created_by_dumbmail": self.created_by_dumbmail, "synced_at": self.synced_at,
            "tracked_buttons": self.tracked_buttons,
            "variables": {"header": v.header, "header_media": v.header_media, "body": v.body, "url_buttons": v.url_buttons.iter().filter(|(_, t)| !t).map(|(i, _)| i).collect::<Vec<_>>()},
            "sendable": self.approved() && v.supported(),
        })
    }
}

/// The variables a template has.
#[derive(Debug, Default, Clone)]
pub struct Vars {
    /// The text header's variable name ("1" or a named parameter).
    pub header: Option<String>,
    /// IMAGE, VIDEO or DOCUMENT: a send gives a link to the file.
    pub header_media: Option<String>,
    pub header_unsupported: Option<String>,
    /// The body's variable names, in order.
    pub body: Vec<String>,
    /// URL buttons with a variable: (index, tracked by DumbMail).
    pub url_buttons: Vec<(usize, bool)>,
    pub unsupported_buttons: Vec<String>,
}

impl Vars {
    /// Whether DumbMail can fill everything this template needs.
    pub fn supported(&self) -> bool {
        self.header_unsupported.is_none() && self.unsupported_buttons.is_empty()
    }
}

/// `{{1}}`, `{{ first_name }}` → their names, in order, without repeats.
pub fn placeholders(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim().to_string();
        if !name.is_empty() && name.len() <= 60 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[end + 2..];
    }
    out
}

pub async fn list(db: &sqlx::PgPool, account_id: Uuid, include_deleted: bool) -> AppResult<Vec<Template>> {
    let q = format!("SELECT {COLS} FROM whatsapp_templates WHERE account_id = $1 AND ($2 OR status <> 'DELETED') ORDER BY name, language LIMIT 1000");
    Ok(sqlx::query_as::<_, Template>(&q).bind(account_id).bind(include_deleted).fetch_all(db).await?)
}

pub async fn get(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<Template> {
    let q = format!("SELECT {COLS} FROM whatsapp_templates WHERE id = $1 AND account_id = $2");
    sqlx::query_as::<_, Template>(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("WhatsApp template"))
}

fn clip(v: &Value, n: usize) -> Option<String> {
    v.as_str().map(|s| s.chars().take(n).collect())
}

/// Read every template from Meta and bring the local copies up to date. Templates Meta no longer
/// lists are marked DELETED (campaigns using them then stop at their checks). Returns the count read.
pub async fn sync(state: &AppState, account_id: Uuid) -> AppResult<usize> {
    let Some((waba_id, _, token)) = settings::load_reader(state, account_id).await? else {
        return Err(AppError::bad("Connect WhatsApp first"));
    };
    // What this read finds is Meta's state as of now: an event Meta sent after this moment (a pause,
    // a category change) wins over it.
    let started = Utc::now();
    let mut after: Option<String> = None;
    let mut seen = 0usize;
    for _ in 0..MAX_PAGES {
        let page = match graph::templates_page(&state.cfg, &token, &waba_id, after.as_deref()).await {
            Ok(p) => p,
            Err(e) => {
                if e.class() == graph::Class::Auth {
                    settings::record_error(&state.db, account_id, &e.describe()).await?;
                }
                return Err(AppError::bad(format!("Couldn't read templates from Meta: {}", e.describe())));
            }
        };
        let items = page["data"].as_array().cloned().unwrap_or_default();
        for t in &items {
            let (Some(meta_id), Some(name), Some(language)) = (clip(&t["id"], 40), clip(&t["name"], 512), clip(&t["language"], 20)) else { continue };
            if !graph::valid_id(&meta_id) {
                continue;
            }
            let components = if t["components"].is_array() && t["components"].to_string().len() <= 32_000 { t["components"].clone() } else { json!([]) };
            sqlx::query(
                "INSERT INTO whatsapp_templates (id, account_id, meta_id, name, language, category, status, parameter_format, components, reason, quality, synced_at, status_at, category_at,
                                                paused_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), $12, $12, CASE WHEN $7 IN ('PAUSED', 'DISABLED') THEN $12 END)
                 ON CONFLICT (account_id, meta_id) DO UPDATE SET name = EXCLUDED.name, language = EXCLUDED.language,
                     category = CASE WHEN whatsapp_templates.category_at > $12 THEN whatsapp_templates.category ELSE EXCLUDED.category END,
                     category_at = GREATEST(whatsapp_templates.category_at, $12),
                     status = CASE WHEN whatsapp_templates.status_at > $12 THEN whatsapp_templates.status
                                   WHEN whatsapp_templates.status IN ('PAUSED', 'DISABLED') AND EXCLUDED.status IN ('PAUSED', 'DISABLED') THEN EXCLUDED.status
                                   WHEN whatsapp_templates.paused_at >= $12 THEN whatsapp_templates.status
                                   ELSE EXCLUDED.status END,
                     paused_at = CASE WHEN EXCLUDED.status IN ('PAUSED', 'DISABLED') AND whatsapp_templates.status NOT IN ('PAUSED', 'DISABLED') AND NOT (whatsapp_templates.status_at > $12)
                                      THEN $12 ELSE whatsapp_templates.paused_at END,
                     status_at = GREATEST(whatsapp_templates.status_at, $12),
                     parameter_format = EXCLUDED.parameter_format, components = EXCLUDED.components,
                     reason = EXCLUDED.reason, quality = EXCLUDED.quality, synced_at = now(),
                     updated_at = CASE WHEN whatsapp_templates.status IS DISTINCT FROM EXCLUDED.status THEN now() ELSE whatsapp_templates.updated_at END",
            )
            .bind(Uuid::now_v7())
            .bind(account_id)
            .bind(&meta_id)
            .bind(&name)
            .bind(&language)
            .bind(clip(&t["category"], 40).unwrap_or_else(|| "MARKETING".into()).to_ascii_uppercase())
            .bind(clip(&t["status"], 40).unwrap_or_else(|| "PENDING".into()).to_ascii_uppercase())
            .bind(clip(&t["parameter_format"], 20).unwrap_or_else(|| "POSITIONAL".into()).to_ascii_uppercase())
            .bind(&components)
            .bind(clip(&t["rejected_reason"], 100).filter(|r| r != "NONE"))
            .bind(clip(&t["quality_score"]["score"], 40))
            .bind(started)
            .execute(&state.db)
            .await?;
            seen += 1;
        }
        after = page["paging"]["cursors"]["after"].as_str().filter(|_| page["paging"]["next"].is_string()).map(|s| s.chars().take(400).collect());
        if after.is_none() || items.is_empty() {
            // Every page was read: what Meta didn't list is gone.
            sqlx::query("UPDATE whatsapp_templates SET status = 'DELETED', updated_at = now() WHERE account_id = $1 AND synced_at < $2 AND status <> 'DELETED'")
                .bind(account_id)
                .bind(started)
                .execute(&state.db)
                .await?;
            return Ok(seen);
        }
    }
    Ok(seen)
}

/// What a template status webhook did.
#[derive(Debug, PartialEq, Eq)]
pub enum StatusChange {
    /// The template isn't known here yet (the caller queues a sync).
    Unknown,
    /// Older than what's in force, or an approval that came from before its pause: nothing changed.
    Ignored,
    Applied {
        /// It just went from usable to paused or disabled (alert the owners, once).
        paused_now: bool,
    },
}

pub fn is_paused(status: &str) -> bool {
    matches!(status, "PAUSED" | "DISABLED")
}

/// Meta's template status webhook, with Meta's time for it (`entry[].time`). Events apply in their
/// own order; once paused or disabled, only an event from after the pause can change it again.
pub async fn status_update(db: &sqlx::PgPool, account_id: Uuid, meta_id: &str, event: &str, reason: Option<&str>, at: DateTime<Utc>) -> sqlx::Result<StatusChange> {
    let mut tx = db.begin().await?;
    let cur: Option<(String, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT status, status_at, paused_at FROM whatsapp_templates WHERE account_id = $1 AND meta_id = $2 FOR UPDATE").bind(account_id).bind(meta_id).fetch_optional(&mut *tx).await?;
    let Some((status, status_at, paused_at)) = cur else { return Ok(StatusChange::Unknown) };
    // Meta's times are whole seconds: compare to the second.
    let secs = |t: DateTime<Utc>| t.timestamp();
    let older = status_at.is_some_and(|s| secs(at) < secs(s));
    let before_pause = paused_at.is_some_and(|p| secs(at) <= secs(p)) && !is_paused(event);
    if older || before_pause {
        return Ok(StatusChange::Ignored);
    }
    let paused_now = is_paused(event) && !is_paused(&status);
    sqlx::query(
        "UPDATE whatsapp_templates SET status = $3, reason = $4, status_at = $5, paused_at = CASE WHEN $6 THEN $5 ELSE paused_at END, updated_at = now()
         WHERE account_id = $1 AND meta_id = $2",
    )
    .bind(account_id)
    .bind(meta_id)
    .bind(event)
    .bind(reason.filter(|r| *r != "NONE"))
    .bind(at)
    .bind(is_paused(event))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusChange::Applied { paused_now })
}

/// Meta's template category webhook (a completed change), with Meta's time for it. Returns false
/// when the template isn't known here yet.
pub async fn category_update(db: &sqlx::PgPool, account_id: Uuid, meta_id: &str, category: &str, at: DateTime<Utc>) -> sqlx::Result<bool> {
    let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM whatsapp_templates WHERE account_id = $1 AND meta_id = $2)").bind(account_id).bind(meta_id).fetch_one(db).await?;
    sqlx::query(
        "UPDATE whatsapp_templates SET category = $3, category_at = $4, updated_at = now()
         WHERE account_id = $1 AND meta_id = $2 AND (category_at IS NULL OR date_trunc('second', category_at) <= date_trunc('second', $4))",
    )
    .bind(account_id)
    .bind(meta_id)
    .bind(category)
    .bind(at)
    .execute(db)
    .await?;
    Ok(known)
}

/// Stop using a template Meta paused or disabled (error 132015 or 132016 on a send). Returns
/// whether it just changed (alert the owners, once).
pub async fn mark_paused(db: &sqlx::PgPool, account_id: Uuid, template_id: Uuid, code: i64) -> sqlx::Result<bool> {
    let status = if code == 132016 { "DISABLED" } else { "PAUSED" };
    let n = sqlx::query(
        "UPDATE whatsapp_templates SET status = $3, paused_at = now(), status_at = GREATEST(status_at, now()), updated_at = now()
         WHERE id = $1 AND account_id = $2 AND status NOT IN ('PAUSED', 'DISABLED')",
    )
    .bind(template_id)
    .bind(account_id)
    .bind(status)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

// ---------- Creating a template ----------

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NewButton {
    /// quick_reply | url
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    /// URL buttons: send clicks through DumbMail's link tracking (default true).
    #[serde(default)]
    pub track: Option<bool>,
}

/// (No `deny_unknown_fields`: the create body flattens this next to `confirm`.)
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NewTemplate {
    pub name: String,
    pub language: String,
    /// MARKETING or UTILITY.
    pub category: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub header_example: Option<String>,
    pub body: String,
    /// One example per body variable ({{1}}, {{2}} ...), in order.
    #[serde(default)]
    pub body_examples: Vec<String>,
    #[serde(default)]
    pub footer: Option<String>,
    #[serde(default)]
    pub buttons: Vec<NewButton>,
}

fn positional(text: &str, what: &str, max: usize) -> AppResult<usize> {
    let names = placeholders(text);
    let opens = text.matches("{{").count();
    if opens != text.matches("}}").count() || opens < names.len() {
        return Err(AppError::bad(format!("{what}: write variables as {{{{1}}}}, {{{{2}}}} and so on")));
    }
    for (i, n) in names.iter().enumerate() {
        if n != &(i + 1).to_string() {
            return Err(AppError::bad(format!("{what}: number the variables {{{{1}}}}, {{{{2}}}} ... in order, with no gaps")));
        }
    }
    if names.len() > max {
        return Err(AppError::bad(format!("{what} can have at most {max} variable{}", if max == 1 { "" } else { "s" })));
    }
    Ok(names.len())
}

/// Check a new template against Meta's rules and build what's submitted, with the buttons
/// DumbMail tracks. Nothing is sent.
pub fn build(n: &NewTemplate, link_base: &str) -> AppResult<(Value, Value)> {
    let name = n.name.trim();
    if name.is_empty() || name.len() > 512 || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(AppError::bad("Template names use only lowercase letters, digits and underscores, like order_update"));
    }
    let lang = n.language.trim();
    let lang_ok = {
        let (l, r) = lang.split_once('_').map_or((lang, None), |(a, b)| (a, Some(b)));
        (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase()) && r.is_none_or(|r| (2..=4).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_alphanumeric()))
    };
    if !lang_ok {
        return Err(AppError::bad("Language is a code like en, en_US or pt_BR"));
    }
    let category = n.category.trim().to_ascii_uppercase();
    if !matches!(category.as_str(), "MARKETING" | "UTILITY") {
        return Err(AppError::bad("Category is MARKETING (promotions) or UTILITY (order and booking updates)"));
    }
    let mut components = Vec::new();
    if let Some(h) = n.header.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        if h.chars().count() > 60 {
            return Err(AppError::bad("The header can be at most 60 characters"));
        }
        let vars = positional(h, "The header", 1)?;
        let mut c = json!({"type": "HEADER", "format": "TEXT", "text": h});
        if vars == 1 {
            let ex = n.header_example.as_deref().map(str::trim).filter(|e| !e.is_empty()).ok_or_else(|| AppError::bad("Give an example for the header's variable"))?;
            c["example"] = json!({"header_text": [ex]});
        }
        components.push(c);
    }
    let body = n.body.trim();
    if body.is_empty() || body.chars().count() > 1024 {
        return Err(AppError::bad("Write the message (up to 1,024 characters)"));
    }
    let vars = positional(body, "The message", 60)?;
    if vars > 0 {
        let first_or_last = body.starts_with("{{") || body.ends_with("}}");
        if first_or_last {
            return Err(AppError::bad("Meta doesn't allow a variable at the very start or end of the message; add words around it"));
        }
        let examples: Vec<&str> = n.body_examples.iter().map(|e| e.trim()).collect();
        if examples.len() != vars || examples.iter().any(|e| e.is_empty() || e.chars().count() > 200) {
            return Err(AppError::bad(format!("Give one example for each of the message's {vars} variable{}", if vars == 1 { "" } else { "s" })));
        }
        components.push(json!({"type": "BODY", "text": body, "example": {"body_text": [examples]}}));
    } else {
        components.push(json!({"type": "BODY", "text": body}));
    }
    if let Some(f) = n.footer.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        if f.chars().count() > 60 || f.contains("{{") {
            return Err(AppError::bad("The footer is up to 60 characters, with no variables"));
        }
        components.push(json!({"type": "FOOTER", "text": f}));
    }
    let mut tracked = serde_json::Map::new();
    if !n.buttons.is_empty() {
        if n.buttons.len() > 10 {
            return Err(AppError::bad("At most 10 buttons"));
        }
        let kinds: Vec<&str> = n.buttons.iter().map(|b| b.kind.as_str()).collect();
        // Quick replies stay together (all before or all after the link buttons).
        let switches = kinds.windows(2).filter(|w| (w[0] == "quick_reply") != (w[1] == "quick_reply")).count();
        if switches > 1 {
            return Err(AppError::bad("Keep quick reply buttons together, before or after the link buttons"));
        }
        if kinds.iter().filter(|k| **k == "url").count() > 2 {
            return Err(AppError::bad("At most 2 link buttons"));
        }
        let mut buttons = Vec::new();
        for (i, b) in n.buttons.iter().enumerate() {
            let text = b.text.trim();
            if text.is_empty() || text.chars().count() > 25 {
                return Err(AppError::bad("Button labels are 1 to 25 characters"));
            }
            match b.kind.as_str() {
                "quick_reply" => buttons.push(json!({"type": "QUICK_REPLY", "text": text})),
                "url" => {
                    let url = b.url.as_deref().map(str::trim).unwrap_or_default();
                    let parsed = url::Url::parse(url).map_err(|_| AppError::bad(format!("Button {:?}: give a full link starting with https://", text)))?;
                    if parsed.scheme() != "https" || url.len() > 2000 || url.contains("{{") || !parsed.username().is_empty() {
                        return Err(AppError::bad(format!("Button {text:?}: give a full https:// link (up to 2,000 characters, no variables)")));
                    }
                    if b.track.unwrap_or(true) {
                        tracked.insert(i.to_string(), json!(url));
                        buttons.push(json!({"type": "URL", "text": text, "url": format!("{link_base}/wa/{{{{1}}}}"), "example": [format!("{link_base}/wa/example")]}));
                    } else {
                        buttons.push(json!({"type": "URL", "text": text, "url": url}));
                    }
                }
                other => return Err(AppError::bad(format!("Unknown button type {other:?}: use quick_reply or url"))),
            }
        }
        components.push(json!({"type": "BUTTONS", "buttons": buttons}));
    }
    let submit = json!({"name": name, "language": lang, "category": category, "components": components});
    Ok((submit, Value::Object(tracked)))
}

/// Submit a template for Meta's review and keep it (status as Meta answered, usually PENDING).
pub async fn create(state: &AppState, account_id: Uuid, n: &NewTemplate) -> AppResult<Template> {
    let Some((waba_id, _, token)) = settings::load_reader(state, account_id).await? else {
        return Err(AppError::bad("Connect WhatsApp first"));
    };
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let (submit, tracked) = build(n, &crate::domains::link_base(state, &account))?;
    let answer = graph::create_template(&state.cfg, &token, &waba_id, &submit).await.map_err(|e| {
        if e.code == 100 && e.subcode == Some(2388024) {
            AppError::Conflict("A template with this name already exists in this language".into())
        } else if e.class() == graph::Class::RateLimited {
            AppError::LimitReached("Meta's limit for template changes was reached; try again in a while".into())
        } else {
            AppError::bad(format!("Meta didn't accept the template: {}", e.describe()))
        }
    })?;
    let meta_id = answer["id"].as_str().filter(|s| graph::valid_id(s)).ok_or_else(|| AppError::bad("Meta didn't return the template's id; sync templates to see it"))?;
    let status = answer["status"].as_str().unwrap_or("PENDING").to_ascii_uppercase();
    let category = answer["category"].as_str().map(str::to_ascii_uppercase).unwrap_or_else(|| submit["category"].as_str().unwrap_or("MARKETING").to_string());
    let q = format!(
        "INSERT INTO whatsapp_templates (id, account_id, meta_id, name, language, category, status, components, tracked_buttons, created_by_dumbmail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, true)
         ON CONFLICT (account_id, meta_id) DO UPDATE SET tracked_buttons = EXCLUDED.tracked_buttons, created_by_dumbmail = true
         RETURNING {COLS}"
    );
    Ok(sqlx::query_as::<_, Template>(&q)
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(meta_id)
        .bind(submit["name"].as_str())
        .bind(submit["language"].as_str())
        .bind(&category)
        .bind(&status)
        .bind(&submit["components"])
        .bind(&tracked)
        .fetch_one(&state.db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(body: &str) -> NewTemplate {
        NewTemplate {
            name: "order_update".into(),
            language: "en_US".into(),
            category: "UTILITY".into(),
            header: None,
            header_example: None,
            body: body.into(),
            body_examples: vec!["Alex".into()],
            footer: None,
            buttons: vec![],
        }
    }

    #[test]
    fn placeholders_in_order() {
        assert_eq!(placeholders("Hi {{1}}, order {{2}} for {{1}}"), vec!["1", "2"]);
        assert_eq!(placeholders("Hi {{ first_name }}"), vec!["first_name"]);
        assert!(placeholders("no vars").is_empty());
    }

    #[test]
    fn builds_and_checks() {
        let (s, t) = build(&new("Hi {{1}}, your order shipped."), "https://l.test").unwrap();
        assert_eq!(s["components"][0]["example"]["body_text"][0][0], "Alex");
        assert_eq!(t, json!({}));
        assert!(build(&new("{{1}} your order shipped."), "https://l.test").is_err(), "leading variable");
        assert!(build(&new("Hi {{2}} there."), "https://l.test").is_err(), "gap");
        let mut n = new("Hi {{1}}, see you.");
        n.name = "Order Update".into();
        assert!(build(&n, "https://l.test").is_err());
        let mut n = new("Hi {{1}}, see you.");
        n.buttons = vec![NewButton { kind: "url".into(), text: "Shop".into(), url: Some("https://shop.test/a".into()), track: None }, NewButton { kind: "quick_reply".into(), text: "Stop".into(), url: None, track: None }];
        let (s, t) = build(&n, "https://l.test").unwrap();
        assert_eq!(s["components"][1]["buttons"][0]["url"], "https://l.test/wa/{{1}}");
        assert_eq!(t["0"], "https://shop.test/a");
        n.buttons.push(NewButton { kind: "url".into(), text: "More".into(), url: Some("https://shop.test/b".into()), track: Some(false) });
        assert!(build(&n, "https://l.test").is_err(), "quick replies split the link buttons");
    }
}

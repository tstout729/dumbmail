//! What the REST API and the MCP tools do: account languages, a message's translations (read,
//! change, mark reviewed, export, import, draft with AI), the glossary, system texts, previews and
//! test sends in a language, and results by language.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::fields;
use super::store::{self, Target};
use super::{draft, exchange, locale, texts};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::Perm;
use crate::features::team::audit;
use crate::state::AppState;

/// The permission reading a message's translations needs.
pub fn read_perm(t: &Target) -> Perm {
    match t {
        Target::Campaign { .. } | Target::FlowStep { .. } => Perm::MessagingRead,
        _ => Perm::ContentRead,
    }
}

/// The permission changing them needs: the one that edits that message.
pub fn write_perm(t: &Target) -> Perm {
    match t {
        Target::Campaign { .. } => Perm::CampaignDraft,
        Target::FlowStep { .. } => Perm::FlowEdit,
        _ => Perm::ContentWrite,
    }
}

/// Whether `ctx` may change a scheduled campaign (people who can send campaigns).
fn scheduled_ok(ctx: &Ctx) -> bool {
    crate::api::messaging::editable_for(ctx).contains(&"scheduled")
}

/// DumbMail's own hosts for this account (see `translations::own_hosts`).
async fn own_hosts(state: &AppState, account_id: Uuid) -> AppResult<Vec<String>> {
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    Ok(super::own_hosts(state, Some(&account)))
}

/// The identity an exported file carries for a stored message: `campaign/<id>[/<version>]`,
/// `flow/<id>/<step>`, `template/<id>`, `saved_block/<id>`.
pub fn message_id(t: &Target) -> String {
    format!("{}/{}{}", t.kind(), t.id(), if t.part().is_empty() { String::new() } else { format!("/{}", t.part()) })
}

/// The identity for content sent by the editor: what it says (bounded), else "unsaved".
fn content_message(m: Option<&str>) -> String {
    m.map(str::trim).filter(|m| !m.is_empty() && m.len() <= 200 && !m.chars().any(char::is_control)).unwrap_or("unsaved").to_string()
}

fn read_file(format: &str, data: &str) -> AppResult<exchange::Imported> {
    match format {
        "xliff" | "xlf" => exchange::from_xliff(data),
        "csv" => exchange::from_csv(data),
        o => Err(AppError::bad(format!("format must be xliff or csv (got {o:?})"))),
    }
}

// ---------- Account languages ----------

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = texts::settings(&state.db, account_id).await?;
    let (used, limit) = draft::usage_today(state, account_id).await?;
    let names: Map<String, Value> = s.all().iter().map(|l| (l.clone(), json!(super::check::label(l)))).collect();
    Ok(json!({
        "default_language": s.default_language,
        "languages": s.languages,
        "names": names,
        "max_languages": fields::MAX_LANGUAGES,
        "machine_translation": {"available": draft::available(state), "model": super::ai::MODEL, "daily_characters": limit, "used_today": used, "price_per_1000_characters_usd": if state.cfg.hosted { state.cfg.price_translation_per_1000_chars } else { 0.0 }},
        "system_text_languages": texts::SHIPPED,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub default_language: Option<String>,
    pub languages: Option<Vec<String>>,
}

pub async fn set_settings(state: &AppState, ctx: &Ctx, b: SettingsBody) -> AppResult<Value> {
    let cur = texts::settings(&state.db, ctx.account_id).await?;
    let default = match b.default_language.as_deref() {
        Some(d) => locale::normalize(d).ok_or_else(|| AppError::bad(format!("{d:?} isn't a language tag like en or fr-CA")))?,
        None => cur.default_language.clone(),
    };
    let raw = b.languages.unwrap_or(cur.languages.clone());
    if raw.len() > fields::MAX_LANGUAGES {
        return Err(AppError::bad(format!("an account can send in at most {} languages besides its default", fields::MAX_LANGUAGES)));
    }
    let mut langs: Vec<String> = Vec::new();
    for l in &raw {
        let n = locale::normalize(l).ok_or_else(|| AppError::bad(format!("{l:?} isn't a language tag like fr or pt-BR")))?;
        if n != default && !langs.contains(&n) {
            langs.push(n);
        }
    }
    sqlx::query(
        "INSERT INTO account_languages (account_id, default_language, languages) VALUES ($1, $2, $3)
         ON CONFLICT (account_id) DO UPDATE SET default_language = EXCLUDED.default_language, languages = EXCLUDED.languages, updated_at = now()",
    )
    .bind(ctx.account_id)
    .bind(&default)
    .bind(&langs)
    .execute(&state.db)
    .await?;
    texts::forget(ctx.account_id);
    audit(state, ctx, "languages.update", None, json!({"default_language": default, "languages": langs})).await;
    settings_json(state, ctx.account_id).await
}

/// A language a message may be translated into: a tag the account sends in, not the default.
async fn check_lang(state: &AppState, account_id: Uuid, raw: &str, default: &str) -> AppResult<String> {
    let l = locale::normalize(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a language tag like fr or pt-BR")))?;
    if l == default {
        return Err(AppError::bad(format!("{l} is this message's default language; translations are for the other languages")));
    }
    let s = texts::settings(&state.db, account_id).await?;
    if !s.languages.contains(&l) {
        return Err(AppError::bad(format!("add {l} to your languages in Settings → Languages first")));
    }
    Ok(l)
}

// ---------- A message's translations ----------

fn default_of(content: &Value, s: &texts::Settings) -> String {
    fields::languages(content).0.unwrap_or_else(|| s.default_language.clone())
}

pub async fn get(state: &AppState, ctx: &Ctx, t: &Target, lang: Option<&str>) -> AppResult<Value> {
    ctx.require(read_perm(t))?;
    let l = store::load(&state.db, ctx.account_id, t).await?;
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let default = default_of(&l.content, &s);
    let (_, langs) = fields::languages(&l.content);
    let selected = match lang {
        Some(raw) => Some(locale::normalize(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a language tag")))?),
        None => langs.first().cloned().or_else(|| s.languages.first().cloned()),
    };
    let strings: Vec<Value> = fields::fields(&l.content)
        .into_iter()
        .map(|f| {
            let (text, status) = match &selected {
                Some(lg) => (fields::entry(&l.content, &f.id, lg).and_then(|e| e.get("text")).cloned().unwrap_or(json!("")), json!(fields::status(&l.content, &f, lg))),
                None => (json!(""), json!("missing")),
            };
            json!({"id": f.id, "where": f.label, "kind": f.kind.name(), "source": f.source, "text": text, "status": status})
        })
        .collect();
    let summary: Map<String, Value> = langs.iter().map(|lg| (lg.clone(), Value::Object(fields::summary(&l.content, lg)))).collect();
    Ok(json!({
        "target": {"kind": t.kind(), "id": t.id(), "part": t.part()},
        "name": l.name,
        "channel": l.channel,
        "default_language": default,
        "languages": langs,
        "account_languages": s.languages,
        "language": selected,
        "strings": strings,
        "summary": summary,
        "machine_translation": draft::available(state),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetString {
    pub id: String,
    /// Empty or missing clears the translation (the default text is sent).
    #[serde(default)]
    pub text: Option<String>,
    /// reviewed (default) or machine.
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetBody {
    #[serde(default)]
    pub version: Option<String>,
    pub language: String,
    #[serde(default)]
    pub strings: Vec<SetString>,
    /// Take the language off this message (all its translations).
    #[serde(default)]
    pub remove_language: bool,
}

pub async fn set(state: &AppState, ctx: &Ctx, t: &Target, b: SetBody) -> AppResult<Value> {
    let own = own_hosts(state, ctx.account_id).await?;
    ctx.require(write_perm(t))?;
    if b.strings.len() > fields::MAX_ENTRIES {
        return Err(AppError::bad(format!("at most {} strings at a time", fields::MAX_ENTRIES)));
    }
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let cur = store::load(&state.db, ctx.account_id, t).await?;
    let default = default_of(&cur.content, &s);
    let lang = if b.remove_language { locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))? } else { check_lang(state, ctx.account_id, &b.language, &default).await? };
    let saved_block = matches!(t, Target::SavedBlock { .. });
    let n = store::update(&state.db, ctx.account_id, t, scheduled_ok(ctx), |l| {
        if b.remove_language {
            fields::remove_language(&mut l.content, &lang);
            return Ok(0);
        }
        let current = fields::fields(&l.content);
        for st in &b.strings {
            let f = current.iter().find(|f| f.id == st.id).ok_or_else(|| AppError::bad(format!("this message has no translatable text {:?}", st.id)))?;
            let status = match st.status.as_deref() {
                None | Some("reviewed") => fields::REVIEWED,
                Some("machine") => fields::MACHINE,
                Some(o) => return Err(AppError::bad(format!("status must be reviewed or machine (got {o:?})"))),
            };
            match st.text.as_deref().filter(|t| !t.is_empty()) {
                Some(text) => {
                    fields::check_text_for(f.kind, &f.source, text, &own).map_err(|e| AppError::bad(format!("{} ({}): {e}", f.label, f.id)))?;
                    fields::set_entry(&mut l.content, &f.id, &lang, Some((text, status)))?;
                }
                None => fields::set_entry(&mut l.content, &f.id, &lang, None)?,
            }
        }
        if !saved_block {
            fields::add_language(&mut l.content, &default, &lang);
        }
        Ok(b.strings.len())
    })
    .await?;
    audit(state, ctx, if b.remove_language { "translation.remove_language" } else { "translation.update" }, Some(&t.describe()), json!({"language": lang, "strings": n})).await;
    get(state, ctx, t, Some(&lang)).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewBody {
    #[serde(default)]
    pub version: Option<String>,
    pub language: String,
    /// Only these field ids (default: every machine draft in the language).
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

pub async fn review(state: &AppState, ctx: &Ctx, t: &Target, b: ReviewBody) -> AppResult<Value> {
    ctx.require(write_perm(t))?;
    let lang = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    let n = store::update(&state.db, ctx.account_id, t, scheduled_ok(ctx), |l| fields::mark_reviewed(&mut l.content, &lang, b.ids.as_deref())).await?;
    audit(state, ctx, "translation.review", Some(&t.describe()), json!({"language": lang, "strings": n})).await;
    let mut v = get(state, ctx, t, Some(&lang)).await?;
    v["reviewed"] = json!(n);
    Ok(v)
}

/// The glossary as lines for translators (`term → translation`, `term: never translate`).
async fn glossary_lines(state: &AppState, account_id: Uuid, lang: &str) -> AppResult<Vec<String>> {
    Ok(draft::glossary_for(&state.db, account_id, lang)
        .await?
        .into_iter()
        .filter_map(|t| match (t.never_translate, t.translation) {
            (true, _) => Some(format!("{}: never translate", t.term)),
            (false, Some(x)) if !x.trim().is_empty() => Some(format!("{} → {x}", t.term)),
            _ => None,
        })
        .collect())
}

/// (file name, media type, body)
pub async fn export(state: &AppState, ctx: &Ctx, t: &Target, lang: &str, format: &str) -> AppResult<(String, &'static str, String)> {
    ctx.require(read_perm(t))?;
    let l = store::load(&state.db, ctx.account_id, t).await?;
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let default = default_of(&l.content, &s);
    let lang = locale::normalize(lang).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    let base: String = l.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>().split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let base = if base.is_empty() { t.kind().to_string() } else { base.chars().take(60).collect() };
    match format {
        "xliff" | "xlf" | "" => {
            let g = glossary_lines(state, ctx.account_id, &lang).await?;
            Ok((format!("{base}.{lang}.xlf"), "application/x-xliff+xml; charset=utf-8", exchange::to_xliff(&l.content, &message_id(t), &default, &lang, &g)))
        }
        "csv" => Ok((format!("{base}.{lang}.csv"), "text/csv; charset=utf-8", exchange::to_csv(&l.content, &lang, &message_id(t))?)),
        o => Err(AppError::bad(format!("format must be xliff or csv (got {o:?})"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportBody {
    #[serde(default)]
    pub version: Option<String>,
    pub language: String,
    /// xliff or csv
    pub format: String,
    /// The file's text.
    pub data: String,
}

pub async fn import(state: &AppState, ctx: &Ctx, t: &Target, b: ImportBody) -> AppResult<Value> {
    let own = own_hosts(state, ctx.account_id).await?;
    ctx.require(write_perm(t))?;
    let imported = read_file(&b.format, &b.data)?;
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let cur = store::load(&state.db, ctx.account_id, t).await?;
    let default = default_of(&cur.content, &s);
    let lang = check_lang(state, ctx.account_id, &b.language, &default).await?;
    imported.check(&message_id(t), &lang)?;
    let units = imported.units;
    let saved_block = matches!(t, Target::SavedBlock { .. });
    let summary = store::update(&state.db, ctx.account_id, t, scheduled_ok(ctx), |l| {
        let r = exchange::apply(&mut l.content, &lang, &units, &own);
        if !saved_block && r["applied"].as_u64().unwrap_or(0) > 0 {
            fields::add_language(&mut l.content, &default, &lang);
        }
        Ok(r)
    })
    .await?;
    audit(state, ctx, "translation.import", Some(&t.describe()), json!({"language": lang, "format": b.format, "applied": summary["applied"]})).await;
    let mut v = get(state, ctx, t, Some(&lang)).await?;
    v["import"] = summary;
    Ok(v)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftBody {
    #[serde(default)]
    pub version: Option<String>,
    pub language: String,
    /// Only these field ids (default: every text that's missing or out of date).
    #[serde(default)]
    pub ids: Option<Vec<String>>,
    /// MCP only: without it, says what a run would translate and cost.
    #[serde(default)]
    pub confirm: bool,
}

pub async fn start_draft(state: &AppState, ctx: &Ctx, t: &Target, b: &DraftBody, dry_run: bool) -> AppResult<Value> {
    ctx.require(write_perm(t))?;
    ctx.require(Perm::ContentWrite)?;
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let cur = store::load(&state.db, ctx.account_id, t).await?;
    let lang = check_lang(state, ctx.account_id, &b.language, &default_of(&cur.content, &s)).await?;
    if b.ids.as_ref().is_some_and(|i| i.len() > fields::MAX_ENTRIES) {
        return Err(AppError::bad("too many ids"));
    }
    // MCP always quotes first; REST quotes first when the run costs money. Either way it runs only
    // with confirm=true.
    if dry_run || !b.confirm {
        let mut p = draft::plan(state, ctx.account_id, t, &lang, b.ids.as_deref()).await?;
        if dry_run || p["price_usd"].as_f64().unwrap_or(0.0) > 0.0 {
            p["dry_run"] = json!(true);
            p["note"] = json!("Nothing was translated. Call again with confirm=true to draft these with AI.");
            return Ok(p);
        }
    }
    draft::start(state, ctx, t, &lang, b.ids.as_deref(), scheduled_ok(ctx)).await
}

// ---------- Glossary ----------

pub const MAX_TERMS: i64 = 500;

pub async fn glossary(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, bool, Value, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, term, never_translate, translations, note, updated_at FROM translation_glossary WHERE account_id = $1 ORDER BY lower(term) LIMIT 500").bind(account_id).fetch_all(&state.db).await?;
    Ok(json!({"data": rows.into_iter().map(|r| json!({"id": r.0, "term": r.1, "never_translate": r.2, "translations": r.3, "note": r.4, "updated_at": r.5})).collect::<Vec<_>>()}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermBody {
    pub term: Option<String>,
    pub never_translate: Option<bool>,
    /// language → fixed translation
    pub translations: Option<Map<String, Value>>,
    pub note: Option<String>,
}

fn check_term(b: &TermBody) -> AppResult<(Option<String>, Option<Value>)> {
    let term = b.term.as_deref().map(str::trim).map(|t| {
        if t.is_empty() || t.chars().count() > 100 || t.chars().any(char::is_control) {
            Err(AppError::bad("a glossary term is 1 to 100 characters"))
        } else {
            Ok(t.to_string())
        }
    });
    let term = term.transpose()?;
    let tr = match &b.translations {
        None => None,
        Some(m) => {
            if m.len() > fields::MAX_LANGUAGES {
                return Err(AppError::bad(format!("at most {} languages per term", fields::MAX_LANGUAGES)));
            }
            let mut out = Map::new();
            for (k, v) in m {
                let l = locale::normalize(k).ok_or_else(|| AppError::bad(format!("{k:?} isn't a language tag")))?;
                let t = v.as_str().map(str::trim).unwrap_or("");
                if t.chars().count() > 200 || t.chars().any(char::is_control) {
                    return Err(AppError::bad("a term's translation is at most 200 characters"));
                }
                if !t.is_empty() {
                    out.insert(l, json!(t));
                }
            }
            Some(Value::Object(out))
        }
    };
    if b.note.as_deref().is_some_and(|n| n.chars().count() > 500) {
        return Err(AppError::bad("a note is at most 500 characters"));
    }
    Ok((term, tr))
}

fn term_taken(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => AppError::Conflict("that term is already in the glossary".into()),
        _ => e.into(),
    }
}

pub async fn add_term(state: &AppState, ctx: &Ctx, b: TermBody) -> AppResult<Value> {
    let (term, tr) = check_term(&b)?;
    let term = term.ok_or_else(|| AppError::bad("term is required"))?;
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 79))").bind(ctx.account_id).execute(&mut *tx).await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM translation_glossary WHERE account_id = $1").bind(ctx.account_id).fetch_one(&mut *tx).await?;
    if n >= MAX_TERMS {
        return Err(AppError::Conflict(format!("a glossary holds {MAX_TERMS} terms")));
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO translation_glossary (id, account_id, term, never_translate, translations, note) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(ctx.account_id)
        .bind(&term)
        .bind(b.never_translate.unwrap_or(false))
        .bind(tr.unwrap_or(json!({})))
        .bind(b.note.unwrap_or_default())
        .execute(&mut *tx)
        .await
        .map_err(term_taken)?;
    tx.commit().await?;
    audit(state, ctx, "glossary.add", Some(&id.to_string()), json!({})).await;
    Ok(json!({"id": id, "term": term}))
}

pub async fn update_term(state: &AppState, ctx: &Ctx, id: Uuid, b: TermBody) -> AppResult<Value> {
    let (term, tr) = check_term(&b)?;
    let n = sqlx::query(
        "UPDATE translation_glossary SET term = COALESCE($3, term), never_translate = COALESCE($4, never_translate), translations = COALESCE($5, translations), note = COALESCE($6, note), updated_at = now()
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(term)
    .bind(b.never_translate)
    .bind(tr)
    .bind(b.note)
    .execute(&state.db)
    .await
    .map_err(term_taken)?
    .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("glossary term"));
    }
    audit(state, ctx, "glossary.update", Some(&id.to_string()), json!({})).await;
    Ok(json!({"id": id, "ok": true}))
}

pub async fn delete_term(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let n = sqlx::query("DELETE FROM translation_glossary WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("glossary term"));
    }
    audit(state, ctx, "glossary.delete", Some(&id.to_string()), json!({})).await;
    Ok(json!({"ok": true}))
}

// ---------- System texts ----------

pub async fn system_texts(state: &AppState, account_id: Uuid, lang: &str) -> AppResult<Value> {
    let l = locale::normalize(lang).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    let s = texts::settings_for(&state.db, account_id, std::slice::from_ref(&l)).await?;
    let items: Vec<Value> = texts::KEYS
        .iter()
        .map(|(k, what, en)| {
            let own = s.texts.get(&(l.clone(), k.to_string()));
            json!({"key": k, "where": what, "english": en, "shipped": texts::shipped_text(&l, k), "text": own, "shown": own.map(String::as_str).or_else(|| texts::shipped_text(&l, k)).unwrap_or(en)})
        })
        .collect();
    Ok(json!({"language": l, "shipped": texts::SHIPPED.contains(&locale::primary(&l)), "rtl": locale::is_rtl(&l), "texts": items}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextsBody {
    pub language: String,
    /// key → your wording (null or "" goes back to the shipped text)
    pub texts: Map<String, Value>,
}

pub async fn set_system_texts(state: &AppState, ctx: &Ctx, b: TextsBody) -> AppResult<Value> {
    let s = texts::settings(&state.db, ctx.account_id).await?;
    let l = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    if !s.all().contains(&l) {
        return Err(AppError::bad(format!("add {l} to your languages in Settings → Languages first")));
    }
    if b.texts.len() > texts::KEYS.len() {
        return Err(AppError::bad("too many texts"));
    }
    let mut tx = state.db.begin().await?;
    for (k, v) in &b.texts {
        let (_, _, en) = texts::KEYS.iter().find(|(key, _, _)| key == k).ok_or_else(|| AppError::bad(format!("{k:?} isn't a system text")))?;
        match v.as_str().map(str::trim).filter(|t| !t.is_empty()) {
            None => {
                sqlx::query("DELETE FROM system_texts WHERE account_id = $1 AND language = $2 AND key = $3").bind(ctx.account_id).bind(&l).bind(k).execute(&mut *tx).await?;
            }
            Some(t) => {
                if t.chars().count() > texts::MAX_TEXT || t.chars().any(char::is_control) {
                    return Err(AppError::bad(format!("{k}: at most {} characters on one line", texts::MAX_TEXT)));
                }
                super::structure::hidden_controls(t).map_err(|e| AppError::bad(format!("{k}: {e}")))?;
                if texts::OPT_OUT_KEYS.contains(&k.as_str()) && !super::structure::decode_entities(t).chars().any(char::is_alphanumeric) {
                    return Err(AppError::bad(format!("{k}: this is how people opt out, so it must show words")));
                }
                if texts::placeholders(t) != texts::placeholders(en) {
                    return Err(AppError::bad(format!("{k}: keep the placeholders of the original ({})", texts::placeholders(en).join(", "))));
                }
                sqlx::query(
                    "INSERT INTO system_texts (account_id, language, key, text) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (account_id, language, key) DO UPDATE SET text = EXCLUDED.text, updated_at = now()",
                )
                .bind(ctx.account_id)
                .bind(&l)
                .bind(k)
                .bind(t)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    texts::forget(ctx.account_id);
    audit(state, ctx, "system_texts.update", None, json!({"language": l, "texts": b.texts.len()})).await;
    system_texts(state, ctx.account_id, &l).await
}

// ---------- Preview, test send, report ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewBody {
    pub channel: String,
    pub content: Value,
    pub language: String,
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub event: Option<Value>,
}

pub async fn preview(state: &AppState, ctx: &Ctx, b: PreviewBody) -> AppResult<Value> {
    let lang = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    // A real person's data only for people who may read profiles (as the plain preview).
    let profile_id = b.profile_id.filter(|_| ctx.can(Perm::ProfilesRead));
    let body = crate::api::messaging::RenderBody { channel: b.channel, content: b.content, profile_id, event: b.event };
    let mut v = super::pinned(lang.clone(), crate::api::messaging::render_preview(state, ctx.account_id, body)).await?;
    v["language"] = json!(lang);
    Ok(v)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestBody {
    pub to: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub language: String,
    /// MCP only.
    #[serde(default)]
    pub confirm: bool,
}

/// A test of a campaign in one language, with the same limits as any test send.
pub async fn test_send(state: &AppState, ctx: &Ctx, id: Uuid, b: &TestBody) -> AppResult<Value> {
    let lang = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    let sent = super::pinned(lang.clone(), crate::features::team::sends::test_send(state, ctx, id, &b.to, b.version.as_deref())).await?;
    Ok(json!({"sent": sent, "language": lang}))
}

#[derive(Debug, Deserialize, Default)]
pub struct ReportQuery {
    pub campaign_id: Option<Uuid>,
    pub flow_id: Option<Uuid>,
}

/// Results of a campaign or flow by the language each message was sent in.
pub async fn report(state: &AppState, account_id: Uuid, q: &ReportQuery) -> AppResult<Value> {
    let (col, id) = match (q.campaign_id, q.flow_id) {
        (Some(c), None) => ("campaign_id", c),
        (None, Some(f)) => ("flow_id", f),
        _ => return Err(AppError::bad("give campaign_id or flow_id")),
    };
    let owned: bool = sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM {} WHERE id = $1 AND account_id = $2)", if col == "campaign_id" { "campaigns" } else { "flows" }))
        .bind(id)
        .bind(account_id)
        .fetch_one(&state.db)
        .await?;
    if !owned {
        return Err(AppError::not_found(if col == "campaign_id" { "campaign" } else { "flow" }));
    }
    type Row = (Option<String>, i64, i64, i64, i64);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT language, count(*) FILTER (WHERE status IN ('sent', 'delivered')), count(opened_at), count(clicked_at), count(unsubscribed_at)
         FROM messages WHERE account_id = $1 AND {col} = $2 AND NOT is_test GROUP BY language ORDER BY 2 DESC LIMIT 50"
    ))
    .bind(account_id)
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let rate = |a: i64, b: i64| if b > 0 { (a as f64) / (b as f64) } else { 0.0 };
    Ok(json!({"data": rows.into_iter().map(|(l, sent, opened, clicked, unsub)| json!({
        "language": l, "name": l.as_deref().map(super::check::label), "sent": sent, "opened": opened, "clicked": clicked, "unsubscribed": unsub,
        "open_rate": rate(opened, sent), "click_rate": rate(clicked, sent),
    })).collect::<Vec<_>>(), "note": "Messages sent before the message had language versions have no language."}))
}

pub async fn jobs(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM translation_jobs WHERE account_id = $1 ORDER BY created_at DESC LIMIT 20").bind(account_id).fetch_all(&state.db).await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(draft::job_json(state, account_id, id).await?);
    }
    Ok(json!({"data": out}))
}

// ---------- REST handlers ----------

#[derive(Debug, Deserialize, Default)]
pub struct MsgQuery {
    pub version: Option<String>,
    pub language: Option<String>,
    pub format: Option<String>,
}

fn campaign(id: Uuid, version: Option<String>) -> AppResult<Target> {
    Target::Campaign { id, version }.checked()
}

fn file(name: String, media: &'static str, body: String) -> Response {
    let disposition = format!("attachment; filename=\"{}\"", name.replace(['"', '\\', '\r', '\n'], ""));
    ([(header::CONTENT_TYPE, media.to_string()), (header::CONTENT_DISPOSITION, disposition)], body).into_response()
}

macro_rules! target_handlers {
    ($get:ident, $set:ident, $review:ident, $export:ident, $import:ident, $draft:ident, $path:ty, $make:expr) => {
        pub async fn $get(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, Query(q): Query<MsgQuery>) -> AppResult<Json<Value>> {
            let t = ($make)(p, q.version.clone())?;
            Ok(Json(get(&state, &ctx, &t, q.language.as_deref()).await?))
        }
        pub async fn $set(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, ApiJson(b): ApiJson<SetBody>) -> AppResult<Json<Value>> {
            let t = ($make)(p, b.version.clone())?;
            Ok(Json(set(&state, &ctx, &t, b).await?))
        }
        pub async fn $review(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, ApiJson(b): ApiJson<ReviewBody>) -> AppResult<Json<Value>> {
            let t = ($make)(p, b.version.clone())?;
            Ok(Json(review(&state, &ctx, &t, b).await?))
        }
        pub async fn $export(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, Query(q): Query<MsgQuery>) -> AppResult<Response> {
            let t = ($make)(p, q.version.clone())?;
            let lang = q.language.as_deref().ok_or_else(|| AppError::bad("give language"))?;
            let (name, media, body) = export(&state, &ctx, &t, lang, q.format.as_deref().unwrap_or("xliff")).await?;
            Ok(file(name, media, body))
        }
        pub async fn $import(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, ApiJson(b): ApiJson<ImportBody>) -> AppResult<Json<Value>> {
            let t = ($make)(p, b.version.clone())?;
            Ok(Json(import(&state, &ctx, &t, b).await?))
        }
        pub async fn $draft(State(state): State<AppState>, ctx: Ctx, Path(p): Path<$path>, ApiJson(b): ApiJson<DraftBody>) -> AppResult<Json<Value>> {
            let t = ($make)(p, b.version.clone())?;
            Ok(Json(start_draft(&state, &ctx, &t, &b, false).await?))
        }
    };
}

target_handlers!(campaign_get, campaign_set, campaign_review, campaign_export, campaign_import, campaign_draft, Uuid, |id: Uuid, v: Option<String>| campaign(id, v));
target_handlers!(flow_get, flow_set, flow_review, flow_export, flow_import, flow_draft, (Uuid, String), |(id, node): (Uuid, String), _v: Option<String>| Target::FlowStep { id, node }.checked());
target_handlers!(template_get, template_set, template_review, template_export, template_import, template_draft, Uuid, |id: Uuid, _v: Option<String>| Ok::<_, AppError>(Target::Template { id }));
target_handlers!(saved_get, saved_set, saved_review, saved_export, saved_import, saved_draft, Uuid, |id: Uuid, _v: Option<String>| Ok::<_, AppError>(Target::SavedBlock { id }));

pub async fn campaign_test(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TestBody>) -> AppResult<Json<Value>> {
    Ok(Json(test_send(&state, &ctx, id, &b).await?))
}

pub async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

pub async fn set_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_settings(&state, &ctx, b).await?))
}

pub async fn glossary_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(glossary(&state, ctx.account_id).await?))
}

pub async fn add_term_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TermBody>) -> AppResult<Json<Value>> {
    Ok(Json(add_term(&state, &ctx, b).await?))
}

pub async fn update_term_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TermBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_term(&state, &ctx, id, b).await?))
}

pub async fn delete_term_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(delete_term(&state, &ctx, id).await?))
}

#[derive(Debug, Deserialize)]
pub struct LangOnly {
    pub language: Option<String>,
}

pub async fn system_texts_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<LangOnly>) -> AppResult<Json<Value>> {
    let s = texts::settings(&state.db, ctx.account_id).await?;
    Ok(Json(system_texts(&state, ctx.account_id, q.language.as_deref().unwrap_or(&s.default_language)).await?))
}

pub async fn set_system_texts_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TextsBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_system_texts(&state, &ctx, b).await?))
}

pub async fn preview_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<PreviewBody>) -> AppResult<Json<Value>> {
    Ok(Json(preview(&state, &ctx, b).await?))
}

pub async fn report_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ReportQuery>) -> AppResult<Json<Value>> {
    Ok(Json(report(&state, ctx.account_id, &q).await?))
}

pub async fn jobs_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(jobs(&state, ctx.account_id).await?))
}

pub async fn job_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(draft::job_json(&state, ctx.account_id, id).await?))
}

// ---------- Working on unsaved content (the editor) ----------
//
// The editor keeps translations inside the content it's editing and saves them with the rest of
// the message. These work on content sent with the request and change nothing stored.

/// The largest content these accept.
pub const MAX_CONTENT_BYTES: usize = 1024 * 1024;

fn check_size(content: &Value) -> AppResult<()> {
    if !content.is_object() || serde_json::to_vec(content).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_CONTENT_BYTES {
        return Err(AppError::bad("content must be a message (1 MB at most)"));
    }
    Ok(())
}

/// A saved block's content is only blocks; messages have a subject or a body.
fn is_message(content: &Value) -> bool {
    content.get("subject").is_some() || content.get("body").is_some() || content.get("html").is_some()
}

fn inspect_json(content: &Value, s: &texts::Settings, lang: Option<&str>) -> Value {
    let default = default_of(content, s);
    let (_, langs) = fields::languages(content);
    let strings: Vec<Value> = fields::fields(content)
        .into_iter()
        .map(|f| {
            let (text, status) = match lang {
                Some(lg) => (fields::entry(content, &f.id, lg).and_then(|e| e.get("text")).cloned().unwrap_or(json!("")), json!(fields::status(content, &f, lg))),
                None => (json!(""), json!("missing")),
            };
            json!({"id": f.id, "where": f.label, "kind": f.kind.name(), "source": f.source, "text": text, "status": status})
        })
        .collect();
    let summary: Map<String, Value> = langs.iter().map(|lg| (lg.clone(), Value::Object(fields::summary(content, lg)))).collect();
    json!({"default_language": default, "languages": langs, "account_languages": s.languages, "language": lang, "strings": strings, "summary": summary})
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectBody {
    pub content: Value,
    #[serde(default)]
    pub language: Option<String>,
}

pub async fn inspect(state: &AppState, account_id: Uuid, b: InspectBody) -> AppResult<Value> {
    check_size(&b.content)?;
    let s = texts::settings(&state.db, account_id).await?;
    let lang = b.language.as_deref().map(|l| locale::normalize(l).ok_or_else(|| AppError::bad("language must be a language tag"))).transpose()?;
    Ok(inspect_json(&b.content, &s, lang.as_deref()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyBody {
    pub content: Value,
    pub language: String,
    #[serde(default)]
    pub strings: Vec<SetString>,
    #[serde(default)]
    pub remove_language: bool,
}

/// `content` with translations set (checked, with their source hashes filled in).
pub async fn apply(state: &AppState, account_id: Uuid, b: ApplyBody) -> AppResult<Value> {
    let own = own_hosts(state, account_id).await?;
    check_size(&b.content)?;
    let s = texts::settings(&state.db, account_id).await?;
    let mut content = b.content;
    let default = default_of(&content, &s);
    if b.remove_language {
        let lang = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
        fields::remove_language(&mut content, &lang);
        let mut v = inspect_json(&content, &s, None);
        v["content"] = content;
        return Ok(v);
    }
    let lang = check_lang(state, account_id, &b.language, &default).await?;
    let current = fields::fields(&content);
    for st in &b.strings {
        let f = current.iter().find(|f| f.id == st.id).ok_or_else(|| AppError::bad(format!("this message has no translatable text {:?}", st.id)))?;
        let status = if st.status.as_deref() == Some("machine") { fields::MACHINE } else { fields::REVIEWED };
        match st.text.as_deref().filter(|t| !t.is_empty()) {
            Some(text) => {
                fields::check_text_for(f.kind, &f.source, text, &own).map_err(|e| AppError::bad(format!("{} ({}): {e}", f.label, f.id)))?;
                fields::set_entry(&mut content, &f.id, &lang, Some((text, status)))?;
            }
            None => fields::set_entry(&mut content, &f.id, &lang, None)?,
        }
    }
    if is_message(&content) {
        fields::add_language(&mut content, &default, &lang);
    }
    fields::check_saved(&content)?;
    let mut v = inspect_json(&content, &s, Some(&lang));
    v["content"] = content;
    Ok(v)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportBody {
    pub content: Value,
    pub language: String,
    /// Which message this is (`campaign/<id>`, `template/<id>` …), so its file can only be
    /// imported back into it; leave it out for a message that isn't saved yet.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    /// For the file name.
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn export_content(state: &AppState, account_id: Uuid, b: ExportBody) -> AppResult<(String, &'static str, String)> {
    check_size(&b.content)?;
    let s = texts::settings(&state.db, account_id).await?;
    let default = default_of(&b.content, &s);
    let lang = locale::normalize(&b.language).ok_or_else(|| AppError::bad("language must be a language tag"))?;
    let message = content_message(b.message.as_deref());
    let name = b.name.unwrap_or_default();
    let base: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>().split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let base = if base.is_empty() { "message".to_string() } else { base.chars().take(60).collect() };
    match b.format.as_deref().unwrap_or("xliff") {
        "xliff" | "xlf" => {
            let g = glossary_lines(state, account_id, &lang).await?;
            Ok((format!("{base}.{lang}.xlf"), "application/x-xliff+xml; charset=utf-8", exchange::to_xliff(&b.content, &message, &default, &lang, &g)))
        }
        "csv" => Ok((format!("{base}.{lang}.csv"), "text/csv; charset=utf-8", exchange::to_csv(&b.content, &lang, &message)?)),
        o => Err(AppError::bad(format!("format must be xliff or csv (got {o:?})"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportContentBody {
    pub content: Value,
    pub language: String,
    /// The same `message` the file was exported with.
    #[serde(default)]
    pub message: Option<String>,
    pub format: String,
    pub data: String,
}

pub async fn import_content(state: &AppState, account_id: Uuid, b: ImportContentBody) -> AppResult<Value> {
    let own = own_hosts(state, account_id).await?;
    check_size(&b.content)?;
    let imported = read_file(&b.format, &b.data)?;
    let s = texts::settings(&state.db, account_id).await?;
    let mut content = b.content;
    let default = default_of(&content, &s);
    let lang = check_lang(state, account_id, &b.language, &default).await?;
    imported.check(&content_message(b.message.as_deref()), &lang)?;
    let units = imported.units;
    let summary = exchange::apply(&mut content, &lang, &units, &own);
    if is_message(&content) && summary["applied"].as_u64().unwrap_or(0) > 0 {
        fields::add_language(&mut content, &default, &lang);
    }
    let mut v = inspect_json(&content, &s, Some(&lang));
    v["content"] = content;
    v["import"] = summary;
    Ok(v)
}

pub async fn inspect_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<InspectBody>) -> AppResult<Json<Value>> {
    Ok(Json(inspect(&state, ctx.account_id, b).await?))
}

pub async fn apply_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ApplyBody>) -> AppResult<Json<Value>> {
    Ok(Json(apply(&state, ctx.account_id, b).await?))
}

pub async fn export_content_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ExportBody>) -> AppResult<Response> {
    let (name, media, body) = export_content(&state, ctx.account_id, b).await?;
    Ok(file(name, media, body))
}

pub async fn import_content_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ImportContentBody>) -> AppResult<Json<Value>> {
    Ok(Json(import_content(&state, ctx.account_id, b).await?))
}

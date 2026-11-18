//! Messages in each person's language: a language per person (`profiles.locale`), the languages an
//! account sends in, language versions of emails, texts, templates and saved blocks, translations
//! exchanged with translators (XLIFF 1.2, CSV), machine-translated drafts a person reviews, a
//! glossary, and DumbMail's own pages and footer lines in the person's language.
//! `docs/TRANSLATIONS.md` is the reference.
//!
//! Sending: the content prepare step asks [`localized`] for the person's version before it does
//! anything else (`features::content::prepare`). Content without language versions is passed on
//! untouched, with no extra query, so nothing changes for an account that never adds a language.

pub mod ai;
pub mod api;
pub mod check;
pub mod draft;
pub mod exchange;
pub mod fields;
pub mod locale;
pub mod store;
pub mod structure;
pub mod texts;

use std::future::Future;

use async_trait::async_trait;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
use axum::Router;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::content::prepare::SavedMap;
use crate::features::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use crate::mcp::{from, obj, tool};
use crate::models::Account;
use crate::state::AppState;
use store::Target;

pub struct Module;

tokio::task_local! {
    /// A language chosen for a preview or test send, overriding the person's.
    static PIN: String;
    /// The campaign whose batch is being sent (its footer wording snapshot applies only there).
    static CAMPAIGN: Uuid;
}

/// Run `f` as part of sending campaign `id`'s batch.
pub async fn in_campaign<F: Future>(id: Uuid, f: F) -> F::Output {
    CAMPAIGN.scope(id, f).await
}

/// Run `f` with every message it prepares going out in `lang` (previews and test sends).
pub async fn pinned<F: Future>(lang: String, f: F) -> F::Output {
    PIN.scope(lang, f).await
}

fn pin() -> Option<String> {
    PIN.try_with(|l| l.clone()).ok()
}

/// For the content prepare step: `content` in the person's language (from `ctx.person.locale`)
/// with `_lang` and the footer lines in that language (`_footer`), and the language; None when
/// the content has no language versions (it's used as it is).
///
/// `saved_langs` are the languages the email's saved blocks carry (see [`saved_languages`]); with
/// those, an email whose only translations are in a saved block is versioned too.
pub async fn localized(state: &AppState, account: &Account, content: &Value, ctx: &Value, saved_langs: &[String]) -> AppResult<Option<(Value, String)>> {
    let person = ctx.pointer("/person/locale").and_then(|v| v.as_str());
    let mut extra = saved_langs.to_vec();
    for l in fields::saved_block_languages(content) {
        if !extra.contains(&l) {
            extra.push(l);
        }
    }
    let account_default = if extra.is_empty() || content.get("translations").is_some() { "en".to_string() } else { texts::settings(&state.db, account.id).await?.default_language };
    let Some(lang) = fields::choose(content, &extra, &account_default, person, pin().as_deref()) else { return Ok(None) };
    let mut out = fields::localize(content, &lang, &own_hosts(state, Some(account)));
    if !lang.is_empty() {
        // A campaign's wording was fixed when its send started; anything else uses the account's
        // current wording (cached, refreshed off the send path).
        out["_footer"] = match CAMPAIGN.try_with(|c| *c).ok().and_then(|campaign| snapshot_footer(&state.cfg.secret_key, account.id, campaign, content, &lang)) {
            Some(f) => f,
            None => texts::footer(&state.db, account.id, &lang).await,
        };
    }
    Ok(Some((out, lang)))
}

/// The languages a message can go out in: its default and versions, and its saved blocks'.
fn content_languages(content: &Value, out: &mut Vec<String>) -> bool {
    let (default, langs) = fields::languages(content);
    let versioned = content.get("translations").is_some_and(|t| t.is_object());
    let saved = fields::saved_block_languages(content);
    if !versioned && saved.is_empty() {
        return false;
    }
    for l in default.into_iter().chain(langs).chain(saved) {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    true
}

/// The signature over a campaign's footer snapshot: the account, the campaign and the wording
/// (as sorted JSON triples, so no text can be read as another field).
fn snapshot_sig(secret: &str, account_id: Uuid, campaign_id: Uuid, footer: &Value) -> String {
    let mut triples: Vec<(String, String, String)> = footer
        .as_object()
        .into_iter()
        .flatten()
        .map(|(l, f)| (l.clone(), f["reason"].as_str().unwrap_or("").to_string(), f["unsubscribe"].as_str().unwrap_or("").to_string()))
        .collect();
    triples.sort();
    let payload = serde_json::to_string(&json!(["system", account_id, campaign_id, triples])).unwrap_or_default();
    let signed = crate::auth::sign(secret, &payload);
    signed.rsplit('.').next().unwrap_or_default().to_string()
}

/// The footer lines in `lang` from campaign `campaign_id`'s snapshot, when the content carries a
/// genuine one issued for that campaign (and it's that campaign being sent).
fn snapshot_footer(secret: &str, account_id: Uuid, campaign_id: Uuid, content: &Value, lang: &str) -> Option<Value> {
    let sys = content.get("_system")?;
    let footer = sys.get("footer")?;
    if sys.get("campaign").and_then(|c| c.as_str()) != Some(campaign_id.to_string().as_str()) || sys.get("sig").and_then(|s| s.as_str()) != Some(snapshot_sig(secret, account_id, campaign_id, footer).as_str()) {
        return None;
    }
    footer.get(lang).cloned()
}

/// When a campaign's send starts (inside the transaction that locks it, on its connection): the
/// footer wording each language the campaign can go out in needs, fixed for the rest of this send,
/// as `_system` for each version's content. None when no version has language versions (then
/// nothing is read).
pub async fn system_snapshot(conn: &mut sqlx::PgConnection, secret: &str, account_id: Uuid, campaign_id: Uuid, contents: &[&Value]) -> sqlx::Result<Option<Value>> {
    let mut langs = Vec::new();
    let mut any = false;
    for c in contents {
        any |= content_languages(c, &mut langs);
    }
    if !any {
        return Ok(None);
    }
    let s = texts::load(conn, account_id).await?;
    for l in s.all() {
        if !langs.contains(&l) {
            langs.push(l);
        }
    }
    let footer: Map<String, Value> = langs.iter().map(|l| (l.clone(), texts::footer_in(&s, l))).collect();
    let footer = Value::Object(footer);
    let sig = snapshot_sig(secret, account_id, campaign_id, &footer);
    Ok(Some(json!({"footer": footer, "campaign": campaign_id, "sig": sig})))
}

/// The languages the saved blocks an email points at carry.
pub fn saved_languages(saved: &SavedMap) -> Vec<String> {
    let mut out = Vec::new();
    for (_, blocks) in saved.values() {
        fields::blocks_languages(blocks, &mut out);
    }
    out
}

/// Saved blocks in the language being sent (their blocks carry their own translations).
pub fn localize_saved(saved: &mut SavedMap, lang: &str, own: &[String]) {
    if lang.is_empty() {
        return;
    }
    for (_, blocks) in saved.values_mut() {
        for b in blocks.as_array_mut().into_iter().flatten() {
            fields::localize_block(b, lang, own);
        }
    }
}

/// DumbMail's own hosts (the app, the link host, the marketing site, retired hosts and the
/// account's branded link domain): links there (unsubscribe, preferences, tracking) can't be
/// localized, and no localized link can point there.
pub fn own_hosts(state: &AppState, account: Option<&Account>) -> Vec<String> {
    let mut v: Vec<String> = crate::features::content::check::configured_own_hosts(&state.cfg).into_iter().collect();
    if let Some(d) = account.and_then(|a| a.link_domain.as_deref()) {
        v.push(d.trim().to_ascii_lowercase());
    }
    v.sort();
    v.dedup();
    v
}

// ---------- A person's language ----------

/// The language an API call, import or form gives in its `locale` (or `language`) field.
/// Some(Some(tag)) sets it, Some(None) clears it (trusted callers only), None leaves it.
/// A `locale` that isn't a tag is an error for trusted callers and ignored from browsers; a
/// `language` that isn't a tag stays an ordinary custom property.
pub fn input_locale(extra: &Map<String, Value>, from_browser: bool) -> AppResult<Option<Option<String>>> {
    if let Some(v) = extra.get("locale") {
        return match v {
            Value::Null => Ok((!from_browser).then_some(None)),
            Value::String(s) if s.trim().is_empty() => Ok((!from_browser).then_some(None)),
            Value::String(s) => match locale::normalize(s) {
                Some(t) => Ok(Some(Some(t))),
                None if from_browser => Ok(None),
                None => Err(AppError::bad(format!("locale {s:?} isn't a language tag like fr or pt-BR"))),
            },
            _ if from_browser => Ok(None),
            _ => Err(AppError::bad("locale must be a language tag like fr or pt-BR")),
        };
    }
    Ok(extra.get("language").and_then(|v| v.as_str()).and_then(locale::normalize).map(Some))
}

/// Whether a custom property key is really the person's language (kept out of the properties).
pub fn is_locale_key(k: &str, v: &Value) -> bool {
    k == "locale" || (k == "language" && v.as_str().and_then(locale::normalize).is_some())
}

/// Save a person's language. Trusted callers (the API, imports, integrations, the dashboard) set
/// it; a browser (a form, the site snippet) only fills it in when it's empty, so it can choose
/// which language someone is sent but never overwrite what a trusted source said. Returns the
/// language now saved.
pub async fn save_locale(db: &PgPool, profile_id: Uuid, value: Option<&str>, from_browser: bool) -> AppResult<Option<String>> {
    let q = if from_browser {
        "UPDATE profiles SET locale = $2, updated_at = now() WHERE id = $1 AND locale IS NULL AND $2::text IS NOT NULL RETURNING locale"
    } else {
        "UPDATE profiles SET locale = $2, updated_at = now() WHERE id = $1 AND locale IS DISTINCT FROM $2 RETURNING locale"
    };
    let changed: Option<Option<String>> = sqlx::query_scalar(q).bind(profile_id).bind(value).fetch_optional(db).await?;
    match changed {
        Some(l) => Ok(l),
        None => Ok(sqlx::query_scalar("SELECT locale FROM profiles WHERE id = $1").bind(profile_id).fetch_optional(db).await?.flatten()),
    }
}

/// A form's language hint (a `locale` or `language` field, else the browser's language): fills an
/// empty profile language only.
pub async fn fill_from_form(db: &PgPool, profile_id: Uuid, answers: &Map<String, Value>, browser: Option<&str>) -> AppResult<()> {
    let given = ["locale", "language"].iter().find_map(|k| answers.get(*k).and_then(|v| v.as_str()).and_then(locale::normalize));
    let tag = given.or_else(|| browser.and_then(|b| locale::accept_language(b).into_iter().next()));
    if let Some(t) = tag {
        save_locale(db, profile_id, Some(&t), true).await?;
    }
    Ok(())
}

// ---------- Permissions ----------

static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/translations/settings", Perm::ContentRead),
    rule("PUT", "/api/translations/settings", Perm::SettingsWrite),
    rule("GET", "/api/translations/glossary", Perm::ContentRead),
    rule("POST", "/api/translations/glossary", Perm::SettingsWrite),
    rule("PATCH", "/api/translations/glossary/{id}", Perm::SettingsWrite),
    rule("DELETE", "/api/translations/glossary/{id}", Perm::SettingsWrite),
    rule("GET", "/api/translations/system-texts", Perm::ContentRead),
    rule("PUT", "/api/translations/system-texts", Perm::SettingsWrite),
    rule("POST", "/api/translations/preview", Perm::ContentRead),
    // These work on content sent with the request and store nothing (the editor saves it).
    rule("POST", "/api/translations/inspect", Perm::ContentRead),
    rule("POST", "/api/translations/apply", Perm::ContentRead),
    rule("POST", "/api/translations/export", Perm::ContentRead),
    rule("POST", "/api/translations/import", Perm::ContentRead),
    rule("GET", "/api/translations/report", Perm::MessagingRead),
    rule("GET", "/api/translations/jobs", Perm::ContentRead),
    rule("GET", "/api/translations/jobs/{id}", Perm::ContentRead),
    rule("GET", "/api/translations/campaigns/{id}", Perm::MessagingRead),
    rule("PUT", "/api/translations/campaigns/{id}", Perm::CampaignDraft),
    rule("POST", "/api/translations/campaigns/{id}/review", Perm::CampaignDraft),
    rule("GET", "/api/translations/campaigns/{id}/export", Perm::MessagingRead),
    rule("POST", "/api/translations/campaigns/{id}/import", Perm::CampaignDraft),
    rule("POST", "/api/translations/campaigns/{id}/draft", Perm::CampaignDraft),
    rule("POST", "/api/translations/campaigns/{id}/test", Perm::TestSend),
    rule("GET", "/api/translations/flows/{id}/steps/{node}", Perm::MessagingRead),
    rule("PUT", "/api/translations/flows/{id}/steps/{node}", Perm::FlowEdit),
    rule("POST", "/api/translations/flows/{id}/steps/{node}/review", Perm::FlowEdit),
    rule("GET", "/api/translations/flows/{id}/steps/{node}/export", Perm::MessagingRead),
    rule("POST", "/api/translations/flows/{id}/steps/{node}/import", Perm::FlowEdit),
    rule("POST", "/api/translations/flows/{id}/steps/{node}/draft", Perm::FlowEdit),
    rule("GET", "/api/translations/templates/{id}", Perm::ContentRead),
    rule("PUT", "/api/translations/templates/{id}", Perm::ContentWrite),
    rule("POST", "/api/translations/templates/{id}/review", Perm::ContentWrite),
    rule("GET", "/api/translations/templates/{id}/export", Perm::ContentRead),
    rule("POST", "/api/translations/templates/{id}/import", Perm::ContentWrite),
    rule("POST", "/api/translations/templates/{id}/draft", Perm::ContentWrite),
    rule("GET", "/api/translations/saved-blocks/{id}", Perm::ContentRead),
    rule("PUT", "/api/translations/saved-blocks/{id}", Perm::ContentWrite),
    rule("POST", "/api/translations/saved-blocks/{id}/review", Perm::ContentWrite),
    rule("GET", "/api/translations/saved-blocks/{id}/export", Perm::ContentRead),
    rule("POST", "/api/translations/saved-blocks/{id}/import", Perm::ContentWrite),
    rule("POST", "/api/translations/saved-blocks/{id}/draft", Perm::ContentWrite),
];

/// Tools that act on a message also check that message's own permission (reading a campaign
/// needs messaging.read; changing it campaigns.draft, a flow flows.edit, a template or saved block
/// content.write), the same as the REST routes.
static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_languages", perm: Perm::ContentRead },
    ToolRule { name: "set_languages", perm: Perm::SettingsWrite },
    ToolRule { name: "get_translations", perm: Perm::ContentRead },
    ToolRule { name: "set_translations", perm: Perm::ContentRead },
    ToolRule { name: "review_translations", perm: Perm::ContentRead },
    ToolRule { name: "export_translations", perm: Perm::ContentRead },
    ToolRule { name: "import_translations", perm: Perm::ContentRead },
    ToolRule { name: "draft_translations", perm: Perm::ContentWrite },
    ToolRule { name: "get_translation_run", perm: Perm::ContentRead },
    ToolRule { name: "list_glossary", perm: Perm::ContentRead },
    ToolRule { name: "save_glossary_term", perm: Perm::SettingsWrite },
    ToolRule { name: "delete_glossary_term", perm: Perm::SettingsWrite },
    ToolRule { name: "get_system_texts", perm: Perm::ContentRead },
    ToolRule { name: "set_system_texts", perm: Perm::SettingsWrite },
    ToolRule { name: "preview_in_language", perm: Perm::ContentRead },
    ToolRule { name: "send_test_in_language", perm: Perm::TestSend },
    ToolRule { name: "translation_report", perm: Perm::MessagingRead },
];

const BODY_LIMIT: usize = 3 * 1024 * 1024;

fn target_arg(a: &Value) -> AppResult<Target> {
    let t = a.get("target").ok_or_else(|| AppError::bad("give target: {kind, id} (and version for an A/B version, step for a flow step)"))?;
    let kind = t.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let id: Uuid = t.get("id").and_then(|i| i.as_str()).and_then(|i| i.parse().ok()).ok_or_else(|| AppError::bad("target.id must be an id"))?;
    let part = match kind {
        "campaign" => t.get("version").and_then(|v| v.as_str()).unwrap_or(""),
        "flow" => t.get("step").and_then(|v| v.as_str()).unwrap_or(""),
        _ => "",
    };
    Target::from_parts(kind, id, part).ok_or_else(|| AppError::bad("target.kind must be campaign, flow, template or saved_block"))?.checked()
}

/// The arguments without `target`, for the body structs.
fn rest(a: &Value) -> Value {
    let mut v = a.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("target");
    }
    v
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "translations"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/translations/settings", get(api::settings_h).put(api::set_settings_h))
            .route("/api/translations/glossary", get(api::glossary_h).post(api::add_term_h))
            .route("/api/translations/glossary/{id}", patch(api::update_term_h).delete(api::delete_term_h))
            .route("/api/translations/system-texts", get(api::system_texts_h).put(api::set_system_texts_h))
            .route("/api/translations/preview", post(api::preview_h))
            .route("/api/translations/inspect", post(api::inspect_h))
            .route("/api/translations/apply", post(api::apply_h))
            .route("/api/translations/export", post(api::export_content_h))
            .route("/api/translations/import", post(api::import_content_h))
            .route("/api/translations/report", get(api::report_h))
            .route("/api/translations/jobs", get(api::jobs_h))
            .route("/api/translations/jobs/{id}", get(api::job_h))
            .route("/api/translations/campaigns/{id}", get(api::campaign_get).put(api::campaign_set))
            .route("/api/translations/campaigns/{id}/review", post(api::campaign_review))
            .route("/api/translations/campaigns/{id}/export", get(api::campaign_export))
            .route("/api/translations/campaigns/{id}/import", post(api::campaign_import))
            .route("/api/translations/campaigns/{id}/draft", post(api::campaign_draft))
            .route("/api/translations/campaigns/{id}/test", post(api::campaign_test))
            .route("/api/translations/flows/{id}/steps/{node}", get(api::flow_get).put(api::flow_set))
            .route("/api/translations/flows/{id}/steps/{node}/review", post(api::flow_review))
            .route("/api/translations/flows/{id}/steps/{node}/export", get(api::flow_export))
            .route("/api/translations/flows/{id}/steps/{node}/import", post(api::flow_import))
            .route("/api/translations/flows/{id}/steps/{node}/draft", post(api::flow_draft))
            .route("/api/translations/templates/{id}", get(api::template_get).put(api::template_set))
            .route("/api/translations/templates/{id}/review", post(api::template_review))
            .route("/api/translations/templates/{id}/export", get(api::template_export))
            .route("/api/translations/templates/{id}/import", post(api::template_import))
            .route("/api/translations/templates/{id}/draft", post(api::template_draft))
            .route("/api/translations/saved-blocks/{id}", get(api::saved_get).put(api::saved_set))
            .route("/api/translations/saved-blocks/{id}/review", post(api::saved_review))
            .route("/api/translations/saved-blocks/{id}/export", get(api::saved_export))
            .route("/api/translations/saved-blocks/{id}/import", post(api::saved_import))
            .route("/api/translations/saved-blocks/{id}/draft", post(api::saved_draft))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let target = json!({"type": "object", "additionalProperties": false, "required": ["kind", "id"], "description": "The message: a campaign (version: an A/B version b-d, or leave it out for version A), a flow step (step: the step's id), a template or a saved block.",
            "properties": {"kind": {"enum": ["campaign", "flow", "template", "saved_block"]}, "id": {"type": "string"}, "version": {"type": "string"}, "step": {"type": "string"}}});
        let lang = json!({"type": "string", "maxLength": locale::MAX_LEN, "description": "A language tag like fr, fr-CA or pt-BR (one of the account's languages)."});
        let strings = json!({"type": "array", "maxItems": fields::MAX_ENTRIES, "items": {"type": "object", "additionalProperties": false, "required": ["id"], "properties": {
            "id": {"type": "string", "description": "A field id from get_translations (subject, preheader, b0:text, b2.c0.1:href ...)."},
            "text": {"type": ["string", "null"], "description": "The translation; empty or null removes it (the default text is sent)."},
            "status": {"enum": ["reviewed", "machine"]}}}});
        let ids = json!({"type": "array", "items": {"type": "string"}, "maxItems": fields::MAX_ENTRIES});
        vec![
            tool("get_languages", "Get the account's languages", "The default language (what content is written in) and the other languages the account sends in, whether machine translation is available, today's machine-translation use and limit.", obj(json!({}), &[]), true, false),
            tool("set_languages", "Set the account's languages", "Set the default language and the languages the account sends in (up to 20, BCP 47 tags like fr, es, pt-BR). Messages can then get versions in those languages; people are sent the version for their language (profile locale), else the default.", obj(json!({"default_language": {"type": "string"}, "languages": {"type": "array", "items": {"type": "string"}, "maxItems": fields::MAX_LANGUAGES}}), &[]), false, false),
            tool("get_translations", "Get a message's translations", "Every translatable text of a campaign (or A/B version), flow step, template or saved block, with its default text and, for the language, the translation and its status: missing, machine (an unreviewed machine draft), reviewed, stale (the default changed since) or default (a link nobody localized). Missing and stale texts are sent in the default language.", obj(json!({"target": target, "language": lang}), &["target"]), true, false),
            tool("set_translations", "Set translations", "Save translations of a message's texts in one language (by field id from get_translations). Template tags like {{ first_name }} must stay exactly as in the default text; links may be localized (https, http, mailto or tel). Hand-written translations are saved as reviewed. remove_language=true takes the language off the message.", obj(json!({"target": target, "language": lang, "strings": strings, "remove_language": {"type": "boolean"}}), &["target", "language"]), false, false),
            tool("review_translations", "Mark translations reviewed", "Mark machine drafts in a language reviewed (all of them, or only ids) once a person has read them.", obj(json!({"target": target, "language": lang, "ids": ids}), &["target", "language"]), false, false),
            tool("export_translations", "Export a message's texts", "A message's texts and their current translations in one language as XLIFF 1.2 (default) or CSV, for a translator. Import the file back with import_translations.", obj(json!({"target": target, "language": lang, "format": {"enum": ["xliff", "csv"]}}), &["target", "language"]), true, false),
            tool("import_translations", "Import translations", "Read a translator's XLIFF 1.2 or CSV file (as exported) and save its translations as reviewed. Texts whose default changed since the export are skipped and listed; translations that drop or add template tags are refused and listed.", obj(json!({"target": target, "language": lang, "format": {"enum": ["xliff", "csv"]}, "data": {"type": "string", "maxLength": exchange::MAX_BYTES}}), &["target", "language", "format", "data"]), false, false),
            tool("draft_translations", "Draft translations with AI", "Machine-translate a message's missing and out-of-date texts (or only ids) into a language, as drafts marked machine for a person to review. Sends only the texts and the account's glossary to the translation model, never anything about people. Runs in the background (get_translation_run shows progress); one run per account at a time, within a daily character limit. Without confirm=true it only says how many texts and characters it would translate.", obj(json!({"target": target, "language": lang, "ids": ids, "confirm": {"type": "boolean"}}), &["target", "language"]), false, false),
            tool("get_translation_run", "Get a translation run", "A Draft with AI run: status (queued, running, done, failed), how many texts were translated or skipped, and why it failed. Without id, the account's recent runs.", obj(json!({"id": {"type": "string"}}), &[]), true, false),
            tool("list_glossary", "List the glossary", "The account's glossary: terms with a fixed translation per language, and terms never translated (like the brand name). Machine drafts follow it and translators see it in exports.", obj(json!({}), &[]), true, false),
            tool("save_glossary_term", "Save a glossary term", "Add a term (or change one, with id): never_translate keeps it as it is in every language; translations gives its fixed translation per language.", obj(json!({"id": {"type": "string"}, "term": {"type": "string", "maxLength": 100}, "never_translate": {"type": "boolean"}, "translations": {"type": "object", "additionalProperties": {"type": "string", "maxLength": 200}}, "note": {"type": "string", "maxLength": 500}}), &[]), false, false),
            tool("delete_glossary_term", "Delete a glossary term", "Remove a term from the glossary. Without confirm=true nothing is deleted.", obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("get_system_texts", "Get system texts", "The words DumbMail itself shows in a language: the unsubscribe and preference pages, the double opt-in email and page, and the email footer lines. Each shows the English, the shipped translation (English, Spanish, French, German, Italian, Portuguese, Dutch) and the account's own wording.", obj(json!({"language": lang}), &[]), true, false),
            tool("set_system_texts", "Reword system texts", "Set the account's own wording of system texts in one of its languages (key → text; null or \"\" goes back to the shipped text). Keep placeholders like {name} and {days}.", obj(json!({"language": lang, "texts": {"type": "object", "additionalProperties": {"type": ["string", "null"], "maxLength": texts::MAX_TEXT}}}), &["language", "texts"]), false, false),
            tool("preview_in_language", "Preview a message in a language", "Render email or text content as it goes out in one language (subject, preheader, HTML and text, with dir=\"rtl\" for right-to-left languages). Uses a sample person unless profile_id is given (needs profiles.read).", obj(json!({"channel": {"enum": ["email", "sms"]}, "content": {"type": "object"}, "language": lang, "profile_id": {"type": "string"}, "event": {"type": "object"}}), &["channel", "content", "language"]), true, false),
            tool("send_test_in_language", "Send a test in a language", "Send a test of a campaign (or an A/B version) in one language to up to 10 addresses, with the same limits as send_test_campaign. Without confirm=true nothing is sent.", obj(json!({"campaign_id": {"type": "string"}, "to": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 10}, "version": {"type": "string"}, "language": lang, "confirm": {"type": "boolean"}}), &["campaign_id", "to", "language"]), false, true),
            tool("translation_report", "Results by language", "A campaign's or flow's messages by the language they were sent in: sent, opened, clicked, unsubscribed, open and click rates.", obj(json!({"campaign_id": {"type": "string"}, "flow_id": {"type": "string"}}), &[]), true, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOLS.iter().any(|t| t.name == name) {
            return None;
        }
        let a = args.clone();
        let acct = ctx.account_id;
        let r = async {
            match name {
                "get_languages" => api::settings_json(state, acct).await,
                "set_languages" => api::set_settings(state, ctx, from(a)?).await,
                "get_translations" => api::get(state, ctx, &target_arg(&a)?, a["language"].as_str()).await,
                "set_translations" => api::set(state, ctx, &target_arg(&a)?, from(rest(&a))?).await,
                "review_translations" => api::review(state, ctx, &target_arg(&a)?, from(rest(&a))?).await,
                "export_translations" => {
                    let lang = a["language"].as_str().ok_or_else(|| AppError::bad("give language"))?;
                    let (name, media, body) = api::export(state, ctx, &target_arg(&a)?, lang, a["format"].as_str().unwrap_or("xliff")).await?;
                    Ok(json!({"file_name": name, "media_type": media, "data": body}))
                }
                "import_translations" => api::import(state, ctx, &target_arg(&a)?, from(rest(&a))?).await,
                "draft_translations" => {
                    let b: api::DraftBody = from(rest(&a))?;
                    api::start_draft(state, ctx, &target_arg(&a)?, &b, !b.confirm).await
                }
                "get_translation_run" => match a["id"].as_str() {
                    Some(id) => draft::job_json(state, acct, id.parse().map_err(|_| AppError::bad("id must be a run id"))?).await,
                    None => api::jobs(state, acct).await,
                },
                "list_glossary" => api::glossary(state, acct).await,
                "save_glossary_term" => {
                    let mut b = a.clone();
                    let id = b.as_object_mut().and_then(|o| o.remove("id")).and_then(|v| v.as_str().and_then(|s| s.parse::<Uuid>().ok()));
                    match id {
                        Some(id) => api::update_term(state, ctx, id, from(b)?).await,
                        None => api::add_term(state, ctx, from(b)?).await,
                    }
                }
                "delete_glossary_term" => {
                    let id: Uuid = a["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("id must be a glossary term id"))?;
                    if a["confirm"].as_bool() != Some(true) {
                        return Ok(json!({"dry_run": true, "note": "Nothing was deleted. Call again with confirm=true to remove this term."}));
                    }
                    api::delete_term(state, ctx, id).await
                }
                "get_system_texts" => {
                    let s = texts::settings(&state.db, acct).await?;
                    api::system_texts(state, acct, a["language"].as_str().unwrap_or(&s.default_language)).await
                }
                "set_system_texts" => api::set_system_texts(state, ctx, from(a)?).await,
                "preview_in_language" => api::preview(state, ctx, from(a)?).await,
                "send_test_in_language" => {
                    let id: Uuid = a["campaign_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("campaign_id must be a campaign id"))?;
                    let mut b = a.clone();
                    if let Some(o) = b.as_object_mut() {
                        o.remove("campaign_id");
                    }
                    let b: api::TestBody = from(b)?;
                    if !b.confirm {
                        return Ok(json!({"dry_run": true, "would_send_to": b.to, "language": b.language, "note": "Nothing was sent. Call again with confirm=true to send the test."}));
                    }
                    api::test_send(state, ctx, id, &b).await
                }
                "translation_report" => {
                    let q = api::ReportQuery {
                        campaign_id: a["campaign_id"].as_str().and_then(|s| s.parse().ok()),
                        flow_id: a["flow_id"].as_str().and_then(|s| s.parse().ok()),
                    };
                    api::report(state, acct, &q).await
                }
                _ => Err(AppError::not_found("tool")),
            }
        }
        .await;
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "translations",
            name: "Messages in each person's language",
            description: "Profile languages, account languages, language versions of emails, texts, templates and saved blocks, XLIFF/CSV, machine-translated drafts, the glossary and system texts",
            text: include_str!("../../../docs/TRANSLATIONS.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        (kind == draft::JOB_KIND).then_some(())?;
        Some(draft::run(state, payload).await)
    }
}

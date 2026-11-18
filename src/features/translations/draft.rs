//! "Draft with AI": machine-translated drafts of a message's missing or out-of-date text.
//!
//! Starting a run fixes what it translates (the strings and the default text they come from are
//! copied into the run), reserves the characters against the account's daily limit in the same
//! statement that checks it, and enqueues the work on the background lane, all in one
//! transaction. At most one run per account is queued or running (a unique index). The run calls
//! the model with no database connection held, then writes each draft only where the default text
//! is still the one it translated and nobody has put a reviewed translation there meanwhile.
//! Drafts are marked `machine` until a person marks them reviewed. A run that dies is picked up
//! again by the job queue; one stuck for [`STUCK_MINUTES`] is closed so the account can start
//! another.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use uuid::Uuid;

use super::ai::{self, Term};
use super::fields::{self, Kind};
use super::store::{self, Target};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const JOB_KIND: &str = "translations_draft";
pub const STUCK_MINUTES: i32 = 30;

/// Whether machine translation is set up on this server.
pub fn available(state: &AppState) -> bool {
    state.cfg.anthropic_api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
}

/// The account's glossary for the prompt and placeholders, in `lang`.
pub async fn glossary_for(db: &sqlx::PgPool, account_id: Uuid, lang: &str) -> AppResult<Vec<Term>> {
    let rows: Vec<(String, bool, Value)> = sqlx::query_as("SELECT term, never_translate, translations FROM translation_glossary WHERE account_id = $1 ORDER BY lower(term) LIMIT 500").bind(account_id).bind(lang).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|(term, never_translate, t)| {
            let translation = t.get(lang).or_else(|| t.get(super::locale::primary(lang))).and_then(|v| v.as_str()).map(str::to_string);
            Term { term, never_translate, translation }
        })
        .collect())
}

/// A string a run translates: its field, kind, default text and that text's hash.
fn picked(content: &Value, lang: &str, ids: Option<&[String]>) -> Vec<Value> {
    fields::fields(content)
        .into_iter()
        .filter(|f| f.kind != Kind::Link && f.source.chars().count() <= ai::MAX_FIELD_CHARS && !f.source.contains('⟦'))
        .filter(|f| match ids {
            Some(ids) => ids.contains(&f.id),
            None => matches!(fields::status(content, f, lang), "missing" | "stale"),
        })
        .map(|f| json!({"id": f.id, "kind": f.kind.name(), "source": f.source, "hash": fields::hash(&f.source)}))
        .collect()
}

/// The cost of `chars` characters, in micro-dollars (0 unless the hosted service sets a price).
pub fn price_micros(state: &AppState, chars: i64) -> i64 {
    if !state.cfg.hosted || state.cfg.price_translation_per_1000_chars <= 0.0 {
        return 0;
    }
    crate::billing::to_micros(state.cfg.price_translation_per_1000_chars * chars as f64 / 1000.0)
}

/// Characters used today (the account's time zone) and the daily limit.
pub async fn usage_today(state: &AppState, account_id: Uuid) -> AppResult<(i64, i64)> {
    let used: Option<i64> = sqlx::query_scalar(
        "SELECT u.chars FROM translation_usage u JOIN accounts a ON a.id = u.account_id
         WHERE u.account_id = $1 AND u.day = (now() AT TIME ZONE a.timezone)::date",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    Ok((used.unwrap_or(0), state.cfg.translation_daily_chars))
}

/// What a run would translate, without starting it.
pub async fn plan(state: &AppState, account_id: Uuid, t: &Target, lang: &str, ids: Option<&[String]>) -> AppResult<Value> {
    let l = store::load(&state.db, account_id, t).await?;
    let strings = picked(&l.content, lang, ids);
    let chars: i64 = strings.iter().map(|s| s["source"].as_str().unwrap_or("").chars().count() as i64).sum();
    let (used, limit) = usage_today(state, account_id).await?;
    Ok(json!({"strings": strings.len(), "characters": chars, "price_usd": crate::billing::to_usd(price_micros(state, chars)), "used_today": used, "daily_limit": limit, "available": available(state)}))
}

/// Start a run translating `t` into `lang` (the missing and out-of-date text, or only `ids`).
pub async fn start(state: &AppState, ctx: &Ctx, t: &Target, lang: &str, ids: Option<&[String]>, scheduled_ok: bool) -> AppResult<Value> {
    if !available(state) {
        return Err(AppError::Conflict("Machine translation isn't set up on this server (ANTHROPIC_API_KEY). Export the text for a translator instead.".into()));
    }
    let l = store::load(&state.db, ctx.account_id, t).await?;
    let (default, _) = fields::languages(&l.content);
    if default.as_deref() == Some(lang) {
        return Err(AppError::bad("that's this message's default language; pick a language to translate into"));
    }
    let strings = picked(&l.content, lang, ids);
    if strings.is_empty() {
        return Err(AppError::bad("nothing to translate: every text in this language is already translated and up to date"));
    }
    let chars: i64 = strings.iter().map(|s| s["source"].as_str().unwrap_or("").chars().count() as i64).sum();
    let limit = state.cfg.translation_daily_chars;
    let price = price_micros(state, chars);
    if price > 0 {
        crate::billing::ensure_funds(state, ctx.account_id, price, "machine translation").await?;
    }
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    // A run that stopped long ago without finishing doesn't block the account forever.
    sqlx::query("UPDATE translation_jobs SET status = 'failed', error = 'stopped before it finished', finished_at = now() WHERE account_id = $1 AND status IN ('queued', 'running') AND created_at < now() - make_interval(mins => $2)")
        .bind(ctx.account_id)
        .bind(STUCK_MINUTES)
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO translation_jobs (id, account_id, target_kind, target_id, target_part, language, strings, chars, requested_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(t.kind())
    .bind(t.id())
    .bind(t.part())
    .bind(lang)
    .bind(Value::Array(strings.clone()))
    .bind(chars as i32)
    .bind(ctx.user_id)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            return Err(AppError::Conflict("A translation is already running for this account; wait for it to finish.".into()));
        }
        Err(e) => return Err(e.into()),
    }
    // The daily limit, checked and reserved in one statement.
    let reserved: Option<i64> = sqlx::query_scalar(
        "INSERT INTO translation_usage AS u (account_id, day, chars)
         SELECT a.id, (now() AT TIME ZONE a.timezone)::date, $2 FROM accounts a WHERE a.id = $1 AND $2 <= $3
         ON CONFLICT (account_id, day) DO UPDATE SET chars = u.chars + EXCLUDED.chars WHERE u.chars + EXCLUDED.chars <= $3
         RETURNING chars",
    )
    .bind(ctx.account_id)
    .bind(chars)
    .bind(limit)
    .fetch_optional(&mut *tx)
    .await?;
    if reserved.is_none() {
        return Err(AppError::LimitReached(format!("That's more than today's machine translation allowance ({limit} characters a day); translate less at a time or try tomorrow.")));
    }
    crate::jobs::enqueue_background(&mut *tx, JOB_KIND, json!({"job_id": id, "scheduled_ok": scheduled_ok}), None).await?;
    tx.commit().await?;
    crate::features::team::audit(state, ctx, "translation.draft", Some(&t.describe()), json!({"language": lang, "strings": strings.len(), "characters": chars})).await;
    job_json(state, ctx.account_id, id).await
}

pub async fn job_json(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    type Row = (Uuid, String, Uuid, String, String, String, i32, i32, i32, Option<String>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>);
    let r: Option<Row> = sqlx::query_as(
        "SELECT id, target_kind, target_id, target_part, language, status, chars, translated, skipped, error, created_at, finished_at FROM translation_jobs WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let r = r.ok_or_else(|| AppError::not_found("translation run"))?;
    Ok(json!({"id": r.0, "target": {"kind": r.1, "id": r.2, "part": r.3}, "language": r.4, "status": r.5, "characters": r.6, "translated": r.7, "skipped": r.8, "error": r.9, "created_at": r.10, "finished_at": r.11}))
}

/// Record how a run ended, and give back its reserved characters when it translated nothing
/// (`refund`), in one transaction: a replay after this finds the run finished and stops.
#[allow(clippy::too_many_arguments)]
async fn finish(state: &AppState, id: Uuid, status: &str, translated: usize, skipped: usize, error: Option<&str>, refund: Option<(Uuid, i32, chrono::DateTime<chrono::Utc>)>) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let n = sqlx::query("UPDATE translation_jobs SET status = $2, translated = $3, skipped = $4, error = $5, strings = '[]', finished_at = now() WHERE id = $1 AND status = 'running'")
        .bind(id)
        .bind(status)
        .bind(translated as i32)
        .bind(skipped as i32)
        .bind(error)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if let (1, Some((account_id, chars, created_at))) = (n, refund) {
        sqlx::query(
            "UPDATE translation_usage u SET chars = GREATEST(u.chars - $2, 0) FROM accounts a
             WHERE u.account_id = $1 AND a.id = u.account_id AND u.day = ($3 AT TIME ZONE a.timezone)::date",
        )
        .bind(account_id)
        .bind(i64::from(chars))
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The background job: translate, write the drafts back, record the outcome.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload.get("job_id").and_then(|v| v.as_str()).and_then(|v| v.parse::<Uuid>().ok()) else { return Ok(()) };
    let scheduled_ok = payload.get("scheduled_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    // Claimed again on a retry; a finished run isn't run twice.
    type Row = (Uuid, String, Uuid, String, String, Value, i32, chrono::DateTime<chrono::Utc>);
    let row: Option<Row> = sqlx::query_as(
        "UPDATE translation_jobs SET status = 'running' WHERE id = $1 AND status IN ('queued', 'running')
         RETURNING account_id, target_kind, target_id, target_part, language, strings, chars, created_at",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, kind, target_id, part, lang, strings, chars, created_at)) = row else { return Ok(()) };
    let refund = Some((account_id, chars, created_at));
    let Some(target) = Target::from_parts(&kind, target_id, &part) else { return finish(state, id, "failed", 0, 0, Some("unknown message kind"), refund).await };
    let Some(key) = state.cfg.anthropic_api_key.clone().filter(|k| !k.trim().is_empty()) else {
        return finish(state, id, "failed", 0, 0, Some("machine translation isn't set up on this server"), refund).await;
    };
    let settings = super::texts::settings(&state.db, account_id).await?;
    let source_lang = store::load(&state.db, account_id, &target).await.ok().and_then(|l| fields::languages(&l.content).0).unwrap_or(settings.default_language.clone());
    let glossary = glossary_for(&state.db, account_id, &lang).await?;

    // Protect each string; group them into requests within the size limits.
    let list: Vec<(String, Kind, String, String)> = strings
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let kind = match s["kind"].as_str()? {
                "html" => Kind::Html,
                _ => Kind::Text,
            };
            Some((s["id"].as_str()?.to_string(), kind, s["source"].as_str()?.to_string(), s["hash"].as_str()?.to_string()))
        })
        .collect();
    let mut protected = BTreeMap::new();
    let mut skipped = 0usize;
    for (fid, kind, source, _) in &list {
        match ai::protect(*kind, source, &glossary) {
            Some(p) => {
                protected.insert(fid.clone(), p);
            }
            None => skipped += 1,
        }
    }
    // Batches are bounded by characters, strings and the request's size in bytes (the
    // instructions and glossary get a fixed share); a string too big for any request is skipped.
    let budget = ai::MAX_REQUEST_BYTES - ai::RESERVED_BYTES;
    let mut batches: Vec<BTreeMap<String, String>> = Vec::new();
    let mut cur = BTreeMap::new();
    let (mut size, mut bytes) = (0, 0);
    for (fid, p) in &protected {
        let n = p.text.chars().count();
        let b = ai::entry_bytes(fid, &p.text);
        if b > budget {
            skipped += 1;
            continue;
        }
        if !cur.is_empty() && (size + n > ai::MAX_REQUEST_CHARS || bytes + b > budget || cur.len() >= ai::MAX_REQUEST_STRINGS) {
            batches.push(std::mem::take(&mut cur));
            size = 0;
            bytes = 0;
        }
        cur.insert(fid.clone(), p.text.clone());
        size += n;
        bytes += b;
    }
    if !cur.is_empty() {
        batches.push(cur);
    }

    // Each draft with its status: `check` when its numbers, links or addresses differ from the original's.
    let mut drafts: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
    let mut failure: Option<String> = None;
    for batch in &batches {
        let req = ai::Request { base_url: state.cfg.anthropic_api_base.as_deref(), api_key: &key, source_lang: &source_lang, target_lang: &lang, glossary: &glossary, strings: batch };
        match ai::translate(&req).await {
            Ok(out) => {
                for fid in batch.keys() {
                    let (Some(p), Some(t), Some((_, _, source, _))) = (protected.get(fid), out.get(fid), list.iter().find(|x| &x.0 == fid)) else {
                        skipped += 1;
                        continue;
                    };
                    match ai::restore(p, t, source) {
                        Ok(text) => {
                            let status = if ai::same_literals(source, &text) { fields::MACHINE } else { fields::CHECK };
                            drafts.insert(fid.clone(), (text, status));
                        }
                        Err(why) => {
                            tracing::info!(job = %id, reason = why, "a machine translation was rejected");
                            skipped += 1;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(job = %id, error = %e, "machine translation request failed");
                skipped += batch.len();
                failure = Some(e.0);
            }
        }
    }
    if drafts.is_empty() {
        return finish(state, id, "failed", 0, skipped, Some(failure.as_deref().unwrap_or("no usable translations came back")), refund).await;
    }

    // Write each draft where its default text is unchanged and nobody reviewed a translation since.
    let hashes: BTreeMap<&str, &str> = list.iter().map(|(fid, _, _, h)| (fid.as_str(), h.as_str())).collect();
    let written = store::update(&state.db, account_id, &target, scheduled_ok, |l| {
        let mut n = 0;
        let mut later = 0;
        let current = fields::fields(&l.content);
        for (fid, (text, status)) in &drafts {
            let Some(f) = current.iter().find(|f| &f.id == fid) else {
                later += 1;
                continue;
            };
            if hashes.get(fid.as_str()).copied() != Some(fields::hash(&f.source).as_str()) || fields::status(&l.content, f, &lang) == fields::REVIEWED {
                later += 1;
                continue;
            }
            fields::set_entry(&mut l.content, fid, &lang, Some((text, status)))?;
            n += 1;
        }
        if n > 0 {
            let default = fields::languages(&l.content).0.unwrap_or(settings.default_language.clone());
            if !matches!(target, Target::SavedBlock { .. }) {
                fields::add_language(&mut l.content, &default, &lang);
            }
        }
        Ok((n, later))
    })
    .await;
    match written {
        Ok((n, later)) => {
            let price = price_micros(state, chars as i64);
            if price > 0 && n > 0 {
                crate::billing::post_entry(&state.db, account_id, "translation", -price, "Machine translation", &format!("translation:{id}")).await?;
            }
            finish(state, id, "done", n, skipped + later, failure.as_deref(), None).await
        }
        Err(AppError::Internal(e)) => Err(e),
        Err(e) => finish(state, id, "failed", 0, skipped + drafts.len(), Some(&e.public_message()), refund).await,
    }
}

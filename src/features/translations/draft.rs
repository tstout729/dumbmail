//! "Draft with AI": machine-translated drafts of a message's missing or out-of-date text.
//!
//! Starting a run fixes what it translates (the strings and the default text they come from are
//! copied into the run), reserves the characters against the account's daily limit in the same
//! statement that checks it, and enqueues the work on the background lane, all in one
//! transaction. At most one run per account is queued or running (a unique index).
//!
//! One worker owns a run at a time: it takes a lease (owner and expiry) and renews it between
//! batches, and every write (drafts, charge, progress) happens in one transaction that first
//! checks it still holds the lease and the run is still running. A run has [`MAX_RUN`] in all;
//! past that it ends failed. The model is called with no database connection held; failures that
//! may pass (busy, a server problem, no answer in time) are tried again within that bound,
//! honouring `Retry-After`; others end the run, saying how many texts weren't drafted.
//! Characters count against the daily limit once they're sent, whether or not what came back was
//! usable; only characters never sent are given back. The copied text is cleared whenever a run
//! ends. Drafts are written only where the default text is still the one that was translated and
//! nobody has put a reviewed translation there meanwhile.

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
/// A run's lease, renewed between batches.
pub const LEASE_SECS: i64 = 120;
/// The longest a run may take in all.
pub const MAX_RUN: std::time::Duration = std::time::Duration::from_secs(600);
/// A queued run nobody picked up for this long, or a running one whose lease ran out, no longer
/// blocks the account from starting another.
pub const STUCK_MINUTES: i32 = 15;
/// Tries per batch for failures that may pass.
const TRIES: u32 = 3;

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
    // A run that stopped without finishing doesn't block the account forever. Its worker, if any is
    // left, no longer holds the lease, so it can't write anything after this.
    sqlx::query(
        "UPDATE translation_jobs SET status = 'failed', error = 'stopped before it finished', strings = '[]', lease_owner = NULL, finished_at = now()
         WHERE account_id = $1 AND ((status = 'queued' AND created_at < now() - make_interval(mins => $2)) OR (status = 'running' AND lease_until < now()))",
    )
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

/// Record how a run ended, if this worker still holds it, and give back the characters it never
/// sent (`refund`), in one transaction. The copied text is cleared.
async fn finish(state: &AppState, id: Uuid, owner: Uuid, status: &str, error: Option<&str>, refund: i64) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let row: Option<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "UPDATE translation_jobs SET status = $3, error = $4, strings = '[]', lease_owner = NULL, finished_at = now()
         WHERE id = $1 AND lease_owner = $2 AND status = 'running' RETURNING account_id, created_at",
    )
    .bind(id)
    .bind(owner)
    .bind(status)
    .bind(error)
    .fetch_optional(&mut *tx)
    .await?;
    if let (Some((account_id, created_at)), true) = (row, refund > 0) {
        sqlx::query(
            "UPDATE translation_usage u SET chars = GREATEST(u.chars - $2, 0) FROM accounts a
             WHERE u.account_id = $1 AND a.id = u.account_id AND u.day = ($3 AT TIME ZONE a.timezone)::date",
        )
        .bind(account_id)
        .bind(refund)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Renew this worker's lease. False: it no longer holds the run (it must stop).
async fn renew(state: &AppState, id: Uuid, owner: Uuid) -> sqlx::Result<bool> {
    let n = sqlx::query("UPDATE translation_jobs SET lease_until = now() + make_interval(secs => $3) WHERE id = $1 AND lease_owner = $2 AND status = 'running'")
        .bind(id)
        .bind(owner)
        .bind(LEASE_SECS as f64)
        .execute(&state.db)
        .await?
        .rows_affected();
    Ok(n == 1)
}

/// What one batch produced, written in one transaction.
struct BatchResult {
    /// field id → (draft, status)
    drafts: BTreeMap<String, (String, &'static str)>,
    /// Field ids handled (drafted or given up on).
    done: Vec<String>,
    skipped: usize,
    /// Characters sent for this batch.
    sent: i64,
}

enum Written {
    Ok,
    /// The worker lost the run; nothing was written.
    Lost,
    /// The run can't go on (the message can't be changed now, the balance ran out).
    Stop(String),
}

/// Write a batch's drafts, its charge and the run's progress together, if this worker still holds
/// the run.
#[allow(clippy::too_many_arguments)]
async fn write_batch(state: &AppState, id: Uuid, owner: Uuid, account_id: Uuid, target: &Target, scheduled_ok: bool, lang: &str, default_lang: &str, hashes: &BTreeMap<String, String>, batch_no: usize, b: &BatchResult) -> anyhow::Result<Written> {
    let mut tx = state.db.begin().await?;
    let held: Option<Uuid> = sqlx::query_scalar("SELECT id FROM translation_jobs WHERE id = $1 AND lease_owner = $2 AND status = 'running' FOR UPDATE").bind(id).bind(owner).fetch_optional(&mut *tx).await?;
    if held.is_none() {
        return Ok(Written::Lost);
    }
    let mut written = 0usize;
    let mut later = 0usize;
    if !b.drafts.is_empty() {
        let res = store::update_in(&mut tx, account_id, target, scheduled_ok, |l| {
            let current = fields::fields(&l.content);
            for (fid, (text, status)) in &b.drafts {
                let Some(f) = current.iter().find(|f| &f.id == fid) else {
                    later += 1;
                    continue;
                };
                if hashes.get(fid) != Some(&fields::hash(&f.source)) || fields::status(&l.content, f, lang) == fields::REVIEWED {
                    later += 1;
                    continue;
                }
                fields::set_entry(&mut l.content, fid, lang, Some((text, status)))?;
                written += 1;
            }
            if written > 0 && !matches!(target, Target::SavedBlock { .. }) {
                let default = fields::languages(&l.content).0.unwrap_or(default_lang.to_string());
                fields::add_language(&mut l.content, &default, lang);
            }
            Ok(())
        })
        .await;
        match res {
            Ok(()) => {}
            Err(AppError::Internal(e)) => return Err(e),
            Err(e) => return Ok(Written::Stop(e.public_message())),
        }
    }
    // The charge for what was sent, with the drafts (a unique reference per batch).
    let price = price_micros(state, b.sent);
    if price > 0 {
        let balance: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_one(&mut *tx).await?;
        if balance < price {
            return Ok(Written::Stop("your balance is empty; add credit in Settings → Billing".into()));
        }
        crate::billing::post_entry_tx(&mut tx, account_id, "translation", -price, "Machine translation", &format!("translation:{id}:{batch_no}")).await?;
    }
    sqlx::query(
        "UPDATE translation_jobs SET translated = translated + $2, skipped = skipped + $3, sent_chars = sent_chars + $4, charged_micros = charged_micros + $5,
            done_ids = done_ids || $6, lease_until = now() + make_interval(secs => $7) WHERE id = $1",
    )
    .bind(id)
    .bind(written as i32)
    .bind((b.skipped + later) as i32)
    .bind(b.sent as i32)
    .bind(price)
    .bind(json!(b.done))
    .bind(LEASE_SECS as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Written::Ok)
}

/// One request, tried again within the run's bound when the failure may pass.
async fn translate_with_retries(state: &AppState, id: Uuid, owner: Uuid, deadline: std::time::Instant, req: &ai::Request<'_>) -> anyhow::Result<Result<BTreeMap<String, String>, ai::Failure>> {
    let mut tries = 0;
    loop {
        tries += 1;
        match ai::translate(req).await {
            Ok(out) => return Ok(Ok(out)),
            Err(f) if f.transient && tries < TRIES => {
                let wait = f.retry_after.unwrap_or(std::time::Duration::from_secs(2u64.pow(tries)));
                if std::time::Instant::now() + wait >= deadline {
                    return Ok(Err(f));
                }
                tracing::info!(job = %id, error = %f, "machine translation will be tried again");
                tokio::time::sleep(wait).await;
                if !renew(state, id, owner).await? {
                    return Ok(Err(ai::Failure::fatal("the run was taken over")));
                }
            }
            Err(f) => return Ok(Err(f)),
        }
    }
}

/// The background job: translate batch by batch, writing each batch's drafts, charge and progress
/// together, and record the outcome.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload.get("job_id").and_then(|v| v.as_str()).and_then(|v| v.parse::<Uuid>().ok()) else { return Ok(()) };
    let scheduled_ok = payload.get("scheduled_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let owner = Uuid::now_v7();
    // Take the run (a finished one, or one another worker holds, isn't touched).
    type Row = (Uuid, String, Uuid, String, String, Value, i32, f64, Value, i32);
    let row: Option<Row> = sqlx::query_as(
        "UPDATE translation_jobs SET status = 'running', lease_owner = $2, lease_until = now() + make_interval(secs => $3), started_at = COALESCE(started_at, now())
         WHERE id = $1 AND status IN ('queued', 'running') AND (lease_until IS NULL OR lease_until < now())
         RETURNING account_id, target_kind, target_id, target_part, language, strings, chars, EXTRACT(EPOCH FROM now() - started_at)::float8, done_ids, sent_chars",
    )
    .bind(id)
    .bind(owner)
    .bind(LEASE_SECS as f64)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, kind, target_id, part, lang, strings, chars, elapsed, done_ids, sent_before)) = row else { return Ok(()) };
    let unsent = |sent: i64| (i64::from(chars) - sent).max(0);
    let elapsed = std::time::Duration::from_secs_f64(elapsed.max(0.0));
    if elapsed >= MAX_RUN {
        return finish(state, id, owner, "failed", Some("it took longer than 10 minutes"), unsent(i64::from(sent_before))).await;
    }
    let deadline = std::time::Instant::now() + (MAX_RUN - elapsed);
    let Some(target) = Target::from_parts(&kind, target_id, &part) else { return finish(state, id, owner, "failed", Some("unknown message kind"), unsent(i64::from(sent_before))).await };
    let Some(key) = state.cfg.anthropic_api_key.clone().filter(|k| !k.trim().is_empty()) else {
        return finish(state, id, owner, "failed", Some("machine translation isn't set up on this server"), unsent(i64::from(sent_before))).await;
    };
    let settings = super::texts::settings(&state.db, account_id).await?;
    let source_lang = store::load(&state.db, account_id, &target).await.ok().and_then(|l| fields::languages(&l.content).0).unwrap_or(settings.default_language.clone());
    let glossary = glossary_for(&state.db, account_id, &lang).await?;
    let done: Vec<String> = done_ids.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();

    // The strings still to do, protected; grouped into requests within the size limits.
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
        .filter(|(fid, ..)| !done.contains(fid))
        .collect();
    let hashes: BTreeMap<String, String> = list.iter().map(|(fid, _, _, h)| (fid.clone(), h.clone())).collect();
    let mut protected = BTreeMap::new();
    let mut unusable: Vec<String> = Vec::new();
    for (fid, kind, source, _) in &list {
        match ai::protect(*kind, source, &glossary) {
            Some(p) => {
                protected.insert(fid.clone(), p);
            }
            None => unusable.push(fid.clone()),
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
            unusable.push(fid.clone());
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
    // Texts that can't be machine translated at all are recorded as skipped up front.
    let source_of = |fid: &str| list.iter().find(|x| x.0 == fid).map(|x| x.2.clone()).unwrap_or_default();
    let mut sent_total = i64::from(sent_before);
    if !unusable.is_empty() {
        let b = BatchResult { drafts: BTreeMap::new(), done: unusable.clone(), skipped: unusable.len(), sent: 0 };
        match write_batch(state, id, owner, account_id, &target, scheduled_ok, &lang, &settings.default_language, &hashes, 0, &b).await? {
            Written::Ok => {}
            Written::Lost => return Ok(()),
            Written::Stop(why) => return finish(state, id, owner, "failed", Some(&why), unsent(sent_total)).await,
        }
    }
    let first_batch = done.len();
    for (i, batch) in batches.iter().enumerate() {
        if std::time::Instant::now() >= deadline {
            return finish(state, id, owner, "failed", Some("it took longer than 10 minutes; what was drafted so far is kept"), unsent(sent_total)).await;
        }
        if !renew(state, id, owner).await? {
            return Ok(());
        }
        let sent: i64 = batch.values().map(|t| t.chars().count() as i64).sum();
        let req = ai::Request { base_url: state.cfg.anthropic_api_base.as_deref(), api_key: &key, source_lang: &source_lang, target_lang: &lang, glossary: &glossary, strings: batch };
        let answer = translate_with_retries(state, id, owner, deadline, &req).await?;
        sent_total += sent;
        let out = match answer {
            Ok(out) => out,
            Err(f) => {
                // What was sent counts; the run ends saying what wasn't drafted.
                tracing::warn!(job = %id, error = %f, "machine translation request failed");
                let left: usize = batches[i..].iter().map(|b| b.len()).sum();
                let b = BatchResult { drafts: BTreeMap::new(), done: batch.keys().cloned().collect(), skipped: batch.len(), sent };
                if let Written::Lost = write_batch(state, id, owner, account_id, &target, scheduled_ok, &lang, &settings.default_language, &hashes, first_batch + i + 1, &b).await? {
                    return Ok(());
                }
                let msg = format!("{left} {} weren't drafted: {}", if left == 1 { "text" } else { "texts" }, f.msg);
                return finish(state, id, owner, "failed", Some(&msg), unsent(sent_total)).await;
            }
        };
        let mut b = BatchResult { drafts: BTreeMap::new(), done: batch.keys().cloned().collect(), skipped: 0, sent };
        for fid in batch.keys() {
            let source = source_of(fid);
            let restored = match (protected.get(fid), out.get(fid)) {
                (Some(p), Some(t)) => ai::restore(p, t, &source),
                _ => Err("missing from the answer"),
            };
            match restored {
                Ok(text) => {
                    let status = if ai::same_literals(&source, &text) { fields::MACHINE } else { fields::CHECK };
                    b.drafts.insert(fid.clone(), (text, status));
                }
                Err(why) => {
                    tracing::info!(job = %id, reason = why, "a machine translation was rejected");
                    b.skipped += 1;
                }
            }
        }
        match write_batch(state, id, owner, account_id, &target, scheduled_ok, &lang, &settings.default_language, &hashes, first_batch + i + 1, &b).await? {
            Written::Ok => {}
            Written::Lost => return Ok(()),
            Written::Stop(why) => return finish(state, id, owner, "failed", Some(&why), unsent(sent_total)).await,
        }
    }
    finish(state, id, owner, "done", None, unsent(sent_total)).await
}

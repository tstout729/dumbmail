//! Privacy requests: export everything held about one person, or erase them fully while keeping a
//! minimal suppression record (a salted one-way code of their address and the reason) so they stay
//! suppressed if they come back in an import. Also the files people download after signing in.
//!
//! [`erase_in`] is the one erasure path: the profile delete, bulk delete and privacy delete all use
//! it. Besides the profile and every row keyed by it, it removes the person's personal exports and
//! every stored multi-person export file of the account (they can't be searched cheaply, expire in
//! 7 days anyway and can be made again), and export jobs still running throw their file away.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::consent::{self, Channel, ConsentSource, ConsentState};
use super::merge::{OnErase, PROFILE_TABLES};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone};
use crate::state::AppState;

pub const FILE_DAYS: i64 = 7;
pub const EXPORT_JOB: &str = "people_privacy_export";
/// The most rows of one kind (events, messages, consent history, notes) a personal export holds per
/// profile. Past it, the file says so at the top.
pub const EXPORT_MAX_ROWS: i64 = 250_000;
const PAGE: i64 = 2_000;
const CHUNK_BYTES: usize = 512 * 1024;
/// Rows the readable HTML page lists per table; everything is in the JSON file.
const HTML_ROWS: usize = 2_000;

/// This account's random salt for one-way codes (made the first time it's needed).
pub async fn salt(db: &PgPool, account_id: Uuid) -> AppResult<String> {
    salt_in(&mut *db.acquire().await?, account_id).await
}

pub async fn salt_in(conn: &mut PgConnection, account_id: Uuid) -> AppResult<String> {
    sqlx::query("INSERT INTO privacy_salts (account_id, salt) VALUES ($1, $2) ON CONFLICT (account_id) DO NOTHING")
        .bind(account_id)
        .bind(crate::auth::random_token(32))
        .execute(&mut *conn)
        .await?;
    Ok(sqlx::query_scalar("SELECT salt FROM privacy_salts WHERE account_id = $1").bind(account_id).fetch_one(&mut *conn).await?)
}

/// The one-way code for an address. Emails are reduced to the inbox they reach (case, +tags and
/// Gmail dots don't matter), so a re-import under a variant of the address still matches.
pub fn address_hash(salt: &str, channel: &str, address: &str) -> String {
    let key = if channel == "email" { crate::optin::email_key(address) } else { address.trim().to_string() };
    crate::auth::sha256_hex(&format!("{salt}|{channel}|{key}"))
}

/// What a profile's state says to keep after it's gone: (channel, address, reason).
fn tombstone_reasons(s: &ConsentState) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    for ch in [Channel::Email, Channel::Sms] {
        let Some(addr) = s.address(ch) else { continue };
        if let Some(r) = s.suppressed(ch) {
            out.push((ch.as_str(), addr.to_string(), r.to_string()));
        }
        if s.consent(ch) == "unsubscribed" {
            out.push((ch.as_str(), addr.to_string(), "unsubscribed".to_string()));
        }
    }
    out
}

/// Keep the suppression record for addresses of `s` that are going away. `keep` lists addresses
/// that stay on a profile (a merge keeps some), which need no record.
pub async fn write_tombstones(conn: &mut PgConnection, account_id: Uuid, salt: &str, s: &ConsentState, keep: &[&str]) -> sqlx::Result<u64> {
    let mut n = 0;
    for (channel, address, reason) in tombstone_reasons(s) {
        if keep.contains(&address.as_str()) {
            continue;
        }
        n += sqlx::query("INSERT INTO suppression_tombstones (account_id, channel, address_hash, reason) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(account_id)
            .bind(channel)
            .bind(address_hash(salt, channel, &address))
            .bind(&reason)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    }
    Ok(n)
}

/// Let go of one retained record (the person's own verified opt-in, or a manual suppression lifted
/// by hand), so the next upsert doesn't bring it back.
pub async fn forget_tombstone_in(conn: &mut PgConnection, account_id: Uuid, ch: Channel, address: &str, reason: &str) -> sqlx::Result<()> {
    let salt: Option<String> = sqlx::query_scalar("SELECT salt FROM privacy_salts WHERE account_id = $1").bind(account_id).fetch_optional(&mut *conn).await?;
    let Some(salt) = salt else { return Ok(()) };
    sqlx::query("DELETE FROM suppression_tombstones WHERE account_id = $1 AND channel = $2 AND address_hash = $3 AND reason = $4")
        .bind(account_id)
        .bind(ch.as_str())
        .bind(address_hash(&salt, ch.as_str(), address))
        .bind(reason)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A profile whose current email or phone number matches a deleted person's suppression record gets
/// it back, in the caller's transaction (which holds the row): the stronger suppression wins and
/// keeps its reason, and a retained unsubscribe applies whatever the caller asked for. Only the
/// person's own verified opt-in ([`consent::opt_in_verified_in`]) lets go of an unsubscribe.
/// Returns whether anything changed.
pub async fn apply_tombstones_in(conn: &mut PgConnection, account_id: Uuid, profile_id: Uuid, src: &ConsentSource) -> AppResult<bool> {
    let salt: Option<String> = sqlx::query_scalar("SELECT salt FROM privacy_salts WHERE account_id = $1").bind(account_id).fetch_optional(&mut *conn).await?;
    // No salt yet means nobody was ever deleted here, so there's nothing to look up.
    let Some(salt) = salt else { return Ok(false) };
    let Some(before) = consent::lock(conn, account_id, profile_id).await? else { return Ok(false) };
    let hashes: Vec<String> = [Channel::Email, Channel::Sms].iter().filter_map(|c| before.address(*c).map(|a| address_hash(&salt, c.as_str(), a))).collect();
    if hashes.is_empty() {
        return Ok(false);
    }
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT channel, reason FROM suppression_tombstones WHERE account_id = $1 AND address_hash = ANY($2)")
        .bind(account_id)
        .bind(&hashes)
        .fetch_all(&mut *conn)
        .await?;
    if rows.is_empty() {
        return Ok(false);
    }
    let mut after = before.clone();
    for ch in [Channel::Email, Channel::Sms] {
        if before.address(ch).is_none() {
            continue;
        }
        for (_, reason) in rows.iter().filter(|(c, _)| c == ch.as_str()) {
            if reason == "unsubscribed" {
                after.set_consent(ch, "unsubscribed");
            } else if consent::suppression_rank(Some(reason)) > consent::suppression_rank(after.suppressed(ch)) {
                after.set_suppressed(ch, Some(reason.clone()));
            }
        }
    }
    let src = ConsentSource { method: "deleted_profile", ..src.clone() };
    Ok(consent::apply(conn, account_id, profile_id, &before, &after, &src).await?)
}

#[derive(sqlx::FromRow)]
struct Doomed {
    id: Uuid,
    external_id: Option<String>,
    #[sqlx(flatten)]
    state: ConsentState,
}

/// Who an erasure removed: (id, email, phone number, external id), for the callers' webhooks.
pub type ErasedPerson = (Uuid, Option<String>, Option<String>, Option<String>);

/// Erase profiles of this account fully, in the caller's transaction: every row keyed by them (see
/// `merge::PROFILE_TABLES`), their personal exports, the account's stored multi-person exports, and
/// background work holding their details, keeping only the minimal suppression record. Returns the
/// rows removed per table (empty when none of the profiles exist).
pub async fn erase_in(conn: &mut PgConnection, account_id: Uuid, ids: &[Uuid]) -> AppResult<Map<String, Value>> {
    Ok(erase_people_in(conn, account_id, ids).await?.0)
}

/// [`erase_in`], also returning who was removed.
pub async fn erase_people_in(conn: &mut PgConnection, account_id: Uuid, ids: &[Uuid]) -> AppResult<(Map<String, Value>, Vec<ErasedPerson>)> {
    let mut removed = Map::new();
    if ids.is_empty() {
        return Ok((removed, Vec::new()));
    }
    // First, so an export job publishing its file waits for this and then sees it (see `erased_since`).
    sqlx::query("INSERT INTO privacy_erasures (account_id, erased_at) VALUES ($1, clock_timestamp()) ON CONFLICT (account_id) DO UPDATE SET erased_at = clock_timestamp()")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    let doomed: Vec<Doomed> = sqlx::query_as(&format!("SELECT id, external_id, {} FROM profiles WHERE account_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE", consent::STATE_COLS))
        .bind(account_id)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    if doomed.is_empty() {
        return Ok((removed, Vec::new()));
    }
    let ids: Vec<Uuid> = doomed.iter().map(|d| d.id).collect();
    // Reviews and photos first, with the star summaries they counted in (features::reviews).
    let reviews = crate::features::reviews::store::erase_reviews_in(conn, account_id, &ids).await?;
    let id_text: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let salt = salt_in(conn, account_id).await?;
    let mut kept = 0;
    let mut addresses: Vec<String> = Vec::new();
    for d in &doomed {
        kept += write_tombstones(conn, account_id, &salt, &d.state, &[]).await?;
        addresses.extend([d.state.email.clone(), d.state.phone_number.clone()].into_iter().flatten());
    }
    let mut count = |k: &str, n: u64| {
        if n > 0 {
            let prev = removed.get(k).and_then(Value::as_u64).unwrap_or(0);
            removed.insert(k.into(), json!(prev + n));
        }
    };
    // Their personal exports (made from any profile later merged into these), then every stored
    // file that holds many people: the account's bulk exports and report exports.
    let personal = sqlx::query("DELETE FROM people_files f USING people_file_subjects s WHERE s.file_id = f.id AND f.account_id = $1 AND s.profile_id = ANY($2)")
        .bind(account_id)
        .bind(&ids)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let bulk = sqlx::query("DELETE FROM people_files WHERE account_id = $1 AND kind <> 'privacy_export'").bind(account_id).execute(&mut *conn).await?.rows_affected();
    let reports = sqlx::query("DELETE FROM exports WHERE account_id = $1").bind(account_id).execute(&mut *conn).await?.rows_affected();
    count("export_files", personal + bulk + reports);
    // Consent history rows waiting to be written late carry their address and browser.
    sqlx::query("DELETE FROM jobs WHERE kind = $1 AND payload->>'profile_id' = ANY($2)").bind(consent::HISTORY_JOB).bind(&id_text).execute(&mut *conn).await?;
    // Saved bulk-action searches that name them (a plain substring test, no LIKE wildcards).
    sqlx::query("UPDATE bulk_actions SET selection = selection - 'q' WHERE account_id = $1 AND EXISTS (SELECT 1 FROM unnest($2::text[]) a WHERE position(lower(a) IN lower(selection->>'q')) > 0)")
        .bind(account_id)
        .bind(&addresses)
        .execute(&mut *conn)
        .await?;
    // A "suppress these addresses" job that hasn't reached them yet: keep the suppression as a
    // one-way code, so taking the address out of the job can't leave them reachable.
    let pending: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT t.address FROM bulk_action_targets t JOIN bulk_actions b ON b.id = t.bulk_id WHERE b.account_id = $1 AND t.address = ANY($2) AND t.seq > b.cursor_seq",
    )
    .bind(account_id)
    .bind(&addresses)
    .fetch_all(&mut *conn)
    .await?;
    for a in &pending {
        let ch = if a.contains('@') { "email" } else { "sms" };
        sqlx::query("INSERT INTO suppression_tombstones (account_id, channel, address_hash, reason) VALUES ($1, $2, $3, 'manual') ON CONFLICT DO NOTHING")
            .bind(account_id)
            .bind(ch)
            .bind(address_hash(&salt, ch, a))
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("DELETE FROM bulk_action_targets t USING bulk_actions b WHERE b.id = t.bulk_id AND b.account_id = $1 AND t.address = ANY($2)")
        .bind(account_id)
        .bind(&addresses)
        .execute(&mut *conn)
        .await?;
    for t in PROFILE_TABLES {
        let n = match t.on_erase {
            OnErase::Delete => sqlx::query(&format!("DELETE FROM {} WHERE {} = ANY($1)", t.table, t.column)).bind(&ids).execute(&mut *conn).await?.rows_affected(),
            // Bulk actions keep their count; these targets are reported as skipped.
            OnErase::Forget => sqlx::query(&format!("UPDATE {} SET {} = NULL WHERE {} = ANY($1)", t.table, t.column, t.column)).bind(&ids).execute(&mut *conn).await?.rows_affected(),
        };
        count(t.table, n);
    }
    sqlx::query("DELETE FROM profiles WHERE account_id = $1 AND id = ANY($2)").bind(account_id).bind(&ids).execute(&mut *conn).await?;
    count("reviews", reviews);
    removed.insert("profiles".into(), json!(ids.len()));
    removed.insert("suppression_records_kept".into(), json!(kept));
    let people = doomed.into_iter().map(|d| (d.id, d.state.email, d.state.phone_number, d.external_id)).collect();
    Ok((removed, people))
}

/// Erase one profile in a transaction of its own. Returns what it held (email, phone number,
/// external id), or None when there's no such profile.
pub async fn delete_profile_fully(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Option<(Option<String>, Option<String>, Option<String>)>> {
    let mut tx = db.begin().await?;
    let (_, people) = erase_people_in(&mut tx, account_id, &[profile_id]).await?;
    tx.commit().await?;
    Ok(people.into_iter().next().map(|(_, e, p, x)| (e, p, x)))
}

/// Make sure this account has an erasure marker row, so a job can lock it (see [`erased_since`]).
pub async fn ensure_erasure_marker(db: &PgPool, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO privacy_erasures (account_id, erased_at) VALUES ($1, 'epoch') ON CONFLICT DO NOTHING").bind(account_id).execute(db).await?;
    Ok(())
}

/// Whether anyone in the account was erased after `since`. Locks the marker row (for share), so an
/// erasure running right now finishes first; call it in the transaction that publishes a file.
pub async fn erased_since(conn: &mut PgConnection, account_id: Uuid, since: DateTime<Utc>) -> sqlx::Result<bool> {
    let at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT erased_at FROM privacy_erasures WHERE account_id = $1 FOR SHARE").bind(account_id).fetch_optional(&mut *conn).await?;
    Ok(at.is_some_and(|t| t >= since))
}

// ---------- Files ----------

/// Start a file (not downloadable until [`publish_file_in`]).
pub async fn create_file(db: &PgPool, account_id: Uuid, kind: &str, filename: &str, content_type: &str, request_id: Option<Uuid>, subjects: &[Uuid]) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO people_files (id, account_id, kind, filename, content_type, request_id, expires_at) VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7))")
        .bind(id)
        .bind(account_id)
        .bind(kind)
        .bind(filename)
        .bind(content_type)
        .bind(request_id)
        .bind(FILE_DAYS as i32)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO people_file_subjects (file_id, profile_id) SELECT $1, unnest($2::uuid[])").bind(id).bind(subjects).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn write_chunk<'e>(db: impl sqlx::PgExecutor<'e>, file_id: Uuid, seq: i32, bytes: &[u8]) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO people_file_chunks (file_id, seq, bytes) VALUES ($1, $2, $3) ON CONFLICT (file_id, seq) DO UPDATE SET bytes = EXCLUDED.bytes")
        .bind(file_id)
        .bind(seq)
        .bind(bytes)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn publish_file_in(conn: &mut PgConnection, file_id: Uuid) -> sqlx::Result<bool> {
    Ok(sqlx::query("UPDATE people_files SET ready = true, expires_at = now() + make_interval(days => $2) WHERE id = $1").bind(file_id).bind(FILE_DAYS as i32).execute(conn).await?.rows_affected() == 1)
}

pub fn file_url(id: Uuid) -> String {
    format!("/api/people/files/{id}")
}

/// A ready file for download, if it belongs to this account and hasn't expired.
pub async fn file(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<(String, String, String, Vec<u8>)> {
    let meta: Option<(String, String, String)> = sqlx::query_as("SELECT kind, filename, content_type FROM people_files WHERE id = $1 AND account_id = $2 AND ready AND expires_at > now()")
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?;
    let (kind, name, ct) = meta.ok_or_else(|| AppError::NotFound("This download has expired or doesn't exist. Downloads last 7 days.".into()))?;
    let chunks: Vec<Vec<u8>> = sqlx::query_scalar("SELECT bytes FROM people_file_chunks WHERE file_id = $1 ORDER BY seq").bind(id).fetch_all(db).await?;
    Ok((kind, name, ct, chunks.concat()))
}

// ---------- Requests ----------

#[derive(Deserialize, Default)]
pub struct PersonQuery {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

struct Person {
    identified_by: &'static str,
    subject: String,
    profile_ids: Vec<Uuid>,
}

async fn find_person(db: &PgPool, account_id: Uuid, q: &PersonQuery) -> AppResult<Person> {
    let email = q.email.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let phone = q.phone_number.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (identified_by, subject) = match (email, phone) {
        (Some(e), None) => ("email", normalize_email(e).ok_or_else(|| AppError::bad(format!("{e:?} is not a valid email")))?),
        (None, Some(p)) => ("phone_number", normalize_phone(p).ok_or_else(|| AppError::bad(format!("{p:?} is not a valid phone number (use +15035550100)")))?),
        _ => return Err(AppError::bad("give either an email or a phone_number")),
    };
    let col = if identified_by == "email" { "email" } else { "phone_number" };
    let profile_ids: Vec<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM profiles WHERE account_id = $1 AND {col} = $2 ORDER BY created_at LIMIT 10"))
        .bind(account_id)
        .bind(&subject)
        .fetch_all(db)
        .await?;
    Ok(Person { identified_by, subject, profile_ids })
}

async fn log_request(conn: &mut PgConnection, account_id: Uuid, kind: &str, status: &str, p: &Person, src: &ConsentSource, result: Value) -> AppResult<Uuid> {
    let salt = salt_in(conn, account_id).await?;
    let id = Uuid::now_v7();
    let channel = if p.identified_by == "email" { "email" } else { "sms" };
    sqlx::query(
        "INSERT INTO privacy_requests (id, account_id, kind, status, subject_hash, identified_by, requested_by_user, via, profiles_found, result)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(account_id)
    .bind(kind)
    .bind(status)
    .bind(address_hash(&salt, channel, &p.subject))
    .bind(p.identified_by)
    .bind(src.actor_user_id)
    .bind(src.via)
    .bind(p.profile_ids.len() as i32)
    .bind(result)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Export everything held about one person (found by email or phone). A background job builds a
/// JSON file and a readable HTML page, downloadable after signing in for 7 days; the request (with
/// its status and, once ready, the download links) is in the request log. Nothing is returned inline.
pub async fn export_person(state: &AppState, account_id: Uuid, q: &PersonQuery, src: &ConsentSource) -> AppResult<Value> {
    let p = find_person(&state.db, account_id, q).await?;
    let mut tx = state.db.begin().await?;
    if p.profile_ids.is_empty() {
        let id = log_request(&mut tx, account_id, "export", "done", &p, src, json!({"found": false})).await?;
        tx.commit().await?;
        return Ok(json!({"request_id": id, "status": "done", "found": 0, "note": "Nobody with that address is in this account, so there's nothing to export."}));
    }
    let id = log_request(&mut tx, account_id, "export", "building", &p, src, json!({"found": true})).await?;
    crate::jobs::enqueue(&mut *tx, EXPORT_JOB, json!({"request_id": id, "account_id": account_id, "profile_ids": p.profile_ids}), None).await?;
    tx.commit().await?;
    Ok(json!({
        "request_id": id, "status": "building", "found": p.profile_ids.len(),
        "note": "The export is being built. In a minute or two the request log (list_privacy_requests) has the JSON and HTML download links; they need sign-in and last 7 days.",
    }))
}

/// One request from the log.
pub async fn request(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let (rows, _) = requests_where(db, account_id, Some(id), 1, 0).await?;
    rows.into_iter().next().ok_or_else(|| AppError::not_found("privacy request"))
}

/// Delete everything held about one person (found by email or phone). Without `confirm` it only
/// says what would be deleted. The request is logged.
pub async fn delete_person(state: &AppState, account_id: Uuid, q: &PersonQuery, src: &ConsentSource) -> AppResult<Value> {
    let p = find_person(&state.db, account_id, q).await?;
    if !q.confirm {
        let mut profiles = Vec::new();
        for pid in &p.profile_ids {
            let counts: (i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM events WHERE profile_id = $1), (SELECT count(*) FROM messages WHERE profile_id = $1), (SELECT count(*) FROM list_members WHERE profile_id = $1)",
            )
            .bind(pid)
            .fetch_one(&state.db)
            .await?;
            profiles.push(json!({"profile_id": pid, "events": counts.0, "messages": counts.1, "lists": counts.2}));
        }
        return Ok(json!({
            "dry_run": true,
            "found": p.profile_ids.len(),
            "profiles": profiles,
            "note": if p.profile_ids.is_empty() { "Nobody with that address is in this account.".to_string() } else {
                "Nothing deleted yet. This deletes the profile with its events, messages, list memberships and consent history, and can't be undone. Stored export files of this account are deleted too (they may hold this person). If they had unsubscribed or were suppressed, a one-way code of the address is kept so they stay suppressed if they're imported again. Call again with confirm=true once the user approves.".to_string()
            },
        }));
    }
    let mut tx = state.db.begin().await?;
    let removed = erase_in(&mut tx, account_id, &p.profile_ids).await?;
    let deleted = removed.get("profiles").and_then(Value::as_u64).unwrap_or(0);
    let files = removed.get("export_files").and_then(Value::as_u64).unwrap_or(0);
    let id = log_request(&mut tx, account_id, "delete", "done", &p, src, json!({"deleted_profiles": deleted, "export_files_deleted": files})).await?;
    tx.commit().await?;
    Ok(json!({"request_id": id, "deleted": deleted, "removed": removed, "export_files_deleted": files}))
}

/// The log of privacy requests, newest first.
pub async fn requests(db: &PgPool, account_id: Uuid, limit: i64, offset: i64) -> AppResult<(Vec<Value>, i64)> {
    requests_where(db, account_id, None, limit, offset).await
}

async fn requests_where(db: &PgPool, account_id: Uuid, only: Option<Uuid>, limit: i64, offset: i64) -> AppResult<(Vec<Value>, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_requests WHERE account_id = $1 AND ($2::uuid IS NULL OR id = $2)").bind(account_id).bind(only).fetch_one(db).await?;
    let rows: Vec<(Uuid, String, String, String, Option<String>, String, i32, Value, DateTime<Utc>, Value)> = sqlx::query_as(
        "SELECT r.id, r.kind, r.status, r.identified_by, u.email, r.via, r.profiles_found, r.result, r.created_at,
                COALESCE((SELECT jsonb_agg(jsonb_build_object('id', f.id, 'name', f.filename, 'expires_at', f.expires_at) ORDER BY f.filename)
                          FROM people_files f WHERE f.request_id = r.id AND f.ready AND f.expires_at > now()), '[]'::jsonb)
         FROM privacy_requests r LEFT JOIN users u ON u.id = r.requested_by_user
         WHERE r.account_id = $1 AND ($4::uuid IS NULL OR r.id = $4) ORDER BY r.created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(account_id)
    .bind(limit.clamp(1, 200))
    .bind(offset.max(0))
    .bind(only)
    .fetch_all(db)
    .await?;
    let data = rows
        .into_iter()
        .map(|r| {
            let files: Vec<Value> = r.9.as_array().cloned().unwrap_or_default().into_iter().map(|mut f| {
                if let Some(id) = f["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                    f["url"] = json!(file_url(id));
                }
                f
            }).collect();
            json!({"id": r.0, "kind": r.1, "status": r.2, "identified_by": r.3, "requested_by": r.4, "via": r.5, "profiles_found": r.6, "result": r.7, "created_at": r.8, "files": files})
        })
        .collect();
    Ok((data, total))
}

// ---------- The export job ----------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// JSON written in bounded chunks to a file.
struct ChunkWriter {
    file_id: Uuid,
    seq: i32,
    buf: String,
}

impl ChunkWriter {
    fn new(file_id: Uuid) -> Self {
        Self { file_id, seq: 0, buf: String::new() }
    }
    fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }
    fn value(&mut self, v: &Value) {
        self.buf.push_str(&v.to_string());
    }
    async fn maybe_flush(&mut self, db: &PgPool) -> sqlx::Result<()> {
        if self.buf.len() >= CHUNK_BYTES {
            self.flush(db).await?;
        }
        Ok(())
    }
    async fn flush(&mut self, db: &PgPool) -> sqlx::Result<()> {
        if !self.buf.is_empty() {
            write_chunk(db, self.file_id, self.seq, self.buf.as_bytes()).await?;
            self.seq += 1;
            self.buf.clear();
        }
        Ok(())
    }
}

/// What a profile's sections hold, counted before anything is written, so the top of the file can
/// say whether it's complete.
struct Counts {
    history: i64,
    events: i64,
    messages: i64,
    notes: i64,
}

async fn counts(db: &PgPool, account_id: Uuid, pid: Uuid) -> AppResult<Counts> {
    let c: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM consent_events WHERE account_id = $1 AND profile_id = $2),
                (SELECT count(*) FROM events WHERE account_id = $1 AND profile_id = $2),
                (SELECT count(*) FROM messages WHERE account_id = $1 AND profile_id = $2 AND NOT is_test),
                (SELECT count(*) FROM profile_notes WHERE account_id = $1 AND profile_id = $2)",
    )
    .bind(account_id)
    .bind(pid)
    .fetch_one(db)
    .await?;
    Ok(Counts { history: c.0, events: c.1, messages: c.2, notes: c.3 })
}

/// The HTML page, built alongside the JSON (each table bounded).
struct Html {
    out: String,
    tz: chrono_tz::Tz,
}

impl Html {
    fn when(&self, v: &Value) -> String {
        v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()).map(|d| d.with_timezone(&self.tz).format("%b %-d, %Y %-I:%M %p %Z").to_string()).unwrap_or_else(|| "—".into())
    }
    fn text(v: &Value) -> String {
        match v {
            Value::Null => "—".to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
    fn row(&mut self, cells: &[String]) {
        self.out.push_str("<tr>");
        for c in cells {
            self.out.push_str(&format!("<td>{}</td>", esc(c)));
        }
        self.out.push_str("</tr>");
    }
    fn more(&mut self, shown: usize, total: i64) {
        if total > shown as i64 {
            self.out.push_str(&format!("<p class=\"muted\">Showing {shown} of {total}. The JSON file has all of them.</p>"));
        }
    }
}

/// Build a personal export (see [`export_person`]). A failure marks the request failed (it can be
/// run again) rather than leaving it building.
pub async fn run_export_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    if let Err(e) = build_export(state, payload).await {
        tracing::error!(request = %payload["request_id"], error = %format!("{e:#}"), "privacy export failed");
        let id: Option<Uuid> = payload["request_id"].as_str().and_then(|s| s.parse().ok());
        sqlx::query("DELETE FROM people_files WHERE request_id = $1").bind(id).execute(&state.db).await?;
        sqlx::query("UPDATE privacy_requests SET status = 'failed', result = result || '{\"error\": \"Something went wrong building this export. Run it again.\"}'::jsonb WHERE id = $1 AND status = 'building'")
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// Exports whose job vanished (a crash) are marked failed, so the log doesn't say "building" forever.
pub async fn fail_stuck_exports(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE privacy_requests r SET status = 'failed', result = r.result || '{\"error\": \"The export stopped before it finished. Run it again.\"}'::jsonb
         WHERE r.status = 'building' AND r.created_at < now() - interval '1 hour'
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $1 AND j.payload->>'request_id' = r.id::text AND j.run_at <> 'infinity')",
    )
    .bind(EXPORT_JOB)
    .execute(db)
    .await?;
    Ok(())
}

/// A retry starts over.
async fn build_export(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let db = &state.db;
    let request_id: Uuid = payload["request_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing request_id"))?;
    let account_id: Uuid = payload["account_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing account_id"))?;
    let ids: Vec<Uuid> = serde_json::from_value(payload["profile_ids"].clone())?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM privacy_requests WHERE id = $1 AND account_id = $2").bind(request_id).bind(account_id).fetch_optional(db).await?;
    if status.as_deref() != Some("building") {
        return Ok(());
    }
    let fail = |why: &'static str| async move {
        sqlx::query("DELETE FROM people_files WHERE request_id = $1").bind(request_id).execute(db).await?;
        sqlx::query("UPDATE privacy_requests SET status = 'failed', result = result || jsonb_build_object('error', $2::text) WHERE id = $1").bind(request_id).bind(why).execute(db).await?;
        anyhow::Ok(())
    };
    // A retried job starts over.
    sqlx::query("DELETE FROM people_files WHERE request_id = $1").bind(request_id).execute(db).await?;
    let present: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND id = ANY($2) ORDER BY created_at").bind(account_id).bind(&ids).fetch_all(db).await?;
    if present.len() != ids.len() {
        return fail("The profile was deleted or merged before the export was built. Run the export again.").await;
    }
    let account = crate::campaigns::load_account(db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let tz: chrono_tz::Tz = account.timezone.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let stamp = Utc::now().with_timezone(&tz).format("%Y-%m-%d");
    let json_id = create_file(db, account_id, "privacy_export", &format!("personal-data-{stamp}.json"), "application/json", Some(request_id), &ids).await?;
    let html_id = create_file(db, account_id, "privacy_export", &format!("personal-data-{stamp}.html"), "text/html; charset=utf-8", Some(request_id), &ids).await?;

    let mut all_counts = Vec::new();
    for pid in &ids {
        all_counts.push(counts(db, account_id, *pid).await.map_err(|e| anyhow::anyhow!(e.public_message()))?);
    }
    let cut: Vec<&str> = all_counts
        .iter()
        .flat_map(|c| [("consent_history", c.history), ("events", c.events), ("messages", c.messages), ("internal_notes", c.notes)])
        .filter(|(_, n)| *n > EXPORT_MAX_ROWS)
        .map(|(k, _)| k)
        .collect();
    let mut w = ChunkWriter::new(json_id);
    w.push("{\"exported_at\":");
    w.value(&json!(Utc::now()));
    w.push(",\"business\":");
    w.value(&json!(account.name));
    w.push(",\"complete\":");
    w.value(&json!(cut.is_empty()));
    w.push(",\"note\":");
    w.value(&json!(if cut.is_empty() {
        "Everything held about this person.".to_string()
    } else {
        format!("Not complete: at most {EXPORT_MAX_ROWS} rows of each kind are included per profile, and these had more: {}.", cut.join(", "))
    }));
    w.push(",\"profiles\":[");
    let mut html = Html { out: String::new(), tz };
    let mut subject = String::new();
    for (i, pid) in ids.iter().enumerate() {
        if i > 0 {
            w.push(",");
        }
        let s = export_profile(state, account_id, *pid, &all_counts[i], &mut w, &mut html, ids.len() > 1, i).await?;
        if subject.is_empty() {
            subject = s;
        }
    }
    w.push("]}");
    w.flush(db).await?;
    let head = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Data held about {s}</title><style>body{{font:14px/1.5 system-ui,sans-serif;color:#18181b;max-width:900px;margin:32px auto;padding:0 16px}}h1{{font-size:22px}}h2{{font-size:17px;margin-top:28px}}table{{border-collapse:collapse;width:100%;margin:8px 0}}td,th{{border:1px solid #e4e4e7;padding:6px 8px;text-align:left;vertical-align:top}}th{{background:#fafafa}}.muted{{color:#71717a}}</style></head><body>\
         <h1>Data {a} holds about {s}</h1><p class=\"muted\">Exported {now}. The JSON file has the same data in full.</p>{warn}",
        s = esc(&subject),
        a = esc(&account.name),
        now = Utc::now().with_timezone(&tz).format("%b %-d, %Y %-I:%M %p %Z"),
        warn = if cut.is_empty() { String::new() } else { format!("<p><strong>Not complete:</strong> at most {EXPORT_MAX_ROWS} rows of each kind are included per profile, and these had more: {}.</p>", esc(&cut.join(", "))) },
    );
    let page = format!("{head}{}</body></html>", html.out);
    for (seq, part) in page.as_bytes().chunks(CHUNK_BYTES).enumerate() {
        write_chunk(db, html_id, seq as i32, part).await?;
    }

    // Publish, unless a subject was erased or merged away meanwhile: their rows are locked for share,
    // so an erasure or merge running now finishes first and is then seen (an erasure also deletes
    // these files through their subjects).
    let mut tx = db.begin().await?;
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT id FROM profiles WHERE account_id = $1 AND id = ANY($2) FOR SHARE) x").bind(account_id).bind(&ids).fetch_one(&mut *tx).await?;
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM people_files WHERE id = ANY($1)").bind([json_id, html_id]).fetch_one(&mut *tx).await?;
    if still != ids.len() as i64 || linked != 2 {
        drop(tx);
        return fail("The profile was deleted or merged while the export was being built. Run the export again.").await;
    }
    publish_file_in(&mut tx, json_id).await?;
    publish_file_in(&mut tx, html_id).await?;
    sqlx::query("UPDATE privacy_requests SET status = 'ready' WHERE id = $1").bind(request_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Write one profile's part of the export. Returns its email or phone number (for the page title).
#[allow(clippy::too_many_arguments)]
async fn export_profile(state: &AppState, account_id: Uuid, pid: Uuid, c: &Counts, w: &mut ChunkWriter, html: &mut Html, several: bool, index: usize) -> anyhow::Result<String> {
    let db = &state.db;
    let ae = |e: AppError| anyhow::anyhow!(e.public_message());
    let detail = crate::api::audience::profile_detail(state, account_id, pid).await.map_err(ae)?;
    let extra: (Option<String>, Option<String>, bool, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT email_suppressed_reason, sms_suppressed_reason, ads_opt_out, ads_opt_out_at FROM profiles WHERE id = $1 AND account_id = $2").bind(pid).bind(account_id).fetch_one(db).await?;
    let mut profile = detail["profile"].clone();
    profile["email_suppressed_reason"] = json!(extra.0);
    profile["sms_suppressed_reason"] = json!(extra.1);
    profile["do_not_sell_or_share"] = json!(extra.2);
    profile["do_not_sell_or_share_since"] = json!(extra.3);
    let subject = profile["email"].as_str().or(profile["phone_number"].as_str()).unwrap_or("this person").to_string();
    // Addresses and profiles this person had before (from their consent history and merges).
    let former: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT channel, address, max(occurred_at) FROM consent_events WHERE account_id = $1 AND profile_id = $2 AND address IS NOT NULL
           AND address IS DISTINCT FROM $3 AND address IS DISTINCT FROM $4 AND channel IN ('email', 'sms') GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 1000",
    )
    .bind(account_id)
    .bind(pid)
    .bind(profile["email"].as_str())
    .bind(profile["phone_number"].as_str())
    .fetch_all(db)
    .await?;
    let merged: Vec<String> = sqlx::query_scalar("SELECT DISTINCT detail->>'merged_profile_id' FROM consent_events WHERE account_id = $1 AND profile_id = $2 AND method = 'merge' AND detail ? 'merged_profile_id' LIMIT 1000")
        .bind(account_id)
        .bind(pid)
        .fetch_all(db)
        .await?;
    let lists: Vec<Value> = detail["lists"].as_array().map(|a| a.iter().map(|l| json!({"id": l["id"], "name": l["name"]})).collect()).unwrap_or_default();
    let segments: Vec<Value> = detail["segments"].as_array().map(|a| a.iter().map(|s| json!({"id": s["id"], "name": s["name"]})).collect()).unwrap_or_default();

    w.push("{\"profile\":");
    w.value(&profile);
    w.push(",\"former_identifiers\":");
    w.value(&json!(former.iter().map(|f| json!({"channel": f.0, "address": f.1, "last_seen": f.2})).collect::<Vec<_>>()));
    w.push(",\"merged_from_profiles\":");
    w.value(&json!(merged));
    w.push(",\"lists\":");
    w.value(&json!(lists));
    w.push(",\"segments\":");
    w.value(&json!(segments));

    if several {
        html.out.push_str(&format!("<h2>Profile {}</h2>", index + 1));
    }
    html.out.push_str("<h2>Profile</h2><table>");
    if let Some(o) = profile.as_object() {
        for (k, v) in o.iter().filter(|(k, _)| *k != "properties") {
            html.out.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&Html::text(v))));
        }
        for (k, v) in o.get("properties").and_then(Value::as_object).into_iter().flatten() {
            html.out.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&Html::text(v))));
        }
    }
    html.out.push_str("</table>");
    if !former.is_empty() {
        html.out.push_str(&format!("<h2>Earlier addresses</h2><p>{}</p>", former.iter().map(|f| esc(&f.1)).collect::<Vec<_>>().join(", ")));
    }
    let names = |v: &[Value]| v.iter().map(|x| esc(&Html::text(&x["name"]))).collect::<Vec<_>>().join(", ");
    html.out.push_str(&format!(
        "<h2>Lists</h2><p>{}</p><h2>Segments</h2><p>{}</p>",
        if lists.is_empty() { "None".into() } else { names(&lists) },
        if segments.is_empty() { "None".into() } else { names(&segments) }
    ));

    // Consent history, oldest last as shown in the dashboard.
    w.push(",\"consent_history\":[");
    html.out.push_str("<h2>Consent history</h2><table><tr><th>When</th><th>What</th><th>How</th></tr>");
    let (mut offset, mut shown, mut first) = (0i64, 0usize, true);
    while offset < c.history.min(EXPORT_MAX_ROWS) {
        let (page, _) = consent::history(db, account_id, pid, 500, offset).await.map_err(ae)?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as i64;
        for h in &page {
            let row = json!({"at": h["occurred_at"], "what": h["summary"], "how": h["how"], "address": h["address"], "ip": h["ip"], "user_agent": h["user_agent"], "recorded_late": h["detail"]["recorded_late"].as_bool().unwrap_or(false)});
            if !first {
                w.push(",");
            }
            first = false;
            w.value(&row);
            if shown < HTML_ROWS {
                let cells = [html.when(&row["at"]), Html::text(&row["what"]), Html::text(&row["how"])];
                html.row(&cells);
                shown += 1;
            }
        }
        w.maybe_flush(db).await?;
    }
    w.push("]");
    html.out.push_str("</table>");
    html.more(shown, c.history);

    // Form sign-ups: the sign-up events, and form submissions that changed consent (the phone step).
    let forms: Vec<(Option<String>, Option<String>, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT e.properties->>'form_id', e.properties->>'form', e.occurred_at, 'sign_up' FROM events e JOIN metrics m ON m.id = e.metric_id
          WHERE e.account_id = $1 AND e.profile_id = $2 AND m.name = 'Subscribed to List' AND e.properties ? 'form_id'
         UNION ALL
         SELECT ce.form_id::text, f.name, ce.occurred_at, 'consent_' || ce.channel FROM consent_events ce LEFT JOIN forms f ON f.id = ce.form_id
          WHERE ce.account_id = $1 AND ce.profile_id = $2 AND ce.method = 'form'
         ORDER BY 3 DESC LIMIT 10000",
    )
    .bind(account_id)
    .bind(pid)
    .fetch_all(db)
    .await?;
    w.push(",\"form_submissions\":");
    w.value(&json!(forms.iter().map(|f| json!({"form_id": f.0, "form": f.1, "submitted_at": f.2, "kind": f.3})).collect::<Vec<_>>()));
    html.out.push_str("<h2>Form sign-ups</h2><table><tr><th>When</th><th>Form</th></tr>");
    for f in forms.iter().take(HTML_ROWS) {
        let cells = [html.when(&json!(f.2)), f.1.clone().unwrap_or_else(|| "—".into())];
        html.row(&cells);
    }
    html.out.push_str("</table>");
    let confirmations: Vec<(String, Option<String>, String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT l.name, f.name, o.email, o.created_at, o.confirmed_at FROM optin_confirmations o JOIN lists l ON l.id = o.list_id
         LEFT JOIN forms f ON f.id = o.form_id WHERE o.account_id = $1 AND o.profile_id = $2 ORDER BY o.created_at DESC LIMIT 10000",
    )
    .bind(account_id)
    .bind(pid)
    .fetch_all(db)
    .await?;
    w.push(",\"double_opt_in_confirmations\":");
    w.value(&json!(confirmations.iter().map(|c| json!({"list": c.0, "form": c.1, "email": c.2, "requested_at": c.3, "confirmed_at": c.4})).collect::<Vec<_>>()));
    w.maybe_flush(db).await?;

    // Events, newest first, a page at a time.
    w.push(",\"events\":[");
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    let (mut written, mut first) = (0i64, true);
    while written < EXPORT_MAX_ROWS {
        let page: Vec<(Uuid, String, Value, Option<f64>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT e.id, m.name, e.properties, e.value, e.occurred_at FROM events e JOIN metrics m ON m.id = e.metric_id
             WHERE e.account_id = $1 AND e.profile_id = $2 AND ($3::timestamptz IS NULL OR (e.occurred_at, e.id) < ($3, $4))
             ORDER BY e.occurred_at DESC, e.id DESC LIMIT $5",
        )
        .bind(account_id)
        .bind(pid)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .bind(PAGE.min(EXPORT_MAX_ROWS - written))
        .fetch_all(db)
        .await?;
        let Some(last) = page.last() else { break };
        cursor = Some((last.4, last.0));
        written += page.len() as i64;
        for e in &page {
            if !first {
                w.push(",");
            }
            first = false;
            w.value(&json!({"metric": e.1, "properties": e.2, "value": e.3, "occurred_at": e.4}));
        }
        w.maybe_flush(db).await?;
    }
    w.push("]");
    html.out.push_str(&format!("<h2>Activity</h2><p>{} recorded events (website visits, orders, email opens and clicks and so on). Each one is in the JSON file.</p>", c.events.min(EXPORT_MAX_ROWS)));

    // Messages sent to them, with subjects and outcomes.
    w.push(",\"messages\":[");
    html.out.push_str("<h2>Messages sent</h2><table><tr><th>When</th><th>Channel</th><th>Subject</th><th>Outcome</th></tr>");
    type MsgRow = (Uuid, String, String, Option<String>, String, Option<String>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, DateTime<Utc>);
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    let (mut written, mut shown, mut first) = (0i64, 0usize, true);
    while written < EXPORT_MAX_ROWS {
        let page: Vec<MsgRow> = sqlx::query_as(
            "SELECT msg.id, msg.channel, msg.to_address, msg.subject, msg.status, c.name, f.name, msg.sent_at, msg.delivered_at, msg.opened_at, msg.clicked_at, msg.unsubscribed_at, msg.created_at
             FROM messages msg LEFT JOIN campaigns c ON c.id = msg.campaign_id LEFT JOIN flows f ON f.id = msg.flow_id
             WHERE msg.account_id = $1 AND msg.profile_id = $2 AND NOT msg.is_test AND ($3::timestamptz IS NULL OR (msg.created_at, msg.id) < ($3, $4))
             ORDER BY msg.created_at DESC, msg.id DESC LIMIT $5",
        )
        .bind(account_id)
        .bind(pid)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .bind(PAGE.min(EXPORT_MAX_ROWS - written))
        .fetch_all(db)
        .await?;
        let Some(last) = page.last() else { break };
        cursor = Some((last.12, last.0));
        written += page.len() as i64;
        for m in &page {
            if !first {
                w.push(",");
            }
            first = false;
            w.value(&json!({"channel": m.1, "to": m.2, "subject": m.3, "status": m.4, "campaign": m.5, "flow": m.6, "sent_at": m.7, "delivered_at": m.8, "opened_at": m.9, "clicked_at": m.10, "unsubscribed_at": m.11, "created_at": m.12}));
            if shown < HTML_ROWS {
                let cells = [html.when(&json!(m.12)), m.1.clone(), m.3.clone().unwrap_or_else(|| "—".into()), m.4.clone()];
                html.row(&cells);
                shown += 1;
            }
        }
        w.maybe_flush(db).await?;
    }
    w.push("]");
    html.out.push_str("</table>");
    html.more(shown, c.messages);

    // Internal notes.
    w.push(",\"internal_notes\":[");
    let (mut offset, mut shown, mut first) = (0i64, 0usize, true);
    let mut notes_html = String::new();
    while offset < c.notes.min(EXPORT_MAX_ROWS) {
        let page: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as("SELECT author_label, body, created_at FROM profile_notes WHERE account_id = $1 AND profile_id = $2 ORDER BY created_at DESC, id DESC LIMIT $3 OFFSET $4")
            .bind(account_id)
            .bind(pid)
            .bind(PAGE)
            .bind(offset)
            .fetch_all(db)
            .await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as i64;
        for n in &page {
            if !first {
                w.push(",");
            }
            first = false;
            w.value(&json!({"author": n.0, "note": n.1, "written_at": n.2}));
            if shown < HTML_ROWS {
                notes_html.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&html.when(&json!(n.2))), esc(&n.0), esc(&n.1)));
                shown += 1;
            }
        }
        w.maybe_flush(db).await?;
    }
    w.push("]}");
    if c.notes > 0 {
        html.out.push_str(&format!("<h2>Internal notes</h2><table><tr><th>When</th><th>By</th><th>Note</th></tr>{notes_html}</table>"));
        html.more(shown, c.notes);
    }
    w.maybe_flush(db).await?;
    Ok(subject)
}

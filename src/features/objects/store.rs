//! Objects: upserts (fields merge, every change recorded), links to people, deletes, and the views
//! the API and dashboard show.
//!
//! One upsert is one transaction: the type (created or given new fields on first use), the object
//! under a row lock, its links, the change record, the object-date fires that depend on it and the
//! job that starts object-triggered flows. A job or request that dies part-way leaves nothing, and
//! writing the same values again changes nothing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::types::{self, ObjectType, MAX_FIELDS, MAX_FIELDS_BYTES};
use crate::audience::{self, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Most people one object may be linked to.
pub const MAX_LINKS: usize = 20;
/// Longest external id and relationship label.
pub const MAX_EXTERNAL_ID: usize = 200;
pub const MAX_RELATIONSHIP: usize = 50;
/// Changes kept per object, and for how long.
pub const HISTORY_PER_OBJECT: i64 = 100;
pub const HISTORY_DAYS: i64 = 365;

/// Who is writing: `api`, `mcp` or `integration:<id>`, with an integration's redact list (paths
/// removed from the fields before they are written, e.g. `**.doorCode`).
#[derive(Debug, Clone)]
pub struct Writer {
    pub account_id: Uuid,
    pub source: String,
    pub redact: Vec<crate::features::integrations::redact::Path>,
}

impl Writer {
    pub fn new(account_id: Uuid, source: &str) -> Self {
        Writer { account_id, source: source.to_string(), redact: Vec::new() }
    }
}

/// A person to link, by id or identifiers (the profile is created if nobody has them).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkInput {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "phone")]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub relationship: Option<String>,
}

/// One object to create or update.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertInput {
    /// The type's key, e.g. `reservation` (created on first use).
    #[serde(rename = "type")]
    pub type_key: String,
    pub external_id: String,
    /// Merged into the object's fields; `null` removes one.
    #[serde(default)]
    pub fields: Map<String, Value>,
    #[serde(default)]
    pub profiles: Vec<LinkInput>,
    /// The change's time or version in your system. An upsert older than the newest one written
    /// is ignored.
    #[serde(default)]
    pub as_of: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Upserted {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub type_key: String,
    pub external_id: String,
    /// created | updated | unchanged | older (ignored: older than what's stored)
    pub result: &'static str,
    pub changed: Vec<String>,
    pub version: i64,
    pub linked: u32,
}

pub fn relationship(r: Option<&str>) -> AppResult<String> {
    let r = r.unwrap_or("").trim().to_lowercase();
    if r.chars().count() > MAX_RELATIONSHIP {
        return Err(AppError::bad(format!("a relationship (like guest or owner) is at most {MAX_RELATIONSHIP} characters")));
    }
    Ok(r)
}

fn check_external_id(e: &str) -> AppResult<String> {
    let e = e.trim();
    if e.is_empty() || e.chars().count() > MAX_EXTERNAL_ID {
        return Err(AppError::bad(format!("external_id must be 1-{MAX_EXTERNAL_ID} characters")));
    }
    Ok(e.to_string())
}

pub fn as_of_text(v: &Option<Value>) -> AppResult<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= 100 => Ok(Some(s.trim().to_string())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        _ => Err(AppError::bad("as_of is a time (2026-11-17T10:00:00Z) or a version number, up to 100 characters")),
    }
}

/// Whether `new` is older than `old` (times as times, numbers as numbers, otherwise as text).
pub fn older(new: &str, old: &str) -> bool {
    if let (Ok(a), Ok(b)) = (DateTime::parse_from_rfc3339(new), DateTime::parse_from_rfc3339(old)) {
        return a < b;
    }
    if let (Ok(a), Ok(b)) = (new.parse::<f64>(), old.parse::<f64>()) {
        return a < b;
    }
    new < old
}

/// Check what can be checked without the database (sizes, keys, links), before any write.
pub fn check_input(b: &UpsertInput) -> AppResult<()> {
    if !types::valid_type_key(b.type_key.trim()) {
        return Err(AppError::bad(format!("object type {:?}: a type key is 1-40 lower-case letters, digits and _ (e.g. reservation)", b.type_key)));
    }
    check_external_id(&b.external_id)?;
    check_fields_shape(&b.fields)?;
    if b.profiles.len() > MAX_LINKS {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people")));
    }
    for l in &b.profiles {
        relationship(l.relationship.as_deref())?;
        if l.id.is_none() && [&l.email, &l.phone_number, &l.external_id].iter().all(|v| v.as_deref().is_none_or(|s| s.trim().is_empty())) {
            return Err(AppError::bad("each linked person needs an id, email, phone_number or external_id"));
        }
    }
    as_of_text(&b.as_of)?;
    Ok(())
}

pub fn check_fields_shape(fields: &Map<String, Value>) -> AppResult<()> {
    if fields.len() > MAX_FIELDS {
        return Err(AppError::bad(format!("at most {MAX_FIELDS} fields per object")));
    }
    if let Some(k) = fields.keys().find(|k| !types::valid_field_key(k)) {
        return Err(AppError::bad(format!("field {k:?}: a field key is 1-64 letters, digits and _ starting with a letter")));
    }
    if serde_json::to_string(fields).map(|s| s.len()).unwrap_or(0) > MAX_FIELDS_BYTES {
        return Err(AppError::bad("an object's fields are over 32 KB"));
    }
    Ok(())
}

/// Find (or create) each linked person: by id in this account, or by identifiers. An existing
/// profile is never changed (its email, phone and consent stay as they are); a new one has only the
/// identifiers given and has never subscribed to anything.
pub async fn resolve_links(db: &PgPool, account_id: Uuid, links: &[LinkInput]) -> AppResult<Vec<(Uuid, String)>> {
    let mut out = Vec::with_capacity(links.len());
    for l in links {
        let rel = relationship(l.relationship.as_deref())?;
        let id = match l.id {
            Some(id) => {
                let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM profiles WHERE id = $1 AND account_id = $2)").bind(id).bind(account_id).fetch_one(db).await?;
                if !ok {
                    return Err(AppError::NotFound(format!("profile {id} not found")));
                }
                id
            }
            None => {
                let input = ProfileInput {
                    email: l.email.clone().filter(|s| !s.trim().is_empty()),
                    phone_number: l.phone_number.clone().filter(|s| !s.trim().is_empty()),
                    external_id: l.external_id.clone().filter(|s| !s.trim().is_empty()),
                    ..Default::default()
                };
                match audience::find_profile(db, account_id, &input).await? {
                    Some(p) => p.id,
                    None => audience::upsert_profile(db, account_id, &input).await?.profile.id,
                }
            }
        };
        if !out.iter().any(|(p, r)| *p == id && *r == rel) {
            out.push((id, rel));
        }
    }
    Ok(out)
}

/// The title from the type as it is now (never a sensitive field; read again on every view, so a
/// title field that later became sensitive or changed never shows a stale value).
pub fn title_of(t: &ObjectType, fields: &Map<String, Value>) -> Option<String> {
    let key = t.title_field.as_deref()?;
    if t.field(key).is_none_or(|f| f.sensitive) {
        return None;
    }
    let v = fields.get(key)?;
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    Some(s.chars().take(200).collect())
}

/// The fields as the type stores them: unknown keys typed and added to the type, values checked
/// and put in one form; `null` stays (it removes the field).
fn normalize_all(t: &ObjectType, fields: &Map<String, Value>) -> AppResult<Map<String, Value>> {
    let mut out = Map::new();
    for (k, v) in fields {
        if v.is_null() {
            out.insert(k.clone(), Value::Null);
            continue;
        }
        let def = t.field(k).ok_or_else(|| AppError::bad(format!("{k:?} isn't a field of {}", t.key)))?;
        out.insert(k.clone(), types::normalize(def, v).map_err(AppError::bad)?);
    }
    Ok(out)
}

fn redacted(w: &Writer, fields: &Map<String, Value>) -> Map<String, Value> {
    if w.redact.is_empty() {
        return fields.clone();
    }
    let mut v = Value::Object(fields.clone());
    crate::features::integrations::redact::apply(&mut v, &w.redact);
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Create or update one object for `w`, linked to `links` (profile ids already in this account).
pub async fn write(state: &AppState, w: &Writer, type_key: &str, external_id: &str, fields: &Map<String, Value>, links: &[(Uuid, String)], as_of: Option<&str>) -> AppResult<Upserted> {
    let external_id = check_external_id(external_id)?;
    let fields = redacted(w, fields);
    check_fields_shape(&fields)?;
    if links.len() > MAX_LINKS {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people")));
    }
    let tz = super::flow::account_tz(&state.db, w.account_id).await?;
    let mut tx = state.db.begin().await?;
    let t = types::for_upsert(&mut tx, w.account_id, type_key, &fields).await?;
    let norm = normalize_all(&t, &fields)?;
    let fresh: Map<String, Value> = norm.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO objects (id, account_id, type_id, external_id, fields, source, as_of) VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (type_id, external_id) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(w.account_id)
    .bind(t.id)
    .bind(&external_id)
    .bind(Value::Object(fresh.clone()))
    .bind(&w.source)
    .bind(as_of)
    .fetch_optional(&mut *tx)
    .await?;
    let (id, result, changed, before, after, version) = match inserted {
        Some(id) => {
            let mut keys: Vec<String> = fresh.keys().cloned().collect();
            keys.sort();
            (id, "created", keys, Map::new(), fresh.clone(), 1i64)
        }
        None => {
            let (id, cur, version, stored_as_of): (Uuid, Value, i64, Option<String>) =
                sqlx::query_as("SELECT id, fields, version, as_of FROM objects WHERE type_id = $1 AND external_id = $2 FOR UPDATE").bind(t.id).bind(&external_id).fetch_one(&mut *tx).await?;
            if let (Some(new), Some(old)) = (as_of, stored_as_of.as_deref()) {
                if older(new, old) {
                    tx.rollback().await?;
                    return Ok(Upserted { id, type_key: t.key, external_id, result: "older", changed: Vec::new(), version, linked: 0 });
                }
            }
            let old = cur.as_object().cloned().unwrap_or_default();
            let mut merged = old.clone();
            for (k, v) in &norm {
                if v.is_null() {
                    merged.remove(k);
                } else {
                    merged.insert(k.clone(), v.clone());
                }
            }
            if serde_json::to_string(&merged).map(|s| s.len()).unwrap_or(0) > MAX_FIELDS_BYTES {
                return Err(AppError::bad("the object's fields would be over 32 KB"));
            }
            let mut changed: Vec<String> = norm.keys().filter(|k| old.get(*k) != merged.get(*k)).cloned().collect();
            changed.sort();
            let before: Map<String, Value> = changed.iter().map(|k| (k.clone(), old.get(k).cloned().unwrap_or(Value::Null))).collect();
            let after: Map<String, Value> = changed.iter().map(|k| (k.clone(), merged.get(k).cloned().unwrap_or(Value::Null))).collect();
            if changed.is_empty() {
                if as_of.is_some() {
                    sqlx::query("UPDATE objects SET as_of = $2 WHERE id = $1").bind(id).bind(as_of).execute(&mut *tx).await?;
                }
                (id, "unchanged", changed, before, after, version)
            } else {
                let version: i64 = sqlx::query_scalar(
                    "UPDATE objects SET fields = $2, source = $3, as_of = COALESCE($4, as_of), version = version + 1, updated_at = now() WHERE id = $1 RETURNING version",
                )
                .bind(id)
                .bind(Value::Object(merged.clone()))
                .bind(&w.source)
                .bind(as_of)
                .fetch_one(&mut *tx)
                .await?;
                (id, "updated", changed, before, after, version)
            }
        }
    };
    let linked = link(&mut tx, w.account_id, id, links).await?;
    if !changed.is_empty() {
        let change_id = Uuid::now_v7();
        sqlx::query("INSERT INTO object_changes (id, object_id, account_id, kind, changed, before, after, source, version) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
            .bind(change_id)
            .bind(id)
            .bind(w.account_id)
            .bind(if result == "created" { "created" } else { "updated" })
            .bind(&changed)
            .bind(Value::Object(before))
            .bind(Value::Object(after))
            .bind(&w.source)
            .bind(version)
            .execute(&mut *tx)
            .await?;
        let dates = t.date_fields();
        if result == "created" || changed.iter().any(|c| dates.contains(&c.as_str())) {
            super::flow::schedule_object(&mut tx, w.account_id, &t.key, id, &tz).await?;
        }
        super::flow::queue_change(&mut tx, w.account_id, &t.key, change_id).await?;
    }
    tx.commit().await?;
    Ok(Upserted { id, type_key: t.key, external_id, result, changed, version, linked })
}

async fn link(conn: &mut sqlx::PgConnection, account_id: Uuid, object_id: Uuid, links: &[(Uuid, String)]) -> AppResult<u32> {
    if links.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = links.iter().map(|(p, _)| *p).collect();
    let rels: Vec<String> = links.iter().map(|(_, r)| r.clone()).collect();
    let n = sqlx::query(
        "INSERT INTO object_links (object_id, profile_id, account_id, relationship)
         SELECT $1, x.pid, $2, x.rel FROM unnest($3::uuid[], $4::text[]) AS x(pid, rel) JOIN profiles p ON p.id = x.pid AND p.account_id = $2
         ON CONFLICT DO NOTHING",
    )
    .bind(object_id)
    .bind(account_id)
    .bind(&ids)
    .bind(&rels)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM object_links WHERE object_id = $1").bind(object_id).fetch_one(&mut *conn).await?;
    if total > MAX_LINKS as i64 {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people")));
    }
    Ok(n as u32)
}

/// Upsert from the API or MCP: links resolved (people created if needed), then [`write`].
pub async fn upsert(state: &AppState, w: &Writer, b: &UpsertInput) -> AppResult<Upserted> {
    check_input(b)?;
    let links = resolve_links(&state.db, w.account_id, &b.profiles).await?;
    write(state, w, &b.type_key, &b.external_id, &b.fields, &links, as_of_text(&b.as_of)?.as_deref()).await
}

/// What an upsert would do, writing nothing (integration test runs and dry runs).
pub async fn preview(state: &AppState, account_id: Uuid, type_key: &str, external_id: &str, fields: &Map<String, Value>, redact: &[crate::features::integrations::redact::Path]) -> AppResult<(&'static str, Vec<String>)> {
    let w = Writer { account_id, source: String::new(), redact: redact.to_vec() };
    let fields = redacted(&w, fields);
    check_fields_shape(&fields)?;
    let t = types::by_key(&state.db, account_id, type_key).await?;
    let mut norm = Map::new();
    for (k, v) in &fields {
        let def = match t.as_ref().and_then(|t| t.field(k)).cloned() {
            Some(d) => d,
            None if v.is_null() => continue,
            None => types::inferred(k, v).map_err(AppError::bad)?,
        };
        norm.insert(k.clone(), if v.is_null() { Value::Null } else { types::normalize(&def, v).map_err(AppError::bad)? });
    }
    let cur: Option<Value> = match &t {
        Some(t) => sqlx::query_scalar("SELECT fields FROM objects WHERE type_id = $1 AND external_id = $2").bind(t.id).bind(external_id.trim()).fetch_optional(&state.db).await?,
        None => None,
    };
    let Some(cur) = cur else {
        let mut keys: Vec<String> = norm.iter().filter(|(_, v)| !v.is_null()).map(|(k, _)| k.clone()).collect();
        keys.sort();
        return Ok(("created", keys));
    };
    let old = cur.as_object().cloned().unwrap_or_default();
    let mut changed: Vec<String> = norm.iter().filter(|(k, v)| if v.is_null() { old.contains_key(*k) } else { old.get(*k) != Some(v) }).map(|(k, _)| k.clone()).collect();
    changed.sort();
    Ok((if changed.is_empty() { "unchanged" } else { "updated" }, changed))
}

/// Delete objects by id. Returns how many went.
pub async fn delete(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> AppResult<u64> {
    Ok(sqlx::query("DELETE FROM objects WHERE account_id = $1 AND id = ANY($2)").bind(account_id).bind(ids).execute(db).await?.rows_affected())
}

/// Object ids for a type and external ids (unknown ones are left out).
pub async fn ids_for(db: &PgPool, account_id: Uuid, type_key: &str, external_ids: &[String]) -> AppResult<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT o.id, o.external_id FROM objects o JOIN object_types t ON t.id = o.type_id WHERE t.account_id = $1 AND t.key = $2 AND o.external_id = ANY($3)",
    )
    .bind(account_id)
    .bind(type_key.trim())
    .bind(external_ids)
    .fetch_all(db)
    .await?)
}

// ---------- Views ----------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ObjectRow {
    pub id: Uuid,
    pub type_id: Uuid,
    pub external_id: String,
    pub fields: Value,
    pub source: Option<String>,
    pub version: i64,
    pub as_of: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const OBJECT_COLS: &str = "o.id, o.type_id, o.external_id, o.fields, o.source, o.version, o.as_of, o.created_at, o.updated_at";

/// The fields this caller may see: sensitive fields only in a detail view, for people who can read
/// profiles. Values of keys the type no longer defines (a field removed from it, maybe one that was
/// sensitive) are held back the same way. Returns the visible fields and the names held back.
pub fn visible(t: &ObjectType, fields: &Value, detail_with_sensitive: bool) -> (Map<String, Value>, Vec<String>) {
    let all = fields.as_object().cloned().unwrap_or_default();
    if detail_with_sensitive {
        return (all, Vec::new());
    }
    let mut out = Map::new();
    let mut hidden = Vec::new();
    for (k, v) in all {
        match t.field(&k) {
            Some(f) if !f.sensitive => {
                out.insert(k, v);
            }
            _ => hidden.push(k),
        }
    }
    hidden.sort();
    (out, hidden)
}

fn summary(t: &ObjectType, r: &ObjectRow) -> Value {
    let (fields, _) = visible(t, &r.fields, false);
    json!({"id": r.id, "type": t.key, "external_id": r.external_id, "title": title_of(t, r.fields.as_object().unwrap_or(&Map::new())), "fields": fields, "version": r.version, "created_at": r.created_at, "updated_at": r.updated_at})
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// A page of a type's objects, newest change first, optionally searched by title or external id.
/// Lists never include sensitive fields.
pub async fn list(db: &PgPool, t: &ObjectType, q: Option<&str>, limit: i64, offset: i64) -> AppResult<Value> {
    let q = q.map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", like_escape(&s.chars().take(100).collect::<String>())));
    let sql = format!(
        "SELECT {OBJECT_COLS} FROM objects o WHERE o.type_id = $1 AND o.account_id = $2 AND ($3::text IS NULL OR o.fields->>$6::text ILIKE $3 OR o.external_id ILIKE $3)
         ORDER BY o.updated_at DESC, o.id DESC LIMIT $4 OFFSET $5"
    );
    // Search the current, non-sensitive title field (never a stored title that may be stale).
    let title_key = t.title_field.clone().filter(|k| t.field(k).is_some_and(|f| !f.sensitive)).unwrap_or_default();
    let rows: Vec<ObjectRow> = sqlx::query_as(&sql).bind(t.id).bind(t.account_id).bind(&q).bind(limit).bind(offset).bind(&title_key).fetch_all(db).await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM objects o WHERE o.type_id = $1 AND o.account_id = $2 AND ($3::text IS NULL OR o.fields->>$4::text ILIKE $3 OR o.external_id ILIKE $3)")
        .bind(t.id)
        .bind(t.account_id)
        .bind(&q)
        .bind(&title_key)
        .fetch_one(db)
        .await?;
    Ok(json!({"data": rows.iter().map(|r| summary(t, r)).collect::<Vec<_>>(), "total": total}))
}

pub async fn load(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<(ObjectType, ObjectRow)> {
    let q = format!("SELECT {OBJECT_COLS} FROM objects o WHERE o.id = $1 AND o.account_id = $2");
    let r: ObjectRow = sqlx::query_as(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("object"))?;
    let t = types::by_id(db, account_id, r.type_id).await?.ok_or_else(|| AppError::not_found("object"))?;
    Ok((t, r))
}

pub async fn by_external(db: &PgPool, account_id: Uuid, type_key: &str, external_id: &str) -> AppResult<Uuid> {
    ids_for(db, account_id, type_key, &[external_id.trim().to_string()]).await?.first().map(|(id, _)| *id).ok_or_else(|| AppError::not_found("object"))
}

/// One object with its history and, for people who can read profiles, who it is linked to and its
/// sensitive fields.
pub async fn detail(db: &PgPool, account_id: Uuid, id: Uuid, profiles_read: bool) -> AppResult<Value> {
    let (t, r) = load(db, account_id, id).await?;
    let (fields, hidden) = visible(&t, &r.fields, profiles_read);
    let rows: Vec<(Uuid, String, Vec<String>, Value, Value, Option<String>, i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, kind, changed, before, after, source, version, created_at FROM object_changes WHERE object_id = $1 AND account_id = $2 ORDER BY created_at DESC, id DESC LIMIT 100",
    )
    .bind(id)
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let mask = |v: &Value| -> Value {
        let mut m = v.as_object().cloned().unwrap_or_default();
        if !profiles_read {
            for (k, x) in m.iter_mut() {
                if t.field(k).is_none_or(|f| f.sensitive) {
                    *x = json!("(hidden)");
                }
            }
        }
        Value::Object(m)
    };
    let history: Vec<Value> = rows
        .iter()
        .map(|(cid, kind, changed, before, after, source, version, at)| json!({"id": cid, "kind": kind, "changed": changed, "before": mask(before), "after": mask(after), "source": source, "version": version, "at": at}))
        .collect();
    let mut out = json!({
        "id": r.id, "type": t.key, "type_name": t.name, "external_id": r.external_id, "title": title_of(&t, r.fields.as_object().unwrap_or(&Map::new())), "fields": fields, "hidden_fields": hidden,
        "source": r.source, "version": r.version, "as_of": r.as_of, "created_at": r.created_at, "updated_at": r.updated_at, "history": history,
    });
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM object_links WHERE object_id = $1").bind(id).fetch_one(db).await?;
    out["linked_count"] = json!(links);
    if profiles_read {
        let people: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT p.id, p.email, p.phone_number, p.first_name, p.last_name, l.relationship FROM object_links l JOIN profiles p ON p.id = l.profile_id
             WHERE l.object_id = $1 AND p.account_id = $2 ORDER BY l.created_at, p.id",
        )
        .bind(id)
        .bind(account_id)
        .fetch_all(db)
        .await?;
        out["profiles"] = json!(people
            .into_iter()
            .map(|(pid, email, phone, first, last, rel)| json!({"id": pid, "email": email, "phone_number": phone, "first_name": first, "last_name": last, "relationship": rel}))
            .collect::<Vec<_>>());
    }
    Ok(out)
}

/// A person's objects, by type, with their title, date and choice fields (never sensitive ones).
pub async fn for_profile(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Value> {
    let q = format!(
        "SELECT {OBJECT_COLS}, l.relationship FROM object_links l JOIN objects o ON o.id = l.object_id
         WHERE l.profile_id = $1 AND l.account_id = $2 AND o.account_id = $2 ORDER BY o.updated_at DESC LIMIT 200"
    );
    let rows = sqlx::query(&q).bind(profile_id).bind(account_id).fetch_all(db).await?;
    let all = types::list(db, account_id).await?;
    let mut groups: Vec<(ObjectType, Vec<Value>)> = Vec::new();
    for row in rows {
        use sqlx::{FromRow, Row};
        let r = ObjectRow::from_row(&row)?;
        let rel: String = row.try_get("relationship")?;
        let Some((t, _)) = all.iter().find(|(t, _)| t.id == r.type_id) else { continue };
        let keys: Vec<&str> = t.fields.iter().filter(|f| !f.sensitive && (f.kind.is_date() || f.kind == types::Kind::Enum || Some(f.key.as_str()) == t.title_field.as_deref())).map(|f| f.key.as_str()).take(6).collect();
        let (fields, _) = visible(t, &r.fields, false);
        let key_fields: Map<String, Value> = keys.iter().filter_map(|k| fields.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
        let v = json!({"id": r.id, "external_id": r.external_id, "title": title_of(t, r.fields.as_object().unwrap_or(&Map::new())), "relationship": rel, "fields": key_fields, "updated_at": r.updated_at});
        match groups.iter_mut().find(|(g, _)| g.id == t.id) {
            Some((_, list)) => list.push(v),
            None => groups.push((t.clone(), vec![v])),
        }
    }
    Ok(json!({"types": groups.into_iter().map(|(t, objects)| json!({"key": t.key, "name": t.name, "plural": t.plural, "fields": t.fields.iter().filter(|f| !f.sensitive).collect::<Vec<_>>(), "objects": objects})).collect::<Vec<_>>()}))
}

/// Trim change history: beyond the newest [`HISTORY_PER_OBJECT`] per object (once an hour old, so
/// a change's flow job always finds it) and older than [`HISTORY_DAYS`]. At most `batch` rows.
pub async fn trim_history(db: &PgPool, batch: i64) -> AppResult<u64> {
    let old = sqlx::query("DELETE FROM object_changes WHERE id IN (SELECT id FROM object_changes WHERE created_at < now() - make_interval(days => $1) LIMIT $2)")
        .bind(HISTORY_DAYS as i32)
        .bind(batch)
        .execute(db)
        .await?
        .rows_affected();
    let extra = sqlx::query(
        "DELETE FROM object_changes WHERE id IN (
           SELECT id FROM (SELECT c.id, c.created_at, row_number() OVER (PARTITION BY c.object_id ORDER BY c.created_at DESC, c.id DESC) AS n
                           FROM object_changes c WHERE c.object_id IN (SELECT object_id FROM object_changes GROUP BY object_id HAVING count(*) > $1 LIMIT 500)) x
           WHERE x.n > $1 AND x.created_at < now() - interval '1 hour' LIMIT $2)",
    )
    .bind(HISTORY_PER_OBJECT)
    .bind(batch)
    .execute(db)
    .await?
    .rows_affected();
    Ok(old + extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_of_orders_times_numbers_and_text() {
        assert!(older("2026-11-17T10:00:00Z", "2026-11-17T11:00:00.000Z"));
        assert!(!older("2026-11-17T12:00:00+01:00", "2026-11-17T10:59:00Z"));
        assert!(older("9", "10"), "numbers as numbers");
        assert!(!older("b", "a"));
        assert!(as_of_text(&Some(json!({"x": 1}))).is_err());
        assert_eq!(as_of_text(&Some(json!(3))).unwrap().as_deref(), Some("3"));
    }

    #[test]
    fn inputs_are_bounded() {
        let mut b = UpsertInput { type_key: "reservation".into(), external_id: "R1".into(), fields: Map::new(), profiles: Vec::new(), as_of: None };
        assert!(check_input(&b).is_ok());
        b.profiles = (0..=MAX_LINKS).map(|i| LinkInput { email: Some(format!("p{i}@x.co")), ..Default::default() }).collect();
        assert!(check_input(&b).is_err());
        b.profiles = vec![LinkInput::default()];
        assert!(check_input(&b).is_err(), "a link needs something to find the person by");
        b.profiles.clear();
        b.fields.insert("check-in".into(), json!("2026-11-20"));
        assert!(check_input(&b).is_err());
        b.fields.clear();
        b.fields.insert("notes".into(), json!("x".repeat(MAX_FIELDS_BYTES)));
        assert!(check_input(&b).is_err());
        b.fields.clear();
        b.external_id = " ".into();
        assert!(check_input(&b).is_err());
    }
}

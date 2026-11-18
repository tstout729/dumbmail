//! Objects: upserts (fields merge, every change recorded), links to people, deletes, and the views
//! the API and dashboard show.
//!
//! One upsert is one transaction: the type (created or given new fields on first use), the version
//! check, the object under a row lock, its people (found or created only once the write is
//! accepted), the change records, the object-date fires that depend on it, the job that starts
//! object-triggered flows and the caller's receipt (a bulk upsert's progress or an integration
//! delivery's ledger entry). A job or request that dies part-way leaves nothing, and a retry
//! carries on after the last write that committed.

use std::cmp::Ordering;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::types::{self, ObjectType, MAX_FIELDS, MAX_FIELDS_BYTES};
use crate::audience::ProfileInput;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone, Profile, PROFILE_COLS};
use crate::state::AppState;

/// Most people one object may be linked to.
pub const MAX_LINKS: usize = 20;
/// Longest external id and relationship label.
pub const MAX_EXTERNAL_ID: usize = 200;
pub const MAX_RELATIONSHIP: usize = 50;
/// Changes kept per object, and for how long.
pub const HISTORY_PER_OBJECT: i64 = 100;
pub const HISTORY_DAYS: i64 = 365;

/// How a writer creates people it links and nobody has yet.
#[derive(Debug, Clone)]
pub enum Creator {
    /// The API and MCP: only the identifiers given, never subscribed (as `POST /api/profiles`).
    Plain,
    /// An integration (`tag` is `integration:<id>`): through its single insert, held back from
    /// marketing unless its owner said its contacts opted in.
    Integration { tag: String, opted_in: bool },
}

/// Who is writing: `api`, `mcp` or `integration:<id>`, with an integration's redact list (paths
/// removed from the fields before they are written, e.g. `**.doorCode`).
#[derive(Debug, Clone)]
pub struct Writer {
    pub account_id: Uuid,
    pub source: String,
    pub redact: Vec<crate::features::integrations::redact::Path>,
    pub creator: Creator,
}

impl Writer {
    pub fn new(account_id: Uuid, source: &str) -> Self {
        Writer { account_id, source: source.to_string(), redact: Vec::new(), creator: Creator::Plain }
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
    /// Relationships whose links `profiles` replace (e.g. `["guest"]`): people linked that way and
    /// not named here are unlinked in the same write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace_links: Vec<String>,
    /// The change's version in your system: a whole number, or a time (RFC 3339). One kind per
    /// type. A write older than or equal to the newest one stored is ignored.
    #[serde(default)]
    pub as_of: Option<Value>,
    /// For evidence about an object as of a moment (e.g. a payment checked at 10:00): written only
    /// if the object exists and its stored version isn't newer than this. Never creates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_if_as_of_at_most: Option<Value>,
}

/// A link request as stored with the version that made it (hashed: no identifiers kept here).
fn link_target(l: &LinkReq) -> String {
    use sha2::Digest;
    let who = match &l.who {
        LinkWho::Id(id) => json!(["id", id]),
        LinkWho::Ids { email, phone_number, external_id } => json!(["ids", email, phone_number, external_id]),
        LinkWho::Property { name, value } => json!(["property", name, value]),
    };
    hex::encode(sha2::Sha256::digest(json!([who, l.relationship]).to_string().as_bytes()))
}

/// Who a link is to.
#[derive(Debug, Clone)]
pub enum LinkWho {
    Id(Uuid),
    /// Normalized identifiers (at least one).
    Ids { email: Option<String>, phone_number: Option<String>, external_id: Option<String> },
    /// A value an integration recorded in `integration_keys` (never creates anyone).
    Property { name: String, value: String },
}

#[derive(Debug, Clone)]
pub struct LinkReq {
    pub who: LinkWho,
    pub relationship: String,
}

/// What a write records with itself, in its own transaction.
#[derive(Debug, Clone, Copy)]
pub enum Receipt<'a> {
    None,
    /// An integration delivery's ledger: action `index` with fingerprint `print` was applied.
    Ledger { delivery: Uuid, index: usize, print: &'a str },
    /// A bulk upsert's progress: item `index` was written (only if the bulk is still at it).
    Bulk { id: Uuid, index: i32 },
}

/// One write.
#[derive(Debug, Clone)]
pub struct Write<'a> {
    pub type_key: &'a str,
    pub external_id: &'a str,
    pub fields: &'a Map<String, Value>,
    /// Fields from a sealed (private) copy: written only if the type, as this write reads it under
    /// its lock, still marks them sensitive (an un-mark erased their old values).
    pub sealed: &'a [String],
    pub links: Vec<LinkReq>,
    pub replace: Vec<String>,
    pub as_of: Option<Version>,
    /// Evidence about an existing object as of a moment: written only if the object exists and
    /// nothing newer than this has been written to it (it never creates one or moves its version).
    pub at_most: Option<Version>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Upserted {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub type_key: String,
    pub external_id: String,
    /// created | updated | unchanged | older (ignored: older than what's stored) | duplicate
    /// (ignored: the same version as what's stored) | already_applied (a retry; nothing written)
    pub result: &'static str,
    /// The fields that changed, never naming one the type marks sensitive (those are counted in
    /// `hidden_changed`).
    pub changed: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub hidden_changed: u32,
    pub version: i64,
    pub linked: u32,
    pub unlinked: u32,
    /// People named who couldn't be found yet (an integration's property lookup): not linked.
    pub not_found: u32,
    #[serde(skip)]
    pub created_profiles: Vec<Profile>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// A write's version: a whole number (kept exactly, as its digits) or a time (kept in UTC with
/// all nine fractional digits, so stored versions of one kind compare correctly and two writes a
/// fraction of a microsecond apart stay in order).
#[derive(Debug, Clone, PartialEq)]
pub enum Version {
    Integer(String),
    Time(String),
}

impl Version {
    pub fn parse(v: &Value) -> AppResult<Option<Version>> {
        let bad = || AppError::bad("as_of is a whole number (a version) or an RFC 3339 time like 2026-11-17T10:00:00Z");
        match v {
            Value::Null => Ok(None),
            Value::Number(n) => n.as_u64().map(|n| Some(Version::Integer(n.to_string()))).ok_or_else(bad),
            Value::String(s) => {
                let s = s.trim();
                if !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_digit()) {
                    let digits = s.trim_start_matches('0');
                    return Ok(Some(Version::Integer(if digits.is_empty() { "0".into() } else { digits.into() })));
                }
                DateTime::parse_from_rfc3339(s).map(|d| Some(Version::Time(d.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Nanos, true)))).map_err(|_| bad())
            }
            _ => Err(bad()),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Version::Integer(_) => "integer",
            Version::Time(_) => "timestamp",
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Version::Integer(s) | Version::Time(s) => s,
        }
    }

    /// Compared with a stored version of the same kind.
    pub fn cmp_stored(&self, stored: &str) -> Ordering {
        match self {
            Version::Integer(s) => s.len().cmp(&stored.len()).then_with(|| s.as_str().cmp(stored)),
            Version::Time(s) => s.as_str().cmp(stored),
        }
    }
}

/// Check what can be checked without the database (sizes, keys, links), before any write.
pub fn check_input(b: &UpsertInput) -> AppResult<()> {
    if !types::valid_type_key(b.type_key.trim()) {
        return Err(AppError::bad(format!("object type {:?}: a type key is 1-40 lower-case letters, digits and _ (e.g. reservation)", b.type_key)));
    }
    check_external_id(&b.external_id)?;
    check_fields_shape(&b.fields)?;
    if b.profiles.len() > MAX_LINKS || b.replace_links.len() > 10 {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people (and replace at most 10 relationships)")));
    }
    links_for(&b.profiles)?;
    for r in &b.replace_links {
        relationship(Some(r))?;
    }
    Version::parse(b.as_of.as_ref().unwrap_or(&Value::Null))?;
    Version::parse(b.only_if_as_of_at_most.as_ref().unwrap_or(&Value::Null))?;
    if b.as_of.as_ref().is_some_and(|v| !v.is_null()) && b.only_if_as_of_at_most.as_ref().is_some_and(|v| !v.is_null()) {
        return Err(AppError::bad("give as_of (a new version) or only_if_as_of_at_most (evidence about the current one), not both"));
    }
    Ok(())
}

/// The API's links as link requests (identifiers checked and normalized).
pub fn links_for(links: &[LinkInput]) -> AppResult<Vec<LinkReq>> {
    let mut out = Vec::with_capacity(links.len());
    for l in links {
        let rel = relationship(l.relationship.as_deref())?;
        let who = match l.id {
            Some(id) => LinkWho::Id(id),
            None => {
                let some = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
                let email = match some(&l.email) {
                    Some(e) => Some(normalize_email(&e).ok_or_else(|| AppError::bad(format!("{e:?} is not a valid email")))?),
                    None => None,
                };
                let phone = match some(&l.phone_number) {
                    Some(p) => Some(normalize_phone(&p).ok_or_else(|| AppError::bad(format!("{p:?} is not a valid phone number (use +15035550100)")))?),
                    None => None,
                };
                let external_id = some(&l.external_id);
                if email.is_none() && phone.is_none() && external_id.is_none() {
                    return Err(AppError::bad("each linked person needs an id, email, phone_number or external_id"));
                }
                LinkWho::Ids { email, phone_number: phone, external_id }
            }
        };
        out.push(LinkReq { who, relationship: rel });
    }
    Ok(out)
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

/// Find (or create) one linked person inside the write's transaction. An existing profile is never
/// changed. Returns the profile id (None: an integration's lookup found nobody yet) and the profile
/// when this created it.
async fn resolve_in(conn: &mut PgConnection, w: &Writer, who: &LinkWho) -> AppResult<(Option<Uuid>, Option<Profile>)> {
    let account_id = w.account_id;
    match who {
        LinkWho::Id(id) => {
            let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM profiles WHERE id = $1 AND account_id = $2)").bind(id).bind(account_id).fetch_one(&mut *conn).await?;
            if !ok {
                return Err(AppError::NotFound(format!("profile {id} not found")));
            }
            Ok((Some(*id), None))
        }
        LinkWho::Property { name, value } => {
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM profiles WHERE account_id = $1 AND integration_keys <> '{}' AND (integration_keys @> $2 OR integration_keys @> $3) LIMIT 2",
            )
            .bind(account_id)
            .bind(json!({name: value}))
            .bind(json!({name: [value]}))
            .fetch_all(&mut *conn)
            .await?;
            Ok((if ids.len() == 1 { Some(ids[0]) } else { None }, None))
        }
        LinkWho::Ids { email, phone_number, external_id } => {
            let find = "SELECT id FROM profiles WHERE account_id = $1 AND (external_id = $2 OR email = $3 OR phone_number = $4)
                        ORDER BY (external_id = $2) DESC NULLS LAST, (email = $3) DESC NULLS LAST LIMIT 1";
            for _ in 0..2 {
                let found: Option<Uuid> = sqlx::query_scalar(find).bind(account_id).bind(external_id).bind(email).bind(phone_number).fetch_optional(&mut *conn).await?;
                if let Some(id) = found {
                    return Ok((Some(id), None));
                }
                let created: Option<Profile> = match &w.creator {
                    Creator::Plain => {
                        let q = format!(
                            "INSERT INTO profiles AS p (id, account_id, email, phone_number, external_id, email_consent, sms_consent)
                             VALUES ($1, $2, $3, $4, $5, 'never_subscribed', 'never_subscribed') ON CONFLICT DO NOTHING RETURNING {PROFILE_COLS}"
                        );
                        sqlx::query_as(&q).bind(Uuid::now_v7()).bind(account_id).bind(email).bind(phone_number).bind(external_id).fetch_optional(&mut *conn).await?
                    }
                    Creator::Integration { tag, opted_in } => {
                        let input = ProfileInput { email: email.clone(), phone_number: phone_number.clone(), external_id: external_id.clone(), ..Default::default() };
                        crate::features::integrations::actions::insert_profile_in(&mut *conn, account_id, tag, *opted_in, &input, &Map::new(), &Map::new(), false).await?
                    }
                };
                if let Some(p) = created {
                    return Ok((Some(p.id), Some(p)));
                }
            }
            Err(AppError::Conflict("that person's profile changed while this ran; try again".into()))
        }
    }
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

/// The fields as the type stores them: values checked and put in one form; `null` stays (it
/// removes the field).
fn normalize_all(t: &ObjectType, fields: &Map<String, Value>) -> AppResult<Map<String, Value>> {
    let mut out = Map::new();
    for (k, v) in fields {
        if v.is_null() {
            out.insert(k.clone(), Value::Null);
            continue;
        }
        let def = t.field(k).ok_or_else(|| AppError::bad(format!("{k:?} isn't a field of {}", t.key)))?;
        out.insert(k.clone(), types::normalize(def, v).map_err(|e| AppError::bad(types::public_error(def, e)))?);
    }
    Ok(out)
}

pub fn redacted(redact: &[crate::features::integrations::redact::Path], fields: &Map<String, Value>) -> Map<String, Value> {
    if redact.is_empty() {
        return fields.clone();
    }
    let mut v = Value::Object(fields.clone());
    crate::features::integrations::redact::apply(&mut v, redact);
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Record the caller's receipt in the write's transaction. False: a bulk upsert already moved past
/// this item (a duplicate job), so the caller must not commit.
async fn record_receipt(conn: &mut PgConnection, r: Receipt<'_>, result: &str) -> AppResult<bool> {
    match r {
        Receipt::None => Ok(true),
        // Claimed only if no other attempt recorded it first; otherwise the write rolls back.
        Receipt::Ledger { delivery, index, print } => crate::features::integrations::actions::mark_applied(&mut *conn, delivery, index, print).await,
        Receipt::Bulk { id, index } => {
            let col = match result {
                "created" => "created",
                "updated" => "updated",
                "failed" => "failed",
                _ => "unchanged",
            };
            let n = sqlx::query(&format!("UPDATE object_bulk_upserts SET done = $2 + 1, {col} = {col} + 1 WHERE id = $1 AND done = $2"))
                .bind(id)
                .bind(index)
                .execute(&mut *conn)
                .await?
                .rows_affected();
            Ok(n > 0)
        }
    }
}

/// Create or update one object for `w`, with its receipt.
pub async fn write(state: &AppState, w: &Writer, req: &Write<'_>, receipt: Receipt<'_>) -> AppResult<Upserted> {
    let external_id = check_external_id(req.external_id)?;
    let mut fields = redacted(&w.redact, req.fields);
    check_fields_shape(&fields)?;
    if req.links.len() > MAX_LINKS {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people")));
    }
    let tz = super::flow::account_tz(&state.db, w.account_id).await?;
    let mut tx = state.db.begin().await?;
    // A sealed field the type no longer marks sensitive isn't written: checked before the type is
    // read for the write (so it adds no field), and again on the type as locked for it.
    if !req.sealed.is_empty() {
        if let Some(t) = types::by_key(&mut *tx, w.account_id, req.type_key).await? {
            fields.retain(|k, _| !req.sealed.contains(k) || t.is_sensitive(k));
        }
    }
    let t = types::for_upsert(&mut tx, w.account_id, req.type_key, &fields).await?;
    fields.retain(|k, _| !req.sealed.contains(k) || t.is_sensitive(k));
    super::hooks::pause(w.account_id, "type_read").await;
    let norm = normalize_all(&t, &fields)?;
    // One kind of version per type, set by the first write that gives one.
    if let Some(v) = &req.as_of {
        let kind: Option<String> = sqlx::query_scalar("UPDATE object_types SET version_kind = COALESCE(version_kind, $2) WHERE id = $1 RETURNING version_kind")
            .bind(t.id)
            .bind(v.kind())
            .fetch_one(&mut *tx)
            .await?;
        if kind.as_deref() != Some(v.kind()) {
            return Err(AppError::bad(format!(
                "{}'s versions (as_of) are {}s; this one is a {}",
                t.key,
                if kind.as_deref() == Some("integer") { "whole number" } else { "time" },
                if v.kind() == "integer" { "whole number" } else { "time" }
            )));
        }
    }
    let version_text = req.as_of.as_ref().map(|v| v.text().to_string());
    // Evidence as of a moment: only onto an object that exists and hasn't changed since.
    if let Some(limit) = &req.at_most {
        let found: Option<(Uuid, i64, Option<String>)> =
            sqlx::query_as("SELECT id, version, as_of FROM objects WHERE type_id = $1 AND external_id = $2 FOR UPDATE").bind(t.id).bind(&external_id).fetch_optional(&mut *tx).await?;
        let stale = match &found {
            None => Some("not_found"),
            Some((_, _, Some(stored))) if t.version_kind.as_deref().is_some_and(|k| k != limit.kind()) => {
                return Err(AppError::bad(format!("{}'s versions are {}s; only_if_as_of_at_most must be one too", t.key, t.version_kind.as_deref().unwrap_or("time"))));
            }
            Some((_, _, Some(stored))) if limit.cmp_stored(stored) == Ordering::Less => Some("stale"),
            _ => None,
        };
        if let Some(why) = stale {
            let (id, version) = found.map(|(id, v, _)| (id, v)).unwrap_or((Uuid::nil(), 0));
            let out = Upserted { id, type_key: t.key.clone(), external_id: external_id.clone(), result: why, changed: Vec::new(), version, linked: 0, unlinked: 0, not_found: 0, created_profiles: Vec::new(), hidden_changed: 0 };
            if !record_receipt(&mut tx, receipt, why).await? {
                return Ok(Upserted { result: "already_applied", ..out });
            }
            tx.commit().await?;
            return Ok(out);
        }
    }
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
    .bind(&version_text)
    .fetch_optional(&mut *tx)
    .await?;
    let mut out = Upserted { id: Uuid::nil(), type_key: t.key.clone(), external_id: external_id.clone(), result: "created", changed: Vec::new(), version: 1, linked: 0, unlinked: 0, not_found: 0, created_profiles: Vec::new(), hidden_changed: 0 };
    let (before, after) = match inserted {
        Some(id) => {
            out.id = id;
            out.changed = fresh.keys().cloned().collect();
            out.changed.sort();
            (Map::new(), fresh.clone())
        }
        None => {
            let (id, cur, version, stored, targets): (Uuid, Value, i64, Option<String>, Option<Vec<String>>) =
                sqlx::query_as("SELECT id, fields, version, as_of, link_targets FROM objects WHERE type_id = $1 AND external_id = $2 FOR UPDATE")
                    .bind(t.id)
                    .bind(&external_id)
                    .fetch_one(&mut *tx)
                    .await?;
            out.id = id;
            out.version = version;
            // Older than or the same as what's stored: ignored (a late or repeated message).
            if let (Some(v), Some(old)) = (&req.as_of, stored.as_deref()) {
                let ord = v.cmp_stored(old);
                if ord != Ordering::Greater {
                    out.result = if ord == Ordering::Equal { "duplicate" } else { "older" };
                    // The same version again (a resend, or the pull catching up with a webhook): its
                    // fields stay as they are, but people the accepted write named who are now
                    // known are linked (never unlinked: that belonged to the write that set this
                    // version). A resend naming anyone else at this version links nobody.
                    let targets = targets.unwrap_or_default();
                    let named: Vec<&LinkReq> = req.links.iter().filter(|l| targets.contains(&link_target(l))).collect();
                    if ord == Ordering::Equal && !named.is_empty() {
                        let mut wanted: Vec<(Uuid, String)> = Vec::new();
                        for l in named {
                            let (pid, created) = resolve_in(&mut tx, w, &l.who).await?;
                            out.created_profiles.extend(created);
                            if let Some(pid) = pid {
                                wanted.push((pid, l.relationship.clone()));
                            }
                        }
                        let (linked, _) = relink(&mut tx, w.account_id, out.id, &wanted, &[]).await?;
                        out.linked = linked.len() as u32;
                        if !linked.is_empty() {
                            let who = Value::Array(linked.iter().map(|(p, r)| json!({"profile_id": p, "relationship": r})).collect());
                            insert_change(&mut tx, Uuid::now_v7(), out.id, w, "links", &[], Map::new(), json!({"linked": who}).as_object().cloned().unwrap_or_default(), out.version).await?;
                        }
                    }
                    if !record_receipt(&mut tx, receipt, out.result).await? {
                        out.result = "already_applied";
                        return Ok(out);
                    }
                    tx.commit().await?;
                    return Ok(out);
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
            out.changed = norm.keys().filter(|k| old.get(*k) != merged.get(*k)).cloned().collect();
            out.changed.sort();
            let before: Map<String, Value> = out.changed.iter().map(|k| (k.clone(), old.get(k).cloned().unwrap_or(Value::Null))).collect();
            let after: Map<String, Value> = out.changed.iter().map(|k| (k.clone(), merged.get(k).cloned().unwrap_or(Value::Null))).collect();
            if out.changed.is_empty() {
                out.result = "unchanged";
                // A newer version with nothing new still moves the stored version forward (never back).
                if version_text.is_some() {
                    sqlx::query("UPDATE objects SET as_of = $2 WHERE id = $1").bind(id).bind(&version_text).execute(&mut *tx).await?;
                }
            } else {
                out.result = "updated";
                out.version = sqlx::query_scalar(
                    "UPDATE objects SET fields = $2, source = $3, as_of = COALESCE($4, as_of), version = version + 1, updated_at = now() WHERE id = $1 RETURNING version",
                )
                .bind(id)
                .bind(Value::Object(merged.clone()))
                .bind(&w.source)
                .bind(&version_text)
                .fetch_one(&mut *tx)
                .await?;
            }
            (before, after)
        }
    };
    // People: found or created only now that the write is accepted, in its transaction.
    let mut wanted: Vec<(Uuid, String)> = Vec::new();
    for l in &req.links {
        let (id, created) = resolve_in(&mut tx, w, &l.who).await?;
        out.created_profiles.extend(created);
        match id {
            Some(id) if !wanted.iter().any(|(p, r)| *p == id && *r == l.relationship) => wanted.push((id, l.relationship.clone())),
            Some(_) => {}
            None => out.not_found += 1,
        }
    }
    let (linked, unlinked) = relink(&mut tx, w.account_id, out.id, &wanted, &req.replace).await?;
    out.linked = linked.len() as u32;
    out.unlinked = unlinked.len() as u32;
    // Who this version named: a resend of it may link them later, and no one else.
    if version_text.is_some() {
        let targets: Vec<String> = req.links.iter().map(link_target).collect();
        sqlx::query("UPDATE objects SET link_targets = $2 WHERE id = $1").bind(out.id).bind(&targets).execute(&mut *tx).await?;
    }
    if out.result == "created" || !out.changed.is_empty() {
        let change_id = Uuid::now_v7();
        insert_change(&mut tx, change_id, out.id, w, if out.result == "created" { "created" } else { "updated" }, &out.changed, before, after, out.version).await?;
        // Any change: a date may have moved, and a fire its filters stopped may now match (paid).
        super::flow::schedule_object(&mut tx, w.account_id, &t.key, out.id, &tz).await?;
        super::flow::queue_change(&mut tx, w.account_id, &t.key, change_id).await?;
    }
    if !linked.is_empty() || !unlinked.is_empty() {
        let who = |l: &[(Uuid, String)]| Value::Array(l.iter().map(|(p, r)| json!({"profile_id": p, "relationship": r})).collect());
        insert_change(&mut tx, Uuid::now_v7(), out.id, w, "links", &[], json!({"unlinked": who(&unlinked)}).as_object().cloned().unwrap_or_default(), json!({"linked": who(&linked)}).as_object().cloned().unwrap_or_default(), out.version).await?;
    }
    if !record_receipt(&mut tx, receipt, out.result).await? {
        tx.rollback().await?;
        out.result = "already_applied";
        out.created_profiles.clear();
        return Ok(out);
    }
    tx.commit().await?;
    // What's reported back never names a sensitive field.
    let before_n = out.changed.len();
    out.changed.retain(|k| !t.is_sensitive(k));
    out.hidden_changed = (before_n - out.changed.len()) as u32;
    let source = if matches!(w.creator, Creator::Integration { .. }) { crate::audience::EventSource::Integration } else { crate::audience::EventSource::Api };
    for p in &out.created_profiles {
        crate::features::webhooks::profile_saved(&state.db, w.account_id, None, None, p, source).await;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn insert_change(conn: &mut PgConnection, id: Uuid, object_id: Uuid, w: &Writer, kind: &str, changed: &[String], before: Map<String, Value>, after: Map<String, Value>, version: i64) -> AppResult<()> {
    sqlx::query("INSERT INTO object_changes (id, object_id, account_id, kind, changed, before, after, source, version) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
        .bind(id)
        .bind(object_id)
        .bind(w.account_id)
        .bind(kind)
        .bind(changed)
        .bind(Value::Object(before))
        .bind(Value::Object(after))
        .bind(&w.source)
        .bind(version)
        .execute(conn)
        .await?;
    Ok(())
}

/// Link the people wanted, and unlink those linked by a relationship in `replace` who aren't among
/// them. Returns (linked, unlinked).
async fn relink(conn: &mut PgConnection, account_id: Uuid, object_id: Uuid, wanted: &[(Uuid, String)], replace: &[String]) -> AppResult<(Vec<(Uuid, String)>, Vec<(Uuid, String)>)> {
    let ids: Vec<Uuid> = wanted.iter().map(|(p, _)| *p).collect();
    let rels: Vec<String> = wanted.iter().map(|(_, r)| r.clone()).collect();
    let unlinked: Vec<(Uuid, String)> = if replace.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "DELETE FROM object_links l WHERE l.object_id = $1 AND l.relationship = ANY($2)
               AND NOT EXISTS (SELECT 1 FROM unnest($3::uuid[], $4::text[]) AS x(pid, rel) WHERE x.pid = l.profile_id AND x.rel = l.relationship)
             RETURNING l.profile_id, l.relationship",
        )
        .bind(object_id)
        .bind(replace)
        .bind(&ids)
        .bind(&rels)
        .fetch_all(&mut *conn)
        .await?
    };
    let linked: Vec<(Uuid, String)> = if wanted.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "INSERT INTO object_links (object_id, profile_id, account_id, relationship)
             SELECT $1, x.pid, $2, x.rel FROM unnest($3::uuid[], $4::text[]) AS x(pid, rel) JOIN profiles p ON p.id = x.pid AND p.account_id = $2
             ON CONFLICT DO NOTHING RETURNING profile_id, relationship",
        )
        .bind(object_id)
        .bind(account_id)
        .bind(&ids)
        .bind(&rels)
        .fetch_all(&mut *conn)
        .await?
    };
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM object_links WHERE object_id = $1").bind(object_id).fetch_one(&mut *conn).await?;
    if total > MAX_LINKS as i64 {
        return Err(AppError::bad(format!("an object can be linked to at most {MAX_LINKS} people")));
    }
    Ok((linked, unlinked))
}

/// Upsert from the API or MCP.
pub async fn upsert(state: &AppState, w: &Writer, b: &UpsertInput) -> AppResult<Upserted> {
    upsert_with(state, w, b, Receipt::None).await
}

pub async fn upsert_with(state: &AppState, w: &Writer, b: &UpsertInput, receipt: Receipt<'_>) -> AppResult<Upserted> {
    check_input(b)?;
    let replace = b.replace_links.iter().map(|r| relationship(Some(r))).collect::<AppResult<Vec<_>>>()?;
    let req = Write {
        type_key: &b.type_key,
        external_id: &b.external_id,
        fields: &b.fields,
        sealed: &[],
        links: links_for(&b.profiles)?,
        replace,
        as_of: Version::parse(b.as_of.as_ref().unwrap_or(&Value::Null))?,
        at_most: Version::parse(b.only_if_as_of_at_most.as_ref().unwrap_or(&Value::Null))?,
    };
    write(state, w, &req, receipt).await
}

/// What an upsert would do, writing nothing (integration test runs and dry runs). Fields the type
/// marks sensitive are never compared (that would tell whether a private value changed): they're
/// listed as `name (private)` when given.
pub async fn preview(state: &AppState, account_id: Uuid, type_key: &str, external_id: &str, fields: &Map<String, Value>, redact: &[crate::features::integrations::redact::Path]) -> AppResult<(&'static str, Vec<String>)> {
    let fields = redacted(redact, fields);
    check_fields_shape(&fields)?;
    let t = types::by_key(&state.db, account_id, type_key).await?;
    let private = |k: &str| t.as_ref().is_some_and(|t| t.is_sensitive(k));
    let mut norm = Map::new();
    let mut hidden = Vec::new();
    for (k, v) in &fields {
        if private(k) {
            hidden.push(k.clone());
            continue;
        }
        let def = match t.as_ref().and_then(|t| t.field(k)).cloned() {
            Some(d) => d,
            None if v.is_null() => continue,
            None => types::inferred(k, v).map_err(AppError::bad)?,
        };
        norm.insert(k.clone(), if v.is_null() { Value::Null } else { types::normalize(&def, v).map_err(|e| AppError::bad(types::public_error(&def, e)))? });
    }
    let hidden: Vec<String> = match hidden.len() {
        0 => Vec::new(),
        1 => vec!["1 hidden field".into()],
        n => vec![format!("{n} hidden fields")],
    };
    let cur: Option<Value> = match &t {
        Some(t) => sqlx::query_scalar("SELECT fields FROM objects WHERE type_id = $1 AND external_id = $2").bind(t.id).bind(external_id.trim()).fetch_optional(&state.db).await?,
        None => None,
    };
    let Some(cur) = cur else {
        let mut keys: Vec<String> = norm.iter().filter(|(_, v)| !v.is_null()).map(|(k, _)| k.clone()).collect();
        keys.sort();
        keys.extend(hidden);
        return Ok(("created", keys));
    };
    let old = cur.as_object().cloned().unwrap_or_default();
    let mut changed: Vec<String> = norm.iter().filter(|(k, v)| if v.is_null() { old.contains_key(*k) } else { old.get(*k) != Some(v) }).map(|(k, _)| k.clone()).collect();
    changed.sort();
    let what = if changed.is_empty() { "unchanged" } else { "updated" };
    changed.extend(hidden);
    Ok((what, changed))
}

/// Delete objects by id. Returns how many went.
pub async fn delete(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> AppResult<u64> {
    delete_with(db, account_id, ids, Receipt::None).await
}

/// Delete objects and record the caller's receipt in the same transaction.
pub async fn delete_with(db: &PgPool, account_id: Uuid, ids: &[Uuid], receipt: Receipt<'_>) -> AppResult<u64> {
    let mut tx = db.begin().await?;
    let n = if ids.is_empty() { 0 } else { sqlx::query("DELETE FROM objects WHERE account_id = $1 AND id = ANY($2)").bind(account_id).bind(ids).execute(&mut *tx).await?.rows_affected() };
    if !record_receipt(&mut tx, receipt, "deleted").await? {
        return Ok(0);
    }
    tx.commit().await?;
    Ok(n)
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
/// Objects one trim run looks at for changes beyond the newest [`HISTORY_PER_OBJECT`].
pub const TRIM_OBJECTS: i64 = 500;

/// One bounded trim run: up to `batch` changes older than [`HISTORY_DAYS`] (by the created_at
/// index), and, for the next [`TRIM_OBJECTS`] objects after `after` (by id), their changes beyond
/// the newest [`HISTORY_PER_OBJECT`] that are over an hour old (by the (object, created_at) index).
/// Returns how many went and the cursor for the next run (None: every object was seen).
pub async fn trim_history(db: &PgPool, batch: i64, after: Option<Uuid>) -> AppResult<(u64, Option<Uuid>)> {
    let old = sqlx::query("DELETE FROM object_changes WHERE id IN (SELECT id FROM object_changes WHERE created_at < now() - make_interval(days => $1) ORDER BY created_at LIMIT $2)")
        .bind(HISTORY_DAYS as i32)
        .bind(batch)
        .execute(db)
        .await?
        .rows_affected();
    let objects: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM objects WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2").bind(after).bind(TRIM_OBJECTS).fetch_all(db).await?;
    let extra = if objects.is_empty() {
        0
    } else {
        sqlx::query(
            "DELETE FROM object_changes WHERE id IN (
               SELECT c.id FROM unnest($1::uuid[]) AS o(id)
               CROSS JOIN LATERAL (SELECT c.id, c.created_at FROM object_changes c WHERE c.object_id = o.id ORDER BY c.created_at DESC, c.id DESC OFFSET $2) c
               WHERE c.created_at < now() - interval '1 hour' LIMIT $3)",
        )
        .bind(&objects)
        .bind(HISTORY_PER_OBJECT)
        .bind(batch)
        .execute(db)
        .await?
        .rows_affected()
    };
    let next = if (objects.len() as i64) < TRIM_OBJECTS { None } else { objects.last().copied() };
    Ok((old + extra, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_whole_numbers_or_times_compared_exactly() {
        let v = |x: Value| Version::parse(&x).unwrap().unwrap();
        assert_eq!(v(json!(7)), Version::Integer("7".into()));
        assert_eq!(v(json!("0042")), Version::Integer("42".into()));
        // Bigger than a float can hold exactly: still ordered by its digits.
        let big = v(json!("123456789012345678901234567890"));
        assert_eq!(big.cmp_stored("123456789012345678901234567889"), Ordering::Greater);
        assert_eq!(v(json!("9")).cmp_stored("10"), Ordering::Less);
        assert_eq!(v(json!("2026-11-17T12:00:00+01:00")), Version::Time("2026-11-17T11:00:00.000000Z".into()));
        assert_eq!(v(json!("2026-11-17T11:00:00Z")).cmp_stored("2026-11-17T11:00:00.000000Z"), Ordering::Equal);
        assert!(Version::parse(&json!(-1)).is_err());
        assert!(Version::parse(&json!(1.5)).is_err());
        assert!(Version::parse(&json!("yesterday")).is_err());
        assert!(Version::parse(&json!({"x": 1})).is_err());
        assert_eq!(Version::parse(&Value::Null).unwrap(), None);
    }

    #[test]
    fn inputs_are_bounded() {
        let mut b = UpsertInput { type_key: "reservation".into(), external_id: "R1".into(), fields: Map::new(), profiles: Vec::new(), replace_links: Vec::new(), as_of: None, only_if_as_of_at_most: None };
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

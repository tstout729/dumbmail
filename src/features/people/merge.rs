//! Merging duplicate profiles, and finding possible duplicates (which only suggests merges).
//!
//! [`PROFILE_TABLES`] lists every table column that holds a profile id, with what a merge does to
//! it. Deleting a person (privacy requests) uses the same list. A test checks the database: a new
//! `profile_id` column anywhere that isn't listed here fails it, so add new tables to this list.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::consent::{self, ConsentSource, ConsentState};
use crate::error::{AppError, AppResult};

/// What a merge does with the merged-away profile's rows in one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMerge {
    /// Point them at the kept profile.
    Move,
    /// Point them at the kept profile unless it already has a row with the same values in these
    /// columns; those duplicates go with the merged profile.
    MoveUnless(&'static [&'static str]),
    /// They go with the merged profile.
    Drop,
    /// Handled by its own code in [`merge`] (named in `custom_merge`).
    Custom,
}

pub struct ProfileRef {
    pub table: &'static str,
    pub column: &'static str,
    pub on_merge: OnMerge,
}

/// Every column that holds a profile id. Deleting a person deletes their rows in all of these.
pub const PROFILE_TABLES: &[ProfileRef] = &[
    ProfileRef { table: "list_members", column: "profile_id", on_merge: OnMerge::MoveUnless(&["list_id"]) },
    ProfileRef { table: "segment_members", column: "profile_id", on_merge: OnMerge::MoveUnless(&["segment_id"]) },
    ProfileRef { table: "events", column: "profile_id", on_merge: OnMerge::Move },
    // Active runs of a flow the kept profile is already in are exited first.
    ProfileRef { table: "flow_runs", column: "profile_id", on_merge: OnMerge::Custom },
    // A campaign the kept profile also got keeps one link to it (the other message stays, unlinked).
    ProfileRef { table: "messages", column: "profile_id", on_merge: OnMerge::Custom },
    ProfileRef { table: "optin_confirmations", column: "profile_id", on_merge: OnMerge::Move },
    // Append-only: copied onto the kept profile.
    ProfileRef { table: "consent_events", column: "profile_id", on_merge: OnMerge::Custom },
    ProfileRef { table: "profile_notes", column: "profile_id", on_merge: OnMerge::Move },
    ProfileRef { table: "identity_conflicts", column: "profile_a", on_merge: OnMerge::Drop },
    ProfileRef { table: "identity_conflicts", column: "profile_b", on_merge: OnMerge::Drop },
    ProfileRef { table: "bulk_action_targets", column: "profile_id", on_merge: OnMerge::Drop },
];

/// Tables whose `OnMerge::Custom` handling lives in [`merge`].
pub const CUSTOM_MERGE: &[&str] = &["flow_runs", "messages", "consent_events"];

#[derive(Debug, Deserialize)]
pub struct MergeBody {
    /// The profile that stays.
    pub keep_id: Uuid,
    /// The profile merged into it and then removed.
    pub merge_id: Uuid,
    /// Per field, which profile's value to use when both have one: "keep" (the default) or "merge".
    /// Fields: email, phone_number, external_id, first_name, last_name, and properties.<key>.
    #[serde(default)]
    pub choices: HashMap<String, String>,
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    external_id: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    properties: Value,
    email_consent: String,
    sms_consent: String,
    email_suppressed_reason: Option<String>,
    sms_suppressed_reason: Option<String>,
    ads_opt_out: bool,
    created_at: DateTime<Utc>,
    last_event_at: Option<DateTime<Utc>>,
}

impl Row {
    fn state(&self) -> ConsentState {
        ConsentState {
            email: self.email.clone(),
            phone_number: self.phone_number.clone(),
            email_consent: self.email_consent.clone(),
            sms_consent: self.sms_consent.clone(),
            email_suppressed_reason: self.email_suppressed_reason.clone(),
            sms_suppressed_reason: self.sms_suppressed_reason.clone(),
            ads_opt_out: self.ads_opt_out,
        }
    }
}

const ROW_COLS: &str = "id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, sms_suppressed_reason, ads_opt_out, created_at, last_event_at";

/// The more restrictive of two consents: unsubscribed, then waiting to confirm, then never
/// subscribed, then subscribed.
pub fn stricter_consent<'a>(a: &'a str, b: &'a str) -> &'a str {
    let rank = |c: &str| match c {
        "unsubscribed" => 0,
        "pending" => 1,
        "never_subscribed" => 2,
        "subscribed" => 3,
        _ => 0,
    };
    if rank(b) < rank(a) { b } else { a }
}

/// The stronger of two suppressions (a hard bounce, then a complaint, then a manual one).
pub fn stronger_suppression(a: Option<String>, b: Option<String>) -> Option<String> {
    let rank = |r: &Option<String>| match r.as_deref() {
        None => 0,
        Some("manual") => 1,
        Some("complaint") => 2,
        Some("hard_bounce") => 3,
        Some(_) => 1,
    };
    if rank(&b) > rank(&a) { b } else { a }
}

struct Merged {
    row: Row,
    fields: Vec<Value>,
}

fn combine(keep: &Row, other: &Row, choices: &HashMap<String, String>) -> AppResult<Merged> {
    for (k, v) in choices {
        if v != "keep" && v != "merge" {
            return Err(AppError::bad(format!("choice for {k:?} must be \"keep\" or \"merge\"")));
        }
    }
    let mut fields = Vec::new();
    let mut pick = |name: &str, a: &Option<String>, b: &Option<String>| -> Option<String> {
        let take_other = choices.get(name).map(String::as_str) == Some("merge");
        let chosen = match (a, b) {
            (Some(_), Some(_)) if take_other => b.clone(),
            (Some(_), _) => a.clone(),
            (None, _) => b.clone(),
        };
        fields.push(json!({"field": name, "keep": a, "merge": b, "result": chosen, "conflict": a.is_some() && b.is_some() && a != b}));
        chosen
    };
    let email = pick("email", &keep.email, &other.email);
    let phone_number = pick("phone_number", &keep.phone_number, &other.phone_number);
    let external_id = pick("external_id", &keep.external_id, &other.external_id);
    let first_name = pick("first_name", &keep.first_name, &other.first_name);
    let last_name = pick("last_name", &keep.last_name, &other.last_name);
    let empty = Map::new();
    let kp = keep.properties.as_object().unwrap_or(&empty);
    let op = other.properties.as_object().unwrap_or(&empty);
    let mut props = op.clone();
    for (k, v) in kp {
        let key = format!("properties.{k}");
        let take_other = choices.get(&key).map(String::as_str) == Some("merge") && op.contains_key(k);
        if !take_other {
            props.insert(k.clone(), v.clone());
        }
        if let Some(ov) = op.get(k) {
            fields.push(json!({"field": key, "keep": v, "merge": ov, "result": props[k], "conflict": v != ov}));
        }
    }
    let row = Row {
        id: keep.id,
        email,
        phone_number,
        external_id,
        first_name,
        last_name,
        properties: Value::Object(props),
        email_consent: stricter_consent(&keep.email_consent, &other.email_consent).to_string(),
        sms_consent: stricter_consent(&keep.sms_consent, &other.sms_consent).to_string(),
        email_suppressed_reason: stronger_suppression(keep.email_suppressed_reason.clone(), other.email_suppressed_reason.clone()),
        sms_suppressed_reason: stronger_suppression(keep.sms_suppressed_reason.clone(), other.sms_suppressed_reason.clone()),
        ads_opt_out: keep.ads_opt_out || other.ads_opt_out,
        created_at: keep.created_at.min(other.created_at),
        last_event_at: keep.last_event_at.max(other.last_event_at),
    };
    Ok(Merged { row, fields })
}

fn row_json(r: &Row) -> Value {
    json!({
        "id": r.id, "email": r.email, "phone_number": r.phone_number, "external_id": r.external_id, "first_name": r.first_name,
        "last_name": r.last_name, "properties": r.properties, "email_consent": r.email_consent, "sms_consent": r.sms_consent,
        "email_suppressed_reason": r.email_suppressed_reason, "sms_suppressed_reason": r.sms_suppressed_reason,
        "ads_opt_out": r.ads_opt_out, "created_at": r.created_at, "last_event_at": r.last_event_at,
    })
}

/// Merge `merge_id` into `keep_id`. Without `confirm` it returns what the merged profile would look
/// like and how many rows would move. With it, everything happens in one transaction.
pub async fn merge(db: &PgPool, account_id: Uuid, b: &MergeBody, src: &ConsentSource) -> AppResult<Value> {
    if b.keep_id == b.merge_id {
        return Err(AppError::bad("pick two different profiles"));
    }
    let salt = super::privacy::salt(db, account_id).await?;
    let mut tx = db.begin().await?;
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {ROW_COLS} FROM profiles WHERE account_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE"))
        .bind(account_id)
        .bind([b.keep_id, b.merge_id])
        .fetch_all(&mut *tx)
        .await?;
    let keep = rows.iter().find(|r| r.id == b.keep_id).cloned().ok_or_else(|| AppError::not_found("profile to keep"))?;
    let other = rows.iter().find(|r| r.id == b.merge_id).cloned().ok_or_else(|| AppError::not_found("profile to merge"))?;
    let merged = combine(&keep, &other, &b.choices)?;

    let mut moves = Map::new();
    for t in PROFILE_TABLES {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {} WHERE {} = $1", t.table, t.column)).bind(other.id).fetch_one(&mut *tx).await?;
        if n > 0 {
            let prev = moves.get(t.table).and_then(Value::as_i64).unwrap_or(0);
            moves.insert(t.table.into(), json!(prev + n));
        }
    }
    let preview = json!({"result": row_json(&merged.row), "fields": merged.fields, "rows_moving": moves, "keep": row_json(&keep), "merge": row_json(&other)});
    if !b.confirm {
        let mut out = preview;
        out["dry_run"] = json!(true);
        out["note"] = json!("Nothing merged yet. Consent on each channel becomes the more restrictive of the two, and any suppression carries over. The merged profile is then removed. Call again with confirm=true once the user approves.");
        return Ok(out);
    }

    let r = &merged.row;
    // Free the identifiers first: they're unique per account.
    sqlx::query("UPDATE profiles SET email = NULL, phone_number = NULL, external_id = NULL WHERE id = $1").bind(other.id).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE profiles SET email = $2, phone_number = $3, external_id = $4, first_name = $5, last_name = $6, properties = $7,
                email_consent = $8, sms_consent = $9, email_suppressed_reason = $10, sms_suppressed_reason = $11,
                ads_opt_out = $12, ads_opt_out_at = CASE WHEN $12 AND NOT ads_opt_out THEN now() ELSE ads_opt_out_at END,
                ads_opt_out_source = CASE WHEN $12 AND NOT ads_opt_out THEN 'merge' ELSE ads_opt_out_source END,
                created_at = $13, last_event_at = $14, updated_at = now()
         WHERE id = $1",
    )
    .bind(keep.id)
    .bind(&r.email)
    .bind(&r.phone_number)
    .bind(&r.external_id)
    .bind(&r.first_name)
    .bind(&r.last_name)
    .bind(&r.properties)
    .bind(&r.email_consent)
    .bind(&r.sms_consent)
    .bind(&r.email_suppressed_reason)
    .bind(&r.sms_suppressed_reason)
    .bind(r.ads_opt_out)
    .bind(r.created_at)
    .bind(r.last_event_at)
    .execute(&mut *tx)
    .await?;
    let src = ConsentSource { method: "merge", ..src.clone() }.with_detail("merged_profile_id", json!(other.id));
    consent::record_diff(&mut tx, account_id, keep.id, Some(&keep.state()), &r.state(), &src).await?;
    // Addresses that don't survive the merge keep their suppression record, so a later import of
    // one of them can't come back more contactable.
    let keep_addrs: Vec<&str> = [r.email.as_deref(), r.phone_number.as_deref()].into_iter().flatten().collect();
    super::privacy::write_tombstones(&mut tx, account_id, &salt, &other.state(), &keep_addrs).await?;
    super::privacy::write_tombstones(&mut tx, account_id, &salt, &keep.state(), &keep_addrs).await?;

    for t in PROFILE_TABLES {
        let (table, col) = (t.table, t.column);
        match t.on_merge {
            OnMerge::Move => {
                sqlx::query(&format!("UPDATE {table} SET {col} = $1 WHERE {col} = $2")).bind(keep.id).bind(other.id).execute(&mut *tx).await?;
            }
            OnMerge::MoveUnless(cols) => {
                let same: String = cols.iter().map(|c| format!(" AND x.{c} = t.{c}")).collect();
                sqlx::query(&format!("UPDATE {table} t SET {col} = $1 WHERE t.{col} = $2 AND NOT EXISTS (SELECT 1 FROM {table} x WHERE x.{col} = $1{same})"))
                    .bind(keep.id)
                    .bind(other.id)
                    .execute(&mut *tx)
                    .await?;
            }
            OnMerge::Drop => {
                sqlx::query(&format!("DELETE FROM {table} WHERE {col} = $1")).bind(other.id).execute(&mut *tx).await?;
            }
            OnMerge::Custom => match table {
                "flow_runs" => {
                    sqlx::query(
                        "UPDATE flow_runs r SET status = 'exited', finished_at = now() WHERE r.profile_id = $2 AND r.status = 'active'
                           AND EXISTS (SELECT 1 FROM flow_runs k WHERE k.profile_id = $1 AND k.flow_id = r.flow_id AND k.status = 'active')",
                    )
                    .bind(keep.id)
                    .bind(other.id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("UPDATE flow_runs SET profile_id = $1 WHERE profile_id = $2").bind(keep.id).bind(other.id).execute(&mut *tx).await?;
                }
                "messages" => {
                    sqlx::query(
                        "UPDATE messages m SET campaign_id = NULL WHERE m.profile_id = $2 AND m.campaign_id IS NOT NULL AND NOT m.is_test
                           AND EXISTS (SELECT 1 FROM messages k WHERE k.profile_id = $1 AND k.campaign_id = m.campaign_id AND NOT k.is_test)",
                    )
                    .bind(keep.id)
                    .bind(other.id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("UPDATE messages SET profile_id = $1 WHERE profile_id = $2").bind(keep.id).bind(other.id).execute(&mut *tx).await?;
                }
                "consent_events" => {
                    sqlx::query(
                        "INSERT INTO consent_events (id, account_id, profile_id, channel, field, old_value, new_value, method, via, address,
                                                     actor_user_id, form_id, message_id, ip, user_agent, detail, occurred_at)
                         SELECT gen_random_uuid(), account_id, $1, channel, field, old_value, new_value, method, via, address,
                                actor_user_id, form_id, message_id, ip, user_agent, detail || jsonb_build_object('merged_from', $2::text), occurred_at
                         FROM consent_events WHERE profile_id = $2",
                    )
                    .bind(keep.id)
                    .bind(other.id)
                    .execute(&mut *tx)
                    .await?;
                }
                other_table => return Err(AppError::Internal(anyhow::anyhow!("no merge handling for {other_table}"))),
            },
        }
    }
    sqlx::query("DELETE FROM profiles WHERE id = $1 AND account_id = $2").bind(other.id).bind(account_id).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut out = preview;
    out["merged"] = json!(true);
    Ok(out)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Brief {
    id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    external_id: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    email_consent: String,
    sms_consent: String,
    created_at: DateTime<Utc>,
}

/// Groups of profiles that look like the same person: the same Gmail inbox spelled differently,
/// the same email in another case, the same phone number written differently, the same external id
/// in another case, or identifiers a sign-up or API call linked together that belong to two
/// profiles. It only suggests; nothing is merged without asking.
pub async fn find_duplicates(db: &PgPool, account_id: Uuid, limit: i64) -> AppResult<Vec<Value>> {
    let limit = limit.clamp(1, 200);
    let groups: Vec<(String, String, Vec<Uuid>)> = sqlx::query_as(
        r"WITH keyed AS (
            SELECT id, created_at,
                   CASE WHEN email IS NULL THEN NULL
                        WHEN split_part(lower(trim(email)), '@', 2) IN ('gmail.com', 'googlemail.com')
                          THEN replace(split_part(split_part(lower(trim(email)), '@', 1), '+', 1), '.', '') || '@gmail.com'
                        ELSE lower(trim(email)) END AS ekey,
                   CASE WHEN phone_number IS NULL THEN NULL
                        WHEN length(regexp_replace(phone_number, '\D', '', 'g')) = 10 THEN '1' || regexp_replace(phone_number, '\D', '', 'g')
                        ELSE regexp_replace(phone_number, '\D', '', 'g') END AS pkey,
                   lower(trim(external_id)) AS xkey
            FROM profiles WHERE account_id = $1)
          SELECT kind, key, ids FROM (
            SELECT 'email' AS kind, ekey AS key, array_agg(id ORDER BY created_at) AS ids FROM keyed WHERE ekey IS NOT NULL GROUP BY ekey HAVING count(*) > 1
            UNION ALL
            SELECT 'phone_number', pkey, array_agg(id ORDER BY created_at) FROM keyed WHERE pkey IS NOT NULL AND pkey <> '' GROUP BY pkey HAVING count(*) > 1
            UNION ALL
            SELECT 'external_id', xkey, array_agg(id ORDER BY created_at) FROM keyed WHERE xkey IS NOT NULL AND xkey <> '' GROUP BY xkey HAVING count(*) > 1
          ) g ORDER BY kind, key LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let conflicts: Vec<(Uuid, Uuid, Value, i32, DateTime<Utc>)> = sqlx::query_as(
        "SELECT profile_a, profile_b, identifiers, times_seen, last_seen FROM identity_conflicts WHERE account_id = $1 ORDER BY last_seen DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut ids: Vec<Uuid> = groups.iter().flat_map(|g| g.2.iter().take(10).copied()).collect();
    ids.extend(conflicts.iter().flat_map(|c| [c.0, c.1]));
    let briefs: Vec<Brief> = sqlx::query_as(
        "SELECT id, email, phone_number, external_id, first_name, last_name, email_consent, sms_consent, created_at FROM profiles WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let by_id: HashMap<Uuid, &Brief> = briefs.iter().map(|b| (b.id, b)).collect();
    let brief = |id: &Uuid| {
        by_id.get(id).map(|b| {
            json!({"id": b.id, "email": b.email, "phone_number": b.phone_number, "external_id": b.external_id, "first_name": b.first_name,
                   "last_name": b.last_name, "email_consent": b.email_consent, "sms_consent": b.sms_consent, "created_at": b.created_at})
        })
    };
    let mut out = Vec::new();
    for (kind, key, gids) in &groups {
        let why = match kind.as_str() {
            "email" => "Same email inbox, written differently (case, dots or a +tag in a Gmail address)",
            "phone_number" => "Same phone number, written differently",
            _ => "Same external id, in a different case",
        };
        out.push(json!({"reason": kind, "why": why, "key": key, "profiles": gids.iter().take(10).filter_map(brief).collect::<Vec<_>>()}));
    }
    for (a, b, identifiers, seen, last) in conflicts {
        let profiles: Vec<Value> = [a, b].iter().filter_map(brief).collect();
        if profiles.len() == 2 {
            out.push(json!({
                "reason": "linked_identifiers", "why": "A sign-up or API call gave identifiers that belong to these two profiles (for example an email on one and a phone number on the other)",
                "key": identifiers, "times_seen": seen, "last_seen": last, "profiles": profiles,
            }));
        }
    }
    Ok(out)
}

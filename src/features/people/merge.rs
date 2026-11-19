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

/// A message claimed longer ago than this was abandoned by its worker (sends are bounded far below it).
pub const STALE_CLAIM_MINUTES: i32 = 15;

/// What a merge does with the merged-away profile's rows in one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMerge {
    /// Point them at the kept profile.
    Move,
    /// Point them at the kept profile unless it already has a row with the same values in these
    /// columns (NULLs count as equal); those duplicates go with the merged profile.
    MoveUnless(&'static [&'static str]),
    /// They go with the merged profile.
    Drop,
    /// Handled by its own code in [`merge`] (named in `CUSTOM_MERGE`).
    Custom,
}

/// What erasing a person does with their rows in one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnErase {
    Delete,
    /// Keep the row but drop the profile id (a bulk action's target, reported as skipped).
    Forget,
}

pub struct ProfileRef {
    pub table: &'static str,
    pub column: &'static str,
    pub on_merge: OnMerge,
    pub on_erase: OnErase,
}

const fn r(table: &'static str, column: &'static str, on_merge: OnMerge) -> ProfileRef {
    ProfileRef { table, column, on_merge, on_erase: OnErase::Delete }
}

/// Every column that holds a profile id, with what a merge and an erasure do to its rows.
pub const PROFILE_TABLES: &[ProfileRef] = &[
    r("list_members", "profile_id", OnMerge::MoveUnless(&["list_id"])),
    r("segment_members", "profile_id", OnMerge::MoveUnless(&["segment_id"])),
    r("events", "profile_id", OnMerge::Move),
    // Active runs of a flow the kept profile is already in are exited first.
    r("flow_runs", "profile_id", OnMerge::Custom),
    // Queued duplicates are cancelled; a campaign the kept profile also got keeps one link to it.
    r("messages", "profile_id", OnMerge::Custom),
    // Waiting confirmations sent to an address the merged profile won't have are voided.
    r("optin_confirmations", "profile_id", OnMerge::Custom),
    // Append-only: copied onto the kept profile (marked merged_from); the originals go with the profile.
    r("consent_events", "profile_id", OnMerge::Custom),
    r("profile_notes", "profile_id", OnMerge::Move),
    r("identity_conflicts", "profile_a", OnMerge::Drop),
    r("identity_conflicts", "profile_b", OnMerge::Drop),
    // A bulk action acts on the people it was approved for: a merged-away target is skipped.
    ProfileRef { table: "bulk_action_targets", column: "profile_id", on_merge: OnMerge::Custom, on_erase: OnErase::Forget },
    // Personal exports follow the person, so erasing the kept profile removes them too.
    r("people_file_subjects", "profile_id", OnMerge::MoveUnless(&["file_id"])),
    r("catalog_interactions", "profile_id", OnMerge::Move),
    // One waiting alert per product and variant.
    r("catalog_stock_subscriptions", "profile_id", OnMerge::Custom),
    // The most recent notice wins, so the cooldown never gets shorter.
    r("catalog_price_drop_notices", "profile_id", OnMerge::Custom),
    // The record of what was sent stays with the person; erasure removes it (it holds the address).
    r("transactional_sends", "profile_id", OnMerge::Move),
    r("campaign_holdouts", "profile_id", OnMerge::MoveUnless(&["campaign_id"])),
    // What a Klaviyo import last wrote: the kept profile's own record stays, the merged one's goes,
    // so a later import never takes the merged consent for something it wrote itself.
    r("klaviyo_consent_writes", "profile_id", OnMerge::Drop),
    r("klaviyo_memberships", "profile_id", OnMerge::MoveUnless(&["list_id"])),
    // One code per person and coupon: the merged profile's code for a coupon the kept one already has
    // stays given out (never reused) but belongs to nobody. Erasure keeps codes the same way.
    ProfileRef { table: "coupon_codes", column: "profile_id", on_merge: OnMerge::Custom, on_erase: OnErase::Forget },
    r("coupon_signup_grants", "profile_id", OnMerge::Move),
    r("object_links", "profile_id", OnMerge::MoveUnless(&["object_id", "relationship"])),
    // One texting thread per person: the merged profile's texts join the kept profile's thread.
    // Before sms_inbound and sms_auto_sends, which point at a thread.
    r("sms_conversations", "profile_id", OnMerge::Custom),
    r("sms_inbound", "profile_id", OnMerge::Move),
    r("sms_auto_sends", "profile_id", OnMerge::Move),
    // A keyword sign-up waiting for "Y" is for the number it was sent to (like an email confirmation).
    r("sms_pending_optins", "profile_id", OnMerge::Custom),
    // The proof of each keyword sign-up stays with the person.
    r("sms_consents", "profile_id", OnMerge::Move),
    // Devices follow the person (erasure deletes them: their tokens are personal data).
    r("push_devices", "profile_id", OnMerge::Move),
    // A push opt-out on either profile stays.
    r("push_opt_outs", "profile_id", OnMerge::MoveUnless(&[])),
    r("push_deliveries", "owner_profile_id", OnMerge::Move),
    // Shopify's mapping of its customers to profiles (customers/redact reads it).
    r("shopify_customers", "profile_id", OnMerge::Move),
    ProfileRef { table: "shopify_discount_codes", column: "profile_id", on_merge: OnMerge::Move, on_erase: OnErase::Forget },
    r("shopify_customer_profiles", "profile_id", OnMerge::MoveUnless(&["account_id", "shop_domain", "customer_id"])),
    // "Shopify created this profile" is about the merged one, not the kept one: it goes.
    r("shopify_created_profiles", "profile_id", OnMerge::Drop),
    r("shopify_history_touched", "profile_id", OnMerge::MoveUnless(&["account_id"])),
    // A sync's staged audience; the next run stages the kept profile from its merged state.
    r("ad_audience_desired", "profile_id", OnMerge::Drop),
    // Reviews follow the person (erasure goes through features::reviews to keep the star summaries right).
    r("review_requests", "profile_id", OnMerge::Move),
    r("reviews", "profile_id", OnMerge::Move),
    r("review_sends", "profile_id", OnMerge::Move),
    r("flow_backfill_members", "profile_id", OnMerge::MoveUnless(&["backfill_id"])),
    // The customer hub's sign-in codes and sessions are for the merged profile's address: they end
    // (the person signs in again). The kept profile's end too when its address changes.
    r("hub_codes", "profile_id", OnMerge::Drop),
    r("hub_sessions", "profile_id", OnMerge::Drop),
    r("hub_generations", "profile_id", OnMerge::Drop),
];

/// Tables whose `OnMerge::Custom` handling lives in [`merge`].
pub const CUSTOM_MERGE: &[&str] = &[
    "flow_runs", "messages", "optin_confirmations", "consent_events", "bulk_action_targets", "catalog_stock_subscriptions", "catalog_price_drop_notices", "coupon_codes",
    "sms_conversations", "sms_pending_optins",
];

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
    marketing_hold: bool,
    /// Opted out of ads by the flag or by a property ad audiences read (`opted_out_sql`).
    ads_effective: bool,
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
            marketing_hold: self.marketing_hold,
        }
    }
}

const ROW_COLS: &str = "id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, sms_suppressed_reason, ads_opt_out, marketing_hold, created_at, last_event_at";

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
        // An ad opt-out either side had, by the flag or by a property, stays (as the flag, so no
        // property the kept profile wins on can drop it).
        ads_opt_out: keep.ads_effective || other.ads_effective,
        // Held if either was (the database lets go of it when the merged email consent is subscribed).
        marketing_hold: keep.marketing_hold || other.marketing_hold,
        ads_effective: keep.ads_effective || other.ads_effective,
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
        "ads_opt_out": r.ads_opt_out, "marketing_hold": r.marketing_hold && r.email_consent != "subscribed", "created_at": r.created_at, "last_event_at": r.last_event_at,
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
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {ROW_COLS}, {} AS ads_effective FROM profiles WHERE account_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
        crate::features::audiences::sync::opted_out_sql("profiles")
    ))
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

    // A send already claimed for either profile can't be moved safely: ask to retry. Every kind of
    // send has its own claim: a message, a transactional send, an automatic text (before its message
    // exists) and a push delivery. Waiting ones are locked, so nobody claims them until this commits.
    // A message claim older than STALE_CLAIM_MINUTES belongs to a worker that died (the campaign
    // sweep gives such messages back); it doesn't hold a merge up forever.
    let ids = [keep.id, other.id];
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = ANY($1) AND status = 'sending' AND COALESCE(claimed_at, created_at) > now() - make_interval(mins => $2))
             OR EXISTS (SELECT 1 FROM transactional_sends WHERE profile_id = ANY($1) AND status = 'sending')
             OR EXISTS (SELECT 1 FROM sms_auto_sends WHERE profile_id = ANY($1) AND status = 'sending')
             OR EXISTS (SELECT 1 FROM push_deliveries WHERE owner_profile_id = ANY($1) AND status = 'sending')",
    )
    .bind(ids)
    .bind(STALE_CLAIM_MINUTES)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(AppError::Conflict("A message to one of these profiles is being sent right now. Try the merge again in a minute.".into()));
    }
    sqlx::query("SELECT id FROM messages WHERE profile_id = ANY($1) AND status = 'queued' ORDER BY id FOR UPDATE").bind(ids).execute(&mut *tx).await?;
    sqlx::query("SELECT id FROM transactional_sends WHERE profile_id = ANY($1) AND status = 'queued' ORDER BY id FOR UPDATE").bind(ids).execute(&mut *tx).await?;
    sqlx::query("SELECT id FROM sms_auto_sends WHERE profile_id = ANY($1) AND status = 'queued' ORDER BY id FOR UPDATE").bind(ids).execute(&mut *tx).await?;
    sqlx::query("SELECT id FROM push_deliveries WHERE owner_profile_id = ANY($1) AND status IN ('queued', 'retry') ORDER BY id FOR UPDATE").bind(ids).execute(&mut *tx).await?;

    let r = &merged.row;
    // Free the identifiers first: they're unique per account.
    sqlx::query("UPDATE profiles SET email = NULL, phone_number = NULL, external_id = NULL WHERE id = $1").bind(other.id).execute(&mut *tx).await?;
    let held: bool = sqlx::query_scalar(
        "UPDATE profiles SET email = $2, phone_number = $3, external_id = $4, first_name = $5, last_name = $6, properties = $7,
                email_consent = $8, sms_consent = $9, email_suppressed_reason = $10, sms_suppressed_reason = $11,
                ads_opt_out = $12, ads_opt_out_at = CASE WHEN $12 AND NOT ads_opt_out THEN now() ELSE ads_opt_out_at END,
                ads_opt_out_source = CASE WHEN $12 AND NOT ads_opt_out THEN 'merge' ELSE ads_opt_out_source END,
                created_at = $13, last_event_at = $14, marketing_hold = $15, updated_at = now()
         WHERE id = $1 RETURNING marketing_hold",
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
    .bind(r.marketing_hold)
    .fetch_one(&mut *tx)
    .await?;
    let src = ConsentSource { method: "merge", ..src.clone() }.with_detail("merged_profile_id", json!(other.id));
    // The merged state is never less restrictive than the kept one, so a failed history write can't
    // block it (see consent::record_diff).
    let saved = ConsentState { marketing_hold: held, ..r.state() };
    // A new address on the kept profile ends its customer hub access, as an address edit does.
    if keep.email.is_some() && keep.email != r.email {
        crate::features::hub::end_sessions(&mut *tx, keep.id).await?;
    }
    consent::note_address_change(&mut tx, account_id, keep.id, &keep.state(), &saved, &src).await?;
    consent::record_diff(&mut tx, account_id, keep.id, Some(&keep.state()), &saved, &src).await?;
    // Addresses that don't survive the merge keep their suppression record, so a later import of
    // one of them can't come back more contactable.
    let keep_addrs: Vec<&str> = [r.email.as_deref(), r.phone_number.as_deref()].into_iter().flatten().collect();
    super::privacy::write_tombstones(&mut tx, account_id, &salt, &other.state(), &keep_addrs).await?;
    super::privacy::write_tombstones(&mut tx, account_id, &salt, &keep.state(), &keep_addrs).await?;
    // History rows still waiting to be written for the merged profile land on the kept one.
    sqlx::query(
        "UPDATE jobs SET payload = jsonb_build_object('profile_id', $1::text, 'rows',
             (SELECT COALESCE(jsonb_agg(x || jsonb_build_object('profile_id', $1::text, 'detail', COALESCE(x->'detail', '{}'::jsonb) || jsonb_build_object('merged_from', $2::text))), '[]'::jsonb)
              FROM jsonb_array_elements(payload->'rows') x))
         WHERE kind = $3 AND payload->>'profile_id' = $2::text",
    )
    .bind(keep.id)
    .bind(other.id)
    .bind(consent::HISTORY_JOB)
    .execute(&mut *tx)
    .await?;

    for t in PROFILE_TABLES {
        let (table, col) = (t.table, t.column);
        match t.on_merge {
            OnMerge::Move => {
                sqlx::query(&format!("UPDATE {table} SET {col} = $1 WHERE {col} = $2")).bind(keep.id).bind(other.id).execute(&mut *tx).await?;
            }
            OnMerge::MoveUnless(cols) => {
                let same: String = cols.iter().map(|c| format!(" AND x.{c} IS NOT DISTINCT FROM t.{c}")).collect();
                sqlx::query(&format!("DELETE FROM {table} t WHERE t.{col} = $2 AND EXISTS (SELECT 1 FROM {table} x WHERE x.{col} = $1{same})"))
                    .bind(keep.id)
                    .bind(other.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(&format!("UPDATE {table} SET {col} = $1 WHERE {col} = $2")).bind(keep.id).bind(other.id).execute(&mut *tx).await?;
            }
            OnMerge::Drop => {
                sqlx::query(&format!("DELETE FROM {table} WHERE {col} = $1")).bind(other.id).execute(&mut *tx).await?;
            }
            OnMerge::Custom => custom_merge(&mut tx, table, keep.id, other.id, r.email.as_deref(), r.phone_number.as_deref()).await?,
        }
    }
    sqlx::query("DELETE FROM profiles WHERE id = $1 AND account_id = $2").bind(other.id).bind(account_id).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut out = preview;
    out["merged"] = json!(true);
    Ok(out)
}

/// The tables a merge handles with its own code (see [`PROFILE_TABLES`]).
async fn custom_merge(tx: &mut sqlx::PgConnection, table: &str, keep: Uuid, other: Uuid, merged_email: Option<&str>, merged_phone: Option<&str>) -> AppResult<()> {
    match table {
        "flow_runs" => {
            sqlx::query(
                "UPDATE flow_runs r SET status = 'exited', finished_at = now() WHERE r.profile_id = $2 AND r.status = 'active'
                   AND EXISTS (SELECT 1 FROM flow_runs k WHERE k.profile_id = $1 AND k.flow_id = r.flow_id AND k.status = 'active')",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE flow_runs SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "messages" => {
            // Both have a queued message for one campaign, and only the merged profile's message names
            // the address it keeps: that one stays, the kept profile's is cancelled.
            sqlx::query(
                "UPDATE messages k SET status = 'skipped', error = 'merged into another profile', campaign_id = NULL
                 WHERE k.profile_id = $1 AND k.status = 'queued' AND NOT k.is_test AND k.campaign_id IS NOT NULL
                   AND k.to_address IS DISTINCT FROM (CASE k.channel WHEN 'sms' THEN $4 ELSE $3 END)
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.profile_id = $2 AND m.campaign_id = k.campaign_id AND m.status = 'queued' AND NOT m.is_test
                                 AND m.to_address = (CASE m.channel WHEN 'sms' THEN $4 ELSE $3 END))",
            )
            .bind(keep)
            .bind(other)
            .bind(merged_email)
            .bind(merged_phone)
            .execute(&mut *tx)
            .await?;
            // A queued message for a campaign or flow step the kept profile already has a message for
            // would reach the same person twice: cancel it. (Both profiles' queued messages are locked
            // above, so a worker can't claim one meanwhile.)
            sqlx::query(
                "UPDATE messages m SET status = 'skipped', error = 'merged into another profile', campaign_id = NULL
                 WHERE m.profile_id = $2 AND m.status = 'queued' AND NOT m.is_test AND (
                   (m.campaign_id IS NOT NULL AND EXISTS (SELECT 1 FROM messages k WHERE k.profile_id = $1 AND k.campaign_id = m.campaign_id AND NOT k.is_test))
                   OR (m.flow_id IS NOT NULL AND EXISTS (SELECT 1 FROM messages k WHERE k.profile_id = $1 AND k.flow_id = m.flow_id AND k.flow_node_id IS NOT DISTINCT FROM m.flow_node_id AND NOT k.is_test)))",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            // A campaign both got keeps one link to it (the other message stays in the history, unlinked).
            sqlx::query(
                "UPDATE messages m SET campaign_id = NULL WHERE m.profile_id = $2 AND m.campaign_id IS NOT NULL AND NOT m.is_test
                   AND EXISTS (SELECT 1 FROM messages k WHERE k.profile_id = $1 AND k.campaign_id = m.campaign_id AND NOT k.is_test)",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            // The rest move. A queued one still names the address it was meant for, and the sender skips
            // it unless that's the merged profile's address now.
            sqlx::query("UPDATE messages SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "optin_confirmations" => {
            // A confirmation is for the address it was sent to: waiting ones (on either profile) for an
            // address the merged profile doesn't have are voided, so the link can't subscribe another one.
            let key = merged_email.map(crate::optin::email_key);
            sqlx::query("UPDATE optin_confirmations SET voided_at = now() WHERE profile_id = ANY($1) AND confirmed_at IS NULL AND voided_at IS NULL AND email_key IS DISTINCT FROM $2")
                .bind([keep, other])
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE optin_confirmations SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "consent_events" => {
            sqlx::query(
                "INSERT INTO consent_events (id, account_id, profile_id, channel, field, old_value, new_value, method, via, address,
                                             actor_user_id, form_id, message_id, ip, user_agent, detail, occurred_at, recorded_at)
                 SELECT gen_random_uuid(), account_id, $1, channel, field, old_value, new_value, method, via, address,
                        actor_user_id, form_id, message_id, ip, user_agent, detail || jsonb_build_object('merged_from', $2::text), occurred_at, recorded_at
                 FROM consent_events WHERE profile_id = $2",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
        }
        "bulk_action_targets" => {
            sqlx::query("UPDATE bulk_action_targets SET profile_id = NULL WHERE profile_id = $1").bind(other).execute(&mut *tx).await?;
        }
        "catalog_stock_subscriptions" => {
            sqlx::query(
                "DELETE FROM catalog_stock_subscriptions t WHERE t.profile_id = $2 AND t.notified_at IS NULL AND EXISTS (
                   SELECT 1 FROM catalog_stock_subscriptions x WHERE x.profile_id = $1 AND x.notified_at IS NULL AND x.product_id = t.product_id AND x.variant_id IS NOT DISTINCT FROM t.variant_id)",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE catalog_stock_subscriptions SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "catalog_price_drop_notices" => {
            sqlx::query(
                "UPDATE catalog_price_drop_notices k SET notified_at = GREATEST(k.notified_at, o.notified_at)
                 FROM catalog_price_drop_notices o WHERE k.profile_id = $1 AND o.profile_id = $2 AND o.product_id = k.product_id",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM catalog_price_drop_notices o WHERE o.profile_id = $2 AND EXISTS (SELECT 1 FROM catalog_price_drop_notices k WHERE k.profile_id = $1 AND k.product_id = o.product_id)")
                .bind(keep)
                .bind(other)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE catalog_price_drop_notices SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "coupon_codes" => {
            sqlx::query(
                "UPDATE coupon_codes o SET profile_id = NULL WHERE o.profile_id = $2 AND EXISTS (
                   SELECT 1 FROM coupon_codes k WHERE k.profile_id = $1 AND k.coupon_id = o.coupon_id AND k.send_key IS NOT DISTINCT FROM o.send_key)",
            )
            .bind(keep)
            .bind(other)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE coupon_codes SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        "sms_conversations" => {
            let kept: Option<Uuid> = sqlx::query_scalar("SELECT id FROM sms_conversations WHERE profile_id = $1 FOR UPDATE").bind(keep).fetch_optional(&mut *tx).await?;
            let merged: Option<Uuid> = sqlx::query_scalar("SELECT id FROM sms_conversations WHERE profile_id = $1 FOR UPDATE").bind(other).fetch_optional(&mut *tx).await?;
            match (kept, merged) {
                (Some(k), Some(m)) => {
                    for t in ["sms_inbound", "sms_auto_sends"] {
                        sqlx::query(&format!("UPDATE {t} SET conversation_id = $1 WHERE conversation_id = $2")).bind(k).bind(m).execute(&mut *tx).await?;
                    }
                    sqlx::query(
                        "UPDATE sms_conversations k SET unread_count = k.unread_count + o.unread_count,
                            last_message_at = GREATEST(k.last_message_at, o.last_message_at), last_inbound_at = GREATEST(k.last_inbound_at, o.last_inbound_at),
                            last_direction = CASE WHEN o.last_message_at > k.last_message_at THEN o.last_direction ELSE k.last_direction END,
                            last_preview = CASE WHEN o.last_message_at > k.last_message_at THEN o.last_preview ELSE k.last_preview END,
                            status = CASE WHEN 'open' IN (k.status, o.status) THEN 'open' ELSE k.status END,
                            last_default_reply_at = GREATEST(k.last_default_reply_at, o.last_default_reply_at),
                            auto_window_start = GREATEST(k.auto_window_start, o.auto_window_start),
                            auto_window_count = GREATEST(k.auto_window_count, o.auto_window_count)
                         FROM sms_conversations o WHERE k.id = $1 AND o.id = $2",
                    )
                    .bind(k)
                    .bind(m)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("DELETE FROM sms_conversations WHERE id = $1").bind(m).execute(&mut *tx).await?;
                }
                (None, Some(m)) => {
                    sqlx::query("UPDATE sms_conversations SET profile_id = $1 WHERE id = $2").bind(keep).bind(m).execute(&mut *tx).await?;
                }
                _ => {}
            }
        }
        "sms_pending_optins" => {
            // A waiting "Y" for a number the merged profile won't have can't subscribe its number.
            sqlx::query("DELETE FROM sms_pending_optins WHERE profile_id = ANY($1) AND phone_number IS DISTINCT FROM $2").bind([keep, other]).bind(merged_phone).execute(&mut *tx).await?;
            sqlx::query("UPDATE sms_pending_optins SET profile_id = $1 WHERE profile_id = $2").bind(keep).bind(other).execute(&mut *tx).await?;
        }
        other_table => return Err(AppError::Internal(anyhow::anyhow!("no merge handling for {other_table}"))),
    }
    Ok(())
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

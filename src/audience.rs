//! Profiles and events: identity resolution, upserts and event recording.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone, valid_consent, Profile, PROFILE_COLS};

/// Fields accepted when creating or identifying a profile. Unknown keys become custom properties.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileInput {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub properties: Option<Map<String, Value>>,
    #[serde(default)]
    pub email_consent: Option<String>,
    #[serde(default)]
    pub sms_consent: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ProfileInput {
    fn merged_properties(&self) -> Map<String, Value> {
        let mut props = self.properties.clone().unwrap_or_default();
        for (k, v) in &self.extra {
            if k != "list_ids" && !k.starts_with('$') {
                props.insert(k.clone(), v.clone());
            }
        }
        props
    }
}

pub struct Upserted {
    pub profile: Profile,
    pub created: bool,
}

pub async fn find_profile(db: &PgPool, account_id: Uuid, input: &ProfileInput) -> AppResult<Option<Profile>> {
    let email = input.email.as_deref().and_then(normalize_email);
    let phone = input.phone_number.as_deref().and_then(normalize_phone);
    let q = format!(
        "SELECT {PROFILE_COLS} FROM profiles p WHERE p.account_id = $1 AND (
            p.id = $2 OR p.external_id = $3 OR p.email = $4 OR p.phone_number = $5)
         ORDER BY (p.id = $2) DESC NULLS LAST, (p.external_id = $3) DESC NULLS LAST, (p.email = $4) DESC NULLS LAST
         LIMIT 1"
    );
    Ok(sqlx::query_as::<_, Profile>(&q)
        .bind(account_id)
        .bind(input.id)
        .bind(&input.external_id)
        .bind(&email)
        .bind(&phone)
        .fetch_optional(db)
        .await?)
}

/// Create or update a profile, merging properties. Identity priority: id, external_id, email, phone.
pub async fn upsert_profile(db: &PgPool, account_id: Uuid, input: &ProfileInput) -> AppResult<Upserted> {
    upsert_inner(db, account_id, input, false).await
}

/// For unauthenticated callers (browser snippet, forms): an existing profile's identity and
/// names are only filled in where blank, never overwritten, so nobody can take over a profile.
pub async fn upsert_profile_public(db: &PgPool, account_id: Uuid, input: &ProfileInput) -> AppResult<Upserted> {
    upsert_inner(db, account_id, input, true).await
}

async fn upsert_inner(db: &PgPool, account_id: Uuid, input: &ProfileInput, fill_only: bool) -> AppResult<Upserted> {
    let email = match input.email.as_deref().filter(|e| !e.trim().is_empty()) {
        Some(e) => Some(normalize_email(e).ok_or_else(|| AppError::bad(format!("{e:?} is not a valid email")))?),
        None => None,
    };
    let phone = match input.phone_number.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => Some(normalize_phone(p).ok_or_else(|| AppError::bad(format!("{p:?} is not a valid phone number (use +15035550100)")))?),
        None => None,
    };
    for c in [&input.email_consent, &input.sms_consent].into_iter().flatten() {
        if !valid_consent(c) {
            return Err(AppError::bad("consent must be subscribed, unsubscribed or never_subscribed"));
        }
    }
    if input.id.is_none() && email.is_none() && phone.is_none() && input.external_id.is_none() {
        return Err(AppError::bad("a profile needs an email, phone_number or external_id"));
    }
    let props = Value::Object(input.merged_properties());

    for attempt in 0..2 {
        let existing = find_profile(db, account_id, input).await?;
        let res = match &existing {
            Some(p) => {
                let q = if fill_only {
                    format!(
                        "UPDATE profiles p SET
                            email = COALESCE(p.email, $2), phone_number = COALESCE(p.phone_number, $3),
                            external_id = COALESCE(p.external_id, $4), first_name = COALESCE(p.first_name, $5),
                            last_name = COALESCE(p.last_name, $6), properties = $7 || p.properties,
                            email_consent = COALESCE($8, p.email_consent), sms_consent = COALESCE($9, p.sms_consent),
                            updated_at = now()
                         WHERE p.id = $1 RETURNING {PROFILE_COLS}"
                    )
                } else {
                    format!(
                        "UPDATE profiles p SET
                            email = COALESCE($2, p.email), phone_number = COALESCE($3, p.phone_number),
                            external_id = COALESCE($4, p.external_id), first_name = COALESCE($5, p.first_name),
                            last_name = COALESCE($6, p.last_name), properties = p.properties || $7,
                            email_consent = COALESCE($8, p.email_consent), sms_consent = COALESCE($9, p.sms_consent),
                            updated_at = now()
                         WHERE p.id = $1 RETURNING {PROFILE_COLS}"
                    )
                };
                sqlx::query_as::<_, Profile>(&q)
                    .bind(p.id)
                    .bind(&email)
                    .bind(&phone)
                    .bind(&input.external_id)
                    .bind(&input.first_name)
                    .bind(&input.last_name)
                    .bind(&props)
                    .bind(&input.email_consent)
                    .bind(&input.sms_consent)
                    .fetch_one(db)
                    .await
                    .map(|profile| Upserted { profile, created: false })
            }
            None => {
                let q = format!(
                    "INSERT INTO profiles AS p (id, account_id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, 'never_subscribed'), COALESCE($10, 'never_subscribed'))
                     RETURNING {PROFILE_COLS}"
                );
                sqlx::query_as::<_, Profile>(&q)
                    .bind(input.id.unwrap_or_else(Uuid::now_v7))
                    .bind(account_id)
                    .bind(&email)
                    .bind(&phone)
                    .bind(&input.external_id)
                    .bind(&input.first_name)
                    .bind(&input.last_name)
                    .bind(&props)
                    .bind(&input.email_consent)
                    .bind(&input.sms_consent)
                    .fetch_one(db)
                    .await
                    .map(|profile| Upserted { profile, created: true })
            }
        };
        match res {
            Ok(u) => return Ok(u),
            // Lost a race with another insert, or the update collided with another profile's identity.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") && attempt == 0 && existing.is_none() => continue,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                return Err(AppError::Conflict("another profile already uses that email, phone number or external id".into()))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Conflict("could not save profile, please retry".into()))
}

pub async fn metric_id(db: &PgPool, account_id: Uuid, name: &str) -> AppResult<Uuid> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(AppError::bad("metric name must be 1-128 characters"));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO metrics (id, account_id, name) VALUES ($1, $2, $3)
         ON CONFLICT (account_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(name)
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub struct NewEvent<'a> {
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub metric: &'a str,
    pub properties: Value,
    pub value: Option<f64>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub unique_id: Option<String>,
    pub message_id: Option<Uuid>,
}

/// Record an event and wake any live flows triggered by that metric. Returns None for duplicates.
pub async fn record_event(db: &PgPool, ev: NewEvent<'_>) -> AppResult<Option<Uuid>> {
    let metric_id = metric_id(db, ev.account_id, ev.metric).await?;
    let id = Uuid::now_v7();
    let value = ev.value.or_else(|| ev.properties.get("$value").and_then(|v| v.as_f64()));
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO events (id, account_id, profile_id, metric_id, properties, value, occurred_at, unique_id, message_id)
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, now()), $8, $9)
         ON CONFLICT (account_id, metric_id, unique_id) WHERE unique_id IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(ev.account_id)
    .bind(ev.profile_id)
    .bind(metric_id)
    .bind(&ev.properties)
    .bind(value)
    .bind(ev.occurred_at)
    .bind(&ev.unique_id)
    .bind(ev.message_id)
    .fetch_optional(db)
    .await?;
    let Some(event_id) = inserted else { return Ok(None) };
    sqlx::query("UPDATE profiles SET last_event_at = GREATEST(COALESCE(last_event_at, 'epoch'), COALESCE($2, now())) WHERE id = $1")
        .bind(ev.profile_id)
        .bind(ev.occurred_at)
        .execute(db)
        .await?;
    let has_flow: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM flows WHERE account_id = $1 AND status = 'live' AND trigger->>'type' = 'metric' AND trigger->>'metric' = $2)",
    )
    .bind(ev.account_id)
    .bind(ev.metric)
    .fetch_one(db)
    .await?;
    if has_flow {
        crate::jobs::enqueue(db, "flow_event", json!({"event_id": event_id}), None).await?;
    }
    Ok(Some(event_id))
}

/// Add profiles to a list and wake list-triggered flows for newly added members.
pub async fn add_to_list(db: &PgPool, account_id: Uuid, list_id: Uuid, profile_ids: &[Uuid]) -> AppResult<u64> {
    let added: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO list_members (list_id, profile_id)
         SELECT $1, p.id FROM profiles p WHERE p.account_id = $2 AND p.id = ANY($3)
           AND EXISTS (SELECT 1 FROM lists l WHERE l.id = $1 AND l.account_id = $2)
         ON CONFLICT DO NOTHING RETURNING profile_id",
    )
    .bind(list_id)
    .bind(account_id)
    .bind(profile_ids)
    .fetch_all(db)
    .await?;
    if !added.is_empty() {
        crate::jobs::enqueue(db, "flow_list", json!({"list_id": list_id, "profile_ids": added}), None).await?;
    }
    Ok(added.len() as u64)
}

pub async fn list_belongs(db: &PgPool, account_id: Uuid, list_id: Uuid) -> AppResult<()> {
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1 AND account_id = $2)")
        .bind(list_id)
        .bind(account_id)
        .fetch_one(db)
        .await?;
    if ok { Ok(()) } else { Err(AppError::not_found("list")) }
}

pub struct ImportResult {
    pub created: u64,
    pub updated: u64,
    pub skipped: u64,
    pub errors: Vec<String>,
}

/// Import a CSV with a header row. Recognizes email, phone, first/last name and external id columns
/// under common spellings; every other column becomes a custom property.
pub async fn import_csv(
    db: &PgPool,
    account_id: Uuid,
    csv_text: &str,
    list_id: Option<Uuid>,
    email_consent: Option<String>,
    sms_consent: Option<String>,
    skip_risky: bool,
) -> AppResult<ImportResult> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(csv_text.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| AppError::bad(format!("could not read CSV header: {e}")))?
        .iter()
        .map(|h| h.trim_start_matches('\u{feff}').to_string())
        .collect();
    let role = |h: &str| -> Option<&'static str> {
        let k: String = h.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
        match k.as_str() {
            "email" | "emailaddress" | "email1" | "mail" => Some("email"),
            "phone" | "phonenumber" | "mobile" | "mobilephone" | "cell" | "sms" => Some("phone_number"),
            "firstname" | "first" | "givenname" | "fname" => Some("first_name"),
            "lastname" | "last" | "surname" | "familyname" | "lname" => Some("last_name"),
            "externalid" | "customerid" | "id" => Some("external_id"),
            _ => None,
        }
    };
    let roles: Vec<Option<&str>> = headers.iter().map(|h| role(h)).collect();
    let mut out = ImportResult { created: 0, updated: 0, skipped: 0, errors: Vec::new() };
    let mut ids = Vec::new();
    let (mut roles_skipped, mut disposable_skipped) = (0u64, 0u64);
    for (i, rec) in rdr.records().enumerate() {
        let line = i + 2;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                out.skipped += 1;
                out.errors.push(format!("line {line}: {e}"));
                continue;
            }
        };
        if i >= 200_000 {
            out.errors.push("stopped after 200,000 rows; split the file".into());
            break;
        }
        let mut input = ProfileInput { email_consent: email_consent.clone(), sms_consent: sms_consent.clone(), ..Default::default() };
        let mut props = Map::new();
        for (j, val) in rec.iter().enumerate() {
            if val.is_empty() {
                continue;
            }
            match roles.get(j).copied().flatten() {
                Some("email") => input.email = Some(val.to_string()),
                Some("phone_number") => input.phone_number = Some(val.to_string()),
                Some("first_name") => input.first_name = Some(val.to_string()),
                Some("last_name") => input.last_name = Some(val.to_string()),
                Some("external_id") => input.external_id = Some(val.to_string()),
                _ => {
                    if let Some(h) = headers.get(j) {
                        let v = match val {
                            "true" | "TRUE" => json!(true),
                            "false" | "FALSE" => json!(false),
                            _ => val.parse::<f64>().ok().filter(|_| !val.starts_with('0') || val == "0" || val.contains('.')).map(|n| json!(n)).unwrap_or(json!(val)),
                        };
                        props.insert(h.clone(), v);
                    }
                }
            }
        }
        if skip_risky {
            let risk = input.email.as_deref().and_then(crate::models::normalize_email).and_then(|e| crate::deliverability::risky_address(&e));
            match risk {
                Some("role") => roles_skipped += 1,
                Some(_) => disposable_skipped += 1,
                None => {}
            }
            if risk.is_some() {
                out.skipped += 1;
                continue;
            }
        }
        if input.sms_consent.as_deref() == Some("subscribed") && input.phone_number.is_none() {
            input.sms_consent = None;
        }
        input.properties = Some(props);
        match upsert_profile(db, account_id, &input).await {
            Ok(u) => {
                if u.created { out.created += 1 } else { out.updated += 1 }
                ids.push(u.profile.id);
            }
            Err(e) => {
                out.skipped += 1;
                if out.errors.len() < 50 {
                    out.errors.push(format!("line {line}: {}", e.public_message()));
                }
            }
        }
    }
    if roles_skipped > 0 {
        let what = if roles_skipped == 1 { "shared inbox address" } else { "shared inbox addresses" };
        out.errors.insert(0, format!("Skipped {roles_skipped} {what} like info@ or sales@. Nobody in particular signed up with them, and they bounce and report spam more often."));
    }
    if disposable_skipped > 0 {
        let what = if disposable_skipped == 1 { "throwaway address" } else { "throwaway addresses" };
        out.errors.insert(0, format!("Skipped {disposable_skipped} {what} (like mailinator.com). They stop working within hours and bounce."));
    }
    if let Some(list_id) = list_id {
        for chunk in ids.chunks(5000) {
            add_to_list(db, account_id, list_id, chunk).await?;
        }
    }
    Ok(out)
}

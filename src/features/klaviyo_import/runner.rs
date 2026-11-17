//! The import itself: a background job that works through the steps one Klaviyo page at a time,
//! saving its place after every page, so it can be paused, survives a restart and never holds more than
//! a page in memory. Everything upserts on Klaviyo's ids, so running it again only adds what's new.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::client::{next_link, q, Client, KlaviyoError};
use super::consent;
use super::store::{self, Import, JOB_RUN, STEPS, SYNC_STEPS};
use crate::state::AppState;

/// How long one job run works before handing over to the next (jobs are unlocked after 15 minutes).
const RUN_BUDGET: Duration = Duration::from_secs(50);
const PROFILE_PAGE: u32 = 100;
const EVENT_PAGE: u32 = 500;
/// The most event properties kept for one event, as JSON text.
const MAX_EVENT_PROPS: usize = 64 * 1024;
const MAX_PROFILE_PROPS: usize = 64 * 1024;
/// Most lists and segments whose Klaviyo size the report fetches (each costs one slow call).
const MAX_COMPARED_GROUPS: usize = 500;

/// Metrics imported when the customer doesn't choose: shopping activity and email/SMS engagement.
pub const DEFAULT_METRICS: &[&str] = &[
    "Placed Order", "Ordered Product", "Started Checkout", "Checkout Started", "Viewed Product", "Added to Cart", "Active on Site",
    "Fulfilled Order", "Cancelled Order", "Refunded Order", "Received Email", "Opened Email", "Clicked Email", "Bounced Email",
    "Marked Email as Spam", "Unsubscribed", "Received SMS", "Clicked SMS", "Unsubscribed from SMS", "Subscribed to List",
];
/// Metrics that mean a person's consent changed; during the ongoing sync their profile is re-read at once.
const CONSENT_METRICS: &[&str] = &["Unsubscribed", "Unsubscribed from SMS", "Marked Email as Spam", "Bounced Email", "Unsubscribed from List", "Subscribed to List", "Subscribed to SMS Marketing", "Subscribed to Email Marketing"];
/// Metrics whose values are revenue in the report.
pub const ORDER_METRICS: &[&str] = &["Placed Order"];

pub struct Run<'a> {
    pub state: &'a AppState,
    pub imp: Import,
    pub k: Client,
    deadline: Instant,
    metrics: Option<HashMap<String, (String, Option<Uuid>)>>,
}

fn s(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|x| !x.is_empty())
}

fn ts(v: &Value) -> Option<DateTime<Utc>> {
    s(v).and_then(|t| DateTime::parse_from_rfc3339(t).ok().map(|d| d.with_timezone(&Utc)).or_else(|| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|n| n.and_utc())))
}

fn included(page: &Value) -> HashMap<(String, String), Value> {
    page["included"].as_array().into_iter().flatten().filter_map(|x| Some(((x["type"].as_str()?.to_string(), x["id"].as_str()?.to_string()), x.clone()))).collect()
}

fn rel_ids(res: &Value, rel: &str) -> Vec<String> {
    match &res["relationships"][rel]["data"] {
        Value::Array(a) => a.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect(),
        Value::Object(o) => o.get("id").and_then(|i| i.as_str()).map(|i| vec![i.to_string()]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Klaviyo stores free-text country names; DumbMail rules use ISO codes for the common ones.
fn country_code(c: &str) -> Option<&'static str> {
    const NAMES: &[(&str, &str)] = &[
        ("united states", "US"), ("united states of america", "US"), ("usa", "US"), ("us", "US"), ("canada", "CA"), ("united kingdom", "GB"), ("uk", "GB"),
        ("great britain", "GB"), ("england", "GB"), ("australia", "AU"), ("new zealand", "NZ"), ("ireland", "IE"), ("germany", "DE"), ("france", "FR"),
        ("spain", "ES"), ("italy", "IT"), ("netherlands", "NL"), ("belgium", "BE"), ("switzerland", "CH"), ("austria", "AT"), ("sweden", "SE"),
        ("norway", "NO"), ("denmark", "DK"), ("finland", "FI"), ("portugal", "PT"), ("poland", "PL"), ("mexico", "MX"), ("brazil", "BR"),
        ("argentina", "AR"), ("japan", "JP"), ("china", "CN"), ("india", "IN"), ("singapore", "SG"), ("south africa", "ZA"), ("israel", "IL"),
        ("united arab emirates", "AE"), ("south korea", "KR"), ("hong kong", "HK"), ("philippines", "PH"), ("czech republic", "CZ"), ("greece", "GR"),
    ];
    let l = c.trim().to_lowercase();
    if l.len() == 2 && l.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return NAMES.iter().map(|(_, code)| *code).find(|code| code.eq_ignore_ascii_case(&l));
    }
    NAMES.iter().find(|(n, _)| *n == l).map(|(_, code)| *code)
}

impl<'a> Run<'a> {
    pub fn new(state: &'a AppState, imp: Import, key: &str) -> Self {
        Run { state, imp, k: Client::new(state.http.clone(), key), deadline: Instant::now() + RUN_BUDGET, metrics: None }
    }

    fn out_of_time(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn acct(&self) -> Uuid {
        self.imp.account_id
    }

    fn st(&self, k: &str) -> &Value {
        &self.imp.step_state[k]
    }

    fn st_i(&self, k: &str) -> usize {
        self.imp.step_state[k].as_u64().unwrap_or(0) as usize
    }

    fn set_st(&mut self, k: &str, v: Value) {
        if !self.imp.step_state.is_object() {
            self.imp.step_state = json!({});
        }
        self.imp.step_state[k] = v;
    }

    async fn save(&self) -> anyhow::Result<()> {
        sqlx::query("UPDATE klaviyo_imports SET phase = $2, cursor = $3, step_state = $4, updated_at = now() WHERE id = $1")
            .bind(self.imp.id)
            .bind(&self.imp.phase)
            .bind(&self.imp.cursor)
            .bind(&self.imp.step_state)
            .execute(&self.state.db)
            .await?;
        Ok(())
    }

    async fn issue(&self, step: &str, kind: &str, item: &str, kid: Option<&str>, local: Option<Uuid>, msg: &str) -> anyhow::Result<()> {
        store::issue(&self.state.db, &self.imp, step, kind, item, kid, local, msg).await?;
        Ok(())
    }

    fn syncing(&self) -> bool {
        self.imp.status == "syncing"
    }

    fn sync_since(&self) -> String {
        let t = self.imp.last_sync_at.or(self.imp.started_at).unwrap_or(self.imp.created_at) - chrono::Duration::minutes(10);
        fmt_ts(t)
    }

    // ---------- metrics ----------

    async fn metric_map(&mut self) -> anyhow::Result<&HashMap<String, (String, Option<Uuid>)>> {
        if self.metrics.is_none() {
            let rows: Vec<(String, Option<String>, Option<Uuid>)> = sqlx::query_as("SELECT klaviyo_id, name, local_id FROM klaviyo_refs WHERE account_id = $1 AND kind = 'metric'")
                .bind(self.acct())
                .fetch_all(&self.state.db)
                .await?;
            self.metrics = Some(rows.into_iter().map(|(k, n, l)| (k, (n.unwrap_or_default(), l))).collect());
        }
        Ok(self.metrics.as_ref().unwrap())
    }

    fn metric_selected(&self, name: &str, kid: &str) -> bool {
        match self.imp.options["metrics"].as_array() {
            Some(a) if !a.is_empty() => a.iter().any(|m| m.as_str().is_some_and(|m| m == kid || m.eq_ignore_ascii_case(name))),
            _ => DEFAULT_METRICS.iter().any(|d| d.eq_ignore_ascii_case(name)),
        }
    }

    async fn step_metrics(&mut self) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/metrics".into());
        let page = self.k.get(&url).await?;
        let events_on = self.imp.wants("events");
        let mut n = 0;
        for m in page["data"].as_array().into_iter().flatten() {
            let (Some(kid), Some(name)) = (m["id"].as_str(), s(&m["attributes"]["name"])) else { continue };
            let name: String = name.chars().take(128).collect();
            let selected = events_on && self.metric_selected(&name, kid);
            let local = if selected { Some(crate::audience::metric_id(&self.state.db, self.acct(), &name).await.map_err(|e| anyhow::anyhow!(e.public_message()))?) } else { None };
            let integration = m["attributes"]["integration"]["name"].as_str().unwrap_or("");
            store::set_ref(&self.state.db, self.acct(), "metric", kid, local, &name, &json!({"selected": selected, "integration": integration})).await?;
            n += 1;
        }
        store::bump(&self.state.db, self.imp.id, "metrics", n, 0).await?;
        self.metrics = None;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- profiles and consent ----------

    /// Create or update one DumbMail profile from a Klaviyo profile resource (with `subscriptions`).
    pub async fn upsert_profile(&self, p: &Value) -> anyhow::Result<Option<Uuid>> {
        let Some(kid) = p["id"].as_str() else { return Ok(None) };
        let a = &p["attributes"];
        let raw_email = s(&a["email"]);
        let email = raw_email.and_then(crate::models::normalize_email);
        let phone = s(&a["phone_number"]).and_then(crate::models::normalize_phone);
        let ext: Option<String> = s(&a["external_id"]).map(|x| x.chars().take(200).collect());
        let subs = &a["subscriptions"];
        let ke = consent::read_email(subs);
        let ks = consent::read_sms(subs);
        let list_sup = consent::list_suppressions(subs);
        let detail = json!({
            "email": ke.as_ref().map(|e| json!({"consent": e.consent, "can_receive": e.can_receive, "suppressed": e.suppressions.iter().any(|r| r != "UNSUBSCRIBE")})),
            "sms": ks.as_ref().map(|k| json!({"consent": k.consent, "can_receive": k.can_receive})),
            "has_email": email.is_some(), "has_phone": phone.is_some(),
            "list_suppressions": list_sup,
        });
        let label = raw_email.or(s(&a["phone_number"])).unwrap_or(kid).to_string();
        if email.is_none() && phone.is_none() && ext.is_none() {
            let why = match raw_email {
                Some(e) => format!("Its email address {e:?} isn't valid and it has no phone number or external id."),
                None => "It has no email address, phone number or external id (often an anonymous website visitor).".to_string(),
            };
            let mut d = detail.clone();
            d["skipped"] = json!(why);
            store::set_ref(&self.state.db, self.acct(), "profile", kid, None, &label, &d).await?;
            self.issue("profiles", "skipped", &label, Some(kid), None, &why).await?;
            return Ok(None);
        }
        if raw_email.is_some() && email.is_none() {
            self.issue("profiles", "skipped", &label, Some(kid), None, "Its email address isn't valid, so it was left off; the rest of the profile came across.").await?;
        }
        // Properties: Klaviyo's custom properties, then its standard fields and location.
        let mut props: Map<String, Value> = a["properties"].as_object().cloned().unwrap_or_default();
        if serde_json::to_string(&props).map(|t| t.len()).unwrap_or(0) > MAX_PROFILE_PROPS {
            props.retain(|_, v| !v.is_object() && !v.is_array());
            self.issue("profiles", "skipped", &label, Some(kid), None, "Its custom properties were too large; lists and nested values were left off.").await?;
        }
        for k in ["organization", "title", "locale", "image"] {
            if let Some(v) = s(&a[k]) {
                props.insert(k.into(), json!(v));
            }
        }
        let loc = &a["location"];
        for k in ["address1", "address2", "city", "region", "zip", "timezone", "latitude", "longitude"] {
            if !loc[k].is_null() && loc[k] != "" {
                props.insert(k.into(), loc[k].clone());
            }
        }
        if let Some(c) = s(&loc["country"]) {
            match country_code(c) {
                Some(code) => {
                    props.insert("country".into(), json!(code));
                }
                None => {
                    props.insert("country".into(), json!(c));
                }
            }
        }
        props.insert("klaviyo_id".into(), json!(kid));
        props.insert("klaviyo_consent".into(), consent::provenance(ke.as_ref(), ks.as_ref()));
        let props = Value::Object(props);
        let es = match &ke {
            Some(k) => consent::map_email(k),
            // No consent record: treat it as one Klaviyo wouldn't email.
            None => consent::map_email(&consent::KlaviyoEmail::default()),
        };
        let sms_k = ks.as_ref().map(|k| consent::map_sms(k, phone.is_some())).unwrap_or("never_subscribed");
        let created = ts(&a["created"]);
        let first = s(&a["first_name"]).map(|x| x.chars().take(200).collect::<String>());
        let last = s(&a["last_name"]).map(|x| x.chars().take(200).collect::<String>());
        let db = &self.state.db;
        for attempt in 0..3 {
            let existing: Option<(Uuid, String, String, Option<String>)> = sqlx::query_as(
                "SELECT p.id, p.email_consent, p.sms_consent, p.email_suppressed_reason FROM profiles p
                 WHERE p.account_id = $1 AND (p.id = (SELECT r.local_id FROM klaviyo_refs r WHERE r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = $2)
                    OR p.external_id = $3 OR p.email = $4 OR p.phone_number = $5)
                 ORDER BY (p.id = (SELECT r.local_id FROM klaviyo_refs r WHERE r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = $2)) DESC NULLS LAST,
                          (p.external_id = $3) DESC NULLS LAST, (p.email = $4) DESC NULLS LAST
                 LIMIT 1",
            )
            .bind(self.acct())
            .bind(kid)
            .bind(&ext)
            .bind(&email)
            .bind(&phone)
            .fetch_optional(db)
            .await?;
            // After an identity clash, keep the profile's own identifiers and update the rest.
            let (e_id, p_id, x_id) = if attempt == 0 { (email.clone(), phone.clone(), ext.clone()) } else { (None, None, None) };
            let res: Result<Uuid, sqlx::Error> = match &existing {
                Some((id, ec, sc, sup)) => {
                    let (consent, suppressed) = consent::merge_email(Some(ec), sup.as_deref(), &es);
                    let sms = consent::merge_sms(Some(sc), sms_k);
                    sqlx::query_scalar(
                        "UPDATE profiles p SET email = COALESCE(p.email, $2), phone_number = COALESCE(p.phone_number, $3), external_id = COALESCE(p.external_id, $4),
                            first_name = COALESCE($5, p.first_name), last_name = COALESCE($6, p.last_name), properties = p.properties || $7,
                            email_consent = $8, sms_consent = CASE WHEN $9 = 'subscribed' AND COALESCE(p.phone_number, $3) IS NULL THEN p.sms_consent ELSE $9 END,
                            email_suppressed_reason = $10, updated_at = now()
                         WHERE p.id = $1 RETURNING p.id",
                    )
                    .bind(id)
                    .bind(&e_id)
                    .bind(&p_id)
                    .bind(&x_id)
                    .bind(&first)
                    .bind(&last)
                    .bind(&props)
                    .bind(&consent)
                    .bind(&sms)
                    .bind(&suppressed)
                    .fetch_one(db)
                    .await
                }
                None => {
                    if attempt == 2 {
                        break;
                    }
                    sqlx::query_scalar(
                        "INSERT INTO profiles (id, account_id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, created_at)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, COALESCE($12, now())) RETURNING id",
                    )
                    .bind(Uuid::now_v7())
                    .bind(self.acct())
                    .bind(&email)
                    .bind(&phone)
                    .bind(&ext)
                    .bind(&first)
                    .bind(&last)
                    .bind(&props)
                    .bind(es.consent)
                    .bind(sms_k)
                    .bind(es.suppressed)
                    .bind(created)
                    .fetch_one(db)
                    .await
                }
            };
            match res {
                Ok(id) => {
                    store::set_ref(db, self.acct(), "profile", kid, Some(id), &label, &detail).await?;
                    return Ok(Some(id));
                }
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let why = "Its email, phone number or external id belongs to another profile that couldn't be merged with it.";
        store::set_ref(db, self.acct(), "profile", kid, None, &label, &json!({"skipped": why})).await?;
        self.issue("profiles", "skipped", &label, Some(kid), None, why).await?;
        Ok(None)
    }

    /// DumbMail ids for a page of Klaviyo profile resources, importing any not seen yet.
    async fn resolve_profiles(&self, resources: &[Value], refresh: bool) -> anyhow::Result<HashMap<String, Uuid>> {
        let ids: Vec<String> = resources.iter().filter_map(|r| r["id"].as_str().map(String::from)).collect();
        let mut out: HashMap<String, Uuid> = if refresh {
            HashMap::new()
        } else {
            let rows: Vec<(String, Uuid)> = sqlx::query_as(
                "SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN profiles p ON p.id = r.local_id
                 WHERE r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = ANY($2)",
            )
            .bind(self.acct())
            .bind(&ids)
            .fetch_all(&self.state.db)
            .await?;
            rows.into_iter().collect()
        };
        for r in resources {
            let Some(kid) = r["id"].as_str() else { continue };
            if !out.contains_key(kid) {
                if let Some(id) = self.upsert_profile(r).await? {
                    out.insert(kid.to_string(), id);
                }
            }
        }
        Ok(out)
    }

    async fn step_profiles(&mut self, step: &str) -> anyhow::Result<bool> {
        let url = match self.imp.cursor.clone() {
            Some(c) => c,
            None => {
                let mut u = format!("/api/profiles?additional-fields%5Bprofile%5D=subscriptions&page%5Bsize%5D={PROFILE_PAGE}&sort=id");
                if self.syncing() {
                    u.push_str(&format!("&filter={}", q(&format!("greater-than(updated,{})", self.sync_since()))));
                }
                u
            }
        };
        let page = self.k.get(&url).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let mut done = 0;
        let mut skipped = 0;
        for p in &data {
            match self.upsert_profile(p).await? {
                Some(_) => done += 1,
                None => skipped += 1,
            }
        }
        store::bump(&self.state.db, self.imp.id, step, done, skipped).await?;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- lists and membership ----------

    async fn step_lists(&mut self) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/lists?page%5Bsize%5D=10".into());
        let page = self.k.get(&url).await?;
        let mut n = 0;
        for l in page["data"].as_array().into_iter().flatten() {
            let Some(kid) = l["id"].as_str() else { continue };
            let name: String = s(&l["attributes"]["name"]).unwrap_or("Klaviyo list").chars().take(200).collect();
            let double = l["attributes"]["opt_in_process"] == "double_opt_in";
            let exists: Option<Uuid> = sqlx::query_scalar("SELECT l.id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' AND r.klaviyo_id = $2")
                .bind(self.acct())
                .bind(kid)
                .fetch_optional(&self.state.db)
                .await?;
            let id = match exists {
                Some(id) => id,
                None => {
                    let id = Uuid::now_v7();
                    sqlx::query("INSERT INTO lists (id, account_id, name, double_opt_in) VALUES ($1, $2, $3, $4)").bind(id).bind(self.acct()).bind(&name).bind(double).execute(&self.state.db).await?;
                    id
                }
            };
            store::set_ref(&self.state.db, self.acct(), "list", kid, Some(id), &name, &json!({"double_opt_in": double})).await?;
            n += 1;
        }
        store::bump(&self.state.db, self.imp.id, "lists", n, 0).await?;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    /// Copy members of Klaviyo lists (or snapshot segments) into their DumbMail lists, one page at a time.
    /// Members are added quietly: no list-triggered flow starts for them.
    async fn step_members(&mut self, step: &str, segments: bool) -> anyhow::Result<bool> {
        let groups: Vec<(String, Uuid)> = if segments {
            sqlx::query_as("SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'segment' AND r.detail->>'mode' = 'snapshot' ORDER BY r.klaviyo_id")
        } else {
            sqlx::query_as("SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' ORDER BY r.klaviyo_id")
        }
        .bind(self.acct())
        .fetch_all(&self.state.db)
        .await?;
        let i = self.st_i("group");
        let Some((kid, list_id)) = groups.get(i).cloned() else {
            self.set_st("group", json!(0));
            return Ok(true);
        };
        let url = match self.imp.cursor.clone() {
            Some(c) => c,
            None => {
                let mut u = format!("/api/{}/{kid}/profiles?additional-fields%5Bprofile%5D=subscriptions&page%5Bsize%5D={PROFILE_PAGE}", if segments { "segments" } else { "lists" });
                if self.syncing() {
                    u.push_str(&format!("&filter={}", q(&format!("greater-than(joined_group_at,{})", self.sync_since()))));
                }
                u
            }
        };
        let page = match self.k.get(&url).await {
            Ok(p) => p,
            Err(KlaviyoError::Status { status: 404, .. }) => {
                // Deleted in Klaviyo since; nothing more to copy.
                self.imp.cursor = None;
                self.set_st("group", json!(i + 1));
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let ids = self.resolve_profiles(&data, false).await?;
        let (mut pids, mut times) = (Vec::new(), Vec::new());
        let mut skipped = 0;
        for m in &data {
            let Some(mid) = m["id"].as_str() else { continue };
            let Some(pid) = ids.get(mid) else {
                skipped += 1;
                continue;
            };
            if !segments && consent::list_suppressions(&m["attributes"]["subscriptions"]).iter().any(|l| l == &kid) {
                skipped += 1;
                continue;
            }
            pids.push(*pid);
            times.push(ts(&m["attributes"]["joined_group_at"]).unwrap_or_else(Utc::now));
        }
        sqlx::query(
            "INSERT INTO list_members (list_id, profile_id, added_at)
             SELECT $1, x.p, x.t FROM unnest($2::uuid[], $3::timestamptz[]) AS x(p, t) JOIN profiles pr ON pr.id = x.p AND pr.account_id = $4
             ON CONFLICT DO NOTHING",
        )
        .bind(list_id)
        .bind(&pids)
        .bind(&times)
        .bind(self.acct())
        .execute(&self.state.db)
        .await?;
        store::bump(&self.state.db, self.imp.id, step, pids.len() as i64, skipped).await?;
        self.imp.cursor = next_link(&page);
        if self.imp.cursor.is_none() {
            self.set_st("group", json!(i + 1));
        }
        Ok(false)
    }

    // ---------- segments ----------

    async fn lists_by_klaviyo_id(&self) -> anyhow::Result<HashMap<String, Uuid>> {
        // Klaviyo list ids, plus snapshot segments (now DumbMail lists).
        let rows: Vec<(String, Uuid)> = sqlx::query_as(
            "SELECT klaviyo_id, local_id FROM klaviyo_refs WHERE account_id = $1 AND local_id IS NOT NULL AND (kind = 'list' OR (kind = 'segment' AND detail->>'mode' = 'snapshot'))",
        )
        .bind(self.acct())
        .fetch_all(&self.state.db)
        .await?;
        Ok(rows.into_iter().collect())
    }

    async fn segment_ctx_maps(&mut self) -> anyhow::Result<(HashMap<String, String>, HashMap<String, String>, HashMap<String, Uuid>)> {
        let events_on = self.imp.wants("events");
        let m = self.metric_map().await?;
        let imported: HashMap<String, String> = m.iter().filter(|(_, (_, l))| l.is_some() && events_on).map(|(k, (n, _))| (k.clone(), n.clone())).collect();
        let all: HashMap<String, String> = m.iter().map(|(k, (n, _))| (k.clone(), n.clone())).collect();
        let lists = self.lists_by_klaviyo_id().await?;
        Ok((imported, all, lists))
    }

    async fn step_segments(&mut self) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/segments?page%5Bsize%5D=10".into());
        let page = self.k.get(&url).await?;
        let (imported, all, lists) = self.segment_ctx_maps().await?;
        let ctx = super::segments::Ctx { metrics: &imported, all_metrics: &all, lists: &lists, events_since: self.imp.events_since(), now: Utc::now() };
        let mut n = 0;
        for sg in page["data"].as_array().into_iter().flatten() {
            let Some(kid) = sg["id"].as_str() else { continue };
            if store::ref_local(&self.state.db, self.acct(), "segment", kid).await?.is_some_and(|(l, _)| l.is_some()) {
                continue;
            }
            let name: String = s(&sg["attributes"]["name"]).unwrap_or("Klaviyo segment").chars().take(180).collect();
            match super::segments::translate(&ctx, &sg["attributes"]["definition"]) {
                Ok(def) => {
                    let id = Uuid::now_v7();
                    sqlx::query("INSERT INTO segments (id, account_id, name, definition) VALUES ($1, $2, $3, $4)").bind(id).bind(self.acct()).bind(&name).bind(&def).execute(&self.state.db).await?;
                    store::set_ref(&self.state.db, self.acct(), "segment", kid, Some(id), &name, &json!({"mode": "rules"})).await?;
                }
                Err(reasons) => {
                    let id = Uuid::now_v7();
                    let list_name = format!("{name} (Klaviyo segment snapshot, {})", Utc::now().format("%b %-d, %Y"));
                    sqlx::query("INSERT INTO lists (id, account_id, name, description) VALUES ($1, $2, $3, $4)")
                        .bind(id)
                        .bind(self.acct())
                        .bind(&list_name)
                        .bind("Members of a Klaviyo segment on the day it was imported. It doesn't change by itself.")
                        .execute(&self.state.db)
                        .await?;
                    store::set_ref(&self.state.db, self.acct(), "segment", kid, Some(id), &name, &json!({"mode": "snapshot", "reasons": reasons})).await?;
                    let msg = format!("Imported as a list of its current members, because DumbMail can't express {}. Rebuild it as a segment if it needs to keep updating.", reasons.join("; "));
                    self.issue("segments", "snapshot", &name, Some(kid), Some(id), &msg).await?;
                }
            }
            n += 1;
        }
        store::bump(&self.state.db, self.imp.id, "segments", n, 0).await?;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- templates ----------

    /// Converted email content for a Klaviyo template: from the imported template, or fetched directly
    /// (campaign and flow emails use their own copies).
    async fn template_content(&mut self, kid: &str, step: &str, item: &str) -> anyhow::Result<Option<Value>> {
        let imported: Option<Value> = sqlx::query_scalar("SELECT t.content FROM klaviyo_refs r JOIN templates t ON t.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'template' AND r.klaviyo_id = $2")
            .bind(self.acct())
            .bind(kid)
            .fetch_optional(&self.state.db)
            .await?;
        if let Some(c) = imported {
            return Ok(Some(c));
        }
        let t = match self.k.get(&format!("/api/templates/{kid}?fields%5Btemplate%5D=name,html")).await {
            Ok(t) => t,
            Err(KlaviyoError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let html = t["data"]["attributes"]["html"].as_str().unwrap_or("");
        let (content, removed) = super::templates::email_content(html, "", None, None, None, None);
        if !removed.is_empty() {
            self.issue(step, "unmapped", item, Some(kid), None, &format!("Removed template tags DumbMail doesn't have: {}", removed.join(", "))).await?;
        }
        Ok(Some(content))
    }

    async fn step_templates(&mut self) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/templates?page%5Bsize%5D=10&fields%5Btemplate%5D=name,editor_type,html,created,updated".into());
        let page = self.k.get(&url).await?;
        let mut n = 0;
        for t in page["data"].as_array().into_iter().flatten() {
            let Some(kid) = t["id"].as_str() else { continue };
            if store::ref_local(&self.state.db, self.acct(), "template", kid).await?.is_some_and(|(l, _)| l.is_some()) {
                continue;
            }
            let name: String = s(&t["attributes"]["name"]).unwrap_or("Klaviyo template").chars().take(200).collect();
            let html = t["attributes"]["html"].as_str().unwrap_or("");
            let (content, removed) = super::templates::email_content(html, "", None, None, None, None);
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO templates (id, account_id, name, channel, content) VALUES ($1, $2, $3, 'email', $4)").bind(id).bind(self.acct()).bind(&name).bind(&content).execute(&self.state.db).await?;
            store::set_ref(&self.state.db, self.acct(), "template", kid, Some(id), &name, &json!({"removed_tags": removed})).await?;
            if !removed.is_empty() {
                self.issue("templates", "unmapped", &name, Some(kid), Some(id), &format!("Removed template tags DumbMail doesn't have: {}", removed.join(", "))).await?;
            }
            n += 1;
        }
        store::bump(&self.state.db, self.imp.id, "templates", n, 0).await?;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- campaigns ----------

    /// Klaviyo list/segment id → a DumbMail audience entry.
    async fn audience_map(&self) -> anyhow::Result<HashMap<String, (Value, String)>> {
        let rows: Vec<(String, String, Uuid, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT klaviyo_id, kind, local_id, detail->>'mode', name FROM klaviyo_refs WHERE account_id = $1 AND kind IN ('list', 'segment') AND local_id IS NOT NULL",
        )
        .bind(self.acct())
        .fetch_all(&self.state.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(k, kind, l, mode, name)| {
                let t = if kind == "segment" && mode.as_deref() == Some("rules") { "segment" } else { "list" };
                (k, (json!({"type": t, "id": l}), name.unwrap_or_default()))
            })
            .collect())
    }

    async fn step_campaigns(&mut self) -> anyhow::Result<bool> {
        // The cursor stays on the current page until every campaign on it is in; done ones are skipped.
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/campaigns?include=campaign-messages,campaign-audiences&page%5Bsize%5D=100&sort=created_at".into());
        let page = self.k.get(&url).await?;
        let inc = included(&page);
        let audiences = self.audience_map().await?;
        for c in page["data"].as_array().into_iter().flatten() {
            let Some(cid) = c["id"].as_str() else { continue };
            let def = &c["attributes"]["definition"];
            let cname = s(&def["name"]).unwrap_or("Klaviyo campaign").to_string();
            let mids = rel_ids(c, "campaign-messages");
            for (mi, mid) in mids.iter().enumerate() {
                if store::ref_local(&self.state.db, self.acct(), "campaign_message", mid).await?.is_some() {
                    continue;
                }
                if self.out_of_time() {
                    return Ok(false);
                }
                let msg_page = match self.k.get(&format!("/api/campaign-messages/{mid}?include=campaign-variations")).await {
                    Ok(p) => p,
                    Err(KlaviyoError::Status { status: 404, .. }) => continue,
                    Err(e) => return Err(e.into()),
                };
                let msg = &msg_page["data"];
                let mdef = &msg["attributes"]["definition"];
                let vinc = included(&msg_page);
                let variation = rel_ids(msg, "campaign-variations").first().and_then(|v| vinc.get(&("campaign-variation".to_string(), v.clone()))).cloned().unwrap_or(Value::Null);
                let details = &variation["attributes"]["definition"]["details"];
                let channel = details["channel"].as_str().unwrap_or("email");
                let name: String = if mids.len() > 1 { format!("{cname} – {}", s(&mdef["name"]).unwrap_or(&format!("message {}", mi + 1))) } else { cname.clone() }.chars().take(200).collect();
                let status_k = mdef["status"].as_str().unwrap_or("draft");
                let sent_at = ts(&mdef["send_options"]["send_time"]).or_else(|| ts(&msg["attributes"]["updated"]));
                let (status, sent_at) = if matches!(status_k, "sent" | "sending") { ("sent", sent_at) } else { ("draft", None) };
                if status_k == "scheduled" {
                    self.issue("campaigns", "note", &name, Some(mid), None, "It was scheduled in Klaviyo. It came across as a draft so it can't go out twice; schedule it in one place only.").await?;
                }
                // Audience.
                let aud_res = rel_ids(msg, "campaign-audience").first().and_then(|a| inc.get(&("campaign-audience".to_string(), a.clone()))).cloned().or_else(|| {
                    rel_ids(c, "campaign-audiences").first().and_then(|a| inc.get(&("campaign-audience".to_string(), a.clone()))).cloned()
                });
                let adef = aud_res.as_ref().map(|a| a["attributes"]["definition"].clone()).unwrap_or(Value::Null);
                let mut aud = json!({"include": [], "exclude": []});
                let mut names = json!({"included": [], "excluded": []});
                for (k, nk) in [("included", "include"), ("excluded", "exclude")] {
                    for gid in adef[k].as_array().into_iter().flatten().filter_map(|g| g.as_str()) {
                        match audiences.get(gid) {
                            Some((entry, n)) => {
                                aud[nk].as_array_mut().unwrap().push(entry.clone());
                                names[k].as_array_mut().unwrap().push(json!(n));
                            }
                            None => {
                                names[k].as_array_mut().unwrap().push(json!(gid));
                                if status == "draft" {
                                    self.issue("campaigns", "unmapped", &name, Some(mid), None, &format!("Its audience used the list or segment {gid}, which wasn't imported. Pick the audience again before sending.")).await?;
                                }
                            }
                        }
                    }
                }
                let content = match channel {
                    "email" => {
                        let mut content = match s(&details["template_id"]) {
                            Some(t) => self.template_content(t, "campaigns", &name).await?.unwrap_or_else(|| json!({"blocks": []})),
                            None => json!({"blocks": []}),
                        };
                        let conv = |x: &str| super::templates::convert(x).text;
                        content["subject"] = json!(conv(details["subject"].as_str().unwrap_or("")));
                        if let Some(p) = s(&details["preview_text"]) {
                            content["preheader"] = json!(conv(p));
                        }
                        for (src, dst) in [("from_label", "from_name"), ("from_email", "from_email"), ("reply_to_email", "reply_to")] {
                            if let Some(v) = s(&details[src]) {
                                content[dst] = json!(v);
                            }
                        }
                        content
                    }
                    "sms" => {
                        let c = super::templates::convert(details["body"].as_str().unwrap_or(""));
                        if !c.removed.is_empty() {
                            self.issue("campaigns", "unmapped", &name, Some(mid), None, &format!("Removed template tags DumbMail doesn't have: {}", c.removed.join(", "))).await?;
                        }
                        json!({"body": c.text})
                    }
                    other => {
                        self.issue("campaigns", "skipped", &name, Some(mid), None, &format!("A {other} campaign; DumbMail sends email and SMS only.")).await?;
                        store::set_ref(&self.state.db, self.acct(), "campaign_message", mid, None, &name, &json!({"skipped": other})).await?;
                        continue;
                    }
                };
                let id = Uuid::now_v7();
                let created = ts(&c["attributes"]["created_at"]).unwrap_or_else(Utc::now);
                sqlx::query("INSERT INTO campaigns (id, account_id, name, channel, status, audience, content, sent_at, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
                    .bind(id)
                    .bind(self.acct())
                    .bind(&name)
                    .bind(channel)
                    .bind(status)
                    .bind(&aud)
                    .bind(&content)
                    .bind(sent_at)
                    .bind(created)
                    .execute(&self.state.db)
                    .await?;
                let detail = json!({"campaign_id": cid, "channel": channel, "status": status_k, "sent_at": sent_at, "subject": details["subject"], "audience": names});
                store::set_ref(&self.state.db, self.acct(), "campaign_message", mid, Some(id), &name, &detail).await?;
                if mi == 0 {
                    store::set_ref(&self.state.db, self.acct(), "campaign", cid, Some(id), &cname, &json!({})).await?;
                }
                store::bump(&self.state.db, self.imp.id, "campaigns", 1, 0).await?;
            }
        }
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    /// Headline numbers for past campaigns, from Klaviyo's reporting API (one call per year of history).
    async fn step_campaign_stats(&mut self) -> anyhow::Result<bool> {
        let earliest: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(c.sent_at) FROM klaviyo_refs r JOIN campaigns c ON c.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'campaign_message' AND c.sent_at IS NOT NULL",
        )
        .bind(self.acct())
        .fetch_one(&self.state.db)
        .await?;
        let Some(earliest) = earliest else { return Ok(true) };
        let w = self.st_i("window");
        let now = Utc::now();
        let end = now - chrono::Duration::days(365 * w as i64);
        let start = end - chrono::Duration::days(365);
        if w >= 5 || end < earliest {
            return Ok(true);
        }
        let conversion: Option<String> = {
            let m = self.metric_map().await?;
            m.iter().find(|(_, (n, _))| ORDER_METRICS.contains(&n.as_str())).or_else(|| m.iter().next()).map(|(k, _)| k.clone())
        };
        let Some(conversion) = conversion else { return Ok(true) };
        let body = json!({"data": {"type": "campaign-values-report", "attributes": {
            "statistics": ["recipients", "delivered", "opens_unique", "clicks_unique", "unsubscribes", "bounced", "spam_complaints", "conversions", "conversion_value"],
            "timeframe": {"start": fmt_ts(start), "end": fmt_ts(end)},
            "conversion_metric_id": conversion}}});
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/campaign-values-reports".into());
        let page = match self.k.post(&url, &body).await {
            Ok(p) => p,
            Err(KlaviyoError::Status { status, detail }) if status < 500 => {
                self.issue("campaign_stats", "skipped", "Past campaign numbers", None, None, &format!("Klaviyo wouldn't report them ({detail}).")).await?;
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        };
        for r in page["data"]["attributes"]["results"].as_array().into_iter().flatten() {
            let Some(mid) = r["groupings"]["campaign_message_id"].as_str() else { continue };
            sqlx::query("UPDATE klaviyo_refs SET detail = jsonb_set(detail, '{stats}', $3), updated_at = now() WHERE account_id = $1 AND kind = 'campaign_message' AND klaviyo_id = $2")
                .bind(self.acct())
                .bind(mid)
                .bind(&r["statistics"])
                .execute(&self.state.db)
                .await?;
        }
        self.imp.cursor = next_link(&page);
        if self.imp.cursor.is_none() {
            self.set_st("window", json!(w + 1));
        }
        Ok(false)
    }

    // ---------- flows ----------

    async fn step_flows(&mut self) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/flows?page%5Bsize%5D=50&sort=created".into());
        let page = self.k.get(&url).await?;
        for f in page["data"].as_array().into_iter().flatten() {
            let Some(kid) = f["id"].as_str() else { continue };
            if store::ref_local(&self.state.db, self.acct(), "flow", kid).await?.is_some() {
                continue;
            }
            if self.out_of_time() {
                return Ok(false);
            }
            let name: String = s(&f["attributes"]["name"]).unwrap_or("Klaviyo flow").chars().take(180).collect();
            if f["attributes"]["archived"] == true {
                store::set_ref(&self.state.db, self.acct(), "flow", kid, None, &name, &json!({"skipped": "archived"})).await?;
                continue;
            }
            let full = match self.k.get(&format!("/api/flows/{kid}?additional-fields%5Bflow%5D=definition")).await {
                Ok(p) => p,
                Err(KlaviyoError::Status { status: 404, .. }) => continue,
                Err(e) => return Err(e.into()),
            };
            let def = full["data"]["attributes"]["definition"].clone();
            let mut templates = HashMap::new();
            for t in super::flows::template_ids(&def) {
                if let Some(c) = self.template_content(&t, "flows", &name).await? {
                    templates.insert(t, c);
                }
            }
            let (imported, all, lists) = self.segment_ctx_maps().await?;
            let segs: HashMap<String, Uuid> = {
                let rows: Vec<(String, Uuid)> = sqlx::query_as("SELECT klaviyo_id, local_id FROM klaviyo_refs WHERE account_id = $1 AND kind = 'segment' AND detail->>'mode' = 'rules' AND local_id IS NOT NULL")
                    .bind(self.acct())
                    .fetch_all(&self.state.db)
                    .await?;
                rows.into_iter().collect()
            };
            let ctx = super::flows::Ctx {
                metric_names: &all,
                seg: super::segments::Ctx { metrics: &imported, all_metrics: &all, lists: &lists, events_since: self.imp.events_since(), now: Utc::now() },
                segments: &segs,
                templates: &templates,
            };
            let kstatus = f["attributes"]["status"].as_str().unwrap_or("");
            match super::flows::translate(&ctx, &def) {
                Ok(t) => {
                    let id = Uuid::now_v7();
                    let partial = t.partial();
                    let fname = if partial { format!("{name} (partial import)") } else { name.clone() };
                    sqlx::query("INSERT INTO flows (id, account_id, name, status, trigger, definition) VALUES ($1, $2, $3, 'draft', $4, $5)")
                        .bind(id)
                        .bind(self.acct())
                        .bind(&fname)
                        .bind(&t.trigger)
                        .bind(&t.definition)
                        .execute(&self.state.db)
                        .await?;
                    store::set_ref(&self.state.db, self.acct(), "flow", kid, Some(id), &name, &json!({"klaviyo_status": kstatus, "partial": partial})).await?;
                    for i in &t.issues {
                        self.issue("flows", "partial", &name, Some(kid), Some(id), i).await?;
                    }
                    store::bump(&self.state.db, self.imp.id, "flows", 1, 0).await?;
                }
                Err(why) => {
                    store::set_ref(&self.state.db, self.acct(), "flow", kid, None, &name, &json!({"klaviyo_status": kstatus, "skipped": why})).await?;
                    self.issue("flows", "skipped", &name, Some(kid), None, &format!("Not imported: {why}. Rebuild it by hand.")).await?;
                    store::bump(&self.state.db, self.imp.id, "flows", 0, 1).await?;
                }
            }
        }
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- forms ----------

    async fn step_forms(&mut self) -> anyhow::Result<bool> {
        fn list_ids(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(o) => {
                    for (k, x) in o {
                        if k == "list_id" {
                            if let Some(id) = x.as_str().filter(|i| !i.is_empty()) {
                                if !out.iter().any(|o| o == id) {
                                    out.push(id.to_string());
                                }
                            }
                        } else {
                            list_ids(x, out);
                        }
                    }
                }
                Value::Array(a) => a.iter().for_each(|x| list_ids(x, out)),
                _ => {}
            }
        }
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/forms?page%5Bsize%5D=100".into());
        let page = self.k.get(&url).await?;
        let lists = self.lists_by_klaviyo_id().await?;
        for f in page["data"].as_array().into_iter().flatten() {
            let Some(kid) = f["id"].as_str() else { continue };
            if store::ref_local(&self.state.db, self.acct(), "form", kid).await?.is_some() {
                continue;
            }
            if self.out_of_time() {
                return Ok(false);
            }
            let name: String = s(&f["attributes"]["name"]).unwrap_or("Klaviyo form").chars().take(200).collect();
            let full = match self.k.get(&format!("/api/forms/{kid}?fields%5Bform%5D=name,status,definition")).await {
                Ok(p) => p,
                Err(KlaviyoError::Status { status: 404, .. }) => continue,
                Err(e) => return Err(e.into()),
            };
            let mut ids = Vec::new();
            list_ids(&full["data"]["attributes"]["definition"], &mut ids);
            let list = ids.iter().find_map(|i| lists.get(i)).copied();
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO forms (id, account_id, name, status, kind, list_id, config) VALUES ($1, $2, $3, 'draft', 'popup', $4, '{}')")
                .bind(id)
                .bind(self.acct())
                .bind(&name)
                .bind(list)
                .execute(&self.state.db)
                .await?;
            let kstatus = f["attributes"]["status"].as_str().unwrap_or("");
            store::set_ref(&self.state.db, self.acct(), "form", kid, Some(id), &name, &json!({"klaviyo_status": kstatus, "list_ids": ids})).await?;
            if list.is_none() && !ids.is_empty() {
                self.issue("forms", "unmapped", &name, Some(kid), Some(id), "The list it signs people up to wasn't imported. Choose a list before publishing it.").await?;
            }
            store::bump(&self.state.db, self.imp.id, "forms", 1, 0).await?;
        }
        self.imp.cursor = next_link(&page);
        if self.imp.cursor.is_none() {
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM klaviyo_refs WHERE account_id = $1 AND kind = 'form' AND local_id IS NOT NULL").bind(self.acct()).fetch_one(&self.state.db).await?;
            if n > 0 {
                self.issue("forms", "note", "Sign-up forms", None, None, &format!("{n} form(s) came across as drafts with their name and list. Klaviyo doesn't share form designs, so rebuild each one in Sign-up forms, publish it, and replace Klaviyo's script on your site with DumbMail's snippet.")).await?;
            }
        }
        Ok(self.imp.cursor.is_none())
    }

    // ---------- events ----------

    async fn step_events(&mut self, step: &str) -> anyhow::Result<bool> {
        let store_events = self.imp.wants("events");
        let url = match self.imp.cursor.clone() {
            Some(c) => c,
            None => {
                let mut u = format!("/api/events?sort=datetime&page%5Bsize%5D={EVENT_PAGE}");
                let since = if self.syncing() {
                    Some(s(self.st("last_event_at")).map(String::from).unwrap_or_else(|| self.sync_since()))
                } else {
                    self.imp.events_since().map(fmt_ts)
                };
                if let Some(since) = since {
                    let op = if self.syncing() { "greater-than" } else { "greater-or-equal" };
                    u.push_str(&format!("&filter={}", q(&format!("{op}(datetime,{since})"))));
                }
                u
            }
        };
        let page = self.k.get(&url).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let metrics = self.metric_map().await?.clone();
        // Profiles: known ones from the refs, the rest read from Klaviyo with their consent.
        let pkids: Vec<String> = data.iter().filter_map(|e| rel_ids(e, "profile").first().cloned()).collect::<HashSet<_>>().into_iter().collect();
        let mut pmap: HashMap<String, Uuid> = {
            let rows: Vec<(String, Uuid)> = sqlx::query_as(
                "SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN profiles p ON p.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = ANY($2)",
            )
            .bind(self.acct())
            .bind(&pkids)
            .fetch_all(&self.state.db)
            .await?;
            rows.into_iter().collect()
        };
        // During the sync, anyone who unsubscribed, complained or bounced is re-read now, not next hour.
        let mut refresh: HashSet<String> = HashSet::new();
        if self.syncing() {
            for e in &data {
                let mname = rel_ids(e, "metric").first().and_then(|m| metrics.get(m)).map(|(n, _)| n.clone()).unwrap_or_default();
                if CONSENT_METRICS.iter().any(|c| c.eq_ignore_ascii_case(&mname)) {
                    if let Some(p) = rel_ids(e, "profile").first() {
                        refresh.insert(p.clone());
                    }
                }
            }
        }
        let wanted = |e: &Value| rel_ids(e, "metric").first().and_then(|m| metrics.get(m)).and_then(|(_, l)| *l).is_some();
        let missing: Vec<String> = pkids
            .iter()
            .filter(|p| refresh.contains(*p) || (store_events && !pmap.contains_key(*p) && data.iter().any(|e| wanted(e) && rel_ids(e, "profile").first() == Some(p))))
            .cloned()
            .collect();
        for p in missing.iter().take(200) {
            match self.k.get(&format!("/api/profiles/{p}?additional-fields%5Bprofile%5D=subscriptions")).await {
                Ok(r) => {
                    if let Some(id) = self.upsert_profile(&r["data"]).await? {
                        pmap.insert(p.clone(), id);
                    }
                }
                Err(KlaviyoError::Status { status: 404, .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let refs_of = |kind: &str| {
            let db = self.state.db.clone();
            let acct = self.acct();
            let kind = kind.to_string();
            async move { store::ref_map(&db, acct, &kind).await }
        };
        let (campaign_msgs, campaigns, flows) = if store_events { (refs_of("campaign_message").await?, refs_of("campaign").await?, refs_of("flow").await?) } else { Default::default() };
        let mut batch = Vec::new();
        let (mut done, mut skipped) = (0i64, 0i64);
        let mut last_seen: Option<String> = None;
        for e in &data {
            let a = &e["attributes"];
            if let Some(d) = s(&a["datetime"]) {
                last_seen = Some(d.to_string());
            }
            if !store_events {
                continue;
            }
            let Some((_, Some(metric_id))) = rel_ids(e, "metric").first().and_then(|m| metrics.get(m)).cloned() else { continue };
            let (Some(eid), Some(pk)) = (e["id"].as_str(), rel_ids(e, "profile").first().cloned()) else { continue };
            let Some(pid) = pmap.get(&pk) else {
                skipped += 1;
                continue;
            };
            let Some(at) = ts(&a["datetime"]).or_else(|| a["timestamp"].as_i64().and_then(|t| DateTime::from_timestamp(t, 0))) else {
                skipped += 1;
                continue;
            };
            let mut props = a["event_properties"].as_object().cloned().unwrap_or_default();
            if serde_json::to_string(&props).map(|t| t.len()).unwrap_or(0) > MAX_EVENT_PROPS {
                props.retain(|_, v| !v.is_object() && !v.is_array());
                props.insert("$truncated".into(), json!(true));
            }
            // Tie past email and SMS to the campaigns and flows imported with them.
            if let Some(m) = props.get("$message").and_then(|v| v.as_str()) {
                if let Some(c) = campaign_msgs.get(m).or_else(|| campaigns.get(m)) {
                    props.insert("campaign_id".into(), json!(c));
                }
            }
            if let Some(f) = props.get("$flow").and_then(|v| v.as_str()).and_then(|f| flows.get(f)) {
                props.insert("flow_id".into(), json!(f));
            }
            props.insert("klaviyo_event_id".into(), json!(eid));
            let value = props.get("$value").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|x| x.parse().ok())));
            batch.push(crate::audience::HistoryEvent { profile_id: *pid, metric_id, properties: Value::Object(props), value, occurred_at: at, unique_id: format!("klaviyo:{eid}") });
            done += 1;
        }
        crate::audience::record_history(&self.state.db, self.acct(), &batch).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        store::bump(&self.state.db, self.imp.id, step, done, skipped).await?;
        if let Some(l) = last_seen {
            self.set_st("last_event_at", json!(l));
        }
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- the Klaviyo side of the comparison report ----------

    async fn step_compare(&mut self) -> anyhow::Result<bool> {
        if !self.imp.source_counts.is_object() || self.st("cmp_until").is_null() {
            self.set_st("cmp_until", json!(fmt_ts(Utc::now())));
            self.set_st("cmp_stage", json!(0));
            self.set_st("cmp_i", json!(0));
            sqlx::query("UPDATE klaviyo_imports SET source_counts = jsonb_build_object('until', $2::text, 'lists', '{}'::jsonb, 'segments', '{}'::jsonb, 'metrics', '{}'::jsonb) WHERE id = $1")
                .bind(self.imp.id)
                .bind(self.st("cmp_until").as_str())
                .execute(&self.state.db)
                .await?;
        }
        let until = ts(self.st("cmp_until")).unwrap_or_else(Utc::now);
        let stage = self.st_i("cmp_stage");
        let i = self.st_i("cmp_i");
        match stage {
            0 | 1 => {
                let kind = if stage == 0 { "list" } else { "segment" };
                let ids: Vec<String> = sqlx::query_scalar("SELECT klaviyo_id FROM klaviyo_refs WHERE account_id = $1 AND kind = $2 AND local_id IS NOT NULL ORDER BY klaviyo_id LIMIT $3")
                    .bind(self.acct())
                    .bind(kind)
                    .bind(MAX_COMPARED_GROUPS as i64)
                    .fetch_all(&self.state.db)
                    .await?;
                let Some(kid) = ids.get(i) else {
                    self.set_st("cmp_stage", json!(stage + 1));
                    self.set_st("cmp_i", json!(0));
                    return Ok(false);
                };
                let path = if stage == 0 { format!("/api/lists/{kid}?additional-fields%5Blist%5D=profile_count") } else { format!("/api/segments/{kid}?additional-fields%5Bsegment%5D=profile_count") };
                match self.k.get_paced(&path, 15).await {
                    Ok(v) => {
                        if let Some(n) = v["data"]["attributes"]["profile_count"].as_i64() {
                            let key = if stage == 0 { "lists" } else { "segments" };
                            sqlx::query("UPDATE klaviyo_imports SET source_counts = jsonb_set(source_counts, ARRAY[$2, $3], to_jsonb($4::bigint), true) WHERE id = $1")
                                .bind(self.imp.id)
                                .bind(key)
                                .bind(kid)
                                .bind(n)
                                .execute(&self.state.db)
                                .await?;
                        }
                    }
                    Err(KlaviyoError::Status { status: 404, .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                self.set_st("cmp_i", json!(i + 1));
                Ok(false)
            }
            2 if self.imp.wants("events") => {
                let selected: Vec<(String, String)> = {
                    let m = self.metric_map().await?;
                    let mut v: Vec<(String, String)> = m.iter().filter(|(_, (_, l))| l.is_some()).map(|(k, (n, _))| (k.clone(), n.clone())).collect();
                    v.sort();
                    v
                };
                let Some((kid, name)) = selected.get(i).cloned() else {
                    self.set_st("cmp_stage", json!(3));
                    return Ok(false);
                };
                let since = match self.imp.events_since() {
                    Some(s) => s,
                    None => sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT min(e.occurred_at) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND m.name = $2 AND e.unique_id LIKE 'klaviyo:%'")
                        .bind(self.acct())
                        .bind(&name)
                        .fetch_one(&self.state.db)
                        .await?
                        .unwrap_or(until),
                };
                let tz = sqlx::query_scalar::<_, String>("SELECT timezone FROM accounts WHERE id = $1").bind(self.acct()).fetch_one(&self.state.db).await?;
                let mut months: Map<String, Value> = Map::new();
                let mut start = since;
                while start < until {
                    let end = (start + chrono::Duration::days(360)).min(until);
                    let body = json!({"data": {"type": "metric-aggregate", "attributes": {
                        "metric_id": kid, "measurements": ["count", "sum_value"], "interval": "month", "page_size": 500, "timezone": tz,
                        "filter": [format!("greater-or-equal(datetime,{})", start.format("%Y-%m-%dT%H:%M:%S")), format!("less-than(datetime,{})", end.format("%Y-%m-%dT%H:%M:%S"))]}}});
                    match self.k.post("/api/metric-aggregates", &body).await {
                        Ok(v) => {
                            let dates: Vec<String> = v["data"]["attributes"]["dates"].as_array().into_iter().flatten().filter_map(|d| d.as_str().and_then(|d| d.get(..7)).map(String::from)).collect();
                            for row in v["data"]["attributes"]["data"].as_array().into_iter().flatten() {
                                let counts = row["measurements"]["count"].as_array().cloned().unwrap_or_default();
                                let sums = row["measurements"]["sum_value"].as_array().cloned().unwrap_or_default();
                                for (j, d) in dates.iter().enumerate() {
                                    let c = counts.get(j).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let sv = sums.get(j).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let e = months.entry(d.clone()).or_insert(json!([0.0, 0.0]));
                                    e[0] = json!(e[0].as_f64().unwrap_or(0.0) + c);
                                    e[1] = json!(e[1].as_f64().unwrap_or(0.0) + sv);
                                }
                            }
                        }
                        Err(KlaviyoError::Status { status, detail }) if status < 500 => {
                            self.issue("compare", "note", &name, Some(&kid), None, &format!("Klaviyo wouldn't count this metric's events ({detail}), so the report can't compare it.")).await?;
                            months.clear();
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    start = end;
                }
                sqlx::query("UPDATE klaviyo_imports SET source_counts = jsonb_set(source_counts, ARRAY['metrics', $2], $3, true) WHERE id = $1")
                    .bind(self.imp.id)
                    .bind(&kid)
                    .bind(json!({"name": name, "since": fmt_ts(since), "months": months}))
                    .execute(&self.state.db)
                    .await?;
                self.set_st("cmp_i", json!(i + 1));
                Ok(false)
            }
            _ => {
                sqlx::query("UPDATE klaviyo_imports SET compared_at = now() WHERE id = $1").bind(self.imp.id).execute(&self.state.db).await?;
                Ok(true)
            }
        }
    }

    async fn step(&mut self, phase: &str) -> anyhow::Result<bool> {
        match phase {
            "metrics" => self.step_metrics().await,
            "profiles" | "sync_profiles" => self.step_profiles(phase).await,
            "lists" => self.step_lists().await,
            "list_members" | "sync_list_members" => self.step_members(phase, false).await,
            "segments" => self.step_segments().await,
            "segment_members" => self.step_members(phase, true).await,
            "templates" => self.step_templates().await,
            "campaigns" => self.step_campaigns().await,
            "campaign_stats" => self.step_campaign_stats().await,
            "flows" => self.step_flows().await,
            "forms" => self.step_forms().await,
            "events" | "sync_events" => self.step_events(phase).await,
            "compare" => self.step_compare().await,
            _ => Ok(true),
        }
    }

    fn wanted(&self, phase: &str) -> bool {
        match phase {
            "lists" | "list_members" | "sync_list_members" => self.imp.wants("lists"),
            "segments" | "segment_members" => self.imp.wants("segments"),
            "templates" => self.imp.wants("templates"),
            "campaigns" | "campaign_stats" => self.imp.wants("campaigns"),
            "flows" => self.imp.wants("flows"),
            "forms" => self.imp.wants("forms"),
            // Consent changes ride on events during the sync, so its event step always runs.
            "events" => self.imp.wants("events"),
            _ => true,
        }
    }

    fn next_phase(&self, current: Option<&str>) -> Option<&'static str> {
        let steps = if self.syncing() { SYNC_STEPS } else { STEPS };
        let start = match current {
            Some(c) => steps.iter().position(|s| *s == c).map(|p| p + 1).unwrap_or(0),
            None => 0,
        };
        steps[start..].iter().copied().find(|p| self.wanted(p))
    }
}

/// Steps where a missing scope only means that part is left out.
fn optional_step(phase: &str) -> bool {
    matches!(phase, "segments" | "segment_members" | "templates" | "campaigns" | "campaign_stats" | "flows" | "forms" | "compare")
}

pub async fn run(state: &AppState, import_id: Uuid) -> anyhow::Result<()> {
    let Some(imp) = store::load(&state.db, import_id).await? else { return Ok(()) };
    if !matches!(imp.status.as_str(), "importing" | "syncing") {
        return Ok(());
    }
    let Some(key) = store::key(state, &imp) else {
        store::close(&state.db, imp.id, "failed", Some("The Klaviyo key is no longer stored. Connect again to continue.")).await?;
        return Ok(());
    };
    let mut r = Run::new(state, imp, &key);
    if r.imp.phase.is_none() {
        r.imp.phase = r.next_phase(None).map(String::from);
        r.save().await?;
    }
    loop {
        let Some(phase) = r.imp.phase.clone() else {
            finish(&mut r).await?;
            return Ok(());
        };
        match r.step(&phase).await {
            Ok(done) => {
                if r.st("retries").as_u64().unwrap_or(0) > 0 {
                    r.set_st("retries", json!(0));
                }
                if done {
                    store::finish_step(&state.db, r.imp.id, &phase).await?;
                    r.imp.cursor = None;
                    r.set_st("group", json!(0));
                    r.imp.phase = r.next_phase(Some(&phase)).map(String::from);
                }
                r.save().await?;
            }
            Err(e) => {
                let ke = e.downcast_ref::<KlaviyoError>();
                match ke {
                    Some(KlaviyoError::RateLimited(secs)) => {
                        r.save().await?;
                        store::enqueue(&state.db, JOB_RUN, r.imp.id, Some(Utc::now() + chrono::Duration::seconds(*secs as i64))).await?;
                        return Ok(());
                    }
                    Some(KlaviyoError::MissingScope(what)) if optional_step(&phase) => {
                        r.issue(&phase, "skipped", "Klaviyo access", None, None, &format!("{}. This part was left out.", KlaviyoError::MissingScope(what.clone()))).await?;
                        store::finish_step(&state.db, r.imp.id, &phase).await?;
                        r.imp.cursor = None;
                        r.imp.phase = r.next_phase(Some(&phase)).map(String::from);
                        r.save().await?;
                    }
                    Some(k @ (KlaviyoError::BadKey | KlaviyoError::MissingScope(_))) => {
                        store::close(&state.db, r.imp.id, "failed", Some(&k.to_string())).await?;
                        return Ok(());
                    }
                    _ => {
                        let n = r.st("retries").as_u64().unwrap_or(0) + 1;
                        let msg = format!("{e:#}");
                        tracing::warn!(import = %r.imp.id, phase = %phase, attempt = n, error = %msg, "klaviyo import step failed");
                        if n > 6 {
                            store::close(&state.db, r.imp.id, "failed", Some(&format!("The import stopped at the {} step: {msg}. Imported data is kept; connect again to pick up where it left off.", phase.replace('_', " ")))).await?;
                            return Ok(());
                        }
                        r.set_st("retries", json!(n));
                        r.save().await?;
                        let wait = 30i64 * 2i64.pow(n as u32);
                        store::enqueue(&state.db, JOB_RUN, r.imp.id, Some(Utc::now() + chrono::Duration::seconds(wait))).await?;
                        return Ok(());
                    }
                }
            }
        }
        if r.out_of_time() {
            store::enqueue(&state.db, JOB_RUN, r.imp.id, None).await?;
            return Ok(());
        }
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM klaviyo_imports WHERE id = $1").bind(r.imp.id).fetch_optional(&state.db).await?;
        if !matches!(status.as_deref(), Some("importing" | "syncing")) {
            return Ok(());
        }
    }
}

async fn finish(r: &mut Run<'_>) -> anyhow::Result<()> {
    let db = &r.state.db;
    if r.syncing() {
        let started = ts(r.st("sync_started")).unwrap_or_else(Utc::now);
        sqlx::query("UPDATE klaviyo_imports SET status = 'done', phase = NULL, cursor = NULL, last_sync_at = $2, updated_at = now() WHERE id = $1 AND status = 'syncing'")
            .bind(r.imp.id)
            .bind(started)
            .execute(db)
            .await?;
        // The sync was turned off while it ran: the key goes now.
        sqlx::query("UPDATE klaviyo_imports SET api_key_enc = NULL WHERE id = $1 AND NOT sync_enabled").bind(r.imp.id).execute(db).await?;
        return Ok(());
    }
    sqlx::query(
        "UPDATE klaviyo_imports SET status = 'done', phase = NULL, cursor = NULL, finished_at = now(), last_sync_at = COALESCE(started_at, now()),
            api_key_enc = CASE WHEN sync_enabled THEN api_key_enc ELSE NULL END, updated_at = now()
         WHERE id = $1 AND status = 'importing'",
    )
    .bind(r.imp.id)
    .execute(db)
    .await?;
    Ok(())
}

/// Begin a sync pass of an import that has finished (the key is still there because sync is on).
pub async fn start_sync(db: &sqlx::PgPool, import_id: Uuid) -> anyhow::Result<bool> {
    let started = sqlx::query(
        "UPDATE klaviyo_imports SET status = 'syncing', phase = NULL, cursor = NULL,
            step_state = jsonb_build_object('sync_started', $2::text, 'last_event_at', step_state->'last_event_at'),
            progress = progress - 'sync_profiles' - 'sync_list_members' - 'sync_events', updated_at = now()
         WHERE id = $1 AND status = 'done' AND sync_enabled AND api_key_enc IS NOT NULL",
    )
    .bind(import_id)
    .bind(fmt_ts(Utc::now()))
    .execute(db)
    .await?
    .rows_affected()
        > 0;
    if started {
        store::enqueue(db, JOB_RUN, import_id, None).await?;
    }
    Ok(started)
}

#[cfg(test)]
mod tests {
    #[test]
    fn countries() {
        assert_eq!(super::country_code("United States"), Some("US"));
        assert_eq!(super::country_code("gb"), Some("GB"));
        assert_eq!(super::country_code("Narnia"), None);
    }
}

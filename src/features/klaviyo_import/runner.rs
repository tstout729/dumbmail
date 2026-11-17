//! The import itself: a background job that works through the steps one Klaviyo page at a time,
//! saving its place after every page, so it can be paused, survives a restart and never holds more than
//! a page in memory. Everything upserts on Klaviyo's ids, so running it again only adds what's new.
//!
//! Replay safety: each imported item and its Klaviyo reference are written in one transaction, and a
//! page's progress counts are saved in the same transaction as the cursor that moves past it (events: in
//! the transaction that writes them), so a crash at any point neither duplicates an item nor counts it
//! twice. Only one worker runs an import at a time (a lease on the row, renewed at every save).
//!
//! Every transaction that writes imported data starts by locking the import row and checking that this
//! run still owns it and it's still importing ([`Run::begin`]). A page whose response arrives after a
//! pause, a cancel or another worker taking over writes nothing.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::client::{next_link, q, Client, KlaviyoError};
use super::consent;
use super::review;
use super::store::{self, Import, JOB_RUN, STEPS};
use crate::state::AppState;

/// How long one job run works before handing over to the next (jobs are unlocked after 15 minutes).
const RUN_BUDGET: Duration = Duration::from_secs(50);
/// How long a run owns the import without saving before another worker may take over.
const LEASE_SECS: i64 = 300;
const PROFILE_PAGE: u32 = 100;
/// Most departed list members taken off in one statement when a list's scan completes.
const REMOVE_BATCH: i64 = 5000;
const EVENT_PAGE: u32 = 500;
/// The most event properties kept for one event, as JSON text.
const MAX_EVENT_PROPS: usize = 64 * 1024;
/// Most lists and segments whose Klaviyo size the report fetches (each costs one slow call).
pub const MAX_COMPARED_GROUPS: usize = 500;

/// Metrics imported when the customer doesn't choose: shopping activity and email/SMS engagement.
pub const DEFAULT_METRICS: &[&str] = &[
    "Placed Order", "Ordered Product", "Started Checkout", "Checkout Started", "Viewed Product", "Added to Cart", "Active on Site",
    "Fulfilled Order", "Cancelled Order", "Refunded Order", "Received Email", "Opened Email", "Clicked Email", "Bounced Email",
    "Marked Email as Spam", "Unsubscribed", "Received SMS", "Clicked SMS", "Unsubscribed from SMS", "Subscribed to List",
];
/// Metrics whose values are revenue in the report.
pub const ORDER_METRICS: &[&str] = &["Placed Order"];

/// This run no longer owns the import (paused, cancelled, failed, or another worker took over): stop
/// without writing.
#[derive(Debug, thiserror::Error)]
#[error("this import run no longer owns the import")]
pub struct Superseded;

pub type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

pub struct Run<'a> {
    pub state: &'a AppState,
    pub imp: Import,
    pub k: Client,
    deadline: Instant,
    metrics: Option<HashMap<String, (String, Option<Uuid>)>>,
    owner: Uuid,
    /// Progress for work done since the last save, written in the same transaction as the cursor.
    pending: Vec<(String, i64, i64)>,
    finished: Vec<String>,
    /// Writing the full profile scan (its references are what the report compares with Klaviyo's total).
    pub(super) in_profile_scan: bool,
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

pub fn fmt_ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl<'a> Run<'a> {
    pub fn new(state: &'a AppState, imp: Import, key: &str) -> Self {
        Run { state, imp, k: Client::new(state.http.clone(), key), deadline: Instant::now() + RUN_BUDGET, metrics: None, owner: Uuid::now_v7(), pending: Vec::new(), finished: Vec::new(), in_profile_scan: false }
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

    fn bump(&mut self, step: &str, done: i64, skipped: i64) {
        self.pending.push((step.to_string(), done, skipped));
    }

    /// A transaction for writing imported data, holding the import row's lock: fails with [`Superseded`]
    /// unless this run still owns the import and it's still importing.
    pub async fn begin(&self) -> anyhow::Result<Tx> {
        let mut tx = self.state.db.begin().await?;
        let ok: Option<i32> = sqlx::query_scalar("SELECT 1 FROM klaviyo_imports WHERE id = $1 AND lease_owner = $2 AND status = 'importing' FOR UPDATE")
            .bind(self.imp.id)
            .bind(self.owner)
            .fetch_optional(&mut *tx)
            .await?;
        if ok.is_none() {
            tx.rollback().await?;
            return Err(Superseded.into());
        }
        Ok(tx)
    }

    /// Take the import for this run (tests drive a run by hand with this).
    pub async fn claim(&self) -> anyhow::Result<bool> {
        Ok(claim(&self.state.db, self.imp.id, self.owner).await?)
    }

    /// Run the current step once, without saving the cursor (tests use this to stop "mid-page").
    pub async fn step_once(&mut self) -> anyhow::Result<bool> {
        let phase = match self.imp.phase.clone() {
            Some(p) => p,
            None => {
                self.imp.phase = self.next_phase(None).map(String::from);
                self.imp.phase.clone().unwrap_or_default()
            }
        };
        self.step(&phase).await
    }

    /// Save the cursor, step state and progress together, and renew the lease. Returns false when this run
    /// no longer owns the import (it must stop).
    pub async fn save(&mut self) -> anyhow::Result<bool> {
        let mut tx = self.begin().await?;
        for (step, d, sk) in self.pending.drain(..) {
            store::bump(&mut *tx, self.imp.id, &step, d, sk).await?;
        }
        for step in self.finished.drain(..) {
            store::finish_step(&mut *tx, self.imp.id, &step).await?;
        }
        let n = sqlx::query(
            "UPDATE klaviyo_imports SET phase = $2, cursor = $3,
                step_state = $4 || jsonb_strip_nulls(jsonb_build_object('issues_over_limit', step_state->'issues_over_limit')),
                lease_until = now() + make_interval(secs => $6), updated_at = now()
             WHERE id = $1 AND lease_owner = $5 AND status = 'importing'",
        )
        .bind(self.imp.id)
        .bind(&self.imp.phase)
        .bind(&self.imp.cursor)
        .bind(&self.imp.step_state)
        .bind(self.owner)
        .bind(LEASE_SECS as f64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    pub(super) async fn issue(&self, step: &str, kind: &str, item: &str, kid: Option<&str>, local: Option<Uuid>, msg: &str) -> anyhow::Result<()> {
        let mut tx = self.begin().await?;
        store::issue(&mut tx, &self.imp, step, kind, item, kid, local, msg).await?;
        tx.commit().await?;
        Ok(())
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
        let mut rows = Vec::new();
        for m in page["data"].as_array().into_iter().flatten() {
            let (Some(kid), Some(name)) = (m["id"].as_str(), s(&m["attributes"]["name"])) else { continue };
            let name: String = name.chars().take(128).collect();
            let selected = events_on && self.metric_selected(&name, kid);
            // The metric itself (DumbMail's, by name) is shared; only the reference is the import's own write.
            let local = if selected { Some(crate::audience::metric_id(&self.state.db, self.acct(), &name).await.map_err(|e| anyhow::anyhow!(e.public_message()))?) } else { None };
            let integration = m["attributes"]["integration"]["name"].as_str().unwrap_or("").to_string();
            rows.push((kid.to_string(), local, name, json!({"selected": selected, "integration": integration, "created": m["attributes"]["created"]})));
        }
        let n = rows.len() as i64;
        let mut tx = self.begin().await?;
        for (kid, local, name, detail) in &rows {
            store::set_ref(&mut *tx, self.acct(), "metric", kid, *local, name, detail).await?;
        }
        tx.commit().await?;
        self.bump("metrics", n, 0);
        self.metrics = None;
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- profiles and consent ----------

    /// Upsert a page of profiles, then keep list-level opt-outs off their lists.
    async fn take_profiles(&mut self, data: &[Value]) -> anyhow::Result<super::profiles::Upserted> {
        let up = self.upsert_profiles(data).await?;
        let mut sups: Vec<(Uuid, String)> = Vec::new();
        for p in data {
            let Some(pid) = p["id"].as_str().and_then(|k| up.ids.get(k)) else { continue };
            for l in consent::list_suppressions(&p["attributes"]["subscriptions"]) {
                sups.push((*pid, l));
            }
        }
        self.apply_list_optouts(&sups).await?;
        Ok(up)
    }

    /// People who unsubscribed from a Klaviyo list: off that DumbMail list, and not added back while the
    /// opt-out stands. Exactly those (list, person) pairs; their other lists and their consent are untouched.
    async fn apply_list_optouts(&self, sups: &[(Uuid, String)]) -> anyhow::Result<()> {
        if sups.is_empty() {
            return Ok(());
        }
        let pids: Vec<Uuid> = sups.iter().map(|x| x.0).collect();
        let kids: Vec<&str> = sups.iter().map(|x| x.1.as_str()).collect();
        let mut tx = self.begin().await?;
        // The leave first (it waits for any import insert of the same pair), then the removal.
        let pairs: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "WITH x AS (SELECT DISTINCT x.p, r.local_id AS l FROM unnest($2::uuid[], $3::text[]) AS x(p, k)
                        JOIN klaviyo_refs r ON r.account_id = $1 AND r.kind = 'list' AND r.klaviyo_id = x.k AND r.local_id IS NOT NULL
                        JOIN lists li ON li.id = r.local_id AND li.account_id = $1)
             INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, left_via)
             SELECT $1, x.l, x.p, 'left', 'klaviyo_list_unsubscribe' FROM x JOIN profiles p ON p.id = x.p AND p.account_id = $1
             ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'left', left_via = COALESCE(CASE WHEN klaviyo_memberships.state = 'left' THEN klaviyo_memberships.left_via END, 'klaviyo_list_unsubscribe'), updated_at = now()
             RETURNING list_id, profile_id",
        )
        .bind(self.acct())
        .bind(&pids)
        .bind(&kids)
        .fetch_all(&mut *tx)
        .await?;
        let (lists, people): (Vec<Uuid>, Vec<Uuid>) = pairs.into_iter().unzip();
        sqlx::query("DELETE FROM list_members lm USING unnest($1::uuid[], $2::uuid[]) AS x(l, p) WHERE lm.list_id = x.l AND lm.profile_id = x.p")
            .bind(&lists)
            .bind(&people)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// DumbMail ids for a page of Klaviyo profile resources. Profiles not seen yet, and those whose
    /// identifiers or consent changed since they were last read, are written first.
    async fn resolve_profiles(&mut self, resources: &[Value]) -> anyhow::Result<HashMap<String, Uuid>> {
        let ids: Vec<String> = resources.iter().filter_map(|r| r["id"].as_str().map(String::from)).collect();
        let rows: Vec<(String, Uuid, Option<String>)> = sqlx::query_as(
            "SELECT r.klaviyo_id, r.local_id, r.detail->>'fp' FROM klaviyo_refs r JOIN profiles p ON p.id = r.local_id
             WHERE r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = ANY($2)",
        )
        .bind(self.acct())
        .bind(&ids)
        .fetch_all(&self.state.db)
        .await?;
        let known: HashMap<String, (Uuid, Option<String>)> = rows.into_iter().map(|(k, l, f)| (k, (l, f))).collect();
        let stale: Vec<Value> = resources
            .iter()
            .filter(|r| r["id"].as_str().is_some_and(|k| known.get(k).is_none_or(|(_, fp)| fp.as_deref() != Some(super::profiles::fingerprint(r).as_str()))))
            .cloned()
            .collect();
        let mut out: HashMap<String, Uuid> = known.into_iter().map(|(k, (l, _))| (k, l)).collect();
        if !stale.is_empty() {
            let up = self.take_profiles(&stale).await?;
            out.extend(up.ids);
        }
        Ok(out)
    }

    async fn step_profiles(&mut self, step: &str) -> anyhow::Result<bool> {
        let url = self.imp.cursor.clone().unwrap_or_else(|| format!("/api/profiles?additional-fields%5Bprofile%5D=subscriptions&page%5Bsize%5D={PROFILE_PAGE}&sort=id"));
        let page = self.k.get(&url).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        self.in_profile_scan = true;
        let up = self.take_profiles(&data).await;
        self.in_profile_scan = false;
        let up = up?;
        self.bump(step, up.done, up.skipped);
        // Klaviyo's own total, counted while reading every page: the report's independent number.
        let n = self.st("scan_profiles").as_i64().unwrap_or(0) + data.len() as i64;
        self.set_st("scan_profiles", json!(n));
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    /// Profiles by Klaviyo id, read in batches of 100 with their consent (events name profiles by id only).
    async fn fetch_profiles(&mut self, kids: &[String]) -> anyhow::Result<HashMap<String, Uuid>> {
        let mut out = HashMap::new();
        for chunk in kids.chunks(PROFILE_PAGE as usize) {
            let list = chunk.iter().map(|k| format!("\"{}\"", k.replace('"', ""))).collect::<Vec<_>>().join(",");
            let mut url = Some(format!("/api/profiles?additional-fields%5Bprofile%5D=subscriptions&page%5Bsize%5D={PROFILE_PAGE}&filter={}", q(&format!("any(id,[{list}])"))));
            while let Some(u) = url {
                let page = self.k.get(&u).await?;
                let data = page["data"].as_array().cloned().unwrap_or_default();
                out.extend(self.take_profiles(&data).await?.ids);
                url = next_link(&page);
            }
        }
        Ok(out)
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
            let mut tx = self.begin().await?;
            let exists: Option<Uuid> = sqlx::query_scalar("SELECT l.id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' AND r.klaviyo_id = $2 FOR UPDATE OF r")
                .bind(self.acct())
                .bind(kid)
                .fetch_optional(&mut *tx)
                .await?;
            let id = match exists {
                Some(id) => id,
                None => {
                    let id = Uuid::now_v7();
                    sqlx::query("INSERT INTO lists (id, account_id, name, double_opt_in) VALUES ($1, $2, $3, $4)").bind(id).bind(self.acct()).bind(&name).bind(double).execute(&mut *tx).await?;
                    id
                }
            };
            store::set_ref(&mut *tx, self.acct(), "list", kid, Some(id), &name, &json!({"double_opt_in": double})).await?;
            tx.commit().await?;
            n += 1;
        }
        self.bump("lists", n, 0);
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    async fn groups(&self, segments: bool) -> anyhow::Result<Vec<(String, Uuid)>> {
        Ok(if segments {
            sqlx::query_as("SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'segment' AND r.detail->>'mode' = 'snapshot' ORDER BY r.klaviyo_id")
        } else {
            sqlx::query_as("SELECT r.klaviyo_id, r.local_id FROM klaviyo_refs r JOIN lists l ON l.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' ORDER BY r.klaviyo_id")
        }
        .bind(self.acct())
        .fetch_all(&self.state.db)
        .await?)
    }

    /// Mark people who left a list in DumbMail since the import last saw them (removed by hand, by a flow,
    /// or through the preference page before leaves were recorded): they're never added back.
    async fn note_dumbmail_leaves(&self, tx: &mut Tx, list_id: Uuid, pids: &[Uuid]) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE klaviyo_memberships m SET state = 'left', left_via = 'dumbmail', updated_at = now()
             WHERE m.list_id = $1 AND m.profile_id = ANY($2) AND m.state = 'member'
               AND NOT EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = m.list_id AND lm.profile_id = m.profile_id)",
        )
        .bind(list_id)
        .bind(pids)
        .execute(&mut **tx)
        .await?;
        // Left through the preference page (recorded as an event) and not back on the list since.
        sqlx::query(
            "INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, left_via)
             SELECT DISTINCT $3::uuid, $1::uuid, e.profile_id, 'left', 'dumbmail' FROM events e JOIN metrics mt ON mt.id = e.metric_id
             WHERE e.account_id = $3 AND mt.account_id = $3 AND mt.name = 'Unsubscribed from List' AND e.properties->>'list_id' = $1::text AND e.profile_id = ANY($2)
               AND NOT EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = $1 AND lm.profile_id = e.profile_id)
             ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'left', left_via = COALESCE(CASE WHEN klaviyo_memberships.state = 'left' THEN klaviyo_memberships.left_via END, 'dumbmail'), updated_at = now()",
        )
        .bind(list_id)
        .bind(pids)
        .bind(self.acct())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// A list or snapshot segment Klaviyo wouldn't give (deleted since, or an error): its members can't be
    /// reconciled this run, and the report says so.
    async fn not_reconciled(&self, list_id: Uuid, why: &str) -> anyhow::Result<()> {
        let mut tx = self.begin().await?;
        sqlx::query(
            "UPDATE klaviyo_refs SET detail = detail || jsonb_build_object('scan', jsonb_build_object('import', $3::text, 'error', $4::text, 'at', now()))
             WHERE account_id = $1 AND local_id = $2 AND kind IN ('list', 'segment')",
        )
        .bind(self.acct())
        .bind(list_id)
        .bind(self.imp.id.to_string())
        .bind(why)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Copy members of Klaviyo lists (or snapshot segments) into their DumbMail lists, one page at a time.
    /// Members join quietly: no list-triggered flow starts. People who left a list in DumbMail are never
    /// added back; people Klaviyo itself took off (or who unsubscribed from the list there) are, once a
    /// later run finds them back on it in Klaviyo with no list opt-out. A full scan of a list ends by
    /// taking off anyone Klaviyo no longer has on it; a snapshot segment's members are staged and swapped
    /// in only when its scan is complete.
    async fn step_members(&mut self, step: &str, segments: bool) -> anyhow::Result<bool> {
        let groups = self.groups(segments).await?;
        let i = self.st_i("group");
        let Some((kid, list_id)) = groups.get(i).cloned() else {
            self.set_st("group", json!(0));
            return Ok(true);
        };
        let url = match self.imp.cursor.clone() {
            Some(c) => c,
            None => {
                // A new full scan of this group.
                self.set_st("scan", json!(Utc::now().timestamp_millis()));
                self.set_st("scan_started", json!(fmt_ts(Utc::now())));
                self.set_st("scan_seen", json!(0));
                let base = if segments { "segments" } else { "lists" };
                format!("/api/{base}/{kid}/profiles?additional-fields%5Bprofile%5D=subscriptions&page%5Bsize%5D={PROFILE_PAGE}")
            }
        };
        let scan = self.st("scan").as_i64().unwrap_or(0);
        let page = match self.k.get(&url).await {
            Ok(p) => p,
            Err(e @ KlaviyoError::Status { .. }) if !e.is_transient() => {
                // Deleted in Klaviyo since, or refused: nothing is taken off on its account, and the report
                // shows the list as not reconciled.
                let why = match &e {
                    KlaviyoError::Status { status: 404, .. } => "Klaviyo no longer has this list (404)".to_string(),
                    other => other.to_string(),
                };
                self.not_reconciled(list_id, &why).await?;
                self.imp.cursor = None;
                self.set_st("group", json!(i + 1));
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let seen_n = self.st("scan_seen").as_i64().unwrap_or(0) + data.len() as i64;
        self.set_st("scan_seen", json!(seen_n));
        let ids = self.resolve_profiles(&data).await?;
        // List-level opt-outs on this page, for every list they name (this one included).
        let sups: Vec<(Uuid, String)> = data
            .iter()
            .filter_map(|m| Some((*ids.get(m["id"].as_str()?)?, consent::list_suppressions(&m["attributes"]["subscriptions"]))))
            .flat_map(|(p, ls)| ls.into_iter().map(move |l| (p, l)))
            .collect();
        self.apply_list_optouts(&sups).await?;
        let (mut pids, mut times) = (Vec::new(), Vec::new());
        let mut skipped = 0;
        for m in &data {
            let Some(pid) = m["id"].as_str().and_then(|mid| ids.get(mid)) else {
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
        let mut tx = self.begin().await?;
        self.note_dumbmail_leaves(&mut tx, list_id, &pids).await?;
        // One statement: record Klaviyo's membership with this scan's number and add exactly those it
        // records as members. The upsert takes each (list, person) row's lock and re-reads it, so a leave
        // recorded in DumbMail meanwhile (see `lists::remove_members`) is always seen: no re-add.
        let added = sqlx::query(
            "WITH x AS (SELECT * FROM unnest($3::uuid[], $4::timestamptz[]) AS x(p, t)),
             j AS (
                INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, scan)
                SELECT $1, $2, x.p, 'member', $5 FROM x JOIN profiles pr ON pr.id = x.p AND pr.account_id = $1
                ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'member', left_via = NULL, scan = EXCLUDED.scan, updated_at = now()
                -- People who left stay left. Only a leave Klaviyo itself recorded is lifted: they're back on the list there.
                WHERE klaviyo_memberships.state = 'member' OR klaviyo_memberships.left_via IN ('klaviyo', 'klaviyo_list_unsubscribe')
                RETURNING profile_id)
             INSERT INTO list_members (list_id, profile_id, added_at)
             SELECT $2, j.profile_id, x.t FROM j JOIN x ON x.p = j.profile_id WHERE NOT $6
             ON CONFLICT DO NOTHING",
        )
        .bind(self.acct())
        .bind(list_id)
        .bind(&pids)
        .bind(&times)
        .bind(scan)
        // Snapshots are swapped in whole when the scan completes.
        .bind(segments)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
        tx.commit().await?;
        self.bump(step, if segments { pids.len() as i64 } else { added }, skipped);
        self.imp.cursor = next_link(&page);
        if self.imp.cursor.is_none() {
            self.complete_scan(list_id, segments, scan).await?;
            self.set_st("group", json!(i + 1));
        }
        Ok(false)
    }

    /// A full scan of one list or snapshot segment finished: reconcile removals, set-based and in bounded
    /// batches (a list that lost a million members never brings their ids into memory).
    async fn complete_scan(&mut self, list_id: Uuid, segments: bool, scan: i64) -> anyhow::Result<()> {
        let started = ts(self.st("scan_started")).unwrap_or_else(Utc::now);
        if segments {
            let mut tx = self.begin().await?;
            // Swap in the new member set: off if not in this scan, on if in it (and hasn't left).
            sqlx::query(
                "DELETE FROM list_members lm WHERE lm.list_id = $1
                 AND NOT EXISTS (SELECT 1 FROM klaviyo_memberships m WHERE m.list_id = $1 AND m.profile_id = lm.profile_id AND m.state = 'member' AND m.scan = $2)",
            )
            .bind(list_id)
            .bind(scan)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM klaviyo_memberships WHERE list_id = $1 AND state = 'member' AND scan <> $2").bind(list_id).bind(scan).execute(&mut *tx).await?;
            // Each member row is locked and re-checked as it's added, so a leave recorded meanwhile wins.
            sqlx::query(
                "WITH j AS (UPDATE klaviyo_memberships m SET updated_at = now() WHERE m.list_id = $1 AND m.state = 'member' AND m.scan = $2 RETURNING m.profile_id)
                 INSERT INTO list_members (list_id, profile_id, added_at) SELECT $1, j.profile_id, now() FROM j ON CONFLICT DO NOTHING",
            )
            .bind(list_id)
            .bind(scan)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        } else {
            // Klaviyo no longer has them on the list (and they didn't join while the scan ran): take them
            // off here too, and remember it so a later import doesn't add them back unless Klaviyo does.
            loop {
                let mut tx = self.begin().await?;
                let n: i64 = sqlx::query_scalar(
                    "WITH gone AS (
                        SELECT profile_id FROM klaviyo_memberships WHERE list_id = $1 AND state = 'member' AND scan < $2 AND updated_at < $3
                        ORDER BY profile_id LIMIT $4 FOR UPDATE),
                     up AS (
                        UPDATE klaviyo_memberships m SET state = 'left', left_via = 'klaviyo', updated_at = now() FROM gone
                        WHERE m.list_id = $1 AND m.profile_id = gone.profile_id RETURNING m.profile_id),
                     d AS (DELETE FROM list_members lm USING up WHERE lm.list_id = $1 AND lm.profile_id = up.profile_id)
                     SELECT count(*) FROM up",
                )
                .bind(list_id)
                .bind(scan)
                .bind(started)
                .bind(REMOVE_BATCH)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                if n < REMOVE_BATCH {
                    break;
                }
            }
        }
        // Klaviyo's members counted by this scan, and how many of them aren't members here (no usable
        // identifier, opted out of the list, or left it in DumbMail).
        let seen = self.st("scan_seen").as_i64().unwrap_or(0);
        let mut tx = self.begin().await?;
        let matched: i64 = sqlx::query_scalar("SELECT count(*) FROM klaviyo_memberships WHERE list_id = $1 AND state = 'member' AND scan = $2").bind(list_id).bind(scan).fetch_one(&mut *tx).await?;
        let skipped = (seen - matched).max(0);
        sqlx::query("UPDATE klaviyo_refs SET detail = detail || jsonb_build_object('scan', jsonb_build_object('members', $3::bigint, 'skipped', $4::bigint, 'import', $5::text, 'at', now())) WHERE account_id = $1 AND local_id = $2 AND kind IN ('list', 'segment')")
            .bind(self.acct())
            .bind(list_id)
            .bind(seen)
            .bind(skipped)
            .bind(self.imp.id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
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
            let id = Uuid::now_v7();
            let mut tx = self.begin().await?;
            let issue = match super::segments::translate(&ctx, &sg["attributes"]["definition"]) {
                Ok(def) => {
                    sqlx::query("INSERT INTO segments (id, account_id, name, definition) VALUES ($1, $2, $3, $4)").bind(id).bind(self.acct()).bind(&name).bind(&def).execute(&mut *tx).await?;
                    store::set_ref(&mut *tx, self.acct(), "segment", kid, Some(id), &name, &json!({"mode": "rules"})).await?;
                    None
                }
                Err(reasons) => {
                    let list_name = format!("{name} (Klaviyo segment snapshot, {})", Utc::now().format("%b %-d, %Y"));
                    sqlx::query("INSERT INTO lists (id, account_id, name, description) VALUES ($1, $2, $3, $4)")
                        .bind(id)
                        .bind(self.acct())
                        .bind(&list_name)
                        .bind("Members of a Klaviyo segment when it was last imported. It doesn't change by itself.")
                        .execute(&mut *tx)
                        .await?;
                    store::set_ref(&mut *tx, self.acct(), "segment", kid, Some(id), &name, &json!({"mode": "snapshot", "reasons": reasons})).await?;
                    Some(format!("Imported as a list of its current members, because DumbMail can't express {}. Rebuild it as a segment if it needs to keep updating.", reasons.join("; ")))
                }
            };
            if let Some(msg) = issue {
                store::issue(&mut tx, &self.imp, "segments", "snapshot", &name, Some(kid), Some(id), &msg).await?;
            }
            tx.commit().await?;
            n += 1;
        }
        self.bump("segments", n, 0);
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- templates ----------

    /// Converted email content for a Klaviyo template: from the imported template, or fetched directly
    /// (campaign and flow emails use their own copies). The content's `imported.review` lists anything
    /// the conversion had to guess.
    async fn template_content(&mut self, kid: &str) -> anyhow::Result<Option<Value>> {
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
        Ok(Some(super::templates::email_content(html, "", None, None, None, None).0))
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
            let (content, removed, notes) = super::templates::email_content(html, "", None, None, None, None);
            let id = Uuid::now_v7();
            let mut tx = self.begin().await?;
            let mut content = content;
            review::mirror(&mut content, &notes);
            sqlx::query("INSERT INTO templates (id, account_id, name, channel, content, import_review) VALUES ($1, $2, $3, 'email', $4, $5)")
                .bind(id)
                .bind(self.acct())
                .bind(&name)
                .bind(&content)
                .bind(review::column(&notes))
                .execute(&mut *tx)
                .await?;
            store::set_ref(&mut *tx, self.acct(), "template", kid, Some(id), &name, &json!({"removed_tags": removed})).await?;
            if !removed.is_empty() {
                store::issue(&mut tx, &self.imp, "templates", "unmapped", &name, Some(kid), Some(id), &format!("Removed template tags DumbMail doesn't have: {}", removed.join(", "))).await?;
            }
            tx.commit().await?;
            n += 1;
        }
        self.bump("templates", n, 0);
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

    /// The audience of one campaign message, exactly (Klaviyo's message → campaign-audience relationship).
    async fn message_audience(&mut self, mid: &str) -> anyhow::Result<Option<Value>> {
        match self.k.get(&format!("/api/campaign-messages/{mid}/campaign-audience")).await {
            Ok(v) if v["data"].is_object() => Ok(Some(v["data"]["attributes"]["definition"].clone())),
            Ok(_) | Err(KlaviyoError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn step_campaigns(&mut self) -> anyhow::Result<bool> {
        // The cursor stays on the current page until every campaign on it is in; done ones are skipped.
        let url = self.imp.cursor.clone().unwrap_or_else(|| "/api/campaigns?include=campaign-messages&page%5Bsize%5D=100&sort=created_at".into());
        let page = self.k.get(&url).await?;
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
                let variations = rel_ids(msg, "campaign-variations");
                let variation = variations.first().and_then(|v| vinc.get(&("campaign-variation".to_string(), v.clone()))).cloned().unwrap_or(Value::Null);
                let details = &variation["attributes"]["definition"]["details"];
                let channel = details["channel"].as_str().unwrap_or("email");
                let name: String = if mids.len() > 1 { format!("{cname} – {}", s(&mdef["name"]).unwrap_or(&format!("message {}", mi + 1))) } else { cname.clone() }.chars().take(200).collect();
                let status_k = mdef["status"].as_str().unwrap_or("draft");
                let send = &mdef["send_options"];
                let sent_at = ts(&send["send_time"]).or_else(|| ts(&msg["attributes"]["updated"]));
                let (status, sent_at) = if matches!(status_k, "sent" | "sending") { ("sent", sent_at) } else { ("draft", None) };
                let mut notes: Vec<String> = Vec::new();
                if status_k == "scheduled" {
                    self.issue("campaigns", "note", &name, Some(mid), None, "It was scheduled in Klaviyo. It came across as a draft so it can't go out twice; schedule it in one place only.").await?;
                }
                // Audience: the message's own, exactly. Anything that doesn't map keeps a draft unsendable.
                let adef = self.message_audience(mid).await?;
                let mut aud = json!({"include": [], "exclude": []});
                let mut names = json!({"included": [], "excluded": []});
                match &adef {
                    None => notes.push("Klaviyo didn't say who this message goes to. Choose the audience.".into()),
                    Some(adef) => {
                        for (k, nk) in [("included", "include"), ("excluded", "exclude")] {
                            for gid in adef[k].as_array().into_iter().flatten().filter_map(|g| g.as_str()) {
                                match audiences.get(gid) {
                                    Some((entry, n)) => {
                                        aud[nk].as_array_mut().unwrap().push(entry.clone());
                                        names[k].as_array_mut().unwrap().push(json!(n));
                                    }
                                    None => {
                                        names[k].as_array_mut().unwrap().push(json!(gid));
                                        notes.push(if k == "excluded" {
                                            format!("It left out the list or segment {gid}, which wasn't imported, so the audience would be wider than in Klaviyo. Set the exclusions again.")
                                        } else {
                                            format!("It went to the list or segment {gid}, which wasn't imported. Choose the audience again.")
                                        });
                                    }
                                }
                            }
                        }
                        if aud["include"].as_array().is_some_and(|a| a.is_empty()) && !notes.iter().any(|n| n.contains("Choose the audience")) {
                            notes.push("Its audience is empty. Choose who it goes to.".into());
                        }
                    }
                }
                if let Some(p) = send["recipient_percentage"].as_i64().filter(|p| *p != 100) {
                    notes.push(format!("Klaviyo sent it to {p}% of the audience; DumbMail would send to everyone. Narrow the audience or set up an A/B test."));
                }
                if send["is_ignoring_unsubscribes"] == true {
                    notes.push("Klaviyo was set to send it to unsubscribed people too. DumbMail never does; check the audience is what you mean.".into());
                }
                if let Some(strat) = send["send_strategy"].as_str().filter(|x| !matches!(*x, "static" | "")) {
                    notes.push(format!("Klaviyo sent it with the \"{strat}\" send strategy (time zones, throttling or smart send time). DumbMail sends at one time."));
                }
                if mdef["ab_test"].is_object() || variations.len() > 1 {
                    notes.push("It was an A/B test in Klaviyo; only its first version came across.".into());
                }
                let content = match channel {
                    "email" => {
                        let mut content = match s(&details["template_id"]) {
                            Some(t) => self.template_content(t).await?.unwrap_or_else(|| {
                                notes.push("Its email design wasn't found in Klaviyo, so the email is empty.".into());
                                json!({"blocks": []})
                            }),
                            None => json!({"blocks": []}),
                        };
                        notes.extend(super::templates::review_notes(&content));
                        for (src, dst) in [("subject", "subject"), ("preview_text", "preheader")] {
                            if let Some(v) = s(&details[src]) {
                                let cv = super::templates::convert(v);
                                if !cv.removed.is_empty() || !cv.review.is_empty() {
                                    notes.push(format!("The {} had Klaviyo tags DumbMail doesn't have ({}); check the wording.", if dst == "subject" { "subject" } else { "preview text" }, cv.removed.join(", ")));
                                }
                                content[dst] = json!(cv.text);
                            }
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
                        if !c.removed.is_empty() || !c.review.is_empty() {
                            notes.push(format!("The text had Klaviyo tags DumbMail doesn't have ({}); check the wording.", c.removed.join(", ")));
                        }
                        json!({"body": c.text})
                    }
                    other => {
                        let mut tx = self.begin().await?;
                        store::set_ref(&mut *tx, self.acct(), "campaign_message", mid, None, &name, &json!({"skipped": other})).await?;
                        store::issue(&mut tx, &self.imp, "campaigns", "skipped", &name, Some(mid), None, &format!("A {other} campaign; DumbMail sends email and SMS only.")).await?;
                        tx.commit().await?;
                        continue;
                    }
                };
                notes.dedup();
                // History can't be sent again, so only drafts carry review notes.
                let notes = if status == "draft" { notes } else { Vec::new() };
                let mut content = content;
                review::mirror(&mut content, &notes);
                let id = Uuid::now_v7();
                let created = ts(&c["attributes"]["created_at"]).unwrap_or_else(Utc::now);
                let detail = json!({"campaign_id": cid, "channel": channel, "status": status_k, "sent_at": sent_at, "subject": details["subject"], "audience": names,
                    "send_options": send, "needs_review": !notes.is_empty()});
                let mut tx = self.begin().await?;
                sqlx::query("INSERT INTO campaigns (id, account_id, name, channel, status, audience, content, sent_at, created_at, import_review) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
                    .bind(id)
                    .bind(self.acct())
                    .bind(&name)
                    .bind(channel)
                    .bind(status)
                    .bind(&aud)
                    .bind(&content)
                    .bind(sent_at)
                    .bind(created)
                    .bind(review::column(&notes))
                    .execute(&mut *tx)
                    .await?;
                store::set_ref(&mut *tx, self.acct(), "campaign_message", mid, Some(id), &name, &detail).await?;
                if mi == 0 {
                    store::set_ref(&mut *tx, self.acct(), "campaign", cid, Some(id), &cname, &json!({})).await?;
                }
                store::bump(&mut *tx, self.imp.id, "campaigns", 1, 0).await?;
                tx.commit().await?;
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
            Err(e @ KlaviyoError::Status { status, .. }) if status < 500 => {
                self.issue("campaign_stats", "skipped", "Past campaign numbers", None, None, &format!("Klaviyo wouldn't report them ({e}).")).await?;
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        };
        let mut tx = self.begin().await?;
        for r in page["data"]["attributes"]["results"].as_array().into_iter().flatten() {
            let Some(mid) = r["groupings"]["campaign_message_id"].as_str() else { continue };
            sqlx::query("UPDATE klaviyo_refs SET detail = jsonb_set(detail, '{stats}', $3), updated_at = now() WHERE account_id = $1 AND kind = 'campaign_message' AND klaviyo_id = $2")
                .bind(self.acct())
                .bind(mid)
                .bind(&r["statistics"])
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
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
                let mut tx = self.begin().await?;
                store::set_ref(&mut *tx, self.acct(), "flow", kid, None, &name, &json!({"skipped": "archived"})).await?;
                tx.commit().await?;
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
                if let Some(c) = self.template_content(&t).await? {
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
                    let mut tx = self.begin().await?;
                    // The flow's own notes go on its definition too, so a copy of it needs review as well.
                    let mut definition = t.definition.clone();
                    review::mirror(&mut definition, &t.review);
                    sqlx::query("INSERT INTO flows (id, account_id, name, status, trigger, definition, import_review) VALUES ($1, $2, $3, 'draft', $4, $5, $6)")
                        .bind(id)
                        .bind(self.acct())
                        .bind(&fname)
                        .bind(&t.trigger)
                        .bind(&definition)
                        .bind(review::column(&t.review))
                        .execute(&mut *tx)
                        .await?;
                    store::set_ref(&mut *tx, self.acct(), "flow", kid, Some(id), &name, &json!({"klaviyo_status": kstatus, "partial": partial, "needs_review": !t.review.is_empty()})).await?;
                    store::bump(&mut *tx, self.imp.id, "flows", 1, 0).await?;
                    for i in &t.issues {
                        store::issue(&mut tx, &self.imp, "flows", "partial", &name, Some(kid), Some(id), i).await?;
                    }
                    tx.commit().await?;
                }
                Err(why) => {
                    let mut tx = self.begin().await?;
                    store::set_ref(&mut *tx, self.acct(), "flow", kid, None, &name, &json!({"klaviyo_status": kstatus, "skipped": why})).await?;
                    store::bump(&mut *tx, self.imp.id, "flows", 0, 1).await?;
                    store::issue(&mut tx, &self.imp, "flows", "skipped", &name, Some(kid), None, &format!("Not imported: {why}. Rebuild it by hand.")).await?;
                    tx.commit().await?;
                }
            }
        }
        self.imp.cursor = next_link(&page);
        Ok(self.imp.cursor.is_none())
    }

    // ---------- forms ----------

    async fn step_forms(&mut self) -> anyhow::Result<bool> {
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
            let ids = form_destinations(&full["data"]["attributes"]["definition"]);
            let (list, problem) = match ids.as_slice() {
                [] => (None, None),
                [one] => match lists.get(one) {
                    Some(l) => (Some(*l), None),
                    None => (None, Some("The list it signs people up to wasn't imported. Choose a list before publishing it.".to_string())),
                },
                many => (None, Some(format!("It signs people up to {} different lists ({}), depending on the step or version. Choose the list before publishing it.", many.len(), many.join(", ")))),
            };
            let id = Uuid::now_v7();
            let kstatus = f["attributes"]["status"].as_str().unwrap_or("");
            let mut tx = self.begin().await?;
            // A form whose list is missing or ambiguous can't be published until someone picks one.
            let notes: Vec<String> = problem.iter().cloned().collect();
            sqlx::query("INSERT INTO forms (id, account_id, name, status, kind, list_id, config, import_review) VALUES ($1, $2, $3, 'draft', 'popup', $4, '{}', $5)")
                .bind(id)
                .bind(self.acct())
                .bind(&name)
                .bind(list)
                .bind(review::column(&notes))
                .execute(&mut *tx)
                .await?;
            store::set_ref(&mut *tx, self.acct(), "form", kid, Some(id), &name, &json!({"klaviyo_status": kstatus, "list_ids": ids})).await?;
            store::bump(&mut *tx, self.imp.id, "forms", 1, 0).await?;
            if let Some(p) = &problem {
                store::issue(&mut tx, &self.imp, "forms", "unmapped", &name, Some(kid), Some(id), p).await?;
            }
            tx.commit().await?;
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

    /// The events step ended without writing (left out): close the history window.
    async fn close_history(&self) -> anyhow::Result<()> {
        let mut tx = self.begin().await?;
        super::history::close(&mut tx, self.imp.id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn step_events(&mut self, step: &str) -> anyhow::Result<bool> {
        let store_events = self.imp.wants("events");
        let url = match self.imp.cursor.clone() {
            Some(c) => c,
            None => {
                let mut u = format!("/api/events?sort=datetime&page%5Bsize%5D={EVENT_PAGE}");
                if let Some(since) = self.imp.events_since().map(fmt_ts) {
                    u.push_str(&format!("&filter={}", q(&format!("greater-or-equal(datetime,{since})"))));
                }
                u
            }
        };
        let page = self.k.get(&url).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let metrics = self.metric_map().await?.clone();
        let wanted = |e: &Value| rel_ids(e, "metric").first().and_then(|m| metrics.get(m)).and_then(|(_, l)| *l).is_some();
        let pkids: Vec<String> = data.iter().filter(|e| wanted(e)).filter_map(|e| rel_ids(e, "profile").first().cloned()).collect::<HashSet<_>>().into_iter().collect();
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
        if store_events {
            // People Klaviyo created since the profile scan: read them (with their consent) first. Every
            // one on the page, before the page is checkpointed.
            let missing: Vec<String> = pkids.iter().filter(|p| !pmap.contains_key(*p)).cloned().collect();
            pmap.extend(self.fetch_profiles(&missing).await?);
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
        for e in &data {
            let a = &e["attributes"];
            if !store_events {
                continue;
            }
            let Some((mname, Some(metric_id))) = rel_ids(e, "metric").first().and_then(|m| metrics.get(m)).cloned() else { continue };
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
            // The latest message Klaviyo delivered, for the "unsubscribes up to date" check.
            if matches!(mname.as_str(), "Received Email" | "Received SMS") && ts(self.st("last_send")).is_none_or(|t| t < at) {
                self.set_st("last_send", json!(fmt_ts(at)));
            }
            let value = props.get("$value").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|x| x.parse().ok())));
            batch.push(crate::audience::HistoryEvent { profile_id: *pid, metric_id, properties: Value::Object(props), value, occurred_at: at, unique_id: format!("klaviyo:{eid}") });
            done += 1;
        }
        let _ = done;
        let next = next_link(&page);
        // One transaction: the events, their progress (only events actually new, so a replayed page counts
        // nothing twice), and the history window (open while pages are written, closed with the last).
        let mut tx = self.begin().await?;
        if !batch.is_empty() {
            super::history::open(&mut tx, self.imp.id, self.acct()).await?;
        }
        let new = crate::audience::record_history(&mut *tx, self.acct(), &batch).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        store::bump(&mut *tx, self.imp.id, step, new as i64, skipped).await?;
        if next.is_none() {
            super::history::close(&mut tx, self.imp.id).await?;
        }
        tx.commit().await?;
        self.imp.cursor = next;
        Ok(self.imp.cursor.is_none())
    }

    // ---------- the Klaviyo side of the comparison report ----------

    async fn step_compare(&mut self) -> anyhow::Result<bool> {
        if self.st("cmp_until").is_null() {
            self.set_st("cmp_until", json!(fmt_ts(Utc::now())));
            self.set_st("cmp_stage", json!(0));
            self.set_st("cmp_i", json!(0));
            let mut tx = self.begin().await?;
            sqlx::query("UPDATE klaviyo_imports SET source_counts = source_counts || jsonb_build_object('until', $2::text, 'lists', '{}'::jsonb, 'segments', '{}'::jsonb, 'metrics', '{}'::jsonb) WHERE id = $1")
                .bind(self.imp.id)
                .bind(self.st("cmp_until").as_str())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        let until = ts(self.st("cmp_until")).unwrap_or_else(Utc::now);
        let stage = self.st_i("cmp_stage");
        let i = self.st_i("cmp_i");
        match stage {
            0 | 1 => {
                let kind = if stage == 0 { "list" } else { "segment" };
                let key = if stage == 0 { "lists" } else { "segments" };
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
                // Klaviyo's own count, or why there isn't one (the report then says "not verified").
                let entry = match self.k.get_paced(&path, 15).await {
                    Ok(v) => match v["data"]["attributes"]["profile_count"].as_i64().filter(|n| *n >= 0) {
                        Some(n) => json!(n),
                        None => json!({"error": "Klaviyo didn't give a count"}),
                    },
                    Err(KlaviyoError::Status { status: 404, .. }) => json!({"error": "deleted in Klaviyo since"}),
                    Err(e @ KlaviyoError::Status { .. }) => json!({"error": e.to_string()}),
                    Err(e) => return Err(e.into()),
                };
                let mut tx = self.begin().await?;
                sqlx::query("UPDATE klaviyo_imports SET source_counts = jsonb_set(source_counts, ARRAY[$2, $3], $4, true) WHERE id = $1")
                    .bind(self.imp.id)
                    .bind(key)
                    .bind(kid)
                    .bind(&entry)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
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
                // "All history" starts when Klaviyo created the metric (Klaviyo's own date, not ours).
                let since = match self.imp.events_since() {
                    Some(s) => Some(s),
                    None => store::ref_local(&self.state.db, self.acct(), "metric", &kid).await?.and_then(|(_, d)| ts(&d["created"])),
                };
                let tz = sqlx::query_scalar::<_, String>("SELECT timezone FROM accounts WHERE id = $1").bind(self.acct()).fetch_one(&self.state.db).await?;
                let mut months: Map<String, Value> = Map::new();
                let mut error: Option<String> = None;
                match since {
                    None => error = Some("Klaviyo didn't say when this metric started, so the range can't be counted".into()),
                    Some(since) => {
                        let mut start = since;
                        while start < until {
                            let end = (start + chrono::Duration::days(360)).min(until);
                            let body = json!({"data": {"type": "metric-aggregate", "attributes": {
                                "metric_id": kid, "measurements": ["count", "sum_value"], "interval": "month", "page_size": 500, "timezone": tz,
                                "filter": [format!("greater-or-equal(datetime,{})", start.format("%Y-%m-%dT%H:%M:%S")), format!("less-than(datetime,{})", end.format("%Y-%m-%dT%H:%M:%S"))]}}});
                            match self.k.post("/api/metric-aggregates", &body).await {
                                Ok(v) => match aggregate_months(&v) {
                                    Ok(got) => {
                                        for (d, (c, sv)) in got {
                                            let e = months.entry(d).or_insert(json!([0.0, 0.0]));
                                            e[0] = json!(e[0].as_f64().unwrap_or(0.0) + c);
                                            e[1] = json!(e[1].as_f64().unwrap_or(0.0) + sv);
                                        }
                                    }
                                    // A malformed or partial answer is no number at all, never zero.
                                    Err(why) => {
                                        error = Some(why);
                                        months.clear();
                                        break;
                                    }
                                },
                                Err(e @ KlaviyoError::Status { .. }) => {
                                    error = Some(format!("Klaviyo wouldn't count this metric's events ({e})"));
                                    months.clear();
                                    break;
                                }
                                Err(e) => return Err(e.into()),
                            }
                            start = end;
                        }
                    }
                }
                let mut tx = self.begin().await?;
                sqlx::query("UPDATE klaviyo_imports SET source_counts = jsonb_set(source_counts, ARRAY['metrics', $2], $3, true) WHERE id = $1")
                    .bind(self.imp.id)
                    .bind(&kid)
                    .bind(json!({"name": name, "since": since.map(fmt_ts), "months": months, "error": error}))
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                self.set_st("cmp_i", json!(i + 1));
                Ok(false)
            }
            _ => {
                let mut tx = self.begin().await?;
                sqlx::query("UPDATE klaviyo_imports SET compared_at = now(), report_cached_at = NULL WHERE id = $1").bind(self.imp.id).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(true)
            }
        }
    }

    async fn step(&mut self, phase: &str) -> anyhow::Result<bool> {
        match phase {
            "metrics" => self.step_metrics().await,
            "profiles" => self.step_profiles(phase).await,
            "lists" => self.step_lists().await,
            "list_members" => self.step_members(phase, false).await,
            "segments" => self.step_segments().await,
            "segment_members" => self.step_members(phase, true).await,
            "templates" => self.step_templates().await,
            "campaigns" => self.step_campaigns().await,
            "campaign_stats" => self.step_campaign_stats().await,
            "flows" => self.step_flows().await,
            "forms" => self.step_forms().await,
            "events" => self.step_events(phase).await,
            "compare" => self.step_compare().await,
            _ => Ok(true),
        }
    }

    fn wanted(&self, phase: &str) -> bool {
        match phase {
            "lists" | "list_members" => self.imp.wants("lists"),
            "segments" | "segment_members" => self.imp.wants("segments"),
            "templates" => self.imp.wants("templates"),
            "campaigns" | "campaign_stats" => self.imp.wants("campaigns"),
            "flows" => self.imp.wants("flows"),
            "forms" => self.imp.wants("forms"),
            "events" => self.imp.wants("events"),
            // Consent steps always run.
            _ => true,
        }
    }

    fn next_phase(&self, current: Option<&str>) -> Option<&'static str> {
        let start = match current {
            Some(c) => STEPS.iter().position(|s| *s == c).map(|p| p + 1).unwrap_or(0),
            None => 0,
        };
        STEPS[start..].iter().copied().find(|p| self.wanted(p))
    }

    /// A step is done: record it.
    async fn step_done(&mut self, phase: &str) -> anyhow::Result<()> {
        self.finished.push(phase.to_string());
        self.imp.cursor = None;
        self.set_st("group", json!(0));
        if phase == "profiles" {
            let n = self.st("scan_profiles").as_i64().unwrap_or(0);
            let mut tx = self.begin().await?;
            sqlx::query("UPDATE klaviyo_imports SET source_counts = source_counts || jsonb_build_object('profiles_scan', jsonb_build_object('total', $2::bigint, 'complete', true, 'import', $3::text, 'at', now())) WHERE id = $1")
                .bind(self.imp.id)
                .bind(n)
                .bind(self.imp.id.to_string())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        self.imp.phase = self.next_phase(Some(phase)).map(String::from);
        Ok(())
    }
}

/// Monthly (count, sum of values) from a metric-aggregates answer, keyed by YYYY-MM. Anything missing or
/// out of shape (no dates, no row, a measurement array of the wrong length, a value that isn't a number)
/// is an error: the report then says "not verified" instead of treating it as zero.
pub fn aggregate_months(v: &Value) -> Result<Vec<(String, (f64, f64))>, String> {
    let bad = || "Klaviyo's count for this metric came back incomplete".to_string();
    let a = &v["data"]["attributes"];
    let dates: Vec<String> = a["dates"].as_array().ok_or_else(bad)?.iter().map(|d| d.as_str().and_then(|d| d.get(..7)).map(String::from).ok_or_else(bad)).collect::<Result<_, _>>()?;
    let rows = a["data"].as_array().ok_or_else(bad)?;
    if rows.is_empty() && !dates.is_empty() {
        return Err(bad());
    }
    let mut out: Vec<(String, (f64, f64))> = dates.iter().map(|d| (d.clone(), (0.0, 0.0))).collect();
    for row in rows {
        let counts = row["measurements"]["count"].as_array().ok_or_else(bad)?;
        let sums = row["measurements"]["sum_value"].as_array().ok_or_else(bad)?;
        if counts.len() != dates.len() || sums.len() != dates.len() {
            return Err(bad());
        }
        for (j, slot) in out.iter_mut().enumerate() {
            slot.1 .0 += counts[j].as_f64().ok_or_else(bad)?;
            slot.1 .1 += sums[j].as_f64().ok_or_else(bad)?;
        }
    }
    Ok(out)
}

/// Where a Klaviyo form signs people up: the `list_id` of a sign-up action in the live versions (or every
/// version when none is live). A sign-up action is a submitting (`submit: true`) next step, close or
/// redirect; other actions that carry a `list_id` (a back-in-stock request) aren't sign-ups. More than
/// one, or none, is for a person to pick.
pub fn form_destinations(def: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if let Some(a) = o.get("action").filter(|a| a.is_object()) {
                    let signup = matches!(a["type"].as_str(), Some("next_step" | "close" | "redirect"));
                    let submits = signup && a["submit"] == true;
                    if let Some(id) = a["properties"]["list_id"].as_str().filter(|i| !i.is_empty()) {
                        if submits && !out.iter().any(|x| x == id) {
                            out.push(id.to_string());
                        }
                    }
                }
                for x in o.values() {
                    walk(x, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let versions = def["versions"].as_array().cloned().unwrap_or_default();
    let live: Vec<&Value> = versions.iter().filter(|v| v["status"] == "live").collect();
    let chosen: Vec<&Value> = if live.is_empty() { versions.iter().collect() } else { live };
    let mut out = Vec::new();
    for v in chosen {
        walk(v, &mut out);
    }
    out
}

/// Steps where a missing scope only means that part is left out.
fn optional_step(phase: &str) -> bool {
    matches!(phase, "segments" | "segment_members" | "templates" | "campaigns" | "campaign_stats" | "flows" | "forms" | "compare")
}

/// Claim the import for this run. Another live run keeps it.
async fn claim(db: &sqlx::PgPool, id: Uuid, owner: Uuid) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE klaviyo_imports SET lease_owner = $2, lease_until = now() + make_interval(secs => $3)
         WHERE id = $1 AND (lease_until IS NULL OR lease_until < now() OR lease_owner = $2)",
    )
    .bind(id)
    .bind(owner)
    .bind(LEASE_SECS as f64)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

async fn release(db: &sqlx::PgPool, id: Uuid, owner: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE klaviyo_imports SET lease_owner = NULL, lease_until = NULL WHERE id = $1 AND lease_owner = $2").bind(id).bind(owner).execute(db).await?;
    Ok(())
}

pub async fn run(state: &AppState, import_id: Uuid) -> anyhow::Result<()> {
    let Some(imp) = store::load(&state.db, import_id).await? else { return Ok(()) };
    if imp.status != "importing" {
        return Ok(());
    }
    let Some(key) = store::key(state, &imp) else {
        store::close(&state.db, imp.id, "failed", Some("The Klaviyo key is no longer stored. Connect again to continue.")).await?;
        return Ok(());
    };
    let mut r = Run::new(state, imp, &key);
    if !claim(&state.db, r.imp.id, r.owner).await? {
        // Another worker has it; look again after its lease.
        store::enqueue(&state.db, JOB_RUN, r.imp.id, Some(r.imp.lease_until.unwrap_or_else(Utc::now) + chrono::Duration::seconds(5))).await?;
        return Ok(());
    }
    let res = drive(&mut r).await;
    release(&state.db, r.imp.id, r.owner).await?;
    match res {
        // Paused, cancelled or taken over while this run worked: it stops without writing.
        Err(e) if e.is::<Superseded>() => Ok(()),
        other => other,
    }
}

async fn drive(r: &mut Run<'_>) -> anyhow::Result<()> {
    let state = r.state;
    if r.imp.phase.is_none() {
        r.imp.phase = r.next_phase(None).map(String::from);
        if !r.save().await? {
            return Ok(());
        }
    }
    loop {
        let Some(phase) = r.imp.phase.clone() else {
            return match finish(r).await {
                Err(e) if e.is::<Superseded>() => Ok(()),
                other => other,
            };
        };
        match r.step(&phase).await {
            Ok(done) => {
                if r.st("retries").as_u64().unwrap_or(0) > 0 {
                    r.set_st("retries", json!(0));
                }
                if done {
                    r.step_done(&phase).await?;
                }
                if !r.save().await? {
                    return Ok(());
                }
            }
            Err(e) if e.is::<Superseded>() => return Ok(()),
            Err(e) => {
                let ke = e.downcast_ref::<KlaviyoError>();
                match ke {
                    Some(KlaviyoError::RateLimited(secs)) => {
                        let secs = *secs;
                        r.save().await?;
                        store::enqueue(&state.db, JOB_RUN, r.imp.id, Some(Utc::now() + chrono::Duration::seconds(secs as i64))).await?;
                        return Ok(());
                    }
                    Some(KlaviyoError::MissingScope(what)) if optional_step(&phase) => {
                        let msg = format!("{}. This part was left out.", KlaviyoError::MissingScope(what.clone()));
                        r.issue(&phase, "skipped", "Klaviyo access", None, None, &msg).await?;
                        if phase == "events" {
                            r.close_history().await?;
                        }
                        r.step_done(&phase).await?;
                        if !r.save().await? {
                            return Ok(());
                        }
                    }
                    Some(k @ (KlaviyoError::BadKey | KlaviyoError::MissingScope(_))) => {
                        store::close(&state.db, r.imp.id, "failed", Some(&k.to_string())).await?;
                        return Ok(());
                    }
                    _ => {
                        let n = r.st("retries").as_u64().unwrap_or(0) + 1;
                        // Only Klaviyo's status and error code, or our own wording: never Klaviyo's detail text.
                        let msg = match ke {
                            Some(k) => k.to_string(),
                            None => format!("{e:#}").chars().take(300).collect(),
                        };
                        tracing::warn!(import = %r.imp.id, phase = %phase, attempt = n, error = %msg, "klaviyo import step failed");
                        if n > 6 {
                            let text = format!("Stopped at the {} step: {msg}. Imported data is kept; connect again to pick up where it left off.", phase.replace('_', " "));
                            store::close(&state.db, r.imp.id, "failed", Some(&text)).await?;
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
        if status.as_deref() != Some("importing") {
            return Ok(());
        }
    }
}

async fn finish(r: &mut Run<'_>) -> anyhow::Result<()> {
    // Done, and the key is deleted, in one transaction that still owns the run.
    let mut tx = r.begin().await?;
    sqlx::query(
        "UPDATE klaviyo_imports SET status = 'done', phase = NULL, cursor = NULL, finished_at = now(), report_cached_at = NULL,
            api_key_enc = NULL, lease_owner = NULL, lease_until = NULL, updated_at = now()
         WHERE id = $1 AND status = 'importing'",
    )
    .bind(r.imp.id)
    .execute(&mut *tx)
    .await?;
    // The last Klaviyo send this run found (a sent campaign, or an email or text Klaviyo delivered), for
    // the "unsubscribes up to date" check.
    sqlx::query(
        "UPDATE klaviyo_imports SET source_counts = source_counts || jsonb_build_object('last_klaviyo_send', (SELECT GREATEST(
            (SELECT max(c.sent_at) FROM klaviyo_refs r JOIN campaigns c ON c.id = r.local_id WHERE r.account_id = $2 AND r.kind = 'campaign_message'),
            $3::timestamptz)))
         WHERE id = $1",
    )
    .bind(r.imp.id)
    .bind(r.acct())
    .bind(ts(r.st("last_send")))
    .execute(&mut *tx)
    .await?;
    super::history::close(&mut tx, r.imp.id).await?;
    tx.commit().await?;
    Ok(())
}

/// Imports that should be running but have no job (a worker died between steps): queue them again.
pub async fn restart_stalled(db: &sqlx::PgPool) -> anyhow::Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM klaviyo_imports i WHERE i.status = 'importing' AND i.updated_at < now() - interval '20 minutes'
           AND (i.lease_until IS NULL OR i.lease_until < now())
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $1 AND j.payload->>'import_id' = i.id::text AND j.run_at <> 'infinity')
         LIMIT 100",
    )
    .bind(JOB_RUN)
    .fetch_all(db)
    .await?;
    for id in ids {
        store::enqueue(db, JOB_RUN, id, None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn form_destinations_follow_the_submit_action_of_live_versions() {
        let def = json!({"versions": [
            {"status": "live", "steps": [{"columns": [{"rows": [{"components": [
                {"type": "button", "action": {"type": "next_step", "submit": true, "properties": {"list_id": "L1"}}},
                {"type": "button", "action": {"type": "close", "submit": false, "properties": {"list_id": "L9"}}}]}]}]}]},
            {"status": "draft", "steps": [{"columns": [{"rows": [{"components": [{"type": "button", "action": {"type": "next_step", "submit": true, "properties": {"list_id": "L2"}}}]}]}]}]}
        ]});
        assert_eq!(super::form_destinations(&def), vec!["L1"]);
        let two = json!({"versions": [
            {"status": "live", "steps": [{"components": [{"action": {"type": "next_step", "submit": true, "properties": {"list_id": "L1"}}}]}]},
            {"status": "live", "steps": [{"components": [{"action": {"type": "redirect", "submit": true, "properties": {"list_id": "L2", "url": "x"}}}]}]}
        ]});
        assert_eq!(super::form_destinations(&two), vec!["L1", "L2"]);
        let bis = json!({"versions": [{"status": "live", "steps": [{"components": [
            {"action": {"type": "submit_back_in_stock", "submit": true, "properties": {"list_id": "LBIS"}}},
            {"action": {"type": "next_step", "submit": true, "properties": {"list_id": "L1"}}}]}]}]});
        assert_eq!(super::form_destinations(&bis), vec!["L1"], "a back-in-stock request isn't a sign-up");
        assert!(super::form_destinations(&json!({"versions": []})).is_empty());
    }
}

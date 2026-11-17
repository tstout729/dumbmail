//! The switch-over report: Klaviyo's numbers next to DumbMail's, line by line.
//!
//! Each line is **match**, **mismatch** or **not verified**. A line matches only when both numbers come
//! from independent sources (Klaviyo's own count, or a full Klaviyo scan that completed, against
//! DumbMail's own data) and agree within the stated tolerance. Anything capped, failed, partial or
//! computed from the importer's own bookkeeping is "not verified", with the reason. The headline ("nothing
//! was lost") shows only when there are no mismatches and nothing unverified.
//!
//! Consent lines compare person by person: what Klaviyo said about each profile (kept verbatim on its
//! reference during a scan that read every Klaviyo profile) against that profile's state in DumbMail now.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::runner::{MAX_COMPARED_GROUPS, ORDER_METRICS};
use super::store::Import;
use crate::error::AppResult;
use crate::state::AppState;

pub const MATCH: &str = "match";
pub const MISMATCH: &str = "mismatch";
pub const NOT_VERIFIED: &str = "not_verified";

#[derive(Default)]
struct Lines(Vec<Value>);

impl Lines {
    fn push(&mut self, section: &str, label: &str, klaviyo: Value, dumbmail: Value, state: &str, note: impl Into<String>) {
        self.0.push(json!({"section": section, "label": label, "klaviyo": klaviyo, "dumbmail": dumbmail, "state": state, "ok": state == MATCH, "note": note.into()}));
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn ts(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc))
}

/// The report from cache when it's younger than `max_age` (the page polls; this keeps it cheap).
pub async fn cached(state: &AppState, imp: &Import, max_age: Duration) -> AppResult<Value> {
    if let (Some(r), Some(at)) = (&imp.report_cache, imp.report_cached_at) {
        if at > Utc::now() - max_age {
            return Ok(r.clone());
        }
    }
    let r = build(state, imp).await?;
    sqlx::query("UPDATE klaviyo_imports SET report_cache = $2, report_cached_at = now() WHERE id = $1").bind(imp.id).bind(&r).execute(&state.db).await?;
    Ok(r)
}

#[derive(sqlx::FromRow, Default)]
struct Consent {
    e_unsub_k: i64,
    e_unsub_d: i64,
    e_sup_k: i64,
    e_sup_d: i64,
    e_sub_k: i64,
    e_sub_d: i64,
    e_sub_r: i64,
    e_never_k: i64,
    e_never_d: i64,
    e_never_held: i64,
    e_never_kept: i64,
    e_never_r: i64,
    e_unknown_k: i64,
    e_unknown_d: i64,
    e_more: i64,
    e_kept: i64,
    s_unsub_k: i64,
    s_unsub_d: i64,
    s_sub_k: i64,
    s_sub_d: i64,
    s_sub_r: i64,
    s_more: i64,
    s_kept: i64,
}

pub async fn build(state: &AppState, imp: &Import) -> AppResult<Value> {
    let db = &state.db;
    let a = imp.account_id;
    let mut l = Lines::default();
    let src = &imp.source_counts;

    // ----- Profiles -----
    let scan = &src["profiles_scan"];
    let scan_done = scan["complete"] == true;
    let scan_total = scan["total"].as_i64();
    let scan_at = ts(&scan["at"]);
    let (seen, at_scan, linked, skipped, gone, conflicts): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE r.created_at <= COALESCE($2, now())), count(DISTINCT p.id), count(*) FILTER (WHERE r.local_id IS NULL),
                count(*) FILTER (WHERE r.local_id IS NOT NULL AND p.id IS NULL), count(*) FILTER (WHERE p.id IS NOT NULL AND jsonb_typeof(r.detail->'conflict') = 'array')
         FROM klaviyo_refs r LEFT JOIN profiles p ON p.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'profile'",
    )
    .bind(a)
    .bind(scan_at)
    .fetch_one(db)
    .await?;
    let merged = (seen - skipped - gone - linked).max(0);
    let mut note = Vec::new();
    if merged > 0 {
        note.push(format!("{} shared an email, phone number or external id with another profile and were combined into one", plural(merged, "Klaviyo profile", "Klaviyo profiles")));
    }
    if skipped > 0 {
        note.push(format!("{} had no usable email, phone number or external id (listed below)", plural(skipped, "profile", "profiles")));
    }
    if gone > 0 {
        note.push(format!("{} were deleted in DumbMail after the import", plural(gone, "profile", "profiles")));
    }
    match (scan_done, scan_total) {
        (true, Some(total)) => {
            let state = if gone == 0 && at_scan >= total { MATCH } else { MISMATCH };
            if at_scan < total {
                note.push(format!("{} Klaviyo returned never reached DumbMail. Run the import again", plural(total - at_scan, "profile", "profiles")));
            }
            l.push("Profiles", "Profiles", json!(total), json!(linked), state, format!("Klaviyo: every profile read in a full scan. DumbMail: profiles made from them. {}", note.join(". ")).trim().to_string());
        }
        _ => l.push("Profiles", "Profiles", Value::Null, json!(linked), NOT_VERIFIED, "The scan of every Klaviyo profile didn't finish, so the total can't be checked."),
    }
    l.push(
        "Profiles",
        "Email or phone differs between Klaviyo and DumbMail",
        json!(0),
        json!(conflicts),
        if conflicts == 0 { MATCH } else { MISMATCH },
        if conflicts == 0 { "None.".to_string() } else { "Klaviyo's consent for those addresses or numbers wasn't applied. Each is listed below with both values; decide which is right and update the profile.".into() },
    );

    // ----- Consent, person by person -----
    let c: Consent = sqlx::query_as(
        "WITH x AS (
            SELECT r.detail AS d, p.email_consent AS ec, p.sms_consent AS sc, p.email_suppressed_reason AS sup, p.marketing_hold AS hold,
                   COALESCE((r.detail->>'applied_email')::boolean, false) AS ae, COALESCE((r.detail->>'applied_sms')::boolean, false) AS asm,
                   r.detail->'email'->>'state' AS es, COALESCE((r.detail->'email'->>'blocked')::boolean, false) AS eb, COALESCE((r.detail->'email'->>'can_receive')::boolean, false) AS ecan,
                   COALESCE(r.detail->'sms'->>'state', 'none') AS ss,
                   COALESCE((r.detail->>'kept_dumbmail_email')::boolean, false) AS ekept, COALESCE((r.detail->>'kept_dumbmail_sms')::boolean, false) AS skept,
                   (p.email_consent IN ('unsubscribed', 'pending') OR p.email_suppressed_reason IS NOT NULL) AS erestr,
                   (p.email IS NOT NULL AND p.email_consent NOT IN ('unsubscribed', 'pending') AND p.email_suppressed_reason IS NULL AND NOT p.marketing_hold) AS ereach,
                   (p.phone_number IS NOT NULL AND p.sms_consent = 'subscribed' AND NOT p.marketing_hold) AS sreach
            FROM klaviyo_refs r JOIN profiles p ON p.id = r.local_id
            WHERE r.account_id = $1 AND r.kind = 'profile')
         SELECT
            count(*) FILTER (WHERE ae AND es = 'unsubscribed') AS e_unsub_k,
            count(*) FILTER (WHERE ae AND es = 'unsubscribed' AND ec = 'unsubscribed') AS e_unsub_d,
            count(*) FILTER (WHERE ae AND eb AND es <> 'unsubscribed') AS e_sup_k,
            count(*) FILTER (WHERE ae AND eb AND es <> 'unsubscribed' AND (sup IS NOT NULL OR ec = 'unsubscribed')) AS e_sup_d,
            count(*) FILTER (WHERE ae AND es = 'subscribed' AND NOT eb) AS e_sub_k,
            count(*) FILTER (WHERE ae AND es = 'subscribed' AND NOT eb AND ec = 'subscribed' AND sup IS NULL) AS e_sub_d,
            count(*) FILTER (WHERE ae AND es = 'subscribed' AND NOT eb AND erestr) AS e_sub_r,
            count(*) FILTER (WHERE ae AND es = 'never' AND NOT eb) AS e_never_k,
            count(*) FILTER (WHERE ae AND es = 'never' AND NOT eb AND ec = 'never_subscribed' AND sup IS NULL) AS e_never_d,
            count(*) FILTER (WHERE ae AND es = 'never' AND NOT eb AND ec = 'never_subscribed' AND sup IS NULL AND hold) AS e_never_held,
            count(*) FILTER (WHERE ae AND es = 'never' AND NOT eb AND ekept) AS e_never_kept,
            count(*) FILTER (WHERE ae AND es = 'never' AND NOT eb AND erestr) AS e_never_r,
            count(*) FILTER (WHERE ae AND es = 'unknown' AND NOT eb) AS e_unknown_k,
            count(*) FILTER (WHERE ae AND es = 'unknown' AND NOT eb AND (hold OR ekept OR erestr)) AS e_unknown_d,
            count(*) FILTER (WHERE ae AND ereach AND NOT ecan AND NOT ekept) AS e_more,
            count(*) FILTER (WHERE ae AND ekept) AS e_kept,
            count(*) FILTER (WHERE asm AND ss = 'unsubscribed') AS s_unsub_k,
            count(*) FILTER (WHERE asm AND ss = 'unsubscribed' AND sc = 'unsubscribed') AS s_unsub_d,
            count(*) FILTER (WHERE asm AND ss = 'subscribed') AS s_sub_k,
            count(*) FILTER (WHERE asm AND ss = 'subscribed' AND sc = 'subscribed') AS s_sub_d,
            count(*) FILTER (WHERE asm AND ss = 'subscribed' AND sc IN ('unsubscribed', 'pending')) AS s_sub_r,
            count(*) FILTER (WHERE asm AND sreach AND ss <> 'subscribed' AND NOT skept) AS s_more,
            count(*) FILTER (WHERE asm AND skept) AS s_kept
         FROM x",
    )
    .bind(a)
    .fetch_one(db)
    .await?;
    let covered = scan_done && scan_total.is_some_and(|t| at_scan >= t);
    let verdict = |ok: bool| if !covered { NOT_VERIFIED } else if ok { MATCH } else { MISMATCH };
    let unverified_note = "Not every Klaviyo profile was read (the profile scan didn't finish), so this can't be checked.";
    let with = |base: String| if covered { base } else { format!("{unverified_note} {base}") };
    l.push("Email", "Unsubscribed in Klaviyo", json!(c.e_unsub_k), json!(c.e_unsub_d), verdict(c.e_unsub_k == c.e_unsub_d),
        with(if c.e_unsub_k == c.e_unsub_d { "Every one is unsubscribed here.".into() } else { format!("{} aren't unsubscribed here. Run the import again; if it still differs, contact support with this report.", c.e_unsub_k - c.e_unsub_d) }));
    l.push("Email", "Suppressed in Klaviyo (bounced, complained, invalid, by hand, or not emailable)", json!(c.e_sup_k), json!(c.e_sup_d), verdict(c.e_sup_k == c.e_sup_d),
        with(if c.e_sup_k == c.e_sup_d { "Every one is suppressed or unsubscribed here.".into() } else { format!("{} can still be emailed here. Run the import again.", c.e_sup_k - c.e_sup_d) }));
    l.push("Email", "Subscribed in Klaviyo", json!(c.e_sub_k), json!(c.e_sub_d), verdict(c.e_sub_k == c.e_sub_d + c.e_sub_r),
        with(if c.e_sub_r > 0 { format!("{} are unsubscribed, waiting to confirm or suppressed in DumbMail, and that wins.", c.e_sub_r) } else { String::new() }));
    l.push("Email", "Never subscribed in Klaviyo", json!(c.e_never_k), json!(c.e_never_d), verdict(c.e_never_k == c.e_never_d + c.e_never_kept + c.e_never_r),
        with({
            let mut n = Vec::new();
            if c.e_never_held > 0 { n.push(format!("{} held back from marketing until they opt in, as chosen", c.e_never_held)); }
            if c.e_never_kept > 0 { n.push(format!("{} had subscribed through DumbMail, which is kept", c.e_never_kept)); }
            if c.e_never_r > 0 { n.push(format!("{} are unsubscribed or suppressed in DumbMail", c.e_never_r)); }
            n.join("; ")
        }));
    if c.e_unknown_k > 0 {
        l.push("Email", "Consent unknown in Klaviyo (held back)", json!(c.e_unknown_k), json!(c.e_unknown_d), verdict(c.e_unknown_k == c.e_unknown_d),
            with("Klaviyo had no consent record DumbMail recognises for them, so they're held back from marketing until they opt in.".into()));
    }
    l.push("Email", "More reachable here than in Klaviyo", json!(0), json!(c.e_more), verdict(c.e_more == 0),
        with(if c.e_more == 0 { "Nobody can get marketing email here that Klaviyo wouldn't have sent.".into() } else { "These people can get marketing email here though Klaviyo wouldn't email them. Contact support with this report.".into() }));
    if c.e_kept > 0 {
        l.push("Email", "Kept their DumbMail opt-in", Value::Null, json!(c.e_kept), MATCH,
            "They subscribed through DumbMail directly and Klaviyo had no opt-out for them, so their consent stays.");
    }
    l.push("SMS", "Unsubscribed in Klaviyo", json!(c.s_unsub_k), json!(c.s_unsub_d), verdict(c.s_unsub_k == c.s_unsub_d),
        with(if c.s_unsub_k == c.s_unsub_d { "Every one is unsubscribed here.".into() } else { "Some can still be texted here. Run the import again.".into() }));
    l.push("SMS", "Subscribed and textable in Klaviyo", json!(c.s_sub_k), json!(c.s_sub_d), verdict(c.s_sub_k == c.s_sub_d + c.s_sub_r),
        with(if c.s_sub_r > 0 { format!("{} replied STOP or are waiting to confirm in DumbMail, and that wins.", c.s_sub_r) } else { String::new() }));
    l.push("SMS", "More textable here than in Klaviyo", json!(0), json!(c.s_more), verdict(c.s_more == 0),
        with(if c.s_more == 0 { "Nobody can get texts here that Klaviyo wouldn't have sent.".into() } else { "These people can get texts here though Klaviyo couldn't text them. Contact support with this report.".into() }));
    if c.s_kept > 0 {
        l.push("SMS", "Kept their DumbMail opt-in", Value::Null, json!(c.s_kept), MATCH, "They opted in to texts through DumbMail directly and Klaviyo had no opt-out for them.");
    }

    // ----- List-level opt-outs -----
    let (lo_k, lo_d): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE m.state = 'left' AND NOT EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = lr.local_id AND lm.profile_id = pr.local_id))
         FROM klaviyo_refs pr CROSS JOIN LATERAL jsonb_array_elements_text(COALESCE(pr.detail->'list_suppressions', '[]')) AS ls(kid)
         JOIN klaviyo_refs lr ON lr.account_id = pr.account_id AND lr.kind = 'list' AND lr.klaviyo_id = ls.kid AND lr.local_id IS NOT NULL
         LEFT JOIN klaviyo_memberships m ON m.list_id = lr.local_id AND m.profile_id = pr.local_id
         WHERE pr.account_id = $1 AND pr.kind = 'profile' AND pr.local_id IS NOT NULL",
    )
    .bind(a)
    .fetch_one(db)
    .await?;
    l.push("List opt-outs", "Unsubscribed from a list in Klaviyo", json!(lo_k), json!(lo_d), verdict(lo_k == lo_d),
        with(if lo_k == lo_d { "Each is off that list here and won't be added back; their other lists aren't touched.".into() } else { format!("{} are still on the list they left. Run the import again.", lo_k - lo_d) }));

    // ----- Lists -----
    let lists: Vec<(String, String, Uuid, i64, i64, Value)> = sqlx::query_as(
        "SELECT r.klaviyo_id, COALESCE(r.name, ''), r.local_id,
                (SELECT count(*) FROM klaviyo_memberships m JOIN list_members lm ON lm.list_id = m.list_id AND lm.profile_id = m.profile_id WHERE m.list_id = r.local_id AND m.state = 'member'),
                (SELECT count(*) FROM list_members lm WHERE lm.list_id = r.local_id),
                COALESCE(r.detail->'scan', 'null')
         FROM klaviyo_refs r JOIN lists li ON li.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' ORDER BY r.klaviyo_id",
    )
    .bind(a)
    .fetch_all(db)
    .await?;
    for (i, (kid, name, _, from_k, total, scan)) in lists.iter().enumerate() {
        let others = total - from_k;
        let extra = if others > 0 { format!(" {} more joined in DumbMail.", others) } else { String::new() };
        if i >= MAX_COMPARED_GROUPS {
            l.push("Lists", name, Value::Null, json!(from_k), NOT_VERIFIED, format!("Only the first {MAX_COMPARED_GROUPS} lists are checked against Klaviyo.{extra}"));
            continue;
        }
        let Some(k) = src["lists"][kid].as_i64() else {
            let why = src["lists"][kid]["error"].as_str().map(|e| format!("Klaviyo's size couldn't be fetched ({e}).")).unwrap_or_else(|| "Klaviyo's size wasn't fetched.".into());
            l.push("Lists", name, Value::Null, json!(from_k), NOT_VERIFIED, format!("{why}{extra}"));
            continue;
        };
        if !scan.is_object() {
            l.push("Lists", name, json!(k), json!(from_k), NOT_VERIFIED, format!("The scan of this list's members didn't finish.{extra}"));
            continue;
        }
        let skipped = scan["skipped"].as_i64().unwrap_or(0);
        let tol = tolerance(k);
        let state = if (k - (from_k + skipped)).abs() <= tol { MATCH } else { MISMATCH };
        let mut note = format!("Members from Klaviyo, within {} (Klaviyo's count is taken after the scan and can move).", tolerance_text(k));
        if skipped > 0 {
            note.push_str(&format!(" {} on Klaviyo's list aren't here: no usable email or phone, opted out of this list, or left it in DumbMail.", plural(skipped, "person", "people")));
        }
        if state == MISMATCH {
            note.push_str(if from_k + skipped < k { " Fewer here than expected: run the import again." } else { " More here than Klaviyo has: check the list." });
        }
        note.push_str(&extra);
        l.push("Lists", name, json!(k), json!(from_k), state, note);
    }
    // ----- Segments -----
    let segs: Vec<(String, String, Uuid, Option<String>, Value)> = sqlx::query_as(
        "SELECT klaviyo_id, COALESCE(name, ''), local_id, detail->>'mode', COALESCE(detail->'scan', 'null') FROM klaviyo_refs WHERE account_id = $1 AND kind = 'segment' AND local_id IS NOT NULL ORDER BY klaviyo_id",
    )
    .bind(a)
    .fetch_all(db)
    .await?;
    for (i, (kid, name, local, mode, scan)) in segs.iter().enumerate() {
        let k = src["segments"][kid].as_i64();
        let missing = if i >= MAX_COMPARED_GROUPS {
            Some(format!("Only the first {MAX_COMPARED_GROUPS} segments are checked against Klaviyo."))
        } else if k.is_none() {
            Some(src["segments"][kid]["error"].as_str().map(|e| format!("Klaviyo's size couldn't be fetched ({e}).")).unwrap_or_else(|| "Klaviyo's size wasn't fetched.".into()))
        } else {
            None
        };
        if mode.as_deref() == Some("snapshot") {
            let d: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members WHERE list_id = $1").bind(local).fetch_one(db).await?;
            let skipped = scan["skipped"].as_i64().unwrap_or(0);
            let base = "Snapshot: a list of the segment's members when it was imported, because its rules couldn't be translated. It won't update by itself.";
            match (k, missing, scan.is_object()) {
                (Some(k), None, true) => {
                    let state = if (k - (d + skipped)).abs() <= tolerance(k) { MATCH } else { MISMATCH };
                    l.push("Segments", name, json!(k), json!(d), state, format!("{base} Within {}.{}", tolerance_text(k), if skipped > 0 { format!(" {} had no usable email or phone.", plural(skipped, "member", "members")) } else { String::new() }));
                }
                (_, Some(why), _) => l.push("Segments", name, json!(k), json!(d), NOT_VERIFIED, format!("{base} {why}")),
                _ => l.push("Segments", name, json!(k), json!(d), NOT_VERIFIED, format!("{base} The scan of its members didn't finish.")),
            }
        } else {
            let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2").bind(local).bind(a).fetch_optional(db).await?;
            let Some(def) = def else { continue };
            let d = match crate::segments::load(db, a, &def, Some(*local)).await {
                Ok(parsed) => Some(crate::segments::preview(db, a, &parsed, 0).await?.0),
                Err(_) => None,
            };
            match (k, d, missing) {
                (Some(k), Some(d), None) => {
                    let state = if (k - d).abs() <= tolerance(k) { MATCH } else { MISMATCH };
                    l.push("Segments", name, json!(k), json!(d), state, format!("Live rules. Sizes within {} count as matching: both sides update as people's data changes.", tolerance_text(k)));
                }
                (_, None, _) => l.push("Segments", name, json!(k), Value::Null, NOT_VERIFIED, "Its rules couldn't be counted here (a segment it uses was deleted?)."),
                (_, Some(d), Some(why)) => l.push("Segments", name, json!(k), json!(d), NOT_VERIFIED, why),
                (None, Some(d), None) => l.push("Segments", name, Value::Null, json!(d), NOT_VERIFIED, "Klaviyo's size wasn't fetched."),
            }
        }
    }
    // ----- Events and revenue -----
    let until = ts(&src["until"]);
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(a).fetch_one(db).await?;
    let events_done = imp.progress["events"]["finished"] == true;
    // Klaviyo can have several metrics with one name (from different integrations); they are one metric here.
    let mut by_name: BTreeMap<String, (f64, BTreeMap<String, (f64, f64)>, String, Option<String>)> = BTreeMap::new();
    for (_, m) in src["metrics"].as_object().into_iter().flatten() {
        let name = m["name"].as_str().unwrap_or("").to_string();
        let since = m["since"].as_str().unwrap_or("").to_string();
        let e = by_name.entry(name).or_insert((0.0, BTreeMap::new(), since.clone(), None));
        if since < e.2 {
            e.2 = since;
        }
        if let Some(err) = m["error"].as_str() {
            e.3 = Some(err.to_string());
        }
        for (month, v) in m["months"].as_object().into_iter().flatten() {
            let c = v[0].as_f64().unwrap_or(0.0);
            let s = v[1].as_f64().unwrap_or(0.0);
            e.0 += c;
            let me = e.1.entry(month.clone()).or_insert((0.0, 0.0));
            me.0 += c;
            me.1 += s;
        }
    }
    for (name, (k_total, k_months, since, err)) in &by_name {
        let since_t = ts(&json!(since));
        let rows: Vec<(String, i64, f64)> = sqlx::query_as(
            "SELECT to_char(e.occurred_at AT TIME ZONE $4, 'YYYY-MM'), count(*), COALESCE(sum(e.value), 0)::float8
             FROM events e JOIN metrics m ON m.id = e.metric_id
             WHERE e.account_id = $1 AND m.name = $2 AND e.unique_id LIKE 'klaviyo:%' AND e.occurred_at < COALESCE($3, now()) AND e.occurred_at >= COALESCE($5, '-infinity')
             GROUP BY 1",
        )
        .bind(a)
        .bind(name)
        .bind(until)
        .bind(&tz)
        .bind(since_t)
        .fetch_all(db)
        .await?;
        let d_total: i64 = rows.iter().map(|r| r.1).sum();
        let k_total = k_total.round() as i64;
        let why_not = match (err, events_done) {
            (Some(e), _) => Some(format!("{e}.")),
            (None, false) => Some("The event import didn't finish.".to_string()),
            _ => None,
        };
        if let Some(why) = &why_not {
            l.push("Events", name, if err.is_some() { Value::Null } else { json!(k_total) }, json!(d_total), NOT_VERIFIED, why.clone());
        } else {
            let tol = k_total / 1000;
            let state = if (k_total - d_total).abs() <= tol { MATCH } else { MISMATCH };
            let note = if k_total == d_total {
                "Klaviyo's own count for the same range.".to_string()
            } else if state == MATCH {
                format!("Within 0.1% ({tol}): events Klaviyo still counts for profiles that couldn't be imported.")
            } else if d_total < k_total {
                "Fewer events here. Run the import again to fetch the rest.".into()
            } else {
                "More events here than Klaviyo counted in this range.".into()
            };
            l.push("Events", name, json!(k_total), json!(d_total), state, note);
        }
        if ORDER_METRICS.contains(&name.as_str()) {
            let d_months: BTreeMap<String, f64> = rows.iter().map(|r| (r.0.clone(), r.2)).collect();
            let mut months: Vec<&String> = k_months.keys().chain(d_months.keys()).collect();
            months.sort();
            months.dedup();
            for m in months {
                let k = k_months.get(m).map(|x| x.1).unwrap_or(0.0);
                let d = d_months.get(m).copied().unwrap_or(0.0);
                if k == 0.0 && d == 0.0 {
                    continue;
                }
                let round = |v: f64| (v * 100.0).round() / 100.0;
                match &why_not {
                    Some(why) => l.push("Revenue by month", m, if err.is_some() { Value::Null } else { json!(round(k)) }, json!(round(d)), NOT_VERIFIED, why.clone()),
                    None => {
                        let ok = (k - d).abs() <= (k.abs() * 0.001).max(0.01);
                        l.push("Revenue by month", m, json!(round(k)), json!(round(d)), if ok { MATCH } else { MISMATCH }, if ok { "Within 0.1%." } else { "Revenue differs; run the import again to fetch missing orders." });
                    }
                }
            }
        }
    }
    if imp.wants("events") && by_name.is_empty() && imp.compared_at.is_some() {
        l.push("Events", "Event history", Value::Null, Value::Null, NOT_VERIFIED, "Klaviyo's event counts weren't fetched (the key may lack Metrics access).");
    }

    // ----- What didn't come across exactly -----
    let issues: Vec<(String, String, i64)> = sqlx::query_as("SELECT step, kind, count(*) FROM klaviyo_import_issues WHERE import_id = $1 GROUP BY 1, 2 ORDER BY 1, 2")
        .bind(imp.id)
        .fetch_all(db)
        .await?;
    let lines = l.0;
    let mismatches = lines.iter().filter(|x| x["state"] == MISMATCH).count();
    let unverified = lines.iter().filter(|x| x["state"] == NOT_VERIFIED).count();
    Ok(json!({
        "import_id": imp.id,
        "status": imp.status,
        "compared_at": imp.compared_at,
        "klaviyo_counts_at": src["until"],
        "all_ok": mismatches == 0 && unverified == 0,
        "mismatches": mismatches,
        "not_verified": unverified,
        "lines_not_ok": mismatches + unverified,
        "lines": lines,
        "issues_by_step": issues.into_iter().map(|(s, k, n)| json!({"step": s, "kind": k, "count": n})).collect::<Vec<_>>(),
    }))
}

/// How far a live segment's two sizes may drift and still match: 2%, at least 5 people.
fn tolerance(k: i64) -> i64 {
    ((k as f64) * 0.02).ceil().max(5.0) as i64
}

fn tolerance_text(k: i64) -> String {
    format!("{} people", tolerance(k))
}

/// The report as CSV, one line per comparison.
pub fn csv(report: &Value) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    let _ = w.write_record(["Section", "What", "Klaviyo", "DumbMail", "Result", "Note"]);
    for l in report["lines"].as_array().into_iter().flatten() {
        let cell = |v: &Value| match v {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let result = match l["state"].as_str() {
            Some(MATCH) => "MATCH",
            Some(NOT_VERIFIED) => "NOT VERIFIED",
            _ => "MISMATCH",
        };
        let _ = w.write_record([cell(&l["section"]), cell(&l["label"]), cell(&l["klaviyo"]), cell(&l["dumbmail"]), result.to_string(), cell(&l["note"])]);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

// ---------- Switch-over checklist ----------

pub const MANUAL_ITEMS: &[&str] = &["klaviyo_removed", "flows_reviewed", "warmup"];

pub async fn checklist(state: &AppState, imp: Option<&Import>, account_id: Uuid) -> AppResult<Value> {
    let db = &state.db;
    let (domain, snippet_seen, website): (bool, Option<chrono::DateTime<chrono::Utc>>, Option<String>) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified'), a.onsite_last_seen_at, a.website FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let (flows_total, flows_live): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE f.status = 'live') FROM klaviyo_refs r JOIN flows f ON f.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'flow'",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let forms_live: i64 = sqlx::query_scalar("SELECT count(*) FROM forms WHERE account_id = $1 AND status = 'live'").bind(account_id).fetch_one(db).await?;
    let test_send: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE account_id = $1 AND is_test AND status IN ('sent', 'delivered') AND created_at > now() - interval '30 days')",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let manual = imp.map(|i| i.checklist.clone()).unwrap_or(json!({}));
    let ticked = |k: &str| manual[k]["done"] == true;
    // From the report (cached: the status page polls), every email, SMS and list opt-out line must match.
    let suppressions = match imp {
        Some(i) if i.status == "done" && i.compared_at.is_some() => {
            let r = cached(state, i, Duration::minutes(10)).await?;
            r["lines"].as_array().into_iter().flatten().filter(|l| matches!(l["section"].as_str(), Some("Email" | "SMS" | "List opt-outs"))).all(|l| l["state"] == MATCH)
        }
        _ => false,
    };
    let snippet_recent = snippet_seen.is_some_and(|t| t > chrono::Utc::now() - chrono::Duration::days(7));
    let klaviyo_check = &manual["klaviyo_removed"];
    let items = vec![
        json!({"key": "sending_domain", "label": "Sending domain verified", "done": domain, "auto": true,
            "detail": if domain { "Your own domain is verified for sending." } else { "Add and verify your domain so email comes from your brand." }, "link": "/settings/domains"}),
        json!({"key": "snippet", "label": "DumbMail snippet on your site", "done": snippet_recent, "auto": true,
            "detail": match snippet_seen { Some(t) => format!("Last seen {}.", t.format("%b %-d, %Y")), None => "Not seen yet. Add the snippet so DumbMail records visits, carts and orders.".into() }, "link": "/settings/website"}),
        json!({"key": "klaviyo_removed", "label": "Klaviyo's script removed from your site", "done": ticked("klaviyo_removed"), "auto": false,
            "detail": match klaviyo_check["found"].as_bool() {
                Some(true) => "Klaviyo's script was still on your home page at the last check.".to_string(),
                Some(false) => "Klaviyo's script wasn't found on your home page at the last check.".to_string(),
                None => format!("Remove Klaviyo's script and forms once DumbMail's are live{}.", if website.is_some() { "; use Check my site to look" } else { "" }),
            }, "checked_at": klaviyo_check["checked_at"]}),
        json!({"key": "flows_reviewed", "label": "Flows rebuilt and reviewed", "done": ticked("flows_reviewed") && (flows_total == 0 || flows_live > 0), "auto": false,
            "detail": format!("{flows_live} of {flows_total} imported flows are live. Imported flows start as drafts: check each one, preview its emails, then turn it on."), "link": "/flows"}),
        json!({"key": "forms_live", "label": "Sign-up forms live", "done": forms_live > 0, "auto": true,
            "detail": format!("{} live in DumbMail.", plural(forms_live, "form", "forms")), "link": "/forms"}),
        json!({"key": "suppressions", "label": "Unsubscribes and suppressions imported", "done": suppressions, "auto": true,
            "detail": if suppressions { "The report shows every email and SMS opt-out, suppression and list opt-out came across." } else { "Finish the import and check the Email, SMS and List opt-outs lines of the report." }}),
        json!({"key": "test_send", "label": "A test send went through", "done": test_send, "auto": true,
            "detail": if test_send { "A test email was sent in the last 30 days." } else { "Send yourself a test from any campaign." }, "link": "/campaigns"}),
        json!({"key": "warmup", "label": "Plan the warm-up of your sending domain", "done": ticked("warmup"), "auto": false,
            "detail": "New senders start with a small daily email limit that grows as people open and don't complain. Send to your most engaged people first and let it grow over a few weeks.", "link": "/settings/sending"}),
    ];
    let done = items.iter().filter(|i| i["done"] == true).count();
    Ok(json!({"items": items, "done": done, "total": items.len()}))
}

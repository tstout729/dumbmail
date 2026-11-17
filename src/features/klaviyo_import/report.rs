//! The switch-over report: Klaviyo's numbers next to DumbMail's, line by line, green or red.
//!
//! Klaviyo's side comes from what Klaviyo reported: each profile's consent as Klaviyo sent it (kept in
//! `klaviyo_refs.detail`), list and segment sizes and event counts per month (fetched by the import's
//! last step). DumbMail's side is counted live from the account's data each time.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use uuid::Uuid;

use super::runner::ORDER_METRICS;
use super::store::Import;
use crate::error::AppResult;
use crate::state::AppState;

#[derive(Default)]
struct Lines(Vec<Value>);

impl Lines {
    fn push(&mut self, section: &str, label: &str, klaviyo: Value, dumbmail: Value, ok: bool, note: impl Into<String>) {
        self.0.push(json!({"section": section, "label": label, "klaviyo": klaviyo, "dumbmail": dumbmail, "ok": ok, "note": note.into()}));
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub async fn build(state: &AppState, imp: &Import) -> AppResult<Value> {
    let db = &state.db;
    let a = imp.account_id;
    let mut l = Lines::default();

    // ----- Profiles -----
    let (seen, linked, skipped, gone): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT p.id), count(*) FILTER (WHERE r.local_id IS NULL), count(*) FILTER (WHERE r.local_id IS NOT NULL AND p.id IS NULL)
         FROM klaviyo_refs r LEFT JOIN profiles p ON p.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'profile'",
    )
    .bind(a)
    .fetch_one(db)
    .await?;
    let merged = seen - skipped - gone - linked;
    let mut note = Vec::new();
    if merged > 0 {
        note.push(format!("{} shared an email, phone number or external id with another profile and were combined into one", plural(merged, "Klaviyo profile", "Klaviyo profiles")));
    }
    if skipped > 0 {
        note.push(format!("{} had no usable email, phone number or external id (see the list below)", plural(skipped, "profile", "profiles")));
    }
    if gone > 0 {
        note.push(format!("{} were deleted in DumbMail after the import", plural(gone, "profile", "profiles")));
    }
    l.push("Profiles", "Profiles", json!(seen), json!(linked), gone == 0 && seen == linked + merged + skipped, note.join(". "));

    // ----- Consent, counted per Klaviyo profile on both sides -----
    #[derive(sqlx::FromRow, Default)]
    struct C {
        k_sub: i64,
        d_sub: i64,
        k_unsub: i64,
        d_unsub: i64,
        k_never: i64,
        d_never: i64,
        k_supp: i64,
        d_supp: i64,
        k_can: i64,
        d_can: i64,
        d_more: i64,
        ks_sub: i64,
        ds_sub: i64,
        ks_unsub: i64,
        ds_unsub: i64,
        ks_never: i64,
        ds_never: i64,
        ds_more: i64,
    }
    let c: C = sqlx::query_as(
        "WITH x AS (
            SELECT r.detail AS d, p.email_consent AS ec, p.sms_consent AS sc, p.email_suppressed_reason AS sup,
                   (p.email IS NOT NULL AND p.email_consent NOT IN ('unsubscribed','pending') AND p.email_suppressed_reason IS NULL) AS can_email,
                   (p.phone_number IS NOT NULL AND p.sms_consent = 'subscribed') AS can_sms
            FROM klaviyo_refs r JOIN profiles p ON p.id = r.local_id
            WHERE r.account_id = $1 AND r.kind = 'profile')
         SELECT
            count(*) FILTER (WHERE d->'email'->>'consent' = 'SUBSCRIBED') AS k_sub, count(*) FILTER (WHERE ec = 'subscribed') AS d_sub,
            count(*) FILTER (WHERE d->'email'->>'consent' = 'UNSUBSCRIBED') AS k_unsub, count(*) FILTER (WHERE ec = 'unsubscribed') AS d_unsub,
            count(*) FILTER (WHERE d->'email'->>'consent' = 'NEVER_SUBSCRIBED') AS k_never, count(*) FILTER (WHERE ec = 'never_subscribed') AS d_never,
            count(*) FILTER (WHERE (d->'email'->>'suppressed')::boolean) AS k_supp, count(*) FILTER (WHERE sup IS NOT NULL) AS d_supp,
            count(*) FILTER (WHERE (d->'email'->>'can_receive')::boolean AND (d->>'has_email')::boolean) AS k_can, count(*) FILTER (WHERE can_email) AS d_can,
            count(*) FILTER (WHERE can_email AND NOT COALESCE((d->'email'->>'can_receive')::boolean, false)) AS d_more,
            count(*) FILTER (WHERE d->'sms'->>'consent' = 'SUBSCRIBED') AS ks_sub, count(*) FILTER (WHERE sc = 'subscribed') AS ds_sub,
            count(*) FILTER (WHERE d->'sms'->>'consent' = 'UNSUBSCRIBED') AS ks_unsub, count(*) FILTER (WHERE sc = 'unsubscribed') AS ds_unsub,
            count(*) FILTER (WHERE COALESCE(d->'sms'->>'consent', 'NEVER_SUBSCRIBED') NOT IN ('SUBSCRIBED','UNSUBSCRIBED')) AS ks_never, count(*) FILTER (WHERE sc = 'never_subscribed') AS ds_never,
            count(*) FILTER (WHERE can_sms AND NOT (COALESCE((d->'sms'->>'can_receive')::boolean, false) AND d->'sms'->>'consent' = 'SUBSCRIBED')) AS ds_more
         FROM x",
    )
    .bind(a)
    .fetch_one(db)
    .await?;
    let safe = "Differences are people moved to a less reachable state: unsubscribed or suppressed on one side wins, and a subscriber Klaviyo couldn't reach isn't subscribed here.";
    let line = |l: &mut Lines, sec: &str, label: &str, k: i64, d: i64, ok_if_lower: bool, ok_if_higher: bool| {
        let ok = k == d || (ok_if_lower && d < k) || (ok_if_higher && d > k);
        l.push(sec, label, json!(k), json!(d), ok, if k == d { String::new() } else if ok { safe.to_string() } else { "Doesn't match. Run the import again; if it still differs, contact support with this report.".to_string() });
    };
    line(&mut l, "Email", "Subscribed", c.k_sub, c.d_sub, true, false);
    line(&mut l, "Email", "Unsubscribed", c.k_unsub, c.d_unsub, false, true);
    line(&mut l, "Email", "Never subscribed", c.k_never, c.d_never, true, true);
    line(&mut l, "Email", "Suppressed (bounced, complained, invalid or suppressed by hand)", c.k_supp, c.d_supp, false, true);
    line(&mut l, "Email", "Can get marketing email", c.k_can, c.d_can, true, false);
    l.push("Email", "More reachable here than in Klaviyo", json!(0), json!(c.d_more), c.d_more == 0,
        if c.d_more == 0 { "Nobody can get email here that Klaviyo wouldn't have sent." } else { "These people can get email here though Klaviyo wouldn't send to them. Usually they signed up again through a DumbMail form after the import." });
    line(&mut l, "SMS", "Subscribed", c.ks_sub, c.ds_sub, true, false);
    line(&mut l, "SMS", "Unsubscribed", c.ks_unsub, c.ds_unsub, false, true);
    line(&mut l, "SMS", "Never subscribed", c.ks_never, c.ds_never, true, true);
    l.push("SMS", "More reachable here than in Klaviyo", json!(0), json!(c.ds_more), c.ds_more == 0,
        if c.ds_more == 0 { "Nobody can get texts here that Klaviyo wouldn't have sent." } else { "These people can get texts here though Klaviyo wouldn't text them. Usually they opted in again through DumbMail after the import." });

    let src = &imp.source_counts;
    // ----- Lists -----
    let lists: Vec<(String, String, Uuid, i64, i64)> = sqlx::query_as(
        "SELECT r.klaviyo_id, COALESCE(r.name, ''), r.local_id, (SELECT count(*) FROM list_members m WHERE m.list_id = r.local_id),
                (SELECT count(*) FROM klaviyo_refs pr WHERE pr.account_id = r.account_id AND pr.kind = 'profile' AND pr.detail->'list_suppressions' ? r.klaviyo_id)
         FROM klaviyo_refs r JOIN lists li ON li.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'list' ORDER BY r.name",
    )
    .bind(a)
    .fetch_all(db)
    .await?;
    for (kid, name, _, d, list_sup) in &lists {
        match src["lists"][kid].as_i64() {
            Some(k) => {
                let ok = k == *d || (*d < k && k - d <= *list_sup + merged);
                let note = if k == *d {
                    String::new()
                } else if ok {
                    "The difference is people who unsubscribed from this list in Klaviyo, or duplicates combined into one profile.".into()
                } else if *d > k {
                    "More members here: people joined after Klaviyo's count was taken.".into()
                } else {
                    "Fewer members here. Run the import again to fill in the rest.".into()
                };
                let ok = ok || *d > k;
                l.push("Lists", name, json!(k), json!(d), ok, note);
            }
            None => l.push("Lists", name, Value::Null, json!(d), true, "Klaviyo's size wasn't fetched for this list."),
        }
    }
    // ----- Segments -----
    let segs: Vec<(String, String, Uuid, Option<String>)> = sqlx::query_as(
        "SELECT klaviyo_id, COALESCE(name, ''), local_id, detail->>'mode' FROM klaviyo_refs WHERE account_id = $1 AND kind = 'segment' AND local_id IS NOT NULL ORDER BY name",
    )
    .bind(a)
    .fetch_all(db)
    .await?;
    for (kid, name, local, mode) in &segs {
        let k = src["segments"][kid].as_i64();
        if mode.as_deref() == Some("snapshot") {
            let d: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members WHERE list_id = $1").bind(local).fetch_one(db).await?;
            let ok = k.is_none_or(|k| (k - d).abs() <= tolerance(k));
            l.push("Segments", name, json!(k), json!(d), ok, "Snapshot: a list of the segment's members on import day, because its rules couldn't be translated. It won't update by itself.");
        } else {
            let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2").bind(local).bind(a).fetch_optional(db).await?;
            let Some(def) = def else { continue };
            let d = match crate::segments::load(db, a, &def, Some(*local)).await {
                Ok(parsed) => crate::segments::preview(db, a, &parsed, 0).await?.0,
                Err(_) => continue,
            };
            let ok = k.is_none_or(|k| (k - d).abs() <= tolerance(k));
            l.push("Segments", name, json!(k), json!(d), ok, format!("Live rules. Sizes within {} count as matching: both sides update as people's data changes.", tolerance_text(k.unwrap_or(d))));
        }
    }
    // ----- Events and revenue -----
    let until = src["until"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&chrono::Utc));
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(a).fetch_one(db).await?;
    // Klaviyo can have several metrics with one name (from different integrations); they are one metric here.
    let mut by_name: BTreeMap<String, (f64, BTreeMap<String, (f64, f64)>, String)> = BTreeMap::new();
    for (_, m) in src["metrics"].as_object().into_iter().flatten() {
        let name = m["name"].as_str().unwrap_or("").to_string();
        let since = m["since"].as_str().unwrap_or("").to_string();
        let e = by_name.entry(name).or_insert((0.0, BTreeMap::new(), since.clone()));
        if since < e.2 {
            e.2 = since;
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
    for (name, (k_total, k_months, since)) in &by_name {
        let since_t = chrono::DateTime::parse_from_rfc3339(since).map(|t| t.with_timezone(&chrono::Utc)).ok();
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
        let ok = (k_total - d_total).abs() <= (k_total / 1000).max(0);
        let note = if k_total == d_total { String::new() } else if ok { "Within 0.1%: events Klaviyo still counts for since-deleted profiles.".into() } else if d_total < k_total { "Fewer events here. Run the import again to fetch the rest.".into() } else { "More events here than Klaviyo counted in this range.".into() };
        l.push("Events", name, json!(k_total), json!(d_total), ok, note);
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
                let ok = (k - d).abs() <= (k.abs() * 0.001).max(0.01);
                l.push("Revenue by month", m, json!((k * 100.0).round() / 100.0), json!((d * 100.0).round() / 100.0), ok, if ok { "" } else { "Revenue differs; run the import again to fetch missing orders." });
            }
        }
    }

    // ----- What didn't come across exactly -----
    let issues: Vec<(String, String, i64)> = sqlx::query_as("SELECT step, kind, count(*) FROM klaviyo_import_issues WHERE import_id = $1 GROUP BY 1, 2 ORDER BY 1, 2")
        .bind(imp.id)
        .fetch_all(db)
        .await?;
    let lines = l.0;
    let red = lines.iter().filter(|x| x["ok"] == false).count();
    Ok(json!({
        "import_id": imp.id,
        "status": imp.status,
        "compared_at": imp.compared_at,
        "klaviyo_counts_at": src["until"],
        "all_ok": red == 0,
        "lines_not_ok": red,
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
        let _ = w.write_record([cell(&l["section"]), cell(&l["label"]), cell(&l["klaviyo"]), cell(&l["dumbmail"]), if l["ok"] == true { "OK".into() } else { "CHECK".into() }, cell(&l["note"])]);
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
    let suppressions = match imp {
        Some(i) if i.status == "done" || i.status == "syncing" => {
            let r = build(state, i).await?;
            r["lines"].as_array().into_iter().flatten().filter(|l| l["section"] == "Email" && (l["label"].as_str().unwrap_or("").starts_with("Suppressed") || l["label"].as_str().unwrap_or("").starts_with("More reachable"))).all(|l| l["ok"] == true)
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
            "detail": if suppressions { "The report shows every unsubscribe and suppression came across." } else { "Finish the import and check the Email lines of the report." }}),
        json!({"key": "test_send", "label": "A test send went through", "done": test_send, "auto": true,
            "detail": if test_send { "A test email was sent in the last 30 days." } else { "Send yourself a test from any campaign." }, "link": "/campaigns"}),
        json!({"key": "warmup", "label": "Plan the warm-up of your sending domain", "done": ticked("warmup"), "auto": false,
            "detail": "New senders start with a small daily email limit that grows as people open and don't complain. Send to your most engaged people first and let it grow over a few weeks.", "link": "/settings/sending"}),
    ];
    let done = items.iter().filter(|i| i["done"] == true).count();
    Ok(json!({"items": items, "done": done, "total": items.len()}))
}

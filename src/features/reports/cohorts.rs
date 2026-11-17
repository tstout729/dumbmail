//! Cohorts and funnels.

use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use super::attribution::{SAVED_CFG, TOUCH_LATERAL};
use super::custom::Range;
use super::query::{self, Arg, Sql};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CohortQuery {
    /// `first_purchase` (default) or `signup`.
    pub by: Option<String>,
    /// How many monthly cohorts, ending with this month: 1-24 (default 12).
    pub months: Option<i64>,
}

fn month_index(d: NaiveDate) -> i64 {
    d.year() as i64 * 12 + d.month0() as i64
}

/// Group people by the month of their first purchase (or sign-up), then show, for each month since,
/// the share who bought (again) and the revenue per person so far.
pub async fn cohorts(state: &AppState, account_id: Uuid, q: CohortQuery) -> AppResult<Value> {
    let by = q.by.as_deref().unwrap_or("first_purchase");
    if by != "first_purchase" && by != "signup" {
        return Err(AppError::bad("by must be first_purchase or signup"));
    }
    let months = q.months.unwrap_or(12);
    if !(1..=24).contains(&months) {
        return Err(AppError::bad("months must be 1-24"));
    }
    let (tz_name, tz) = query::account_tz(state, account_id).await?;
    let settings = super::attribution::load(&state.db, account_id).await?;
    let today = Utc::now().with_timezone(&tz).date_naive();
    let this_month = today.with_day(1).unwrap_or(today);
    let mut first = this_month;
    for _ in 1..months {
        first = (first - chrono::Duration::days(1)).with_day(1).unwrap_or(first);
    }
    let since = query::day_start(tz, first);

    let mut s = Sql::new(account_id);
    let tzp = s.bind(Arg::Text(tz_name.clone()));
    let sincep = s.bind(Arg::Ts(since));
    let metricsp = s.bind(Arg::TextArr(settings.conversion_metrics.clone()));
    let cohort = if by == "signup" {
        format!("SELECT p.id AS profile_id, date_trunc('month', p.created_at AT TIME ZONE {tzp}) AS cm FROM profiles p WHERE p.account_id = $1 AND p.created_at >= {sincep}")
    } else {
        format!("SELECT profile_id, date_trunc('month', occurred_at AT TIME ZONE {tzp}) AS cm FROM conv WHERE n = 1 AND occurred_at >= {sincep}")
    };
    // For first-purchase cohorts the first order isn't a repeat; for sign-up cohorts every order counts.
    let buyer = if by == "signup" { "true" } else { "conv.n > 1" };
    s.push(&format!(
        "WITH conv AS (
            SELECT e.profile_id, e.occurred_at, COALESCE(e.value, 0) AS v,
                   row_number() OVER (PARTITION BY e.profile_id ORDER BY e.occurred_at, e.id) AS n
            FROM events e
            WHERE e.account_id = $1 AND e.metric_id = ANY(ARRAY(SELECT id FROM metrics WHERE account_id = $1 AND name = ANY({metricsp})))),
         cohort AS ({cohort}),
         sizes AS (SELECT cm, count(*) AS size FROM cohort GROUP BY 1),
         act AS (
            SELECT c.cm, date_trunc('month', conv.occurred_at AT TIME ZONE {tzp}) AS am,
                   count(DISTINCT conv.profile_id) FILTER (WHERE {buyer}) AS buyers, sum(conv.v) AS revenue
            FROM cohort c JOIN conv ON conv.profile_id = c.profile_id AND conv.occurred_at >= {sincep}
            GROUP BY 1, 2)
         SELECT to_char(s.cm, 'YYYY-MM-DD') AS cm, s.size::int8 AS size, to_char(a.am, 'YYYY-MM-DD') AS am,
                COALESCE(a.buyers, 0)::int8 AS buyers, COALESCE(a.revenue, 0)::float8 AS revenue
         FROM sizes s LEFT JOIN act a ON a.cm = s.cm AND a.am >= s.cm
         ORDER BY 1, 3"
    ));
    let mut tx = query::timed(state).await?;
    let rows = s.fetch_all(&mut tx).await?;
    tx.commit().await?;

    let parse = |v: Option<String>| v.and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok());
    let mut cohorts: Vec<(NaiveDate, i64, Vec<(i64, f64)>)> = Vec::new();
    for r in rows {
        let cm = parse(r.try_get("cm").ok().flatten()).ok_or_else(|| AppError::Internal(anyhow::anyhow!("bad cohort month")))?;
        let size: i64 = r.try_get("size").unwrap_or(0);
        if cohorts.last().map(|c| c.0) != Some(cm) {
            let span = (month_index(this_month) - month_index(cm) + 1).max(1) as usize;
            cohorts.push((cm, size, vec![(0, 0.0); span]));
        }
        if let Some(am) = parse(r.try_get("am").ok().flatten()) {
            let k = (month_index(am) - month_index(cm)) as usize;
            let cells = &mut cohorts.last_mut().expect("pushed").2;
            if let Some(cell) = cells.get_mut(k) {
                *cell = (r.try_get("buyers").unwrap_or(0), r.try_get("revenue").unwrap_or(0.0));
            }
        }
    }
    let out: Vec<Value> = cohorts
        .into_iter()
        .map(|(cm, size, cells)| {
            let mut cum = 0.0;
            let cells: Vec<Value> = cells
                .into_iter()
                .enumerate()
                .map(|(k, (buyers, revenue))| {
                    cum += revenue;
                    let rate = if size > 0 { buyers as f64 / size as f64 } else { 0.0 };
                    let per = if size > 0 { cum / size as f64 } else { 0.0 };
                    json!({"month": k, "customers": buyers, "rate": (rate * 1e6).round() / 1e6, "revenue": (revenue * 100.0).round() / 100.0,
                           "cumulative_revenue_per_person": (per * 100.0).round() / 100.0})
                })
                .collect();
            json!({"cohort": cm.format("%Y-%m").to_string(), "size": size, "cells": cells})
        })
        .collect();
    Ok(json!({
        "by": by, "months": months, "timezone": tz_name, "metrics": settings.conversion_metrics,
        "rate_label": if by == "signup" { "Bought that month" } else { "Bought again that month" },
        "cohorts": out,
    }))
}

pub fn cohorts_csv(v: &Value) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    let max = v["cohorts"].as_array().map(|c| c.iter().map(|x| x["cells"].as_array().map_or(0, |a| a.len())).max().unwrap_or(0)).unwrap_or(0);
    let mut header = vec!["Cohort".to_string(), "People".to_string()];
    for k in 0..max {
        header.push(format!("Month {k} bought (%)"));
    }
    for k in 0..max {
        header.push(format!("Month {k} revenue per person (cumulative)"));
    }
    let _ = w.write_record(&header);
    for c in v["cohorts"].as_array().cloned().unwrap_or_default() {
        let cells = c["cells"].as_array().cloned().unwrap_or_default();
        let mut rec = vec![c["cohort"].as_str().unwrap_or("").to_string(), c["size"].to_string()];
        for k in 0..max {
            rec.push(cells.get(k).map(|x| format!("{:.2}", x["rate"].as_f64().unwrap_or(0.0) * 100.0)).unwrap_or_default());
        }
        for k in 0..max {
            rec.push(cells.get(k).map(|x| format!("{:.2}", x["cumulative_revenue_per_person"].as_f64().unwrap_or(0.0))).unwrap_or_default());
        }
        let _ = w.write_record(&rec);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Split {
    /// `event_property` (of the first step's event), `profile_property` or `source` (whether the
    /// first step came from a campaign or flow message, by your attribution settings).
    #[serde(rename = "type")]
    pub kind: String,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FunnelQuery {
    /// 2-6 metric names, in order.
    pub steps: Vec<String>,
    /// Time from the first step to finish the funnel: 1-90 days (default 7).
    pub window_days: Option<i64>,
    /// When the first step happened (default the last 30 days, at most 365).
    #[serde(default)]
    pub range: Range,
    pub split_by: Option<Split>,
}

const MAX_SPLITS: usize = 10;

/// People who did step 1 in the range, and how many went on to each later step, in order, within
/// the window from their first step 1. Each person counts once, from their first step 1 in the range.
pub async fn funnel(state: &AppState, account_id: Uuid, q: FunnelQuery) -> AppResult<Value> {
    let steps: Vec<String> = q.steps.iter().map(|s| s.trim().to_string()).collect();
    if !(2..=6).contains(&steps.len()) || steps.iter().any(|s| s.is_empty() || s.len() > 200) {
        return Err(AppError::bad("a funnel needs 2-6 steps, each a metric name"));
    }
    let window_days = q.window_days.unwrap_or(7);
    if !(1..=90).contains(&window_days) {
        return Err(AppError::bad("window_days must be 1-90"));
    }
    let (tz_name, tz) = query::account_tz(state, account_id).await?;
    let w = query::resolve_range(tz, q.range.days, q.range.start, q.range.end)?;
    if (w.end - w.start).num_days() > 366 {
        return Err(AppError::bad("a funnel's range can cover at most 365 days"));
    }
    let split = match &q.split_by {
        None => None,
        Some(sp) => {
            let key = sp.key.as_deref().map(str::trim).unwrap_or("").to_string();
            match sp.kind.as_str() {
                "event_property" | "profile_property" if !key.is_empty() && key.len() <= 100 => Some((sp.kind.clone(), key)),
                "event_property" | "profile_property" => return Err(AppError::bad("split_by needs `key`, the property's name")),
                "source" => Some(("source".to_string(), String::new())),
                _ => return Err(AppError::bad("split_by.type must be event_property, profile_property or source")),
            }
        }
    };

    let mut s = Sql::new(account_id);
    let start = s.bind(Arg::Ts(w.start));
    let end = s.bind(Arg::Ts(w.end));
    let window = s.bind(Arg::Int(window_days));
    let mut ctes = Vec::new();
    for (i, name) in steps.iter().enumerate() {
        let mp = s.bind(Arg::Text(name.clone()));
        let metric = format!("(SELECT id FROM metrics WHERE account_id = $1 AND name = {mp})");
        if i == 0 {
            ctes.push(format!(
                "s0 AS (SELECT DISTINCT ON (e.profile_id) e.profile_id, e.occurred_at AS t, e.occurred_at AS t0, e.properties
                        FROM events e WHERE e.account_id = $1 AND e.metric_id = {metric} AND e.occurred_at >= {start} AND e.occurred_at < {end}
                        ORDER BY e.profile_id, e.occurred_at, e.id)"
            ));
        } else {
            // The same metric twice in a row needs a second, later event.
            let after = if steps[i - 1] == *name { ">" } else { ">=" };
            ctes.push(format!(
                "s{i} AS (SELECT p.profile_id, min(e.occurred_at) AS t, min(p.t0) AS t0
                          FROM s{prev} p JOIN events e ON e.profile_id = p.profile_id AND e.metric_id = {metric}
                               AND e.occurred_at {after} p.t AND e.occurred_at <= p.t0 + make_interval(days => {window}::int)
                          GROUP BY 1)",
                prev = i - 1
            ));
        }
    }
    let (grp, joins) = match &split {
        None => ("''".to_string(), String::new()),
        Some((kind, key)) if kind == "event_property" => {
            let kp = s.bind(Arg::Text(key.clone()));
            (format!("COALESCE(s0.properties->>{kp}, '')"), String::new())
        }
        Some((kind, key)) if kind == "profile_property" => {
            let kp = s.bind(Arg::Text(key.clone()));
            (format!("COALESCE(p.properties->>{kp}, '')"), " LEFT JOIN profiles p ON p.id = s0.profile_id".to_string())
        }
        Some(_) => (
            "CASE WHEN att.campaign_id IS NOT NULL THEN 'campaign' WHEN att.flow_id IS NOT NULL THEN 'flow' ELSE 'none' END".to_string(),
            format!(" CROSS JOIN {SAVED_CFG} cfg CROSS JOIN LATERAL (SELECT s0.profile_id, s0.t AS occurred_at) e {TOUCH_LATERAL}"),
        ),
    };
    let counts = (0..steps.len()).map(|i| format!("count(s{i}.profile_id)::int8 AS c{i}")).collect::<Vec<_>>().join(", ");
    let step_joins = (1..steps.len()).map(|i| format!(" LEFT JOIN s{i} ON s{i}.profile_id = s0.profile_id")).collect::<String>();
    s.push(&format!("WITH {} SELECT {grp} AS grp, {counts} FROM s0{step_joins}{joins} GROUP BY 1 ORDER BY 2 DESC LIMIT 500", ctes.join(",\n")));
    let mut tx = query::timed(state).await?;
    let rows = s.fetch_all(&mut tx).await?;
    tx.commit().await?;

    let mut groups: Vec<(String, Vec<i64>)> = rows
        .into_iter()
        .map(|r| {
            let g: Option<String> = r.try_get("grp").ok().flatten();
            let c: Vec<i64> = (0..steps.len()).map(|i| r.try_get::<i64, _>(format!("c{i}").as_str()).unwrap_or(0)).collect();
            (g.unwrap_or_default(), c)
        })
        .collect();
    // Keep the biggest splits; fold the rest into "Other".
    if groups.len() > MAX_SPLITS {
        let rest = groups.split_off(MAX_SPLITS - 1);
        let mut other = vec![0i64; steps.len()];
        for (_, c) in rest {
            for (i, v) in c.into_iter().enumerate() {
                other[i] += v;
            }
        }
        groups.push(("\u{0}other".into(), other));
    }
    let shape = |counts: &[i64]| -> Vec<Value> {
        counts
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let prev = if i == 0 { n } else { counts[i - 1] };
                let first = counts[0];
                let conv = if prev > 0 { n as f64 / prev as f64 } else { 0.0 };
                let overall = if first > 0 { n as f64 / first as f64 } else { 0.0 };
                json!({"step": i + 1, "metric": steps[i], "people": n, "conversion_from_previous": (conv * 1e6).round() / 1e6,
                       "conversion_from_start": (overall * 1e6).round() / 1e6, "dropped": if i == 0 { 0 } else { prev - n }})
            })
            .collect()
    };
    let mut total = vec![0i64; steps.len()];
    for (_, c) in &groups {
        for (i, v) in c.iter().enumerate() {
            total[i] += v;
        }
    }
    let label = |k: &str| -> String {
        match (split.as_ref().map(|s| s.0.as_str()), k) {
            (_, "\u{0}other") => "Other".into(),
            (Some("source"), "campaign") => "Came from a campaign".into(),
            (Some("source"), "flow") => "Came from a flow".into(),
            (Some("source"), "none") => "No message".into(),
            (_, "") => "(none)".into(),
            (_, k) => k.to_string(),
        }
    };
    Ok(json!({
        "steps": steps, "window_days": window_days, "timezone": tz_name, "range": {"start": w.start, "end": w.end},
        "split_by": q.split_by,
        "total": shape(&total),
        "splits": if split.is_some() { groups.iter().map(|(k, c)| json!({"key": k.trim_start_matches('\u{0}'), "label": label(k), "steps": shape(c)})).collect::<Vec<_>>() } else { Vec::new() },
    }))
}

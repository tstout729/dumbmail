//! Revenue attribution: which message gets the credit for a conversion.
//!
//! [`ATTRIBUTION_SQL`] is the one query every revenue number uses (the overview, campaign and flow
//! reports, custom reports, cohorts and funnels). It reads the account's saved settings, so a change
//! applies everywhere at once. [`PREVIEW_SQL`] is the same query with the settings passed in, used to
//! show what a change would do over past data before it's saved.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The message credited for something person `e.profile_id` did at `e.occurred_at`, under the
/// settings relation `cfg`: a LATERAL join named `att` with `message_id, channel, campaign_id,
/// flow_id, flow_node_id, variant` (all NULL when nothing gets the credit). A message counts when
/// the person opened it (if opens count) within the open window, or clicked it within the click
/// window, before that moment. Last touch credits the message touched most recently; first touch
/// the one touched earliest.
macro_rules! touch_lateral {
    () => {
        "
    LEFT JOIN LATERAL (
        SELECT m.id AS message_id, m.channel, m.campaign_id, m.flow_id, m.flow_node_id, m.variant
        FROM messages m
        CROSS JOIN LATERAL (
            SELECT CASE WHEN m.channel = 'email' THEN ARRAY[
                       CASE WHEN cfg.count_opens AND m.opened_at BETWEEN e.occurred_at - cfg.email_open_window AND e.occurred_at THEN m.opened_at END,
                       CASE WHEN m.clicked_at BETWEEN e.occurred_at - cfg.email_click_window AND e.occurred_at THEN m.clicked_at END]
                   ELSE ARRAY[CASE WHEN m.clicked_at BETWEEN e.occurred_at - cfg.sms_click_window AND e.occurred_at THEN m.clicked_at END]
                   END AS touches
        ) tt
        CROSS JOIN LATERAL (SELECT GREATEST(tt.touches[1], tt.touches[2]) AS last_at, LEAST(tt.touches[1], tt.touches[2]) AS first_at) t
        WHERE m.profile_id = e.profile_id AND NOT m.is_test AND m.created_at <= e.occurred_at AND t.last_at IS NOT NULL
        ORDER BY CASE WHEN cfg.first_touch THEN t.first_at END ASC, t.last_at DESC, m.id
        LIMIT 1
    ) att ON true"
    };
}

/// The account's ($1) saved settings as the relation `cfg` (defaults when it has none), with the
/// columns `metrics text[]`, `count_opens bool`, `email_open_window`, `email_click_window` and
/// `sms_click_window` (intervals) and `first_touch bool`. The defaults must match [`Settings::default`].
macro_rules! saved_cfg {
    () => {
        "(SELECT COALESCE(s.conversion_metrics, ARRAY[a.conversion_metric]) AS metrics,
             COALESCE(s.count_opens, true) AS count_opens,
             make_interval(hours => COALESCE(s.email_open_window_hours, 120)) AS email_open_window,
             make_interval(hours => COALESCE(s.email_click_window_hours, 120)) AS email_click_window,
             make_interval(hours => COALESCE(s.sms_click_window_hours, 24)) AS sms_click_window,
             COALESCE(s.model = 'first_touch', false) AS first_touch
      FROM accounts a LEFT JOIN attribution_settings s ON s.account_id = a.id WHERE a.id = $1)"
    };
}

/// Conversion events of the account ($1) with the message credited for each:
/// `event_id, value, occurred_at, profile_id, message_id, channel, campaign_id, flow_id,
/// flow_node_id, variant`. Conversions are events of the settings' metrics.
macro_rules! attribution_sql {
    ($cfg:expr) => {
        concat!(
            "
    SELECT e.id AS event_id, COALESCE(e.value, 0) AS value, e.occurred_at, e.profile_id,
           att.message_id, att.channel, att.campaign_id, att.flow_id, att.flow_node_id, att.variant
    FROM events e
    CROSS JOIN ",
            $cfg,
            " cfg
    JOIN metrics mt ON mt.id = e.metric_id AND mt.name = ANY(cfg.metrics)",
            touch_lateral!(),
            "
    WHERE e.account_id = $1"
        )
    };
}

/// Every revenue number uses this, with the account's saved settings.
pub const ATTRIBUTION_SQL: &str = attribution_sql!(saved_cfg!());

/// The saved settings relation, for queries that credit other events (funnels): select from it
/// `AS cfg` and join [`TOUCH_LATERAL`] with `e.profile_id` and `e.occurred_at` in scope.
pub const SAVED_CFG: &str = saved_cfg!();
pub const TOUCH_LATERAL: &str = touch_lateral!();

/// Settings bound as $2 (metrics), $3 (count opens), $4-$6 (open, click and SMS windows in hours) and
/// $7 (first touch). Queries using it start their own binds at $8.
pub const PREVIEW_SQL: &str = attribution_sql!(
    "(SELECT $2::text[] AS metrics, $3::bool AS count_opens, make_interval(hours => $4::int) AS email_open_window,
             make_interval(hours => $5::int) AS email_click_window, make_interval(hours => $6::int) AS sms_click_window,
             $7::bool AS first_touch)"
);

pub const MAX_WINDOW_HOURS: i32 = 30 * 24;
const MAX_METRICS: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub email_open_window_hours: i32,
    pub email_click_window_hours: i32,
    pub sms_click_window_hours: i32,
    /// False means "clicks only": opens never earn credit (Apple Mail opens many emails by itself).
    pub count_opens: bool,
    /// `last_touch` or `first_touch`.
    pub model: String,
    pub conversion_metrics: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            email_open_window_hours: 120,
            email_click_window_hours: 120,
            sms_click_window_hours: 24,
            count_opens: true,
            model: "last_touch".into(),
            conversion_metrics: vec!["Placed Order".into()],
        }
    }
}

/// A partial update: fields left out keep their current value.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub email_open_window_hours: Option<i32>,
    pub email_click_window_hours: Option<i32>,
    pub sms_click_window_hours: Option<i32>,
    pub count_opens: Option<bool>,
    pub model: Option<String>,
    pub conversion_metrics: Option<Vec<String>>,
}

impl Settings {
    pub fn apply(mut self, p: Patch) -> AppResult<Self> {
        if let Some(v) = p.email_open_window_hours {
            self.email_open_window_hours = v;
        }
        if let Some(v) = p.email_click_window_hours {
            self.email_click_window_hours = v;
        }
        if let Some(v) = p.sms_click_window_hours {
            self.sms_click_window_hours = v;
        }
        if let Some(v) = p.count_opens {
            self.count_opens = v;
        }
        if let Some(v) = p.model {
            self.model = v;
        }
        if let Some(v) = p.conversion_metrics {
            self.conversion_metrics = v;
        }
        self.validate()?;
        Ok(self)
    }

    fn validate(&mut self) -> AppResult<()> {
        for (name, v) in [
            ("email_open_window_hours", self.email_open_window_hours),
            ("email_click_window_hours", self.email_click_window_hours),
            ("sms_click_window_hours", self.sms_click_window_hours),
        ] {
            if !(1..=MAX_WINDOW_HOURS).contains(&v) {
                return Err(AppError::bad(format!("{name} must be between 1 and {MAX_WINDOW_HOURS} hours (30 days)")));
            }
        }
        if self.model != "last_touch" && self.model != "first_touch" {
            return Err(AppError::bad("model must be last_touch or first_touch"));
        }
        let mut metrics: Vec<String> = Vec::new();
        for m in &self.conversion_metrics {
            let m = m.trim();
            if m.is_empty() || m.len() > 200 {
                return Err(AppError::bad("each conversion metric needs a name of 1-200 characters"));
            }
            if !metrics.iter().any(|x| x == m) {
                metrics.push(m.to_string());
            }
        }
        if metrics.is_empty() || metrics.len() > MAX_METRICS {
            return Err(AppError::bad(format!("pick 1-{MAX_METRICS} conversion metrics")));
        }
        self.conversion_metrics = metrics;
        Ok(())
    }

    /// Plain-language summary, for hints in the dashboard and scheduled emails.
    pub fn describe(&self) -> String {
        let span = |h: i32| if h % 24 == 0 { format!("{} day{}", h / 24, if h == 24 { "" } else { "s" }) } else { format!("{h} hour{}", if h == 1 { "" } else { "s" }) };
        let email = if self.count_opens {
            if self.email_open_window_hours == self.email_click_window_hours {
                format!("opened or clicked an email in the {} before", span(self.email_click_window_hours))
            } else {
                format!("opened an email in the {} before or clicked one in the {} before", span(self.email_open_window_hours), span(self.email_click_window_hours))
            }
        } else {
            format!("clicked an email in the {} before", span(self.email_click_window_hours))
        };
        let who = if self.model == "first_touch" { "The earliest message" } else { "The newest message" };
        format!(
            "{} counts when the buyer {email}, or clicked a text in the {} before. {who} gets the credit.",
            self.conversion_metrics.join(" or "),
            span(self.sms_click_window_hours)
        )
    }
}

/// The account's settings, with defaults for anything it never changed.
pub async fn load<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> AppResult<Settings> {
    let row: Option<(String, Option<i32>, Option<i32>, Option<i32>, Option<bool>, Option<String>, Option<Vec<String>>)> = sqlx::query_as(
        "SELECT a.conversion_metric, s.email_open_window_hours, s.email_click_window_hours, s.sms_click_window_hours, s.count_opens, s.model, s.conversion_metrics
         FROM accounts a LEFT JOIN attribution_settings s ON s.account_id = a.id WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let (metric, open, click, sms, opens, model, metrics) = row.ok_or_else(|| AppError::not_found("account"))?;
    let d = Settings::default();
    Ok(Settings {
        email_open_window_hours: open.unwrap_or(d.email_open_window_hours),
        email_click_window_hours: click.unwrap_or(d.email_click_window_hours),
        sms_click_window_hours: sms.unwrap_or(d.sms_click_window_hours),
        count_opens: opens.unwrap_or(d.count_opens),
        model: model.unwrap_or(d.model),
        conversion_metrics: metrics.unwrap_or_else(|| vec![metric]),
    })
}

/// Save settings. One conversion metric is kept on the account (`conversion_metric`, which
/// `update_account` also changes); two or more are stored here and take precedence.
pub async fn save(state: &AppState, account_id: Uuid, s: &Settings) -> AppResult<Settings> {
    let mut tx = state.db.begin().await?;
    let many = (s.conversion_metrics.len() > 1).then(|| s.conversion_metrics.clone());
    sqlx::query("UPDATE accounts SET conversion_metric = $2 WHERE id = $1")
        .bind(account_id)
        .bind(&s.conversion_metrics[0])
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO attribution_settings (account_id, email_open_window_hours, email_click_window_hours, sms_click_window_hours, count_opens, model, conversion_metrics, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         ON CONFLICT (account_id) DO UPDATE SET email_open_window_hours = $2, email_click_window_hours = $3, sms_click_window_hours = $4,
             count_opens = $5, model = $6, conversion_metrics = $7, updated_at = now()",
    )
    .bind(account_id)
    .bind(s.email_open_window_hours)
    .bind(s.email_click_window_hours)
    .bind(s.sms_click_window_hours)
    .bind(s.count_opens)
    .bind(&s.model)
    .bind(many)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    load(&state.db, account_id).await
}

pub fn to_json(s: &Settings) -> Value {
    let d = Settings::default();
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["description"] = json!(s.describe());
    v["is_default"] = json!(s.email_open_window_hours == d.email_open_window_hours
        && s.email_click_window_hours == d.email_click_window_hours
        && s.sms_click_window_hours == d.sms_click_window_hours
        && s.count_opens == d.count_opens
        && s.model == d.model);
    v
}

/// Attributed totals for `[since, now)` under the given settings.
async fn totals(state: &AppState, account_id: Uuid, s: &Settings, since: chrono::DateTime<chrono::Utc>) -> AppResult<Value> {
    let sql = format!(
        "SELECT count(*), COALESCE(sum(value), 0)::float8,
                count(*) FILTER (WHERE message_id IS NOT NULL), COALESCE(sum(value) FILTER (WHERE message_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE campaign_id IS NOT NULL), 0)::float8, COALESCE(sum(value) FILTER (WHERE flow_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE channel = 'email'), 0)::float8, COALESCE(sum(value) FILTER (WHERE channel = 'sms'), 0)::float8
         FROM ({PREVIEW_SQL} AND e.occurred_at >= $8) x"
    );
    let mut tx = super::query::timed(state).await?;
    let (orders, revenue, att_orders, att_rev, camp, flow, email, sms): (i64, f64, i64, f64, f64, f64, f64, f64) = sqlx::query_as(&sql)
        .bind(account_id)
        .bind(&s.conversion_metrics)
        .bind(s.count_opens)
        .bind(s.email_open_window_hours)
        .bind(s.email_click_window_hours)
        .bind(s.sms_click_window_hours)
        .bind(s.model == "first_touch")
        .bind(since)
        .fetch_one(&mut *tx)
        .await
        .map_err(super::query::db_err)?;
    tx.commit().await?;
    Ok(json!({
        "conversions": orders, "revenue": revenue, "attributed_conversions": att_orders, "attributed_revenue": att_rev,
        "campaign_revenue": camp, "flow_revenue": flow, "email_revenue": email, "sms_revenue": sms,
    }))
}

/// What the overview would show over the last `days` days with `proposed` instead of the saved
/// settings. Nothing is saved.
pub async fn preview(state: &AppState, account_id: Uuid, proposed: &Settings, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let current = load(&state.db, account_id).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(days);
    let now = totals(state, account_id, &current, since).await?;
    let next = totals(state, account_id, proposed, since).await?;
    Ok(json!({
        "days": days,
        "current": {"settings": to_json(&current), "totals": now},
        "proposed": {"settings": to_json(proposed), "totals": next},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_description() {
        let s = Settings::default();
        assert_eq!(
            s.describe(),
            "Placed Order counts when the buyer opened or clicked an email in the 5 days before, or clicked a text in the 1 day before. The newest message gets the credit."
        );
        let clicks = s.clone().apply(Patch { count_opens: Some(false), email_click_window_hours: Some(72), ..Default::default() }).unwrap();
        assert!(clicks.describe().contains("clicked an email in the 3 days before"));
        assert!(s.clone().apply(Patch { sms_click_window_hours: Some(0), ..Default::default() }).is_err());
        assert!(s.clone().apply(Patch { email_open_window_hours: Some(MAX_WINDOW_HOURS + 1), ..Default::default() }).is_err());
        assert!(s.clone().apply(Patch { model: Some("linear".into()), ..Default::default() }).is_err());
        assert!(s.clone().apply(Patch { conversion_metrics: Some(vec![]), ..Default::default() }).is_err());
        let dedup = s.apply(Patch { conversion_metrics: Some(vec![" Placed Order ".into(), "Placed Order".into(), "Ordered".into()]), ..Default::default() }).unwrap();
        assert_eq!(dedup.conversion_metrics, vec!["Placed Order", "Ordered"]);
    }
}

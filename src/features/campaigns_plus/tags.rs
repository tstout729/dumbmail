//! Tags and archiving for campaigns, flows and templates, and the filters on their lists.
//!
//! Tags are free text, shared by the account's campaigns, flows and templates: at most
//! [`MAX_PER_ITEM`] on one item and [`MAX_PER_ACCOUNT`] in an account, names up to [`MAX_NAME`]
//! characters. Both limits are checked under one per-account lock in the transaction that writes
//! them, so two requests at once can't pass a limit. Archived items are hidden from lists unless
//! asked for, are still in reports, and can't be sent or scheduled (campaigns) or turned on (flows).

use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{CampaignRow, FlowRow, Template};
use crate::state::AppState;

pub const MAX_PER_ITEM: i64 = 20;
pub const MAX_PER_ACCOUNT: i64 = 200;
pub const MAX_NAME: usize = 40;
/// Items one bulk request may change.
pub const MAX_BULK: usize = 100;
pub const MAX_PAGE: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Campaign,
    Flow,
    Template,
}

impl Kind {
    pub fn parse(s: &str) -> AppResult<Kind> {
        match s {
            "campaign" => Ok(Kind::Campaign),
            "flow" => Ok(Kind::Flow),
            "template" => Ok(Kind::Template),
            _ => Err(AppError::bad("type must be campaign, flow or template")),
        }
    }
    fn table(self) -> &'static str {
        match self {
            Kind::Campaign => "campaigns",
            Kind::Flow => "flows",
            Kind::Template => "templates",
        }
    }
    /// The item's column in `item_tags`.
    fn col(self) -> &'static str {
        match self {
            Kind::Campaign => "campaign_id",
            Kind::Flow => "flow_id",
            Kind::Template => "template_id",
        }
    }
    fn noun(self) -> &'static str {
        match self {
            Kind::Campaign => "campaign",
            Kind::Flow => "flow",
            Kind::Template => "template",
        }
    }
}

fn tag_name(raw: &str) -> AppResult<String> {
    let n = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.is_empty() {
        return Err(AppError::bad("a tag needs a name"));
    }
    if n.chars().count() > MAX_NAME {
        return Err(AppError::bad(format!("tag names are at most {MAX_NAME} characters")));
    }
    if n.chars().any(char::is_control) {
        return Err(AppError::bad("tag names can't contain control characters"));
    }
    Ok(n)
}

/// One writer of an account's tags at a time, until the transaction ends.
async fn lock_tags(db: &mut PgConnection, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('dumbmail.tags.' || $1::text, 0))").bind(account_id).execute(&mut *db).await?;
    Ok(())
}

/// The account's tags with how many campaigns, flows and templates use each.
pub async fn list_tags(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT t.id, t.name, count(it.campaign_id), count(it.flow_id), count(it.template_id)
         FROM tags t LEFT JOIN item_tags it ON it.tag_id = t.id WHERE t.account_id = $1 GROUP BY t.id ORDER BY lower(t.name)",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "data": rows.iter().map(|(id, name, c, f, t)| json!({"id": id, "name": name, "campaigns": c, "flows": f, "templates": t})).collect::<Vec<_>>(),
        "limits": {"per_item": MAX_PER_ITEM, "per_account": MAX_PER_ACCOUNT, "name": MAX_NAME},
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyBody {
    pub ids: Vec<Uuid>,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Add and remove tags (by name; new names become tags) on up to [`MAX_BULK`] items. Returns each
/// item's tags afterwards.
pub async fn apply(state: &AppState, account_id: Uuid, kind: Kind, b: ApplyBody) -> AppResult<Value> {
    let mut ids = b.ids;
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > MAX_BULK {
        return Err(AppError::bad(format!("tag 1 to {MAX_BULK} {}s at a time", kind.noun())));
    }
    if b.add.is_empty() && b.remove.is_empty() {
        return Err(AppError::bad("say which tags to add or remove"));
    }
    if b.add.len() > MAX_PER_ITEM as usize || b.remove.len() > MAX_PER_ACCOUNT as usize {
        return Err(AppError::bad(format!("add at most {MAX_PER_ITEM} tags at a time")));
    }
    let add: Vec<String> = b.add.iter().map(|n| tag_name(n)).collect::<AppResult<_>>()?;
    let remove: Vec<String> = b.remove.iter().map(|n| tag_name(n).map(|n| n.to_lowercase())).collect::<AppResult<_>>()?;
    let mut tx = state.db.begin().await?;
    lock_tags(&mut tx, account_id).await?;
    let found: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {} WHERE account_id = $1 AND id = ANY($2)", kind.table())).bind(account_id).bind(&ids).fetch_one(&mut *tx).await?;
    if found != ids.len() as i64 {
        return Err(AppError::not_found(kind.noun()));
    }
    if !add.is_empty() {
        let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM tags WHERE account_id = $1").bind(account_id).fetch_one(&mut *tx).await?;
        let new: i64 = sqlx::query_scalar("SELECT count(DISTINCT lower(n)) FROM unnest($2::text[]) n WHERE NOT EXISTS (SELECT 1 FROM tags t WHERE t.account_id = $1 AND lower(t.name) = lower(n))")
            .bind(account_id)
            .bind(&add)
            .fetch_one(&mut *tx)
            .await?;
        if existing + new > MAX_PER_ACCOUNT {
            return Err(AppError::LimitReached(format!("an account can have at most {MAX_PER_ACCOUNT} tags; delete some you don't use")));
        }
        for n in &add {
            sqlx::query("INSERT INTO tags (id, account_id, name) VALUES ($1, $2, $3) ON CONFLICT (account_id, lower(name)) DO NOTHING").bind(Uuid::now_v7()).bind(account_id).bind(n).execute(&mut *tx).await?;
        }
        let lower: Vec<String> = add.iter().map(|n| n.to_lowercase()).collect();
        let col = kind.col();
        sqlx::query(&format!(
            "INSERT INTO item_tags (tag_id, account_id, {col}) SELECT t.id, $1, i FROM tags t CROSS JOIN unnest($2::uuid[]) i
             WHERE t.account_id = $1 AND lower(t.name) = ANY($3) ON CONFLICT ({col}, tag_id) WHERE {col} IS NOT NULL DO NOTHING"
        ))
        .bind(account_id)
        .bind(&ids)
        .bind(&lower)
        .execute(&mut *tx)
        .await?;
        let over: Option<Uuid> = sqlx::query_scalar(&format!("SELECT {col} FROM item_tags WHERE {col} = ANY($1) GROUP BY {col} HAVING count(*) > $2 LIMIT 1"))
            .bind(&ids)
            .bind(MAX_PER_ITEM)
            .fetch_optional(&mut *tx)
            .await?;
        if over.is_some() {
            return Err(AppError::LimitReached(format!("a {} can have at most {MAX_PER_ITEM} tags", kind.noun())));
        }
    }
    if !remove.is_empty() {
        let col = kind.col();
        sqlx::query(&format!("DELETE FROM item_tags it USING tags t WHERE t.id = it.tag_id AND t.account_id = $1 AND it.{col} = ANY($2) AND lower(t.name) = ANY($3)"))
            .bind(account_id)
            .bind(&ids)
            .bind(&remove)
            .execute(&mut *tx)
            .await?;
    }
    let col = kind.col();
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(&format!(
        "SELECT i, COALESCE((SELECT jsonb_agg(jsonb_build_object('id', t.id, 'name', t.name) ORDER BY lower(t.name)) FROM item_tags it JOIN tags t ON t.id = it.tag_id WHERE it.{col} = i), '[]')
         FROM unnest($1::uuid[]) i"
    ))
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"data": rows.into_iter().map(|(id, tags)| json!({"id": id, "tags": tags})).collect::<Vec<_>>()}))
}

pub async fn rename(state: &AppState, account_id: Uuid, id: Uuid, name: &str) -> AppResult<Value> {
    let name = tag_name(name)?;
    let mut tx = state.db.begin().await?;
    lock_tags(&mut tx, account_id).await?;
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tags WHERE account_id = $1 AND lower(name) = lower($2) AND id <> $3)").bind(account_id).bind(&name).bind(id).fetch_one(&mut *tx).await?;
    if taken {
        return Err(AppError::Conflict(format!("there's already a tag called {name:?}")));
    }
    let row: Option<(Uuid, String)> = sqlx::query_as("UPDATE tags SET name = $3 WHERE id = $1 AND account_id = $2 RETURNING id, name").bind(id).bind(account_id).bind(&name).fetch_optional(&mut *tx).await?;
    let (id, name) = row.ok_or_else(|| AppError::not_found("tag"))?;
    tx.commit().await?;
    Ok(json!({"id": id, "name": name}))
}

pub async fn delete(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM tags WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("tag"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveBody {
    pub ids: Vec<Uuid>,
    /// false unarchives. Default true.
    pub archived: Option<bool>,
}

/// Archive or unarchive up to [`MAX_BULK`] items. A campaign that's scheduled, sending or waiting on
/// its A/B test, or a live flow, is left as it is (the status is checked in the write, so a schedule
/// or a turn-on landing at the same moment and this can't both win). Returns what changed and what didn't.
pub async fn archive(state: &AppState, account_id: Uuid, kind: Kind, b: ArchiveBody) -> AppResult<Value> {
    let mut ids = b.ids;
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > MAX_BULK {
        return Err(AppError::bad(format!("archive 1 to {MAX_BULK} {}s at a time", kind.noun())));
    }
    let archived = b.archived.unwrap_or(true);
    let allowed = match (kind, archived) {
        (_, false) | (Kind::Template, true) => "true",
        (Kind::Campaign, true) => "status IN ('draft', 'sent', 'cancelled', 'failed')",
        (Kind::Flow, true) => "status <> 'live'",
    };
    let changed: Vec<Uuid> = sqlx::query_scalar(&format!(
        "UPDATE {} SET archived_at = CASE WHEN $3 THEN now() END, updated_at = now()
         WHERE account_id = $1 AND id = ANY($2) AND (archived_at IS NULL) = $3 AND {allowed} RETURNING id",
        kind.table()
    ))
    .bind(account_id)
    .bind(&ids)
    .bind(archived)
    .fetch_all(&state.db)
    .await?;
    let rest: Vec<Uuid> = ids.iter().filter(|i| !changed.contains(i)).copied().collect();
    let status = if kind == Kind::Template { "''" } else { "status" };
    let skipped: Vec<(Uuid, String, bool)> = sqlx::query_as(&format!("SELECT id, {status}, archived_at IS NOT NULL FROM {} WHERE account_id = $1 AND id = ANY($2)", kind.table()))
        .bind(account_id)
        .bind(&rest)
        .fetch_all(&state.db)
        .await?;
    if changed.is_empty() && skipped.len() < rest.len() {
        return Err(AppError::not_found(kind.noun()));
    }
    let why = |status: &str, is_archived: bool| -> String {
        if is_archived == archived {
            return if archived { "already archived".into() } else { "not archived".into() };
        }
        match kind {
            Kind::Flow => "turn the flow off first".into(),
            _ => format!("it's {status}; unschedule or cancel it first"),
        }
    };
    Ok(json!({
        "changed": changed,
        "skipped": skipped.iter().map(|(id, s, a)| json!({"id": id, "reason": why(s, *a)})).collect::<Vec<_>>(),
    }))
}

/// Refuse to send, schedule or turn on an archived item, with a clear reason. (The write that does
/// it checks again, so an archive at the same moment can't slip past.)
pub async fn refuse_if_archived(db: &sqlx::PgPool, kind: Kind, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let archived: Option<bool> = sqlx::query_scalar(&format!("SELECT archived_at IS NOT NULL FROM {} WHERE id = $1 AND account_id = $2", kind.table())).bind(id).bind(account_id).fetch_optional(db).await?;
    if archived == Some(true) {
        let action = if kind == Kind::Flow { "turned on" } else { "sent or scheduled" };
        return Err(AppError::Conflict(format!("this {} is archived and can't be {action}; unarchive it first", kind.noun())));
    }
    Ok(())
}

// ---------- Lists ----------

/// Filters for the campaign, flow and template lists. Every one is optional; with none, a list runs
/// one statement as before, now leaving archived items out.
#[derive(Deserialize, Default, Debug, Clone)]
pub struct ListQuery {
    pub channel: Option<String>,
    /// One status, or several separated by commas.
    pub status: Option<String>,
    /// A tag's id.
    pub tag: Option<String>,
    /// false (default): not archived; true: only archived; all: both.
    pub archived: Option<String>,
    /// Text in the name (campaigns: or the subject).
    pub q: Option<String>,
    /// Campaigns whose send date (or creation date, for drafts with none) falls on these days, YYYY-MM-DD in the account's time zone.
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

struct Filters {
    channel: Option<String>,
    statuses: Option<Vec<String>>,
    tag: Option<Uuid>,
    archived: Option<bool>,
    q: Option<String>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    limit: i64,
    offset: i64,
}

fn parse(q: &ListQuery) -> AppResult<Filters> {
    let num = |v: &Option<String>, name: &str, default: i64, max: i64| -> AppResult<i64> {
        match v.as_deref().filter(|s| !s.is_empty()) {
            None => Ok(default),
            Some(s) => s.parse::<i64>().ok().filter(|n| (0..=max).contains(n)).ok_or_else(|| AppError::bad(format!("{name} must be a number from 0 to {max}"))),
        }
    };
    let date = |v: &Option<String>, name: &str| -> AppResult<Option<NaiveDate>> {
        v.as_deref().filter(|s| !s.is_empty()).map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::bad(format!("{name} must be a date like 2026-11-27")))).transpose()
    };
    let text = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if text.is_some_and(|t| t.chars().count() > 200) {
        return Err(AppError::bad("search text is at most 200 characters"));
    }
    let statuses = q.status.as_deref().filter(|s| !s.is_empty()).map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).take(10).collect::<Vec<_>>());
    Ok(Filters {
        channel: q.channel.clone().filter(|s| !s.is_empty()),
        statuses,
        tag: q.tag.as_deref().filter(|s| !s.is_empty()).map(|s| s.parse().map_err(|_| AppError::bad("tag must be a tag's id"))).transpose()?,
        archived: match q.archived.as_deref().unwrap_or("false") {
            "false" | "" => Some(false),
            "true" => Some(true),
            "all" => None,
            _ => return Err(AppError::bad("archived must be false, true or all")),
        },
        q: text.map(|t| format!("%{}%", t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))),
        from: date(&q.from, "from")?,
        to: date(&q.to, "to")?,
        limit: num(&q.limit, "limit", MAX_PAGE, MAX_PAGE)?.max(1),
        offset: num(&q.offset, "offset", 0, 1_000_000)?,
    })
}

fn tags_sql(col: &str, alias: &str) -> String {
    format!("COALESCE((SELECT jsonb_agg(jsonb_build_object('id', t.id, 'name', t.name) ORDER BY lower(t.name)) FROM item_tags it JOIN tags t ON t.id = it.tag_id WHERE it.{col} = {alias}.id), '[]') AS cp_tags")
}

fn push_common(qb: &mut QueryBuilder<'_, Postgres>, f: &Filters, kind: Kind, alias: &str) {
    if let Some(a) = f.archived {
        qb.push(format!(" AND {alias}.archived_at IS {}", if a { "NOT NULL" } else { "NULL" }));
    }
    if let Some(tag) = f.tag {
        qb.push(format!(" AND EXISTS (SELECT 1 FROM item_tags it WHERE it.{} = {alias}.id AND it.tag_id = ", kind.col())).push_bind(tag).push(")");
    }
    if let Some(s) = &f.statuses {
        qb.push(format!(" AND {alias}.status = ANY(")).push_bind(s.clone()).push(")");
    }
}

fn extras(row: &PgRow) -> (Value, Option<chrono::DateTime<Utc>>, i64) {
    (row.try_get("cp_tags").unwrap_or(json!([])), row.try_get("archived_at").ok().flatten(), row.try_get("cp_total").unwrap_or(0))
}

/// The campaign list: each campaign as the API shows it, plus `tags`, `archived_at` and `resend_of`;
/// and the total matching (for paging). One statement, then each campaign's stats as before.
pub async fn campaigns(state: &AppState, account_id: Uuid, q: &ListQuery) -> AppResult<(Vec<Value>, i64)> {
    let f = parse(q)?;
    let mut qb = QueryBuilder::new("SELECT c.*, count(*) OVER () AS cp_total, ");
    qb.push(tags_sql("campaign_id", "c"));
    qb.push(", (SELECT r.original_id FROM campaign_resends r WHERE r.campaign_id = c.id) AS cp_resend_of FROM campaigns c WHERE c.account_id = ").push_bind(account_id);
    if let Some(ch) = &f.channel {
        qb.push(" AND c.channel = ").push_bind(ch.clone());
    }
    push_common(&mut qb, &f, Kind::Campaign, "c");
    if let Some(t) = &f.q {
        qb.push(" AND (c.name ILIKE ").push_bind(t.clone()).push(" OR c.content->>'subject' ILIKE ").push_bind(t.clone()).push(")");
    }
    if f.from.is_some() || f.to.is_some() {
        let zone = "(SELECT a.timezone FROM accounts a WHERE a.id = c.account_id)";
        if let Some(d) = f.from {
            qb.push(" AND COALESCE(c.sent_at, c.send_at, c.created_at) >= (").push_bind(d).push(format!("::date)::timestamp AT TIME ZONE {zone}"));
        }
        if let Some(d) = f.to {
            qb.push(" AND COALESCE(c.sent_at, c.send_at, c.created_at) < (").push_bind(d).push(format!("::date + 1)::timestamp AT TIME ZONE {zone}"));
        }
    }
    qb.push(" ORDER BY COALESCE(c.sent_at, c.send_at, c.created_at) DESC, c.id DESC LIMIT ").push_bind(f.limit).push(" OFFSET ").push_bind(f.offset);
    let rows = qb.build().fetch_all(&state.db).await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut total = 0;
    for row in &rows {
        let c = CampaignRow::from_row(row)?;
        let (tags, archived_at, t) = extras(row);
        total = t;
        let resend_of: Option<Uuid> = row.try_get("cp_resend_of").ok().flatten();
        let mut v = crate::api::messaging::campaign_json(state, &c).await?;
        v["tags"] = tags;
        v["archived_at"] = json!(archived_at);
        v["resend_of"] = json!(resend_of);
        out.push(v);
    }
    Ok((out, total))
}

/// The flow list (live first, then most recently changed), with `tags` and `archived_at`.
pub async fn flows(state: &AppState, account_id: Uuid, q: &ListQuery) -> AppResult<(Vec<Value>, i64)> {
    let f = parse(q)?;
    let mut qb = QueryBuilder::new("SELECT f.*, count(*) OVER () AS cp_total, ");
    qb.push(tags_sql("flow_id", "f"));
    qb.push(" FROM flows f WHERE f.account_id = ").push_bind(account_id);
    push_common(&mut qb, &f, Kind::Flow, "f");
    if let Some(t) = &f.q {
        qb.push(" AND f.name ILIKE ").push_bind(t.clone());
    }
    qb.push(" ORDER BY (f.status = 'live') DESC, f.updated_at DESC LIMIT ").push_bind(f.limit).push(" OFFSET ").push_bind(f.offset);
    let rows = qb.build().fetch_all(&state.db).await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut total = 0;
    for row in &rows {
        let fl = FlowRow::from_row(row)?;
        let (tags, archived_at, t) = extras(row);
        total = t;
        let mut v = crate::api::messaging::flow_json(state, &fl).await?;
        v["tags"] = tags;
        v["archived_at"] = json!(archived_at);
        out.push(v);
    }
    Ok((out, total))
}

/// The template list (most recently changed first), with `tags` and `archived_at`.
pub async fn templates(state: &AppState, account_id: Uuid, q: &ListQuery) -> AppResult<(Vec<Value>, i64)> {
    let f = parse(q)?;
    let mut qb = QueryBuilder::new("SELECT t0.id, t0.name, t0.channel, t0.content, t0.created_at, t0.updated_at, t0.archived_at, count(*) OVER () AS cp_total, ");
    qb.push(tags_sql("template_id", "t0"));
    qb.push(" FROM templates t0 WHERE t0.account_id = ").push_bind(account_id);
    if let Some(ch) = &f.channel {
        qb.push(" AND t0.channel = ").push_bind(ch.clone());
    }
    push_common(&mut qb, &f, Kind::Template, "t0");
    if let Some(t) = &f.q {
        qb.push(" AND t0.name ILIKE ").push_bind(t.clone());
    }
    qb.push(" ORDER BY t0.updated_at DESC LIMIT ").push_bind(f.limit).push(" OFFSET ").push_bind(f.offset);
    let rows = qb.build().fetch_all(&state.db).await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut total = 0;
    for row in &rows {
        let t = Template::from_row(row)?;
        let (tags, archived_at, n) = extras(row);
        total = n;
        let mut v = serde_json::to_value(&t)?;
        v["tags"] = tags;
        v["archived_at"] = json!(archived_at);
        out.push(v);
    }
    Ok((out, total))
}

/// How many campaigns are in each status (not archived), and how many are archived: the list's tabs.
pub async fn campaign_counts(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT status, count(*) FROM campaigns WHERE account_id = $1 AND archived_at IS NULL GROUP BY status").bind(account_id).fetch_all(&state.db).await?;
    let archived: i64 = sqlx::query_scalar("SELECT count(*) FROM campaigns WHERE account_id = $1 AND archived_at IS NOT NULL").bind(account_id).fetch_one(&state.db).await?;
    let mut by = serde_json::Map::new();
    for (s, n) in rows {
        by.insert(s, json!(n));
    }
    Ok(json!({"by_status": by, "archived": archived}))
}

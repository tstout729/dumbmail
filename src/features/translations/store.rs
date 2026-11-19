//! Where a message's content lives, and changing it safely: a campaign (or one of its A/B
//! versions), a flow's email or text step, a template, or a saved block. Every change is a
//! read-modify-write of one row under its row lock, checked like any other save of that content.

use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// A message whose text can be translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A campaign; `version` is an A/B version id (b, c, d), None for version A.
    Campaign { id: Uuid, version: Option<String> },
    /// A flow's email or text step.
    FlowStep { id: Uuid, node: String },
    Template { id: Uuid },
    SavedBlock { id: Uuid },
}

impl Target {
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Campaign { .. } => "campaign",
            Target::FlowStep { .. } => "flow",
            Target::Template { .. } => "template",
            Target::SavedBlock { .. } => "saved_block",
        }
    }

    pub fn id(&self) -> Uuid {
        match self {
            Target::Campaign { id, .. } | Target::FlowStep { id, .. } | Target::Template { id } | Target::SavedBlock { id } => *id,
        }
    }

    /// The version letter or flow step id ("" when there's none).
    pub fn part(&self) -> String {
        match self {
            Target::Campaign { version, .. } => version.clone().unwrap_or_default(),
            Target::FlowStep { node, .. } => node.clone(),
            _ => String::new(),
        }
    }

    pub fn from_parts(kind: &str, id: Uuid, part: &str) -> Option<Self> {
        Some(match kind {
            "campaign" => Target::Campaign { id, version: (!part.is_empty()).then(|| part.to_string()) },
            "flow" => Target::FlowStep { id, node: part.to_string() },
            "template" => Target::Template { id },
            "saved_block" => Target::SavedBlock { id },
            _ => return None,
        })
    }

    /// For people and the audit log: `campaign <id> version b`.
    pub fn describe(&self) -> String {
        match self {
            Target::Campaign { id, version: Some(v) } => format!("campaign {id} version {v}"),
            Target::FlowStep { id, node } => format!("flow {id} step {node}"),
            t => format!("{} {}", t.kind(), t.id()),
        }
    }
}

/// A message's content as translations see it, with its channel and name.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub content: Value,
    /// email or sms
    pub channel: String,
    pub name: String,
}

/// Version letters are a single lower-case letter b-d (version A is the campaign's own content).
fn version_id(v: &Option<String>) -> AppResult<Option<String>> {
    match v.as_deref().map(|s| s.trim().to_lowercase()) {
        None => Ok(None),
        Some(s) if s.is_empty() || s == "a" => Ok(None),
        Some(s) if matches!(s.as_str(), "b" | "c" | "d") => Ok(Some(s)),
        Some(_) => Err(AppError::bad("version must be a, b, c or d")),
    }
}

impl Target {
    /// Normalize the version letter.
    pub fn checked(self) -> AppResult<Self> {
        Ok(match self {
            Target::Campaign { id, version } => Target::Campaign { id, version: version_id(&version)? },
            Target::FlowStep { id, node } if node.trim().is_empty() || node.len() > 100 => return Err(AppError::bad(format!("give the flow step's id (got {node:?} for flow {id})"))),
            t => t,
        })
    }
}

fn campaign_content(content: &Value, ab: &Option<Value>, version: &Option<String>) -> AppResult<Value> {
    match version {
        None => Ok(content.clone()),
        Some(v) => ab
            .as_ref()
            .and_then(|t| t.get("variants"))
            .and_then(|vs| vs.as_array())
            .and_then(|vs| vs.iter().find(|x| x.get("id").and_then(|i| i.as_str()) == Some(v.as_str())))
            .and_then(|x| x.get("content"))
            .cloned()
            .ok_or_else(|| AppError::not_found("campaign version")),
    }
}

fn flow_content(def: &Value, node: &str) -> AppResult<(Value, String)> {
    let n = def
        .get("nodes")
        .and_then(|n| n.as_array())
        .and_then(|ns| ns.iter().find(|n| n.get("id").and_then(|i| i.as_str()) == Some(node)))
        .ok_or_else(|| AppError::not_found("flow step"))?;
    let kind = n.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "email" && kind != "sms" {
        return Err(AppError::bad("only email and text steps have text to translate"));
    }
    Ok((n.get("content").cloned().unwrap_or(json!({})), kind.to_string()))
}

/// Read a message (account-scoped).
pub async fn load(db: &PgPool, account_id: Uuid, t: &Target) -> AppResult<Loaded> {
    let mut conn = db.acquire().await?;
    load_on(&mut conn, account_id, t, false).await.map(|(l, _)| l)
}

/// Read a message on `conn` (inside a transaction).
pub async fn load_in(conn: &mut PgConnection, account_id: Uuid, t: &Target) -> AppResult<Loaded> {
    load_on(conn, account_id, t, false).await.map(|(l, _)| l)
}

/// Read a message on `conn`, locking its row when `lock` (inside a transaction). Also returns the
/// campaign's status.
async fn load_on(conn: &mut PgConnection, account_id: Uuid, t: &Target, lock: bool) -> AppResult<(Loaded, String)> {
    let lock = if lock { " FOR UPDATE" } else { "" };
    match t {
        Target::Campaign { id, version } => {
            let row: Option<(String, String, Value, Option<Value>, String)> =
                sqlx::query_as(&format!("SELECT name, channel, content, ab_test, status FROM campaigns WHERE id = $1 AND account_id = $2{lock}")).bind(id).bind(account_id).fetch_optional(&mut *conn).await?;
            let (name, channel, content, ab, status) = row.ok_or_else(|| AppError::not_found("campaign"))?;
            Ok((Loaded { content: campaign_content(&content, &ab, version)?, channel, name }, status))
        }
        Target::FlowStep { id, node } => {
            let row: Option<(String, Value)> = sqlx::query_as(&format!("SELECT name, definition FROM flows WHERE id = $1 AND account_id = $2{lock}")).bind(id).bind(account_id).fetch_optional(&mut *conn).await?;
            let (name, def) = row.ok_or_else(|| AppError::not_found("flow"))?;
            let (content, channel) = flow_content(&def, node)?;
            Ok((Loaded { content, channel, name }, String::new()))
        }
        Target::Template { id } => {
            let row: Option<(String, String, Value)> = sqlx::query_as(&format!("SELECT name, channel, content FROM templates WHERE id = $1 AND account_id = $2{lock}")).bind(id).bind(account_id).fetch_optional(&mut *conn).await?;
            let (name, channel, content) = row.ok_or_else(|| AppError::not_found("template"))?;
            Ok((Loaded { content, channel, name }, String::new()))
        }
        Target::SavedBlock { id } => {
            let row: Option<(String, Value)> =
                sqlx::query_as(&format!("SELECT name, blocks FROM saved_blocks WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL{lock}")).bind(id).bind(account_id).fetch_optional(&mut *conn).await?;
            let (name, blocks) = row.ok_or_else(|| AppError::not_found("saved block"))?;
            Ok((Loaded { content: json!({"blocks": blocks}), channel: "email".into(), name }, String::new()))
        }
    }
}

/// Campaign statuses whose content may change here: drafts, and scheduled campaigns for people
/// who may change those (`scheduled_ok`).
fn campaign_editable(status: &str, scheduled_ok: bool) -> AppResult<()> {
    match status {
        "draft" => Ok(()),
        "scheduled" if scheduled_ok => Ok(()),
        "scheduled" => Err(AppError::Conflict("this campaign is scheduled; only people who can send campaigns can change it now".into())),
        s => Err(AppError::Conflict(format!("this campaign is {s} and can't be edited; duplicate it instead"))),
    }
}

/// Change a message's content with `f`, under its row lock: read it, apply `f`, check the result
/// like any save of it, write it. `scheduled_ok`: a scheduled campaign may change too.
pub async fn update<R>(db: &PgPool, account_id: Uuid, t: &Target, scheduled_ok: bool, f: impl FnOnce(&mut Loaded) -> AppResult<R>) -> AppResult<R> {
    let mut tx = db.begin().await?;
    let out = update_in(&mut tx, account_id, t, scheduled_ok, f).await?;
    tx.commit().await?;
    if matches!(t, Target::FlowStep { .. } | Target::SavedBlock { .. }) {
        crate::features::screening::content_changed(account_id);
    }
    Ok(out)
}

/// [`update`] inside the caller's transaction (it commits).
pub async fn update_in<R>(tx: &mut PgConnection, account_id: Uuid, t: &Target, scheduled_ok: bool, f: impl FnOnce(&mut Loaded) -> AppResult<R>) -> AppResult<R> {
    let (mut l, status) = load_on(&mut *tx, account_id, t, true).await?;
    if let Target::Campaign { .. } = t {
        campaign_editable(&status, scheduled_ok)?;
    }
    let out = f(&mut l)?;
    // Every block keeps (or gets) its stable key, like on any other save.
    super::fields::assign_keys(&mut l.content);
    // The same check every save of this content gets (coupon tags and language versions).
    crate::features::coupons::template::check_content(&l.channel, &l.content)?;
    match t {
        Target::Campaign { id, version: None } => {
            sqlx::query("UPDATE campaigns SET content = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).bind(&l.content).execute(&mut *tx).await?;
        }
        Target::Campaign { id, version: Some(v) } => {
            let ab: Option<Value> = sqlx::query_scalar("SELECT ab_test FROM campaigns WHERE id = $1").bind(id).fetch_one(&mut *tx).await?;
            let mut ab = ab.ok_or_else(|| AppError::not_found("campaign version"))?;
            let slot = ab
                .get_mut("variants")
                .and_then(|vs| vs.as_array_mut())
                .and_then(|vs| vs.iter_mut().find(|x| x.get("id").and_then(|i| i.as_str()) == Some(v.as_str())))
                .ok_or_else(|| AppError::not_found("campaign version"))?;
            slot["content"] = l.content.clone();
            sqlx::query("UPDATE campaigns SET ab_test = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).bind(&ab).execute(&mut *tx).await?;
        }
        Target::FlowStep { id, node } => {
            let mut def: Value = sqlx::query_scalar("SELECT definition FROM flows WHERE id = $1").bind(id).fetch_one(&mut *tx).await?;
            let n = def
                .get_mut("nodes")
                .and_then(|n| n.as_array_mut())
                .and_then(|ns| ns.iter_mut().find(|n| n.get("id").and_then(|i| i.as_str()) == Some(node.as_str())))
                .ok_or_else(|| AppError::not_found("flow step"))?;
            n["content"] = l.content.clone();
            sqlx::query("UPDATE flows SET definition = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).bind(&def).execute(&mut *tx).await?;
        }
        Target::Template { id } => {
            sqlx::query("UPDATE templates SET content = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).bind(&l.content).execute(&mut *tx).await?;
        }
        Target::SavedBlock { id } => {
            let blocks = crate::features::content::saved::check_blocks(l.content.get("blocks").unwrap_or(&Value::Null))?;
            sqlx::query("UPDATE saved_blocks SET blocks = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).bind(&blocks).execute(&mut *tx).await?;
        }
    }
    // What a live flow or a saved block sends changed outside the editors' checks: sender screening
    // matches messages to a check again (features::screening).
    if matches!(t, Target::FlowStep { .. } | Target::SavedBlock { .. }) {
        crate::features::screening::content_changed(account_id);
    }
    Ok(out)
}

//! Getting email content ready to render, for each recipient (and each preview):
//!
//! - saved blocks are replaced by their current content (or, once a campaign's send has started,
//!   by the copy taken then: see [`freeze_campaign`]);
//! - display rules (`show_if`) are decided: a simple rule is checked here against the person and
//!   event, and an advanced rule becomes `if_open` / `if_close` markers the template engine checks;
//! - social icons and countdown images get their DumbMail-hosted addresses.
//!
//! Runs before `render::render_email` in `sending::send` and the preview endpoint. Fields starting
//! with `_` are computed here; anything content sets in them is replaced.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::blocks::{network_label, ICON_STYLES, NETWORKS};
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, Profile};
use crate::state::AppState;

/// The most blocks an email may have once saved blocks are filled in.
pub const MAX_BLOCKS: usize = 300;

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// An advanced display rule that's safe to drop into `{% if ... %}`: it can't close the tag or
/// open another one, and it parses as an expression.
pub fn checked_expr(expr: &str) -> Option<&str> {
    let e = expr.trim();
    if e.is_empty() || e.len() > 500 || e.contains(['{', '}', '%', '#']) || e.chars().any(|c| c.is_control()) {
        return None;
    }
    minijinja::Environment::new().compile_expression(e).ok().map(|_| e)
}

pub const RULE_OPS: [&str; 7] = ["set", "not_set", "eq", "neq", "contains", "gt", "lt"];

/// A display rule: `{"field": "person.properties.vip", "op": "eq", "value": "true"}` or
/// `{"expr": "event.value > 100"}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    Simple { field: Vec<String>, op: String, value: String },
    Expr(String),
}

pub fn parse_rule(v: &Value) -> Result<Rule, String> {
    if let Some(e) = v.get("expr").and_then(|e| e.as_str()).filter(|e| !e.trim().is_empty()) {
        return checked_expr(e).map(|e| Rule::Expr(e.to_string())).ok_or_else(|| {
            "the advanced display rule isn't a valid expression (it can't contain { } % or #; try something like event.value > 100)".to_string()
        });
    }
    let field = s(v, "field").trim();
    let parts: Vec<String> = field.split('.').map(|p| p.trim().to_string()).collect();
    let root_ok = parts.first().is_some_and(|r| !r.is_empty() && r.len() <= 64 && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    if !root_ok || parts.len() > 6 || parts.iter().any(|p| p.is_empty() || p.len() > 64 || p.chars().any(|c| c.is_control())) {
        return Err("pick what the display rule checks (like person.properties.vip or event.value)".into());
    }
    let op = s(v, "op");
    if !RULE_OPS.contains(&op) {
        return Err(format!("the display rule's op must be one of {}", RULE_OPS.join(", ")));
    }
    let value = match v.get("value") {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    };
    if value.len() > 200 {
        return Err("the display rule's value is too long (200 characters at most)".into());
    }
    if matches!(op, "gt" | "lt") && value.trim().parse::<f64>().is_err() {
        return Err("a greater than / less than rule needs a number".into());
    }
    Ok(Rule::Simple { field: parts, op: op.to_string(), value })
}

fn lookup<'a>(ctx: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(ctx, |v, k| match v {
        Value::Object(o) => o.get(k),
        Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Whether a simple rule holds for this recipient's personalization data.
pub fn rule_matches(field: &[String], op: &str, value: &str, ctx: &Value) -> bool {
    let v = lookup(ctx, field);
    let set = match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(_) => true,
    };
    let want = value.trim().to_lowercase();
    match op {
        "set" => set,
        "not_set" => !set,
        "eq" => v.is_some_and(|v| as_text(v).to_lowercase() == want),
        "neq" => !v.is_some_and(|v| as_text(v).to_lowercase() == want),
        "contains" => match v {
            Some(Value::Array(a)) => a.iter().any(|x| as_text(x).to_lowercase() == want),
            Some(x) => set && as_text(x).to_lowercase().contains(&want),
            None => false,
        },
        "gt" | "lt" => match (v.and_then(as_number), want.parse::<f64>()) {
            (Some(a), Ok(b)) => if op == "gt" { a > b } else { a < b },
            _ => false,
        },
        _ => false,
    }
}

/// Saved blocks this content points at, keyed by id: (name, blocks).
async fn load_saved(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> AppResult<HashMap<Uuid, (String, Value)>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Deleted saved blocks still resolve, so emails that point at one keep showing it.
    let rows: Vec<(Uuid, String, Value)> = sqlx::query_as("SELECT id, name, blocks FROM saved_blocks WHERE account_id = $1 AND id = ANY($2)")
        .bind(account_id)
        .bind(ids)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|(id, n, b)| (id, (n, b))).collect())
}

fn saved_id(b: &Value) -> Option<Uuid> {
    s(b, "saved_block_id").parse().ok()
}

/// Ids of saved blocks referenced (and not yet copied in) at the top level or inside columns.
fn live_refs(blocks: &[Value], out: &mut Vec<Uuid>) {
    for b in blocks {
        match s(b, "type") {
            "saved" if !b.get("frozen").and_then(|f| f.as_bool()).unwrap_or(false) => out.extend(saved_id(b)),
            "columns" => {
                for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                    live_refs(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), out);
                }
            }
            _ => {}
        }
    }
}

/// What a saved-block reference shows: the saved block's current content, or (when it was
/// copied in, or doesn't belong to this account) the copy stored with the reference.
fn saved_content(b: &Value, saved: &HashMap<Uuid, (String, Value)>) -> Vec<Value> {
    let frozen = b.get("frozen").and_then(|f| f.as_bool()).unwrap_or(false);
    let from_db = (!frozen).then(|| saved_id(b).and_then(|id| saved.get(&id))).flatten().map(|(_, v)| v);
    from_db
        .or_else(|| b.get("blocks"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|x| s(x, "type") != "saved").cloned().collect())
        .unwrap_or_default()
}

pub struct Prep<'a> {
    pub account: &'a Account,
    pub ctx: &'a Value,
    pub public_url: &'a str,
    pub secret: &'a str,
    saved: HashMap<Uuid, (String, Value)>,
    count: usize,
}

impl Prep<'_> {
    fn expand(&mut self, list: &[Value], depth: usize, out: &mut Vec<Value>) -> AppResult<()> {
        for (i, b) in list.iter().enumerate() {
            let kind = s(b, "type");
            // The markers are made here only; content can't place its own.
            if kind == "if_open" || kind == "if_close" {
                continue;
            }
            let mut b = b.clone();
            let rule = match b.as_object_mut().and_then(|o| o.remove("show_if")) {
                Some(Value::Null) | None => None,
                Some(r) => Some(parse_rule(&r).map_err(|e| AppError::bad(format!("block {}: {e}", i + 1)))?),
            };
            let expr = match rule {
                Some(Rule::Simple { field, op, value }) => {
                    if !rule_matches(&field, &op, &value, self.ctx) {
                        continue;
                    }
                    None
                }
                Some(Rule::Expr(e)) => Some(e),
                None => None,
            };
            if let Some(e) = &expr {
                out.push(json!({"type": "if_open", "expr": e}));
            }
            match kind {
                "saved" => {
                    let inner = saved_content(&b, &self.saved);
                    self.expand(&inner, depth, out)?;
                }
                "columns" if depth == 0 => {
                    let cols: Vec<Value> = b.get("columns").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                    let mut next = Vec::with_capacity(cols.len());
                    for col in cols.iter().take(4) {
                        let mut inner = Vec::new();
                        self.expand(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), depth + 1, &mut inner)?;
                        next.push(Value::Array(inner));
                    }
                    b["columns"] = Value::Array(next);
                    self.push(out, b)?;
                }
                _ => {
                    self.fill(&mut b);
                    self.push(out, b)?;
                }
            }
            if expr.is_some() {
                out.push(json!({"type": "if_close"}));
            }
        }
        Ok(())
    }

    fn push(&mut self, out: &mut Vec<Value>, b: Value) -> AppResult<()> {
        self.count += 1;
        if self.count > MAX_BLOCKS {
            return Err(AppError::bad(format!("an email can have at most {MAX_BLOCKS} blocks, counting the ones inside saved blocks")));
        }
        out.push(b);
        Ok(())
    }

    /// Hosted icon and countdown addresses for blocks that show them.
    fn fill(&self, b: &mut Value) {
        let Some(o) = b.as_object_mut() else { return };
        o.retain(|k, _| !k.starts_with('_'));
        match o.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "social" => {
                let icons = self.icons(o.get("links"), o.get("style"));
                o.insert("_icons".into(), icons);
            }
            "footer" => {
                let style = o.get("style").cloned().or(Some(json!("light")));
                let icons = self.icons(o.get("social"), style.as_ref());
                o.insert("_icons".into(), icons);
            }
            "countdown" => {
                if let Some((src, alt)) = super::countdown::image_for(self, &Value::Object(o.clone())) {
                    o.insert("_src".into(), json!(src));
                    o.insert("_alt".into(), json!(alt));
                }
            }
            _ => {}
        }
    }

    /// Icons for a list of `{network, url}`; with no list, the account's social links.
    fn icons(&self, links: Option<&Value>, style: Option<&Value>) -> Value {
        let style = style.and_then(|s| s.as_str()).filter(|s| ICON_STYLES.contains(s)).unwrap_or("color");
        let given: Vec<(String, String)> = links
            .and_then(|l| l.as_array())
            .map(|a| a.iter().take(10).map(|l| (s(l, "network").to_string(), s(l, "url").trim().to_string())).collect())
            .unwrap_or_default();
        let pairs: Vec<(String, String)> = if given.is_empty() {
            crate::brand::SOCIAL_NETWORKS
                .iter()
                .filter_map(|n| self.account.social_links.get(*n).and_then(|u| u.as_str()).map(|u| (n.to_string(), u.to_string())))
                .collect()
        } else {
            given
        };
        let icons: Vec<Value> = pairs
            .into_iter()
            .filter(|(n, _)| NETWORKS.contains(&n.as_str()))
            .filter_map(|(n, url)| {
                let href = if n == "email" && !url.contains(':') && url.contains('@') { format!("mailto:{url}") } else { url };
                let lower = href.to_ascii_lowercase();
                let ok = lower.starts_with("https://") || lower.starts_with("http://") || (n == "email" && lower.starts_with("mailto:")) || href.starts_with("{{");
                ok.then(|| json!({"href": href, "src": format!("{}/e/icons/{style}-{n}.png", self.public_url), "label": network_label(&n)}))
            })
            .collect();
        Value::Array(icons)
    }
}

/// `content` ready to render for the person and event in `ctx` (see the module docs).
pub async fn prepare(state: &AppState, account: &Account, content: &Value, ctx: &Value) -> AppResult<Value> {
    let Some(blocks) = content.get("blocks").and_then(|b| b.as_array()).filter(|b| !b.is_empty()) else {
        return Ok(content.clone());
    };
    let mut ids = Vec::new();
    live_refs(blocks, &mut ids);
    ids.sort();
    ids.dedup();
    let saved = load_saved(&state.db, account.id, &ids).await?;
    let mut prep = Prep { account, ctx, public_url: state.cfg.public_url(), secret: &state.cfg.secret_key, saved, count: 0 };
    let mut out = Vec::with_capacity(blocks.len());
    prep.expand(blocks, 0, &mut out)?;
    let mut content = content.clone();
    content["blocks"] = Value::Array(out);
    Ok(content)
}

/// [`prepare`] for a real send: the person's own data, and the triggering event in flows.
pub async fn prepare_for_send(state: &AppState, account: &Account, profile: &Profile, event: Option<&Value>, content: &Value) -> AppResult<Value> {
    if content.get("blocks").is_none() {
        return Ok(content.clone());
    }
    let ctx = crate::render::context(account, Some(profile), event, "");
    prepare(state, account, content, &ctx).await
}

/// Copy saved blocks' current content into `content`, marking each reference `frozen`.
/// Returns None when nothing pointed at a saved block.
pub async fn freeze(db: &mut PgConnection, account_id: Uuid, content: &Value) -> AppResult<Option<Value>> {
    let Some(blocks) = content.get("blocks").and_then(|b| b.as_array()) else { return Ok(None) };
    let mut ids = Vec::new();
    live_refs(blocks, &mut ids);
    if ids.is_empty() {
        return Ok(None);
    }
    let rows: Vec<(Uuid, String, Value)> = sqlx::query_as("SELECT id, name, blocks FROM saved_blocks WHERE account_id = $1 AND id = ANY($2)")
        .bind(account_id)
        .bind(&ids)
        .fetch_all(&mut *db)
        .await?;
    let saved: HashMap<Uuid, (String, Value)> = rows.into_iter().map(|(id, n, b)| (id, (n, b))).collect();
    fn walk(list: &mut [Value], saved: &HashMap<Uuid, (String, Value)>) {
        for b in list.iter_mut() {
            match s(b, "type") {
                "saved" if !b.get("frozen").and_then(|f| f.as_bool()).unwrap_or(false) => {
                    let inner = Value::Array(saved_content(b, saved));
                    let name = saved_id(b).and_then(|id| saved.get(&id)).map(|(n, _)| n.clone());
                    if let Some(o) = b.as_object_mut() {
                        o.insert("blocks".into(), inner);
                        o.insert("frozen".into(), json!(true));
                        if let Some(n) = name {
                            o.insert("name".into(), json!(n));
                        }
                    }
                }
                "columns" => {
                    if let Some(cols) = b.get_mut("columns").and_then(|c| c.as_array_mut()) {
                        for col in cols {
                            if let Some(a) = col.as_array_mut() {
                                walk(a, saved);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = content.clone();
    if let Some(list) = out.get_mut("blocks").and_then(|b| b.as_array_mut()) {
        walk(list, &saved);
    }
    Ok(Some(out))
}

/// When a campaign's send starts, copy the saved blocks it uses into its content (and each A/B
/// version's), so editing a saved block later doesn't change what this campaign sends.
/// Runs inside the transaction that holds the campaign's row lock.
pub async fn freeze_campaign(db: &mut PgConnection, c: &CampaignRow) -> anyhow::Result<()> {
    let to_anyhow = |e: AppError| anyhow::anyhow!(e.public_message());
    let content = freeze(db, c.account_id, &c.content).await.map_err(to_anyhow)?;
    let mut ab = c.ab_test.clone().filter(|v| !v.is_null());
    let mut ab_changed = false;
    if let Some(variants) = ab.as_mut().and_then(|t| t.get_mut("variants")).and_then(|v| v.as_array_mut()) {
        for v in variants {
            if let Some(next) = freeze(db, c.account_id, &v["content"]).await.map_err(to_anyhow)? {
                v["content"] = next;
                ab_changed = true;
            }
        }
    }
    if content.is_none() && !ab_changed {
        return Ok(());
    }
    sqlx::query("UPDATE campaigns SET content = COALESCE($2, content), ab_test = CASE WHEN $3 THEN $4 ELSE ab_test END WHERE id = $1")
        .bind(c.id)
        .bind(content)
        .bind(ab_changed)
        .bind(ab)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// The block list the countdown and icon code read; exposed for tests.
pub fn empty_ctx() -> Value {
    Value::Object(Map::new())
}

/// Parse an RFC 3339 time (`2026-12-01T08:00:00-08:00`).
pub fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim()).ok().map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Value {
        json!({"first_name": "Ana", "person": {"first_name": "Ana", "properties": {"vip": true, "tier": "Gold", "visits": "12", "tags": ["a", "b"]}}, "event": {"value": 120.5, "properties": {"items": [1]}}})
    }

    fn rule(v: Value) -> bool {
        match parse_rule(&v).unwrap() {
            Rule::Simple { field, op, value } => rule_matches(&field, &op, &value, &ctx()),
            Rule::Expr(_) => panic!("expected a simple rule"),
        }
    }

    #[test]
    fn simple_rules() {
        assert!(rule(json!({"field": "person.properties.vip", "op": "eq", "value": "true"})));
        assert!(rule(json!({"field": "person.properties.tier", "op": "eq", "value": "gold"})));
        assert!(rule(json!({"field": "person.properties.tier", "op": "neq", "value": "silver"})));
        assert!(rule(json!({"field": "person.properties.missing", "op": "not_set"})));
        assert!(!rule(json!({"field": "person.properties.missing", "op": "set"})));
        assert!(rule(json!({"field": "person.properties.visits", "op": "gt", "value": 10})));
        assert!(!rule(json!({"field": "person.properties.tier", "op": "gt", "value": 10})), "text never compares as a number");
        assert!(rule(json!({"field": "event.value", "op": "gt", "value": "100"})));
        assert!(rule(json!({"field": "person.properties.tags", "op": "contains", "value": "B"})));
        assert!(rule(json!({"field": "first_name", "op": "contains", "value": "an"})));
        assert!(rule(json!({"field": "event.properties.nothing.deeper", "op": "not_set"})));
    }

    #[test]
    fn bad_rules_are_refused() {
        assert!(parse_rule(&json!({"field": "", "op": "eq"})).is_err());
        assert!(parse_rule(&json!({"field": "a b", "op": "eq"})).is_err());
        assert!(parse_rule(&json!({"field": "person.x", "op": "like"})).is_err());
        assert!(parse_rule(&json!({"field": "person.x", "op": "gt", "value": "ten"})).is_err());
        assert!(parse_rule(&json!({"expr": "x %}{{ secret }}{% if y"})).is_err());
        assert!(parse_rule(&json!({"expr": "event.value >"})).is_err());
        assert_eq!(parse_rule(&json!({"expr": " event.value > 100 "})).unwrap(), Rule::Expr("event.value > 100".into()));
    }
}

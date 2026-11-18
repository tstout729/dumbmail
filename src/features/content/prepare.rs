//! Getting email content ready to render, for each recipient (and each preview):
//!
//! - saved blocks are replaced by their current content (or, once a campaign's send has started,
//!   by the copy taken then: see [`freeze_campaign`]);
//! - display rules (`show_if`) are decided for the person and event: a block whose rule is false,
//!   or fails for this person, is left out (a rule never fails a send);
//! - links and image addresses in this module's blocks are personalized here and checked after
//!   personalization (only `http`, `https`, `mailto` and `tel` links and `http(s)` images survive);
//! - social icons and countdown images get their DumbMail-hosted addresses.
//!
//! Email that uses none of this (see [`uses_content_features`]) is passed through untouched, so it
//! renders exactly as it did before this module existed. Runs before `render::render_email` in
//! `sending::send` and the preview endpoint. Fields starting with `_` are computed here; anything
//! content sets in them is replaced.

use std::borrow::Cow;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::blocks::{network_label, ICON_STYLES, NETWORKS};
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow};
use crate::state::AppState;

/// Block types this module adds. Content with none of them, no saved block and no display rule is
/// left exactly as it is.
pub const CONTENT_TYPES: [&str; 10] = ["saved", "social", "video", "countdown", "table", "quote", "header", "nav", "footer", "image_text"];

/// The most blocks saved blocks may add to one email (an email's own blocks aren't limited here).
pub const MAX_FROM_SAVED: usize = 300;

/// The most rules one block's `show_if` may list (all must hold).
pub const MAX_RULES: usize = 5;

/// Work budget for one advanced rule or one personalized link (template engine instructions).
const FUEL: u64 = 5_000;

/// Why a recipient got nothing: every block was hidden for them.
pub const NOTHING_TO_SHOW: &str = "nothing to show for this person";

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn has_rule(b: &Value) -> bool {
    b.get("show_if").is_some_and(|r| !r.is_null())
}

fn block_uses(b: &Value, depth: usize) -> bool {
    if CONTENT_TYPES.contains(&s(b, "type")) || has_rule(b) {
        return true;
    }
    depth == 0
        && s(b, "type") == "columns"
        && b.get("columns").and_then(|c| c.as_array()).is_some_and(|cols| cols.iter().any(|col| col.as_array().is_some_and(|a| a.iter().any(|x| block_uses(x, 1)))))
}

/// Whether `content` uses anything this module adds: a saved block, a display rule or one of
/// [`CONTENT_TYPES`]. Reads the block list once without copying anything; content that doesn't is
/// rendered by the core path untouched.
pub fn uses_content_features(content: &Value) -> bool {
    content.get("blocks").and_then(|b| b.as_array()).is_some_and(|bs| bs.iter().any(|b| block_uses(b, 0)))
}

/// Transactional email renders on its own path, without this module's per-recipient step, so it
/// takes the core blocks and the table block only: why `content` can't be used there, if it can't.
pub fn transactional_problem(content: &Value) -> Option<String> {
    fn walk(list: &[Value], depth: usize) -> Option<String> {
        for b in list {
            let t = s(b, "type");
            if has_rule(b) {
                return Some("display rules (show_if) aren't supported in transactional email; every recipient gets the same blocks".into());
            }
            if t == "saved" {
                return Some("saved blocks aren't supported in transactional email; detach it (use its blocks) instead".into());
            }
            if t != "table" && CONTENT_TYPES.contains(&t) {
                return Some(format!("the {t} block isn't supported in transactional email (use the core blocks or a table)"));
            }
            if t == "columns" && depth == 0 {
                for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                    if let Some(p) = walk(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), 1) {
                        return Some(p);
                    }
                }
            }
        }
        None
    }
    walk(content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), 0)
}

// ---------- Display rules ----------

/// The template engine for display rules and personalized links: missing values are empty, every
/// evaluation has a fixed work budget, and nothing is HTML-escaped (links are escaped where they land).
static ENGINE: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_fuel(Some(FUEL));
    env
});

/// Filters an advanced rule may use: cheap ones whose work is bounded by the value they get.
const RULE_FILTERS: [&str; 17] = ["int", "float", "lower", "upper", "length", "count", "default", "d", "trim", "first", "last", "string", "bool", "abs", "round", "title", "capitalize"];

/// An advanced display rule that parses as an expression, can't open or close a template tag, and
/// can only compare, combine and look at values: no function calls, no string or list repetition
/// (`*`) or joining (`~`), and only [`RULE_FILTERS`]. With the work budget, that bounds what one
/// rule can cost for one recipient.
pub fn checked_expr(expr: &str) -> Option<&str> {
    static STRINGS: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r#"'[^']*'|"[^"]*""#).unwrap());
    static FILTER: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\|\s*([A-Za-z_]\w*)").unwrap());
    static CALL: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(\|\s*)?\b([A-Za-z_]\w*)\s*\(").unwrap());
    let e = expr.trim();
    // No backslashes: without escapes, a quoted string is exactly what STRINGS matches, so the
    // screen below reads the rule the way the template engine will.
    if e.is_empty() || e.len() > 500 || e.contains(['{', '}', '%', '#', '\\']) || e.chars().any(|c| c.is_control()) {
        return None;
    }
    let code = STRINGS.replace_all(e, "''");
    if code.contains(['*', '~']) || FILTER.captures_iter(&code).any(|c| !RULE_FILTERS.contains(&&c[1])) || CALL.captures_iter(&code).any(|c| c.get(1).is_none()) {
        return None;
    }
    ENGINE.compile_expression(e).ok().map(|_| e)
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

/// A block's `show_if`: one rule, or a list of up to [`MAX_RULES`] that must all hold.
pub fn parse_rules(v: &Value) -> Result<Vec<Rule>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(list) if list.len() > MAX_RULES => Err(format!("a block can have at most {MAX_RULES} display rules")),
        Value::Array(list) => list.iter().map(parse_rule).collect(),
        one => Ok(vec![parse_rule(one)?]),
    }
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

/// Evaluate an advanced rule against one recipient's data, within the work budget.
pub fn eval_expr(expr: &str, ctx: &Value) -> Result<bool, String> {
    let e = ENGINE.compile_expression(expr).map_err(|e| e.to_string())?;
    e.eval(ctx).map(|v| v.is_true()).map_err(|e| e.to_string())
}

/// Whether one rule holds; Err when it can't be decided for this recipient.
pub fn rule_holds(rule: &Rule, ctx: &Value) -> Result<bool, String> {
    match rule {
        Rule::Simple { field, op, value } => Ok(rule_matches(field, op, value, ctx)),
        Rule::Expr(e) => eval_expr(e, ctx),
    }
}

// ---------- Links ----------

/// A link or image address that may go in an email: `http(s)` with a host, or (links only)
/// `mailto:` / `tel:`. Braces are percent-encoded so it can never open a template tag.
pub fn clean_url(raw: &str, image: bool) -> Option<String> {
    let u = raw.trim();
    if u.is_empty() || u.len() > 2048 || u.chars().any(|c| c.is_control()) {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    let ok = if lower.starts_with("https://") || lower.starts_with("http://") {
        url::Url::parse(u).ok().is_some_and(|p| p.host_str().is_some_and(|h| !h.is_empty()))
    } else {
        !image && ((lower.starts_with("mailto:") && u.len() > 7) || (lower.starts_with("tel:") && u.len() > 4))
    };
    ok.then(|| u.replace('{', "%7B").replace('}', "%7D"))
}

/// [`clean_url`] for an address [`resolve_url`] already personalized, at render time: the email's own
/// coupon tags it kept stay as they are, for the renderer to fill in. (Tags that came from a
/// person's data were percent-encoded by `resolve_url`, so they can't reach here as tags.)
pub fn clean_url_keeping_coupons(raw: &str, image: bool) -> Option<String> {
    let (u, tags) = crate::features::coupons::template::hide_tags(raw.trim());
    clean_url(&u, image).map(|c| if tags.is_empty() { c } else { crate::features::coupons::template::restore_tags(&c, &tags) })
}

/// Personalize a link or image address for this recipient, then keep it only if [`clean_url`]
/// allows the result. Empty when it's not allowed (the block then shows without the link).
/// A coupon tag the email puts in a link (`https://shop.example/discount/{% coupon_code 'WELCOME' %}`)
/// is left for the renderer, which puts the person's code there once it's claimed (see
/// features::coupons); one that comes from a person's data is percent-encoded like any other brace.
pub fn resolve_url(raw: &str, ctx: &Value, image: bool) -> String {
    let (t, tags) = crate::features::coupons::template::hide_tags(raw.trim());
    let personalized: Cow<'_, str> = if t.contains("{{") || t.contains("{%") {
        match ENGINE.render_str(&t, ctx) {
            Ok(v) => Cow::Owned(v),
            Err(_) => return String::new(),
        }
    } else {
        Cow::Borrowed(&t)
    };
    match clean_url(&personalized, image) {
        Some(u) if !tags.is_empty() => crate::features::coupons::template::restore_tags(&u, &tags),
        Some(u) => u,
        None => String::new(),
    }
}

// ---------- Saved blocks ----------

pub type SavedMap = HashMap<Uuid, (String, Value)>;

/// Saved blocks the given ids point at, keyed by id: (name, blocks). Deleted ones still resolve, so
/// emails that point at one keep showing it.
async fn load_saved(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> AppResult<SavedMap> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
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

fn frozen(b: &Value) -> bool {
    b.get("frozen").and_then(|f| f.as_bool()).unwrap_or(false)
}

/// Ids of saved blocks referenced (and not yet copied in) at the top level or inside columns.
pub fn live_refs(blocks: &[Value], out: &mut Vec<Uuid>) {
    for b in blocks {
        match s(b, "type") {
            "saved" if !frozen(b) => out.extend(saved_id(b)),
            "columns" => {
                for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                    live_refs(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), out);
                }
            }
            _ => {}
        }
    }
}

/// Live saved-block ids anywhere in an email's content.
pub fn content_refs(content: &Value) -> Vec<Uuid> {
    let mut ids = Vec::new();
    live_refs(content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), &mut ids);
    ids.sort();
    ids.dedup();
    ids
}

/// What a saved-block reference shows: the saved block's current content, or (when it was
/// copied in, or doesn't belong to this account) the copy stored with the reference.
fn saved_content(b: &Value, saved: &SavedMap) -> Vec<Value> {
    let from_db = (!frozen(b)).then(|| saved_id(b).and_then(|id| saved.get(&id))).flatten().map(|(_, v)| v);
    from_db
        .or_else(|| b.get("blocks"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|x| s(x, "type") != "saved").cloned().collect())
        .unwrap_or_default()
}

// ---------- Expanding ----------

pub struct Prep<'a> {
    pub account: &'a Account,
    pub ctx: &'a Value,
    pub public_url: &'a str,
    pub secret: &'a str,
    saved: &'a SavedMap,
    from_saved: usize,
    /// Keep every block whatever its display rule says (checks and validation look at all of them).
    all: bool,
    /// Rules that couldn't be decided for this recipient (their blocks were hidden).
    pub rule_errors: usize,
}

impl Prep<'_> {
    /// Whether a block with this `show_if` shows. A rule that can't be decided hides its block.
    fn shows(&mut self, rule: Option<&Value>) -> bool {
        let Some(r) = rule.filter(|r| !r.is_null()) else { return true };
        if self.all {
            return true;
        }
        let rules = match parse_rules(r) {
            Ok(rules) => rules,
            Err(_) => {
                self.rule_errors += 1;
                return false;
            }
        };
        for rule in &rules {
            match rule_holds(rule, self.ctx) {
                Ok(true) => {}
                Ok(false) => return false,
                Err(_) => {
                    self.rule_errors += 1;
                    return false;
                }
            }
        }
        true
    }

    fn expand(&mut self, list: &[Value], depth: usize, in_saved: bool, out: &mut Vec<Value>) -> AppResult<()> {
        for b in list {
            let kind = s(b, "type");
            let mut b = b.clone();
            let rule = b.as_object_mut().and_then(|o| o.remove("show_if"));
            if !self.shows(rule.as_ref()) {
                continue;
            }
            match kind {
                "saved" => {
                    let inner = saved_content(&b, self.saved);
                    self.expand(&inner, depth, true, out)?;
                }
                "columns" if depth == 0 => {
                    let cols: Vec<Value> = b.get("columns").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                    let mut next = Vec::with_capacity(cols.len());
                    for col in &cols {
                        let mut inner = Vec::new();
                        self.expand(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), depth + 1, in_saved, &mut inner)?;
                        next.push(Value::Array(inner));
                    }
                    b["columns"] = Value::Array(next);
                    self.push(out, b, in_saved)?;
                }
                _ => {
                    self.fill(&mut b);
                    self.push(out, b, in_saved)?;
                }
            }
        }
        Ok(())
    }

    fn push(&mut self, out: &mut Vec<Value>, b: Value, in_saved: bool) -> AppResult<()> {
        if in_saved {
            self.from_saved += 1;
            if self.from_saved > MAX_FROM_SAVED {
                return Err(AppError::bad(format!("the saved blocks in this email add more than {MAX_FROM_SAVED} blocks; use fewer or smaller saved blocks")));
            }
        }
        out.push(b);
        Ok(())
    }

    fn link(&self, o: &mut Map<String, Value>, key: &str, image: bool) {
        if let Some(raw) = o.get(key).and_then(|v| v.as_str()) {
            let v = resolve_url(raw, self.ctx, image);
            o.insert(key.into(), json!(v));
        }
    }

    fn link_list(&self, o: &mut Map<String, Value>, key: &str, field: &str) {
        if let Some(list) = o.get_mut(key).and_then(|l| l.as_array_mut()) {
            for l in list.iter_mut() {
                if let Some(raw) = l.get(field).and_then(|v| v.as_str()) {
                    let v = resolve_url(raw, self.ctx, false);
                    l[field] = json!(v);
                }
            }
        }
    }

    /// Personalized links and hosted icon and countdown addresses for blocks that show them.
    fn fill(&self, b: &mut Value) {
        let Some(o) = b.as_object_mut() else { return };
        o.retain(|k, _| !k.starts_with('_'));
        match o.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "social" => {
                let icons = self.icons(o.get("links"), o.get("style"));
                o.insert("_icons".into(), icons);
            }
            "footer" => {
                self.link_list(o, "links", "href");
                let style = o.get("style").cloned().or(Some(json!("light")));
                let icons = self.icons(o.get("social"), style.as_ref());
                o.insert("_icons".into(), icons);
            }
            "video" => {
                self.link(o, "url", false);
                self.link(o, "thumbnail", true);
            }
            "quote" => self.link(o, "image", true),
            "header" => {
                self.link(o, "logo", true);
                self.link(o, "logo_href", false);
                self.link_list(o, "links", "href");
            }
            "nav" => self.link_list(o, "links", "href"),
            "image_text" => {
                self.link(o, "src", true);
                self.link(o, "href", false);
                self.link(o, "button_href", false);
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

    /// Icons for a list of `{network, url}`; with no list, the account's social links. Icons whose
    /// link isn't allowed after personalization are left out.
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
                let url = if n == "email" && !url.contains(':') && url.contains('@') { format!("mailto:{url}") } else { url };
                let href = resolve_url(&url, self.ctx, false);
                let lower = href.to_ascii_lowercase();
                // Only an email icon may open a mail app; the others are web links.
                let ok = if n == "email" { lower.starts_with("mailto:") || lower.starts_with("http") } else { lower.starts_with("http") };
                ok.then(|| json!({"href": href, "src": format!("{}/e/icons/{style}-{n}.png", self.public_url), "label": network_label(&n)}))
            })
            .collect();
        Value::Array(icons)
    }
}

/// Whether anything is left to show: a block that isn't columns, or columns with something in them.
fn visible(blocks: &[Value]) -> bool {
    blocks.iter().any(|b| {
        s(b, "type") != "columns" || b.get("columns").and_then(|c| c.as_array()).is_some_and(|cols| cols.iter().any(|c| c.as_array().is_some_and(|a| !a.is_empty())))
    })
}

fn expand_with(state: &AppState, account: &Account, content: &Value, ctx: &Value, saved: &SavedMap, all: bool) -> AppResult<(Value, bool, usize)> {
    let blocks = content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
    let mut prep = Prep { account, ctx, public_url: state.cfg.public_url(), secret: &state.cfg.secret_key, saved, from_saved: 0, all, rule_errors: 0 };
    let mut out = Vec::with_capacity(blocks.len());
    prep.expand(blocks, 0, false, &mut out)?;
    let shows = visible(&out);
    let mut content = content.clone();
    content["blocks"] = Value::Array(out);
    Ok((content, shows, prep.rule_errors))
}

/// `content` ready to render for the person and event in `ctx` (see the module docs), or None when
/// every block is hidden for them (the recipient is skipped with [`NOTHING_TO_SHOW`]). Content that
/// uses nothing from this module comes back as is, without being read further or copied.
pub async fn prepare<'c>(state: &AppState, account: &Account, content: &'c Value, ctx: &Value) -> AppResult<Option<Cow<'c, Value>>> {
    // Saved blocks are read first (when the email uses any), so the languages they carry count too.
    let saved = if uses_content_features(content) { Some(load_saved(&state.db, account.id, &content_refs(content)).await?) } else { None };
    let saved_langs = saved.as_ref().map(crate::features::translations::saved_languages).unwrap_or_default();
    // Language versions (see features::translations): the person's language replaces the
    // translated text, here and in saved blocks. Content without versions is passed on untouched.
    if let Some((local, lang)) = crate::features::translations::localized(state, account, content, ctx, &saved_langs).await? {
        let mut saved = saved;
        if let Some(s) = saved.as_mut() {
            crate::features::translations::localize_saved(s, &lang, &crate::features::translations::own_hosts(state, Some(account)));
        }
        return Ok(prepare_in(state, account, &local, ctx, saved).await?.map(|c| Cow::Owned(c.into_owned())));
    }
    prepare_in(state, account, content, ctx, saved).await
}

async fn prepare_in<'c>(state: &AppState, account: &Account, content: &'c Value, ctx: &Value, saved: Option<SavedMap>) -> AppResult<Option<Cow<'c, Value>>> {
    if !uses_content_features(content) {
        return Ok(Some(Cow::Borrowed(content)));
    }
    let saved = match saved {
        Some(s) => s,
        None => load_saved(&state.db, account.id, &content_refs(content)).await?,
    };
    let (prepared, shows, rule_errors) = expand_with(state, account, content, ctx, &saved, false)?;
    if rule_errors > 0 {
        tracing::info!(account = %account.id, rules = rule_errors, "display rules couldn't be decided for a recipient; their blocks were hidden");
    }
    Ok(shows.then_some(Cow::Owned(prepared)))
}

/// `content` with saved blocks filled in and every block kept whatever its display rule says (for
/// checks), personalized for the sample person in `ctx`.
pub async fn prepare_all(state: &AppState, account: &Account, content: &Value, ctx: &Value) -> AppResult<Value> {
    if !uses_content_features(content) {
        return Ok(content.clone());
    }
    let saved = load_saved(&state.db, account.id, &content_refs(content)).await?;
    Ok(expand_with(state, account, content, ctx, &saved, true)?.0)
}

/// The saved blocks `content` points at, as they are now.
pub async fn load_saved_for(state: &AppState, account: &Account, content: &Value) -> AppResult<SavedMap> {
    load_saved(&state.db, account.id, &content_refs(content)).await
}

/// For one recipient: whether anything is left to show, and how many rules couldn't be decided.
pub fn shows_for(state: &AppState, account: &Account, content: &Value, ctx: &Value, saved: &SavedMap) -> (bool, usize) {
    match expand_with(state, account, content, ctx, saved, false) {
        Ok((_, shows, errors)) => (shows, errors),
        Err(_) => (true, 0),
    }
}

/// The display rules in `content` and the saved blocks it uses, as JSON text (empty: none).
pub fn rules_text(content: &Value, saved: &SavedMap) -> String {
    fn walk(list: &[Value], saved: &SavedMap, out: &mut String) {
        for b in list {
            if let Some(r) = b.get("show_if").filter(|r| !r.is_null()) {
                out.push_str(&r.to_string());
            }
            match s(b, "type") {
                "saved" => walk(&saved_content(b, saved), saved, out),
                "columns" => {
                    for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                        walk(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), saved, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    walk(content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), saved, &mut out);
    out
}

/// Every display rule in `content` parses (the first problem, as "block N: ...").
pub fn check_rules(content: &Value) -> Result<(), String> {
    fn walk(list: &[Value], label: &str) -> Result<(), String> {
        for (i, b) in list.iter().enumerate() {
            let here = if label.is_empty() { format!("block {}", i + 1) } else { format!("{label}, block {}", i + 1) };
            if let Some(r) = b.get("show_if").filter(|r| !r.is_null()) {
                parse_rules(r).map_err(|e| format!("{here}: {e}"))?;
            }
            if s(b, "type") == "columns" {
                for (c, col) in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten().enumerate() {
                    walk(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), &format!("{here}, column {}", c + 1))?;
                }
            }
        }
        Ok(())
    }
    walk(content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), "")
}

/// The sample person previews and checks use.
pub fn sample_ctx(account: &Account, event: Option<&Value>) -> Value {
    let mut ctx = crate::render::context(account, None, event, "#unsubscribe");
    ctx["first_name"] = json!("Alex");
    ctx["last_name"] = json!("Rivera");
    ctx["email"] = json!("alex@example.com");
    ctx["person"] = json!({"first_name": "Alex", "last_name": "Rivera", "email": "alex@example.com", "properties": {}});
    // Coupon tags show a sample code and use none.
    crate::features::coupons::template::fill_sample(&mut ctx);
    ctx
}

/// Whether this exact email, with `saved` standing in for its saved blocks, can be sent to anyone:
/// its rules parse, saved blocks don't add too much, and every block (shown or not) renders.
pub fn check_sendable(state: &AppState, account: &Account, content: &Value, saved: &SavedMap) -> Result<(), String> {
    if !uses_content_features(content) {
        return Ok(());
    }
    check_rules(content)?;
    let ctx = sample_ctx(account, Some(&json!({"properties": {}})));
    let (all, _, _) = expand_with(state, account, content, &ctx, saved, true).map_err(|e| e.public_message())?;
    let ec: crate::render::EmailContent = serde_json::from_value(all).map_err(|e| format!("this email's content isn't valid: {e}"))?;
    crate::render::render_email(account, &ec, &ctx, None).map(|_| ()).map_err(|e| e.public_message())
}

/// [`check_sendable`] with the saved blocks as they are now.
pub async fn check_sendable_now(state: &AppState, account: &Account, content: &Value) -> AppResult<Result<(), String>> {
    if !uses_content_features(content) {
        return Ok(Ok(()));
    }
    let saved = load_saved(&state.db, account.id, &content_refs(content)).await?;
    Ok(check_sendable(state, account, content, &saved))
}

/// The preview endpoint's content: checked like a send (bad rules are refused), then prepared for
/// the person in `ctx`. With no particular person (`sample`) and every block hidden for the sample
/// person, every block is shown; for a real person it's an error that says they'd be skipped.
pub async fn for_preview<'c>(state: &AppState, account: &Account, content: &'c Value, ctx: &Value, sample: bool) -> AppResult<Cow<'c, Value>> {
    if !uses_content_features(content) && !crate::features::translations::fields::has_versions(content) {
        return Ok(Cow::Borrowed(content));
    }
    check_sendable_now(state, account, content).await?.map_err(AppError::bad)?;
    match prepare(state, account, content, ctx).await? {
        Some(c) => Ok(c),
        None if sample => Ok(Cow::Owned(prepare_all(state, account, content, ctx).await?)),
        None => Err(AppError::bad("Nothing to show for this person: every block is hidden by a display rule, so they'd be skipped.")),
    }
}

// ---------- Freezing, relinking ----------

/// Copy saved blocks' content from `saved` into `content`, marking each reference `frozen`.
/// None when nothing pointed at a live saved block.
pub fn freeze_with(content: &Value, saved: &SavedMap) -> Option<Value> {
    if content_refs(content).is_empty() {
        return None;
    }
    fn walk(list: &mut [Value], saved: &SavedMap) {
        for b in list.iter_mut() {
            match s(b, "type") {
                "saved" if !frozen(b) => {
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
        walk(list, saved);
    }
    Some(out)
}

/// `c` with the saved blocks in its content and each A/B version's filled in as a send started now
/// would freeze them, for checks that must see what will be sent (the coupons a campaign uses).
pub async fn with_saved_filled(state: &AppState, c: &CampaignRow) -> AppResult<CampaignRow> {
    let variants = c.ab_test.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array());
    let mut ids = content_refs(&c.content);
    for v in variants.into_iter().flatten() {
        ids.extend(content_refs(&v["content"]));
    }
    let mut out = c.clone();
    if ids.is_empty() {
        return Ok(out);
    }
    ids.sort();
    ids.dedup();
    let saved = load_saved(&state.db, c.account_id, &ids).await?;
    if let Some(next) = freeze_with(&c.content, &saved) {
        out.content = next;
    }
    if let Some(variants) = out.ab_test.as_mut().and_then(|t| t.get_mut("variants")).and_then(|v| v.as_array_mut()) {
        for v in variants {
            if let Some(next) = freeze_with(&v["content"], &saved) {
                v["content"] = next;
            }
        }
    }
    Ok(out)
}

/// When a campaign's send starts, copy the saved blocks it uses into its content and each A/B
/// version's, all from one read, so editing a saved block later doesn't change what it sends. The
/// frozen copy is then checked as it will be sent: Some(problem) when it can't be (the caller fails
/// the campaign before anyone gets it). Runs inside the transaction that holds the campaign's row lock.
pub async fn freeze_campaign(state: &AppState, db: &mut PgConnection, c: &CampaignRow) -> anyhow::Result<Option<String>> {
    if c.channel != "email" {
        return Ok(None);
    }
    let mut ab = c.ab_test.clone().filter(|v| !v.is_null());
    let mut ids = content_refs(&c.content);
    if let Some(variants) = ab.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()) {
        for v in variants {
            ids.extend(content_refs(&v["content"]));
        }
    }
    // Language versions (see features::translations) are fixed with the rest of the content.
    let versioned = c.content.get("translations").is_some() || ab.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()).is_some_and(|vs| vs.iter().any(|v| v["content"].get("translations").is_some()));
    if ids.is_empty() && !versioned {
        return Ok(None);
    }
    ids.sort();
    ids.dedup();
    let rows: Vec<(Uuid, String, Value)> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT id, name, blocks FROM saved_blocks WHERE account_id = $1 AND id = ANY($2)").bind(c.account_id).bind(&ids).fetch_all(&mut *db).await?
    };
    let saved: SavedMap = rows.into_iter().map(|(id, n, b)| (id, (n, b))).collect();
    let mut content = freeze_with(&c.content, &saved);
    let mut ab_changed = false;
    if let Some(variants) = ab.as_mut().and_then(|t| t.get_mut("variants")).and_then(|v| v.as_array_mut()) {
        for v in variants {
            if let Some(next) = freeze_with(&v["content"], &saved) {
                v["content"] = next;
                ab_changed = true;
            }
        }
    }
    // The footer wording each language needs, fixed for this send, so a later edit can't change
    // what the rest of this campaign's recipients get.
    let mut all: Vec<&Value> = vec![content.as_ref().unwrap_or(&c.content)];
    all.extend(ab.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()).into_iter().flatten().map(|v| &v["content"]));
    if let Some(sys) = crate::features::translations::system_snapshot(&mut *db, &state.cfg.secret_key, c.account_id, c.id, &all).await? {
        let mut next = content.unwrap_or_else(|| c.content.clone());
        next["_system"] = sys.clone();
        content = Some(next);
        if let Some(variants) = ab.as_mut().and_then(|t| t.get_mut("variants")).and_then(|v| v.as_array_mut()) {
            for v in variants {
                v["content"]["_system"] = sys.clone();
                ab_changed = true;
            }
        }
    }
    // Check exactly what will be sent.
    // On the connection that holds the campaign lock: no second connection while holding it.
    let account: Account = sqlx::query_as(
        "SELECT a.*, ARRAY(SELECT d.domain FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified' ORDER BY d.created_at) AS verified_domains
         FROM accounts a WHERE a.id = $1",
    )
    .bind(c.account_id)
    .fetch_one(&mut *db)
    .await?;
    let empty = SavedMap::new();
    let mut versions: Vec<(&str, &Value)> = vec![("", content.as_ref().unwrap_or(&c.content))];
    if let Some(variants) = ab.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()) {
        versions.extend(variants.iter().map(|v| (v["id"].as_str().unwrap_or(""), &v["content"])));
    }
    for (id, v) in versions {
        if let Err(e) = check_sendable(state, &account, v, &empty) {
            return Ok(Some(if id.is_empty() { e } else { format!("version {}: {e}", id.to_uppercase()) }));
        }
    }
    sqlx::query("UPDATE campaigns SET content = COALESCE($2, content), ab_test = CASE WHEN $3 THEN $4 ELSE ab_test END WHERE id = $1")
        .bind(c.id)
        .bind(content)
        .bind(ab_changed)
        .bind(ab)
        .execute(&mut *db)
        .await?;
    Ok(None)
}

/// A copy of `content` whose saved blocks show their current content again (for duplicating an
/// email that was sent: the copy is a new, unsent email).
pub fn relink(content: &Value) -> Value {
    fn walk(list: &mut [Value]) {
        for b in list.iter_mut() {
            match s(b, "type") {
                "saved" => {
                    if let Some(o) = b.as_object_mut() {
                        o.remove("frozen");
                    }
                }
                "columns" => {
                    for col in b.get_mut("columns").and_then(|c| c.as_array_mut()).into_iter().flatten() {
                        if let Some(a) = col.as_array_mut() {
                            walk(a);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = content.clone();
    if let Some(list) = out.get_mut("blocks").and_then(|b| b.as_array_mut()) {
        walk(list);
    }
    // The copy gets its own footer wording when it's sent (see features::translations).
    if let Some(o) = out.as_object_mut() {
        o.remove("_system");
    }
    out
}

/// [`relink`] for a campaign's A/B test versions.
pub fn relink_ab(ab: &Option<Value>) -> Option<Value> {
    let mut ab = ab.clone()?;
    if let Some(variants) = ab.get_mut("variants").and_then(|v| v.as_array_mut()) {
        for v in variants {
            let next = relink(&v["content"]);
            v["content"] = next;
        }
    }
    Some(ab)
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
        let six: Vec<Value> = (0..6).map(|_| json!({"field": "x", "op": "set"})).collect();
        assert!(parse_rules(&Value::Array(six)).is_err());
        assert_eq!(parse_rules(&json!([{"field": "x", "op": "set"}, {"expr": "true"}])).unwrap().len(), 2);
    }

    #[test]
    fn advanced_rules_fail_closed_per_recipient() {
        let c = ctx();
        assert_eq!(eval_expr("event.value > 100", &c), Ok(true));
        assert_eq!(eval_expr("person.properties.missing", &c), Ok(false));
        // Parses, but can't be worked out for this person: an error, never a panic or a send failure.
        assert!(eval_expr("person.properties.tier + 1 > 3", &c).is_err());
        // Anything that could make one rule expensive is refused when the rule is read.
        // An escaped quote would make a regex see `* 30000000` inside a string where the engine sees code.
        for e in ["'x\\'y' * 30000000 == 'z'", "'x' * 100000000", "range(100000)|length > 1", "person.properties.tier ~ 'x' == 'y'", "person.properties.tier|indent(100000000)", "debug()", "event.value ** 99"] {
            assert!(checked_expr(e).is_none(), "{e}");
        }
        assert!(parse_rule(&json!({"expr": "event.value * 2 > 1"})).is_err(), "no * at all, so no repetition");
        for e in ["person.properties.tier|lower == 'gold'", "person.properties.visits|int > 10 and event.value is defined", "'a*b' in person.properties.tags", "person.properties.tags|length > 1"] {
            assert!(checked_expr(e).is_some(), "{e}");
        }
    }

    #[test]
    fn urls_are_checked_after_personalization() {
        let c = json!({"event": {"properties": {"url": "javascript:alert(1)", "good": "https://a.test/x?q={{y}}"}}});
        assert_eq!(resolve_url("{{ event.properties.url }}", &c, false), "");
        assert_eq!(resolve_url("{{ event.properties.good }}", &c, false), "https://a.test/x?q=%7B%7By%7D%7D");
        assert_eq!(resolve_url("{{ event.properties.missing|default('https://b.test') }}", &c, false), "https://b.test");
        assert_eq!(resolve_url("mailto:hi@a.test", &c, false), "mailto:hi@a.test");
        assert_eq!(resolve_url("mailto:hi@a.test", &c, true), "", "images are web addresses only");
        assert_eq!(resolve_url("tel:+15035550100", &c, false), "tel:+15035550100");
        assert_eq!(resolve_url("#top", &c, false), "");
        assert_eq!(resolve_url("https://", &c, false), "");
        assert_eq!(resolve_url("data:text/html,x", &c, true), "");
        assert_eq!(resolve_url("{{ broken", &c, false), "");
    }

    #[test]
    fn legacy_content_is_detected_without_features() {
        let legacy = json!({"blocks": [{"type": "text", "html": "x"}, {"type": "columns", "columns": [[{"type": "image", "src": "https://a.test/x.png"}], [], [], [], []]}, {"type": "if_open", "expr": "x"}]});
        assert!(!uses_content_features(&legacy));
        assert!(!uses_content_features(&json!({"html": "<p>x</p>"})));
        assert!(!uses_content_features(&json!({"body": "sms"})));
        for b in [json!({"type": "social"}), json!({"type": "text", "show_if": {"field": "x", "op": "set"}}), json!({"type": "columns", "columns": [[{"type": "saved"}]]})] {
            assert!(uses_content_features(&json!({"blocks": [b.clone()]})), "{b}");
        }
        assert!(!uses_content_features(&json!({"blocks": [{"type": "text", "show_if": null}]})));
    }

    #[test]
    fn relinking_a_sent_copy_drops_the_frozen_mark() {
        let c = json!({"blocks": [{"type": "saved", "saved_block_id": "x", "frozen": true, "blocks": []}, {"type": "columns", "columns": [[{"type": "saved", "frozen": true}]]}]});
        let r = relink(&c);
        assert!(!r.to_string().contains("frozen"));
    }
}

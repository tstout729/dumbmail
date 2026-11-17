//! Coupons and their codes: create, change, add (upload or generate), search, stock and export.

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::template::valid_name;
use crate::error::{AppError, AppResult};

/// Most codes one upload or generate call may add.
pub const MAX_PER_UPLOAD: usize = 200_000;
/// Most codes one coupon may hold.
pub const MAX_PER_COUPON: i64 = 2_000_000;
/// Longest code accepted.
pub const MAX_CODE_LEN: usize = 100;
/// Most coupons per account.
pub const MAX_COUPONS: i64 = 500;
/// Rows per insert statement.
const CHUNK: usize = 5_000;

/// Letters and digits without the look-alikes 0/O, 1/I/L.
const LETTERS_DIGITS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const LETTERS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Coupon {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: Uuid,
    pub name: String,
    pub description: String,
    pub source: String,
    pub pattern: Option<Value>,
    pub expires_at: Option<DateTime<Utc>>,
    pub expires_after_days: Option<i32>,
    pub new_code_each_time: bool,
    pub low_stock_threshold: Option<i32>,
    pub form_codes_per_hour: i32,
    pub codes_total: i64,
    pub out_of_codes_at: Option<DateTime<Utc>>,
    pub low_stock_alerted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, account_id, name, description, source, pattern, expires_at, expires_after_days, new_code_each_time, low_stock_threshold, form_codes_per_hour, codes_total, out_of_codes_at, low_stock_alerted_at, created_at, updated_at";

pub async fn load(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Coupon> {
    sqlx::query_as::<_, Coupon>(&format!("SELECT {COLS} FROM coupons WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("coupon"))
}

pub async fn by_name(db: &PgPool, account_id: Uuid, name: &str) -> AppResult<Option<Coupon>> {
    Ok(sqlx::query_as::<_, Coupon>(&format!("SELECT {COLS} FROM coupons WHERE account_id = $1 AND lower(name) = lower($2) AND deleted_at IS NULL"))
        .bind(account_id)
        .bind(name.trim())
        .fetch_optional(db)
        .await?)
}

/// A coupon by id, or by name when `key` isn't an id.
pub async fn find(db: &PgPool, account_id: Uuid, key: &str) -> AppResult<Coupon> {
    if let Ok(id) = key.parse::<Uuid>() {
        return load(db, account_id, id).await;
    }
    by_name(db, account_id, key).await?.ok_or_else(|| AppError::not_found("coupon"))
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    #[serde(default)]
    pub prefix: String,
    pub length: usize,
    #[serde(default = "default_alphabet")]
    pub alphabet: String,
}

fn default_alphabet() -> String {
    "letters_digits".into()
}

impl Pattern {
    fn chars(&self) -> &'static [u8] {
        match self.alphabet.as_str() {
            "letters" => LETTERS,
            "digits" => DIGITS,
            _ => LETTERS_DIGITS,
        }
    }

    pub fn check(&self) -> AppResult<()> {
        if !matches!(self.alphabet.as_str(), "letters_digits" | "letters" | "digits") {
            return Err(AppError::bad("pattern.alphabet must be letters_digits, letters or digits"));
        }
        if !(4..=24).contains(&self.length) {
            return Err(AppError::bad("pattern.length (the random part) must be 4 to 24 characters"));
        }
        if self.prefix.chars().count() > 20 || !self.prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(AppError::bad("pattern.prefix can have up to 20 letters, digits, - and _"));
        }
        Ok(())
    }

    /// How many different codes the pattern can make (capped, to avoid overflow).
    fn space(&self) -> f64 {
        (self.chars().len() as f64).powi(self.length as i32)
    }

    fn make(&self, rng: &mut impl Rng) -> String {
        let chars = self.chars();
        let mut s = self.prefix.clone();
        for _ in 0..self.length {
            s.push(chars[rng.gen_range(0..chars.len())] as char);
        }
        s
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: Option<String>,
    pub description: Option<String>,
    pub source: Option<String>,
    pub pattern: Option<Option<Pattern>>,
    /// RFC 3339 time; null clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: Option<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_after_days: Option<Option<i32>>,
    pub new_code_each_time: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    pub low_stock_threshold: Option<Option<i32>>,
    pub form_codes_per_hour: Option<i32>,
}

/// Tells "left out" (None) apart from "set to null" (Some(None)).
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

fn check(c: &Coupon) -> AppResult<()> {
    if !valid_name(&c.name) {
        return Err(AppError::bad("name can have up to 64 letters, digits, spaces, . _ and -, starting with a letter or digit"));
    }
    if c.description.chars().count() > 500 {
        return Err(AppError::bad("description is too long (at most 500 characters)"));
    }
    if !matches!(c.source.as_str(), "uploaded" | "generated") {
        return Err(AppError::bad("source must be uploaded or generated"));
    }
    if c.expires_at.is_some() && c.expires_after_days.is_some() {
        return Err(AppError::bad("set either expires_at (a date) or expires_after_days, not both"));
    }
    if c.expires_after_days.is_some_and(|d| !(1..=3650).contains(&d)) {
        return Err(AppError::bad("expires_after_days must be 1 to 3650"));
    }
    if c.low_stock_threshold.is_some_and(|t| !(1..=1_000_000).contains(&t)) {
        return Err(AppError::bad("low_stock_threshold must be 1 to 1,000,000"));
    }
    if !(1..=10_000).contains(&c.form_codes_per_hour) {
        return Err(AppError::bad("form_codes_per_hour must be 1 to 10,000"));
    }
    match (&c.source[..], &c.pattern) {
        ("generated", None) => return Err(AppError::bad("a generated coupon needs a pattern: {\"prefix\": \"WELCOME-\", \"length\": 8}")),
        (_, Some(p)) => serde_json::from_value::<Pattern>(p.clone()).map_err(|e| AppError::bad(format!("invalid pattern: {e}")))?.check()?,
        _ => {}
    }
    Ok(())
}

fn apply(c: &mut Coupon, b: Input, creating: bool) -> AppResult<()> {
    if let Some(n) = b.name {
        if !creating && n.trim() != c.name {
            return Err(AppError::bad("a coupon's name can't change: templates use it. Create a new coupon instead."));
        }
        c.name = n.trim().to_string();
    }
    if let Some(d) = b.description {
        c.description = d.trim().to_string();
    }
    if let Some(s) = b.source {
        c.source = s;
    }
    if let Some(p) = b.pattern {
        c.pattern = p.map(|p| json!(p));
    }
    if let Some(v) = b.expires_at {
        c.expires_at = v;
    }
    if let Some(v) = b.expires_after_days {
        c.expires_after_days = v;
    }
    if let Some(v) = b.new_code_each_time {
        c.new_code_each_time = v;
    }
    if let Some(v) = b.low_stock_threshold {
        c.low_stock_threshold = v;
    }
    if let Some(v) = b.form_codes_per_hour {
        c.form_codes_per_hour = v;
    }
    check(c)
}

fn name_taken(e: sqlx::Error, name: &str) -> AppError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => AppError::Conflict(format!("there's already a coupon named {name:?}")),
        _ => e.into(),
    }
}

pub async fn create(db: &PgPool, account_id: Uuid, b: Input) -> AppResult<Coupon> {
    let now = Utc::now();
    let mut c = Coupon {
        id: Uuid::now_v7(),
        account_id,
        name: String::new(),
        description: String::new(),
        source: "uploaded".into(),
        pattern: None,
        expires_at: None,
        expires_after_days: None,
        new_code_each_time: false,
        low_stock_threshold: None,
        form_codes_per_hour: 200,
        codes_total: 0,
        out_of_codes_at: None,
        low_stock_alerted_at: None,
        created_at: now,
        updated_at: now,
    };
    if b.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
        return Err(AppError::bad("name is required"));
    }
    apply(&mut c, b, true)?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM coupons WHERE account_id = $1 AND deleted_at IS NULL").bind(account_id).fetch_one(db).await?;
    if n >= MAX_COUPONS {
        return Err(AppError::LimitReached(format!("an account can have up to {MAX_COUPONS} coupons; delete one you no longer use")));
    }
    sqlx::query(
        "INSERT INTO coupons (id, account_id, name, description, source, pattern, expires_at, expires_after_days, new_code_each_time, low_stock_threshold, form_codes_per_hour)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(c.id)
    .bind(account_id)
    .bind(&c.name)
    .bind(&c.description)
    .bind(&c.source)
    .bind(&c.pattern)
    .bind(c.expires_at)
    .bind(c.expires_after_days)
    .bind(c.new_code_each_time)
    .bind(c.low_stock_threshold)
    .bind(c.form_codes_per_hour)
    .execute(db)
    .await
    .map_err(|e| name_taken(e, &c.name))?;
    load(db, account_id, c.id).await
}

pub async fn update(db: &PgPool, account_id: Uuid, id: Uuid, b: Input) -> AppResult<Coupon> {
    let mut c = load(db, account_id, id).await?;
    apply(&mut c, b, false)?;
    sqlx::query(
        "UPDATE coupons SET description = $3, source = $4, pattern = $5, expires_at = $6, expires_after_days = $7, new_code_each_time = $8,
                low_stock_threshold = $9, form_codes_per_hour = $10, low_stock_alerted_at = NULL, updated_at = now()
         WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(account_id)
    .bind(&c.description)
    .bind(&c.source)
    .bind(&c.pattern)
    .bind(c.expires_at)
    .bind(c.expires_after_days)
    .bind(c.new_code_each_time)
    .bind(c.low_stock_threshold)
    .bind(c.form_codes_per_hour)
    .execute(db)
    .await?;
    load(db, account_id, id).await
}

/// Codes given out (to anyone, delivered or not yet) and redeemed.
pub async fn stock(db: &PgPool, c: &Coupon) -> AppResult<Value> {
    let (assigned, redeemed, unverified): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'assigned'), count(*) FILTER (WHERE status = 'redeemed'), count(*) FILTER (WHERE status = 'redeemed' AND redeemed_verified IS NOT TRUE)
         FROM coupon_codes WHERE coupon_id = $1 AND status <> 'available'",
    )
    .bind(c.id)
    .fetch_one(db)
    .await?;
    Ok(json!({"total": c.codes_total, "available": (c.codes_total - assigned - redeemed).max(0), "assigned": assigned, "redeemed": redeemed, "redeemed_unverified": unverified}))
}

/// A coupon for the API: its settings, stock and alerts.
pub async fn to_json(db: &PgPool, c: &Coupon) -> AppResult<Value> {
    let mut v = serde_json::to_value(c)?;
    let stock = stock(db, c).await?;
    let low = c.low_stock_threshold.is_some_and(|t| stock["available"].as_i64().unwrap_or(0) < i64::from(t)) && c.codes_total > 0;
    let holds: Vec<(Option<Uuid>, Option<String>, Option<Uuid>, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT h.campaign_id, cm.name, h.flow_id, f.name, h.first_at FROM coupon_holds h
         LEFT JOIN campaigns cm ON cm.id = h.campaign_id LEFT JOIN flows f ON f.id = h.flow_id
         WHERE h.coupon_id = $1 ORDER BY h.first_at LIMIT 50",
    )
    .bind(c.id)
    .fetch_all(db)
    .await?;
    v["stock"] = stock;
    v["alerts"] = json!({
        "out_of_codes": c.out_of_codes_at.is_some(),
        "out_of_codes_since": c.out_of_codes_at,
        "low_stock": low,
        "waiting": holds.iter().map(|(cid, cname, fid, fname, at)| match cid {
            Some(id) => json!({"type": "campaign", "id": id, "name": cname, "since": at}),
            None => json!({"type": "flow", "id": fid, "name": fname, "since": at}),
        }).collect::<Vec<_>>(),
    });
    v["usage"] = json!({"tag": format!("{{% coupon_code '{}' %}}", c.name)});
    Ok(v)
}

pub async fn list(db: &PgPool, account_id: Uuid, limit: i64, offset: i64) -> AppResult<(Vec<Value>, i64)> {
    let rows: Vec<Coupon> = sqlx::query_as(&format!("SELECT {COLS} FROM coupons WHERE account_id = $1 AND deleted_at IS NULL ORDER BY created_at DESC LIMIT $2 OFFSET $3"))
        .bind(account_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM coupons WHERE account_id = $1 AND deleted_at IS NULL").bind(account_id).fetch_one(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for c in &rows {
        out.push(to_json(db, c).await?);
    }
    Ok((out, total))
}

/// Codes from pasted text or a CSV: one per line, the first column, a header row skipped.
/// Returns the codes and how many lines were refused.
pub fn parse_codes(text: &str) -> AppResult<(Vec<String>, usize)> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut out = Vec::new();
    let mut refused = 0;
    for (i, rec) in rdr.records().enumerate() {
        let Ok(rec) = rec else {
            refused += 1;
            continue;
        };
        let Some(first) = rec.get(0).map(str::trim).filter(|s| !s.is_empty()) else { continue };
        if i == 0 && matches!(first.to_lowercase().as_str(), "code" | "codes" | "coupon" | "coupon code" | "discount code") {
            continue;
        }
        if out.len() + refused >= MAX_PER_UPLOAD {
            return Err(AppError::bad(format!("upload at most {MAX_PER_UPLOAD} codes at a time")));
        }
        if valid_code(first) {
            out.push(first.to_string());
        } else {
            refused += 1;
        }
    }
    Ok((out, refused))
}

/// 1-100 characters with no spaces or control characters.
pub fn valid_code(c: &str) -> bool {
    !c.is_empty() && c.chars().count() <= MAX_CODE_LEN && !c.chars().any(|ch| ch.is_whitespace() || ch.is_control())
}

/// Add codes to a coupon. Codes it already has (in any letter case) are skipped. Returns (added, skipped).
pub async fn add_codes(db: &PgPool, c: &Coupon, codes: Vec<String>) -> AppResult<(i64, i64)> {
    if codes.len() > MAX_PER_UPLOAD {
        return Err(AppError::bad(format!("upload at most {MAX_PER_UPLOAD} codes at a time")));
    }
    // Same code twice in one upload (in any case) counts once.
    let mut seen = std::collections::HashSet::with_capacity(codes.len());
    let codes: Vec<String> = codes.into_iter().map(|s| s.trim().to_string()).filter(|s| valid_code(s) && seen.insert(s.to_uppercase())).collect();
    let given = seen.len() as i64;
    let mut tx = db.begin().await?;
    // One upload per coupon at a time, so the per-coupon cap holds.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-codes:{}", c.id)).execute(&mut *tx).await?;
    let total: i64 = sqlx::query_scalar("SELECT codes_total FROM coupons WHERE id = $1").bind(c.id).fetch_one(&mut *tx).await?;
    if total + given > MAX_PER_COUPON {
        return Err(AppError::bad(format!("a coupon can hold up to {MAX_PER_COUPON} codes; this one has {total}")));
    }
    let mut added = 0i64;
    for chunk in codes.chunks(CHUNK) {
        added += sqlx::query(
            "INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm) SELECT $1, $2, c, upper(c) FROM unnest($3::text[]) c
             ON CONFLICT (coupon_id, code_norm) DO NOTHING",
        )
        .bind(c.account_id)
        .bind(c.id)
        .bind(chunk)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    }
    if added > 0 {
        restocked(&mut tx, c, added).await?;
    }
    tx.commit().await?;
    Ok((added, given - added))
}

/// New codes arrived: count them, lift the out-of-codes and low-stock alerts, and let waiting
/// campaigns carry on (their next retry, within minutes, sends).
async fn restocked(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, c: &Coupon, added: i64) -> AppResult<()> {
    sqlx::query("UPDATE coupons SET codes_total = codes_total + $2, out_of_codes_at = NULL, low_stock_alerted_at = NULL, updated_at = now() WHERE id = $1")
        .bind(c.id)
        .bind(added)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE campaigns SET error = NULL, updated_at = now() WHERE id IN (SELECT campaign_id FROM coupon_holds WHERE coupon_id = $1) AND status = 'sending'")
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM coupon_holds WHERE coupon_id = $1").bind(c.id).execute(&mut **tx).await?;
    Ok(())
}

/// Make `count` new codes from the coupon's pattern (or `pattern`, which is then saved). Returns how many were added.
pub async fn generate(db: &PgPool, c: &Coupon, count: usize, pattern: Option<Pattern>) -> AppResult<i64> {
    if !(1..=MAX_PER_UPLOAD).contains(&count) {
        return Err(AppError::bad(format!("generate 1 to {MAX_PER_UPLOAD} codes at a time")));
    }
    let pattern = match pattern {
        Some(p) => {
            p.check()?;
            sqlx::query("UPDATE coupons SET pattern = $2, updated_at = now() WHERE id = $1").bind(c.id).bind(json!(p)).execute(db).await?;
            p
        }
        None => match &c.pattern {
            Some(p) => serde_json::from_value::<Pattern>(p.clone()).map_err(|e| AppError::bad(format!("invalid pattern: {e}")))?,
            None => return Err(AppError::bad("give a pattern, e.g. {\"prefix\": \"WELCOME-\", \"length\": 8}")),
        },
    };
    if pattern.space() < 4.0 * (c.codes_total as f64 + count as f64) {
        return Err(AppError::bad("that pattern can't make enough different codes; make the random part longer"));
    }
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-codes:{}", c.id)).execute(&mut *tx).await?;
    let total: i64 = sqlx::query_scalar("SELECT codes_total FROM coupons WHERE id = $1").bind(c.id).fetch_one(&mut *tx).await?;
    if total + count as i64 > MAX_PER_COUPON {
        return Err(AppError::bad(format!("a coupon can hold up to {MAX_PER_COUPON} codes; this one has {total}")));
    }
    let mut added = 0i64;
    // Collisions are rare with a big enough space; a few rounds top up any that clashed.
    for _ in 0..5 {
        let want = count as i64 - added;
        if want <= 0 {
            break;
        }
        let batch: Vec<String> = {
            let mut rng = rand::thread_rng();
            (0..want).map(|_| pattern.make(&mut rng)).collect()
        };
        for chunk in batch.chunks(CHUNK) {
            added += sqlx::query(
                "INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm) SELECT $1, $2, c, upper(c) FROM unnest($3::text[]) c
                 ON CONFLICT (coupon_id, code_norm) DO NOTHING",
            )
            .bind(c.account_id)
            .bind(c.id)
            .bind(chunk)
            .execute(&mut *tx)
            .await?
            .rows_affected() as i64;
        }
    }
    if added > 0 {
        restocked(&mut tx, c, added).await?;
    }
    tx.commit().await?;
    Ok(added)
}

#[derive(Debug, Default, Deserialize)]
pub struct CodeQuery {
    /// The start of a code, or (with an @) a person's email.
    pub q: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn like_prefix(s: &str) -> String {
    format!("{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

/// One page of a coupon's codes, newest given first, with who has each.
pub async fn codes(db: &PgPool, c: &Coupon, q: &CodeQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).clamp(0, 10_000);
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    if status.is_some_and(|s| !matches!(s, "available" | "assigned" | "redeemed")) {
        return Err(AppError::bad("status must be available, assigned, redeemed or all"));
    }
    let term = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(254).collect::<String>());
    let (code_like, email) = match &term {
        Some(t) if t.contains('@') => (None, Some(t.to_lowercase())),
        Some(t) => (Some(like_prefix(&t.to_uppercase())), None),
        None => (None, None),
    };
    let where_ = "cc.coupon_id = $1 AND ($2::text IS NULL OR cc.status = $2) AND ($3::text IS NULL OR cc.code_norm LIKE $3)
                  AND ($4::text IS NULL OR cc.profile_id IN (SELECT id FROM profiles WHERE account_id = $5 AND lower(email) = $4))";
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, Option<Uuid>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<f64>, Option<String>, Option<bool>)> =
        sqlx::query_as(&format!(
            "SELECT cc.id, cc.code, cc.status, cc.profile_id, p.email, cc.assigned_at, cc.delivered_at, cc.expires_at, cc.redeemed_at, cc.order_value, cc.assigned_via, cc.redeemed_verified
             FROM coupon_codes cc LEFT JOIN profiles p ON p.id = cc.profile_id
             WHERE {where_} ORDER BY cc.assigned_at DESC NULLS LAST, cc.id LIMIT $6 OFFSET $7"
        ))
        .bind(c.id)
        .bind(status)
        .bind(&code_like)
        .bind(&email)
        .bind(c.account_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
    // Counting a 2-million-code pool exactly is slow; past 10,000 the total says "10,000+".
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM (SELECT 1 FROM coupon_codes cc WHERE {where_} LIMIT 10001) x"))
        .bind(c.id)
        .bind(status)
        .bind(&code_like)
        .bind(&email)
        .bind(c.account_id)
        .fetch_one(db)
        .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, code, status, profile_id, email, assigned_at, delivered_at, expires_at, redeemed_at, order_value, via, verified)| {
            json!({"id": id, "code": code, "status": status, "profile_id": profile_id, "email": email, "given_at": assigned_at,
                   "delivered_at": delivered_at, "expires_at": expires_at, "redeemed_at": redeemed_at, "redeemed_verified": verified,
                   "order_value": order_value, "given_via": via})
        })
        .collect();
    Ok(json!({"data": data, "total": total.min(10_000), "total_capped": total > 10_000}))
}

fn csv_field(s: &str) -> String {
    // Keep spreadsheets from treating a value as a formula.
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

/// One chunk of the CSV export after code id `after`: (csv text, last id), or None when done.
pub async fn export_chunk(db: &PgPool, c: &Coupon, status: Option<&str>, after: i64, header: bool) -> AppResult<Option<(String, i64)>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<f64>, Option<bool>)> = sqlx::query_as(
        "SELECT cc.id, cc.code, cc.status, p.email, cc.assigned_at, cc.expires_at, cc.redeemed_at, cc.order_value, cc.redeemed_verified
         FROM coupon_codes cc LEFT JOIN profiles p ON p.id = cc.profile_id
         WHERE cc.coupon_id = $1 AND cc.id > $2 AND ($3::text IS NULL OR cc.status = $3) ORDER BY cc.id LIMIT 5000",
    )
    .bind(c.id)
    .bind(after)
    .bind(status)
    .fetch_all(db)
    .await?;
    if rows.is_empty() && !header {
        return Ok(None);
    }
    let mut out = String::new();
    if header {
        out.push_str("code,status,email,given_at,expires_at,redeemed_at,order_value,order_verified\n");
    }
    let t = |d: &Option<DateTime<Utc>>| d.map(|d| d.to_rfc3339()).unwrap_or_default();
    let mut last = after;
    for (id, code, status, email, given, expires, redeemed, value, verified) in &rows {
        last = *id;
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            csv_field(code),
            status,
            csv_field(email.as_deref().unwrap_or("")),
            t(given),
            t(expires),
            t(redeemed),
            value.map(|v| v.to_string()).unwrap_or_default(),
            verified.map(|v| if v { "yes" } else { "no" }).unwrap_or("")
        ));
    }
    Ok(Some((out, last)))
}

/// Campaigns about to send, live flows and live forms that use the coupon (they'd fail without it).
pub async fn users(db: &PgPool, c: &Coupon) -> AppResult<Vec<String>> {
    let needle = format!("%{}%", c.name.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT 'campaign', name FROM campaigns WHERE account_id = $1 AND status IN ('scheduled', 'sending', 'testing') AND (lower(content::text) LIKE $2 OR lower(COALESCE(ab_test::text, '')) LIKE $2)
         UNION ALL SELECT 'flow', name FROM flows WHERE account_id = $1 AND status IN ('live', 'paused') AND lower(definition::text) LIKE $2
         UNION ALL SELECT 'form', name FROM forms WHERE account_id = $1 AND status = 'live' AND lower(config->>'coupon') = lower($3)
         LIMIT 20",
    )
    .bind(c.account_id)
    .bind(&needle)
    .bind(&c.name)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(k, n)| format!("{k} {n:?}")).collect())
}

/// Delete a coupon and (in the background) its codes.
pub async fn delete(db: &PgPool, c: &Coupon) -> AppResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE coupons SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL").bind(c.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM coupon_holds WHERE coupon_id = $1").bind(c.id).execute(&mut *tx).await?;
    crate::jobs::enqueue(&mut *tx, "coupons_purge", json!({"coupon_id": c.id}), None).await?;
    tx.commit().await?;
    Ok(())
}

/// Job: remove a deleted coupon's codes in batches, then the coupon.
pub async fn purge(db: &PgPool, coupon_id: Uuid) -> anyhow::Result<()> {
    let deleted: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM coupons WHERE id = $1").bind(coupon_id).fetch_optional(db).await?.unwrap_or(false);
    if !deleted {
        return Ok(());
    }
    for _ in 0..100 {
        let n = sqlx::query("DELETE FROM coupon_codes WHERE id IN (SELECT id FROM coupon_codes WHERE coupon_id = $1 LIMIT 20000)").bind(coupon_id).execute(db).await?.rows_affected();
        if n == 0 {
            sqlx::query("DELETE FROM coupons WHERE id = $1 AND deleted_at IS NOT NULL").bind(coupon_id).execute(db).await?;
            return Ok(());
        }
    }
    // More left: carry on in a new job so no single run takes too long.
    crate::jobs::enqueue(db, "coupons_purge", json!({"coupon_id": coupon_id}), None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasted_and_csv_codes() {
        let (codes, refused) = parse_codes("Code,Value\nSAVE-1,10\n\"SAVE-2\",10\n  SAVE-3  \nbad code\n\n").unwrap();
        assert_eq!(codes, vec!["SAVE-1", "SAVE-2", "SAVE-3"]);
        assert_eq!(refused, 1);
        let (codes, _) = parse_codes("A1\nB2").unwrap();
        assert_eq!(codes, vec!["A1", "B2"], "no header row");
    }

    #[test]
    fn generated_codes_skip_look_alikes() {
        let p = Pattern { prefix: "WELCOME-".into(), length: 10, alphabet: "letters_digits".into() };
        p.check().unwrap();
        let mut rng = rand::thread_rng();
        for _ in 0..200 {
            let c = p.make(&mut rng);
            assert!(c.starts_with("WELCOME-") && c.len() == 18, "{c}");
            assert!(!c[8..].contains(['0', 'O', '1', 'I', 'L']), "{c}");
        }
        assert!(Pattern { prefix: "x y".into(), length: 8, alphabet: "letters".into() }.check().is_err());
        assert!(Pattern { prefix: "".into(), length: 3, alphabet: "digits".into() }.check().is_err());
        assert!(Pattern { prefix: "".into(), length: 8, alphabet: "emoji".into() }.check().is_err());
    }

    #[test]
    fn csv_cells_cant_become_formulas() {
        assert_eq!(csv_field("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("SAVE10"), "SAVE10");
    }
}

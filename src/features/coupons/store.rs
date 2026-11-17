//! Coupons and their codes: create, change, add (upload or generate), search, stock, export and delete.

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
/// Most bytes one upload may send.
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Most codes one coupon may hold.
pub const MAX_PER_COUPON: i64 = 2_000_000;
/// Codes are 4 to 64 ASCII letters, digits, `-` and `_`.
pub const MIN_CODE_LEN: usize = 4;
pub const MAX_CODE_LEN: usize = 64;
/// Generated codes are at least this long, with at least `MIN_RANDOM` random characters.
const MIN_GENERATED_LEN: usize = 8;
const MIN_RANDOM: usize = 6;
/// Most coupons per account.
pub const MAX_COUPONS: i64 = 500;
/// Rows per insert or delete statement.
pub const CHUNK: usize = 5_000;
/// Generated inline (in the request) up to this many; more run as a background job.
pub const GENERATE_INLINE: usize = CHUNK;
/// Codes one generation job run makes before handing over to the next run.
const GENERATE_PER_RUN: usize = 50_000;
/// Rows one purge job run deletes before handing over to the next run.
const PURGE_PER_RUN: usize = 50_000;
/// An upload or generation lease nobody released (a crash) expires after this long.
const LEASE_MINUTES: i32 = 10;
/// Most refused lines listed back to the uploader.
const MAX_REFUSED_LISTED: usize = 20;

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
    pub generate_pending: i32,
    pub out_of_codes_at: Option<DateTime<Utc>>,
    pub low_stock_alerted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, account_id, name, description, source, pattern, expires_at, expires_after_days, new_code_each_time, low_stock_threshold, form_codes_per_hour, codes_total, generate_pending, out_of_codes_at, low_stock_alerted_at, created_at, updated_at";

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

/// A code as matched: upper-cased ASCII. None when it isn't a valid code (see [`code_problem`]).
pub fn normalize(code: &str) -> Option<String> {
    code_problem(code).is_none().then(|| code.to_ascii_uppercase())
}

/// Why `code` can't be a code, if it can't: 4 to 64 ASCII letters, digits, `-` and `_`.
pub fn code_problem(code: &str) -> Option<&'static str> {
    if code.len() < MIN_CODE_LEN {
        Some("shorter than 4 characters")
    } else if code.len() > MAX_CODE_LEN {
        Some("longer than 64 characters")
    } else if !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Some("only letters A-Z, digits, - and _ are allowed")
    } else {
        None
    }
}

pub fn valid_code(code: &str) -> bool {
    code_problem(code).is_none()
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
        if !(MIN_RANDOM..=24).contains(&self.length) {
            return Err(AppError::bad("pattern.length (the random part) must be 6 to 24 characters"));
        }
        if self.prefix.len() > 20 || !self.prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(AppError::bad("pattern.prefix can have up to 20 letters, digits, - and _"));
        }
        if self.prefix.len() + self.length < MIN_GENERATED_LEN {
            return Err(AppError::bad("generated codes must be at least 8 characters: make the prefix or the random part longer"));
        }
        Ok(())
    }

    /// How many different codes the pattern can make.
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
        generate_pending: 0,
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

/// Codes given out (to anyone, sent yet or not), redeemed in verified orders, and with only an
/// unverified (website script) order.
pub async fn stock(db: &PgPool, c: &Coupon) -> AppResult<Value> {
    let (assigned, redeemed, unverified): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE redeemed_at IS NOT NULL), count(*) FILTER (WHERE redeemed_at IS NULL AND browser_order_at IS NOT NULL)
         FROM coupon_codes WHERE coupon_id = $1 AND status = 'assigned'",
    )
    .bind(c.id)
    .fetch_one(db)
    .await?;
    Ok(json!({"total": c.codes_total, "available": (c.codes_total - assigned).max(0), "assigned": assigned, "redeemed": redeemed, "redeemed_unverified": unverified, "generating": c.generate_pending}))
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
    v["usage"] = json!({"tag": format!("{{% coupon_code '{}' %}}", c.name), "expiry_tag": format!("{{% coupon_expiry '{}' %}}", c.name)});
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

// ---------- Adding codes ----------

/// One upload or generation at a time per account: a lease row, taken without holding a connection.
pub struct Lease {
    account_id: Uuid,
    token: Uuid,
}

impl Lease {
    /// Take the account's lease, or None when another upload or generation holds it.
    pub async fn take(db: &PgPool, account_id: Uuid) -> AppResult<Option<Lease>> {
        let token = Uuid::now_v7();
        let got: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO coupon_work_leases (account_id, token) VALUES ($1, $2)
             ON CONFLICT (account_id) DO UPDATE SET token = EXCLUDED.token, started_at = now()
                WHERE coupon_work_leases.started_at < now() - make_interval(mins => $3)
             RETURNING token",
        )
        .bind(account_id)
        .bind(token)
        .bind(LEASE_MINUTES)
        .fetch_optional(db)
        .await?;
        Ok(got.map(|token| Lease { account_id, token }))
    }

    pub async fn release(self, db: &PgPool) {
        if let Err(e) = sqlx::query("DELETE FROM coupon_work_leases WHERE account_id = $1 AND token = $2").bind(self.account_id).bind(self.token).execute(db).await {
            tracing::warn!(account = %self.account_id, error = %e, "couldn't release a coupon upload lease (it expires on its own)");
        }
    }
}

pub fn busy() -> AppError {
    AppError::Conflict("codes are already being added to a coupon on this account; try again when that finishes".into())
}

/// Insert one batch of valid codes (each statement on its own, no long transaction), within the
/// coupon's cap. Returns how many were new. The caller holds the account's [`Lease`].
pub async fn insert_batch(db: &PgPool, c: &Coupon, codes: &[String]) -> AppResult<i64> {
    if codes.is_empty() {
        return Ok(0);
    }
    let total: i64 = sqlx::query_scalar("SELECT codes_total FROM coupons WHERE id = $1").bind(c.id).fetch_one(db).await?;
    if total + codes.len() as i64 > MAX_PER_COUPON {
        return Err(AppError::bad(format!("a coupon can hold up to {MAX_PER_COUPON} codes; this one has {total}")));
    }
    let added = sqlx::query(
        "INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm) SELECT $1, $2, c, upper(c) FROM unnest($3::text[]) c
         ON CONFLICT (coupon_id, code_norm) DO NOTHING",
    )
    .bind(c.account_id)
    .bind(c.id)
    .bind(codes)
    .execute(db)
    .await?
    .rows_affected() as i64;
    if added > 0 {
        restocked(db, c, added).await?;
    }
    Ok(added)
}

/// New codes arrived: count them, lift the out-of-codes and low-stock alerts (a new empty spell
/// alerts again), and let waiting campaigns carry on (their next retry, within minutes, sends).
async fn restocked(db: &PgPool, c: &Coupon, added: i64) -> AppResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE coupons SET codes_total = codes_total + $2, out_of_codes_at = NULL, low_stock_alerted_at = NULL, updated_at = now() WHERE id = $1")
        .bind(c.id)
        .bind(added)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE campaigns SET error = NULL, updated_at = now() WHERE id IN (SELECT campaign_id FROM coupon_holds WHERE coupon_id = $1) AND status = 'sending'")
        .bind(c.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM coupon_holds WHERE coupon_id = $1").bind(c.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// What an upload did.
#[derive(Debug, Default, Serialize)]
pub struct UploadResult {
    pub added: i64,
    /// Codes the coupon already had (in any letter case), or repeated in the upload.
    pub duplicates: i64,
    pub refused: i64,
    /// The first refused lines: {line, reason}.
    pub refused_lines: Vec<Value>,
    /// Set when the upload stopped at a limit: what was added before it stays added.
    pub stopped: Option<String>,
}

/// Reads codes from pasted text or a CSV a piece at a time: one code per line, the first column, a
/// header row skipped. Bytes may arrive in any chunks; a line is handled once it's complete.
#[derive(Default)]
pub struct LineReader {
    partial: Vec<u8>,
    line: usize,
    bytes: usize,
    pending: Vec<String>,
    pub result: UploadResult,
}

impl LineReader {
    /// Stop reading at a limit. Batches already added stay added; the result says where it stopped.
    fn stop(&mut self, why: String) {
        self.result.stopped.get_or_insert(why);
        self.partial.clear();
    }

    /// Whether the upload stopped at a limit (the caller stops sending more).
    pub fn stopped(&self) -> bool {
        self.result.stopped.is_some()
    }

    /// Feed more bytes.
    pub fn feed(&mut self, bytes: &[u8]) -> AppResult<()> {
        if self.stopped() {
            return Ok(());
        }
        self.bytes += bytes.len();
        if self.bytes > MAX_UPLOAD_BYTES {
            self.stop(format!("Stopped after line {}: an upload can be at most {} MB. The codes before it were added; upload the rest separately.", self.line, MAX_UPLOAD_BYTES / 1024 / 1024));
            return Ok(());
        }
        let mut rest = bytes;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            if self.stopped() {
                return Ok(());
            }
            self.partial.extend_from_slice(&rest[..i]);
            let line = std::mem::take(&mut self.partial);
            self.take_line(&line)?;
            rest = &rest[i + 1..];
        }
        self.partial.extend_from_slice(rest);
        if self.partial.len() > 4096 {
            self.stop(format!("Stopped at line {}: it's too long to be a code. The codes before it were added.", self.line + 1));
        }
        Ok(())
    }

    /// The last line (without a newline).
    pub fn finish(&mut self) -> AppResult<()> {
        let line = std::mem::take(&mut self.partial);
        if !line.is_empty() && !self.stopped() {
            self.take_line(&line)?;
        }
        Ok(())
    }

    fn take_line(&mut self, raw: &[u8]) -> AppResult<()> {
        self.line += 1;
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_start_matches('\u{feff}');
        let first = text.split(',').next().unwrap_or("").trim().trim_matches('"').trim();
        if first.is_empty() {
            return Ok(());
        }
        if self.line == 1 && matches!(first.to_ascii_lowercase().as_str(), "code" | "codes" | "coupon" | "coupon code" | "discount code") {
            return Ok(());
        }
        if self.pending.len() as i64 + self.result.added + self.result.duplicates + self.result.refused >= MAX_PER_UPLOAD as i64 {
            self.stop(format!("Stopped at line {}: an upload can have at most {MAX_PER_UPLOAD} codes. The codes before it were added; upload the rest separately.", self.line));
            return Ok(());
        }
        match code_problem(first) {
            None => self.pending.push(first.to_string()),
            Some(why) => {
                self.result.refused += 1;
                if self.result.refused_lines.len() < MAX_REFUSED_LISTED {
                    self.result.refused_lines.push(json!({"line": self.line, "reason": why}));
                }
            }
        }
        Ok(())
    }

    /// Codes ready to insert: a full batch, or (with `all`) whatever is left.
    pub fn ready(&mut self, all: bool) -> Option<Vec<String>> {
        if self.pending.len() >= CHUNK || (all && !self.pending.is_empty()) {
            let n = self.pending.len().min(CHUNK);
            Some(self.pending.drain(..n).collect())
        } else {
            None
        }
    }

    /// Insert whatever is ready (stopping, not failing, when the coupon is full).
    pub async fn flush(&mut self, db: &PgPool, c: &Coupon, all: bool) -> AppResult<()> {
        while let Some(batch) = self.ready(all) {
            let total: i64 = sqlx::query_scalar("SELECT codes_total FROM coupons WHERE id = $1").bind(c.id).fetch_one(db).await?;
            if total + batch.len() as i64 > MAX_PER_COUPON {
                self.pending.clear();
                self.stop(format!("Stopped: a coupon can hold up to {MAX_PER_COUPON} codes and this one has {total}. The codes before that were added."));
                return Ok(());
            }
            let added = insert_batch(db, c, &batch).await?;
            self.result.added += added;
            self.result.duplicates += batch.len() as i64 - added;
        }
        Ok(())
    }
}

/// Add a list of codes (the JSON and MCP form), in batches. The caller holds the [`Lease`].
pub async fn add_list(db: &PgPool, c: &Coupon, codes: &[String]) -> AppResult<UploadResult> {
    if codes.len() > MAX_PER_UPLOAD {
        return Err(AppError::bad(format!("upload at most {MAX_PER_UPLOAD} codes at a time")));
    }
    let mut r = LineReader::default();
    for code in codes {
        if r.stopped() {
            break;
        }
        r.take_line(code.as_bytes())?;
        r.flush(db, c, false).await?;
    }
    r.flush(db, c, true).await?;
    Ok(r.result)
}

/// Add codes from pasted text or a CSV held in memory (MCP), in batches. The caller holds the [`Lease`].
pub async fn add_text(db: &PgPool, c: &Coupon, text: &str) -> AppResult<UploadResult> {
    let mut r = LineReader::default();
    for piece in text.as_bytes().chunks(64 * 1024) {
        if r.stopped() {
            break;
        }
        r.feed(piece)?;
        r.flush(db, c, false).await?;
    }
    r.finish()?;
    r.flush(db, c, true).await?;
    Ok(r.result)
}

fn saved_pattern(c: &Coupon) -> AppResult<Pattern> {
    match &c.pattern {
        Some(p) => serde_json::from_value::<Pattern>(p.clone()).map_err(|e| AppError::bad(format!("invalid pattern: {e}"))),
        None => Err(AppError::bad("give a pattern, e.g. {\"prefix\": \"WELCOME-\", \"length\": 8}")),
    }
}

/// Make up to `count` new codes from `pattern` in one bounded batch (collisions are retried a few times).
async fn generate_batch(db: &PgPool, c: &Coupon, pattern: &Pattern, count: usize) -> AppResult<i64> {
    let mut added = 0i64;
    for _ in 0..5 {
        let want = count as i64 - added;
        if want <= 0 {
            break;
        }
        let batch: Vec<String> = {
            let mut rng = rand::thread_rng();
            (0..want).map(|_| pattern.make(&mut rng)).collect()
        };
        added += insert_batch(db, c, &batch).await?;
    }
    Ok(added)
}

/// Make `count` new codes from the coupon's pattern (or `pattern`, which is then saved). Up to
/// [`GENERATE_INLINE`] are made now; more are queued for a background job. Returns (added now, queued).
pub async fn generate(db: &PgPool, c: &Coupon, count: usize, pattern: Option<Pattern>) -> AppResult<(i64, i64)> {
    if !(1..=MAX_PER_UPLOAD).contains(&count) {
        return Err(AppError::bad(format!("generate 1 to {MAX_PER_UPLOAD} codes at a time")));
    }
    let pattern = match pattern {
        Some(p) => {
            p.check()?;
            p
        }
        None => {
            let p = saved_pattern(c)?;
            p.check()?;
            p
        }
    };
    if pattern.space() < 4.0 * (c.codes_total as f64 + f64::from(c.generate_pending) + count as f64) {
        return Err(AppError::bad("that pattern can't make enough different codes; make the random part longer"));
    }
    if c.codes_total + i64::from(c.generate_pending) + count as i64 > MAX_PER_COUPON {
        return Err(AppError::bad(format!("a coupon can hold up to {MAX_PER_COUPON} codes; this one has {}", c.codes_total)));
    }
    sqlx::query("UPDATE coupons SET pattern = $2, updated_at = now() WHERE id = $1").bind(c.id).bind(json!(pattern)).execute(db).await?;
    if count > GENERATE_INLINE {
        let mut tx = db.begin().await?;
        sqlx::query("UPDATE coupons SET generate_pending = generate_pending + $2 WHERE id = $1").bind(c.id).bind(count as i32).execute(&mut *tx).await?;
        crate::jobs::enqueue(&mut *tx, "coupons_generate", json!({"coupon_id": c.id}), None).await?;
        tx.commit().await?;
        return Ok((0, count as i64));
    }
    let Some(lease) = Lease::take(db, c.account_id).await? else { return Err(busy()) };
    let res = generate_batch(db, c, &pattern, count).await;
    lease.release(db).await;
    Ok((res?, 0))
}

/// Job: make some of a coupon's pending generated codes, then hand over to the next run.
pub async fn generate_job(db: &PgPool, coupon_id: Uuid) -> anyhow::Result<()> {
    let Some(c): Option<Coupon> = sqlx::query_as(&format!("SELECT {COLS} FROM coupons WHERE id = $1 AND deleted_at IS NULL")).bind(coupon_id).fetch_optional(db).await? else {
        return Ok(());
    };
    if c.generate_pending <= 0 {
        return Ok(());
    }
    let Some(lease) = Lease::take(db, c.account_id).await? else {
        // An upload is running: try again shortly.
        crate::jobs::enqueue(db, "coupons_generate", json!({"coupon_id": c.id}), Some(Utc::now() + chrono::Duration::seconds(30))).await?;
        return Ok(());
    };
    let res: AppResult<()> = async {
        let pattern = saved_pattern(&c)?;
        let mut left = (c.generate_pending as usize).min(GENERATE_PER_RUN);
        while left > 0 {
            let n = left.min(CHUNK);
            generate_batch(db, &c, &pattern, n).await?;
            sqlx::query("UPDATE coupons SET generate_pending = GREATEST(generate_pending - $2, 0) WHERE id = $1").bind(c.id).bind(n as i32).execute(db).await?;
            left -= n;
        }
        Ok(())
    }
    .await;
    lease.release(db).await;
    if let Err(e) = res {
        // Don't retry forever on a lasting problem (the cap, a bad pattern): stop and say so in the log.
        sqlx::query("UPDATE coupons SET generate_pending = 0 WHERE id = $1").bind(c.id).execute(db).await?;
        tracing::warn!(coupon = %c.id, error = %e.public_message(), "stopped generating coupon codes");
        return Ok(());
    }
    let more: i32 = sqlx::query_scalar("SELECT generate_pending FROM coupons WHERE id = $1").bind(c.id).fetch_one(db).await?;
    if more > 0 {
        crate::jobs::enqueue(db, "coupons_generate", json!({"coupon_id": c.id}), None).await?;
    }
    Ok(())
}

// ---------- Reading codes ----------

#[derive(Debug, Default, Deserialize)]
pub struct CodeQuery {
    /// The start of a code, or (with an @) a person's email.
    pub q: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    /// From `next_cursor` of the previous page.
    pub cursor: Option<i64>,
}

fn like_prefix(s: &str) -> String {
    format!("{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

fn check_status(status: Option<&str>) -> AppResult<Option<&str>> {
    let status = status.filter(|s| !s.is_empty() && *s != "all");
    if status.is_some_and(|s| !matches!(s, "available" | "assigned" | "redeemed")) {
        return Err(AppError::bad("status must be available, assigned, redeemed or all"));
    }
    Ok(status)
}

const STATUS_SQL: &str = "($2::text IS NULL OR ($2 = 'redeemed' AND cc.redeemed_at IS NOT NULL) OR ($2 <> 'redeemed' AND cc.status = $2))";

/// One page of a coupon's codes, newest added first, with who has each. Pages follow `next_cursor`.
pub async fn codes(db: &PgPool, c: &Coupon, q: &CodeQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let status = check_status(q.status.as_deref())?;
    let term = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(254).collect::<String>());
    let (code_like, email) = match &term {
        Some(t) if t.contains('@') => (None, Some(t.to_lowercase())),
        Some(t) => (Some(like_prefix(&t.to_ascii_uppercase())), None),
        None => (None, None),
    };
    let where_ = format!(
        "cc.coupon_id = $1 AND {STATUS_SQL} AND ($3::text IS NULL OR cc.code_norm LIKE $3)
         AND ($4::text IS NULL OR cc.profile_id IN (SELECT id FROM profiles WHERE account_id = $5 AND lower(email) = $4))"
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, Option<Uuid>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<f64>, Option<String>, Option<DateTime<Utc>>)> =
        sqlx::query_as(&format!(
            "SELECT cc.id, cc.code, cc.status, cc.profile_id, p.email, cc.assigned_at, cc.delivered_at, cc.expires_at, cc.redeemed_at, cc.order_value, cc.assigned_via, cc.browser_order_at
             FROM coupon_codes cc LEFT JOIN profiles p ON p.id = cc.profile_id
             WHERE {where_} AND ($6::bigint IS NULL OR cc.id < $6) ORDER BY cc.id DESC LIMIT $7"
        ))
        .bind(c.id)
        .bind(status)
        .bind(&code_like)
        .bind(&email)
        .bind(c.account_id)
        .bind(q.cursor)
        .bind(limit + 1)
        .fetch_all(db)
        .await?;
    let more = rows.len() as i64 > limit;
    let rows = &rows[..rows.len().min(limit as usize)];
    // An exact count of a 2-million-code pool is slow: searches count up to 10,000.
    let total: i64 = if term.is_none() {
        let s = stock(db, c).await?;
        s[status.unwrap_or("total")].as_i64().unwrap_or(0)
    } else {
        sqlx::query_scalar(&format!("SELECT count(*) FROM (SELECT 1 FROM coupon_codes cc WHERE {where_} LIMIT 10001) x"))
            .bind(c.id)
            .bind(status)
            .bind(&code_like)
            .bind(&email)
            .bind(c.account_id)
            .fetch_one(db)
            .await?
    };
    let data: Vec<Value> = rows
        .iter()
        .map(|(id, code, status, profile_id, email, assigned_at, delivered_at, expires_at, redeemed_at, order_value, via, browser_at)| {
            json!({"id": id, "code": code, "status": if redeemed_at.is_some() { "redeemed" } else { status.as_str() }, "profile_id": profile_id, "email": email,
                   "given_at": assigned_at, "delivered_at": delivered_at, "expires_at": expires_at, "redeemed_at": redeemed_at,
                   "order_value": order_value, "unverified_order_at": browser_at, "given_via": via})
        })
        .collect();
    let next = if more { rows.last().map(|r| r.0) } else { None };
    let capped = term.is_some() && total > 10_000;
    Ok(json!({"data": data, "total": if capped { 10_000 } else { total }, "total_capped": capped, "next_cursor": next}))
}

fn csv_field(s: &str) -> String {
    // Keep spreadsheets from treating a value as a formula.
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

/// Export formats: `codes` is one code per line exactly as stored (it uploads back unchanged);
/// `csv` adds who has each code and is safe to open in a spreadsheet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Codes,
    Csv,
}

impl Format {
    pub fn parse(s: Option<&str>) -> AppResult<Format> {
        match s.unwrap_or("csv") {
            "csv" => Ok(Format::Csv),
            "codes" | "txt" => Ok(Format::Codes),
            _ => Err(AppError::bad("format must be csv or codes")),
        }
    }
}

/// One chunk of an export after code id `after` (at most `limit` codes): (text, last id, codes in it).
pub async fn export_chunk(db: &PgPool, c: &Coupon, status: Option<&str>, format: Format, after: i64, header: bool, limit: i64) -> AppResult<(String, i64, usize)> {
    let status = check_status(status)?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<f64>, Option<DateTime<Utc>>)> = sqlx::query_as(&format!(
        "SELECT cc.id, cc.code, cc.status, p.email, cc.assigned_at, cc.expires_at, cc.redeemed_at, cc.order_value, cc.browser_order_at
         FROM coupon_codes cc LEFT JOIN profiles p ON p.id = cc.profile_id
         WHERE cc.coupon_id = $1 AND cc.id > $3 AND {STATUS_SQL} ORDER BY cc.id LIMIT $4"
    ))
    .bind(c.id)
    .bind(status)
    .bind(after)
    .bind(limit.clamp(1, 10_000))
    .fetch_all(db)
    .await?;
    let mut out = String::new();
    if header && format == Format::Csv {
        out.push_str("code,status,email,given_at,expires_at,redeemed_at,order_value,unverified_order_at\n");
    }
    let t = |d: &Option<DateTime<Utc>>| d.map(|d| d.to_rfc3339()).unwrap_or_default();
    let mut last = after;
    for (id, code, status, email, given, expires, redeemed, value, browser) in &rows {
        last = *id;
        match format {
            Format::Codes => {
                out.push_str(code);
                out.push('\n');
            }
            Format::Csv => out.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                csv_field(code),
                if redeemed.is_some() { "redeemed" } else { status },
                csv_field(email.as_deref().unwrap_or("")),
                t(given),
                t(expires),
                t(redeemed),
                value.map(|v| v.to_string()).unwrap_or_default(),
                t(browser)
            )),
        }
    }
    Ok((out, last, rows.len()))
}

// ---------- Deleting ----------

/// Everything that uses the coupon, so it can't be deleted from under them: segments and flows whose
/// rules name it, campaigns (not yet finished), flows and forms whose content or settings use it.
pub async fn users(db: &PgPool, c: &Coupon) -> AppResult<Vec<String>> {
    let id_like = format!("%{}%", c.id);
    let name_like = format!("%{}%", c.name.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut out: Vec<String> = sqlx::query_scalar::<_, String>(
        "SELECT 'segment ' || to_json(name)::text FROM segments WHERE account_id = $1 AND definition::text LIKE $2
         UNION ALL SELECT 'form ' || to_json(name)::text FROM forms WHERE account_id = $1 AND lower(config->>'coupon') = lower($3)
         LIMIT 20",
    )
    .bind(c.account_id)
    .bind(&id_like)
    .bind(&c.name)
    .fetch_all(db)
    .await?;
    // Content that may name it (a cheap text match), then checked properly.
    let flows: Vec<(String, Value)> = sqlx::query_as("SELECT name, definition FROM flows WHERE account_id = $1 AND (definition::text LIKE $2 OR lower(definition::text) LIKE $3) LIMIT 200")
        .bind(c.account_id)
        .bind(&id_like)
        .bind(&name_like)
        .fetch_all(db)
        .await?;
    let me = c.name.to_lowercase();
    let uses = |channel: &str, content: &Value| super::template::message_names(channel, content).map(|n| n.contains(&me)).unwrap_or(false);
    for (name, def) in flows {
        let by_rule = def.to_string().contains(&c.id.to_string());
        let by_content = def["nodes"].as_array().into_iter().flatten().any(|n| {
            let kind = n["type"].as_str().unwrap_or("");
            matches!(kind, "email" | "sms") && uses(kind, &n["content"])
        });
        if by_rule || by_content {
            out.push(format!("flow {name:?}"));
        }
    }
    let campaigns: Vec<(String, String, Value, Option<Value>)> = sqlx::query_as(
        "SELECT name, channel, content, ab_test FROM campaigns WHERE account_id = $1 AND status IN ('draft', 'scheduled', 'sending', 'testing')
           AND (lower(content::text) LIKE $2 OR lower(COALESCE(ab_test::text, '')) LIKE $2) LIMIT 200",
    )
    .bind(c.account_id)
    .bind(&name_like)
    .fetch_all(db)
    .await?;
    for (name, channel, content, ab) in campaigns {
        let in_versions = ab.as_ref().and_then(|a| a["variants"].as_array()).into_iter().flatten().any(|v| uses(&channel, &v["content"]));
        if uses(&channel, &content) || in_versions {
            out.push(format!("campaign {name:?}"));
        }
    }
    out.truncate(20);
    Ok(out)
}

/// Delete a coupon and (in the background) its codes.
pub async fn delete(db: &PgPool, c: &Coupon) -> AppResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE coupons SET deleted_at = now(), updated_at = now(), generate_pending = 0 WHERE id = $1 AND deleted_at IS NULL").bind(c.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM coupon_holds WHERE coupon_id = $1").bind(c.id).execute(&mut *tx).await?;
    crate::jobs::enqueue(&mut *tx, "coupons_purge", json!({"coupon_id": c.id}), None).await?;
    tx.commit().await?;
    Ok(())
}

/// Job: remove some of a deleted coupon's codes in bounded batches, then hand over to the next run;
/// the coupon row goes last.
pub async fn purge(db: &PgPool, coupon_id: Uuid) -> anyhow::Result<()> {
    let deleted: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM coupons WHERE id = $1").bind(coupon_id).fetch_optional(db).await?.unwrap_or(false);
    if !deleted {
        return Ok(());
    }
    let mut removed = 0;
    while removed < PURGE_PER_RUN {
        let n = sqlx::query("DELETE FROM coupon_codes WHERE id IN (SELECT id FROM coupon_codes WHERE coupon_id = $1 LIMIT $2)")
            .bind(coupon_id)
            .bind(CHUNK as i64)
            .execute(db)
            .await?
            .rows_affected() as usize;
        if n == 0 {
            sqlx::query("DELETE FROM coupons WHERE id = $1 AND deleted_at IS NOT NULL").bind(coupon_id).execute(db).await?;
            return Ok(());
        }
        removed += n;
    }
    crate::jobs::enqueue(db, "coupons_purge", json!({"coupon_id": coupon_id}), None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pasted_and_csv_codes_line_by_line() {
        let mut r = LineReader::default();
        // Split mid-line on purpose: lines are handled once complete.
        for piece in ["Code,Value\nSAVE-1,10\n\"SAVE-2\",1", "0\n  SAVE-3  \nbad code\nstraße\nab\n\n", "LAST_1"] {
            r.feed(piece.as_bytes()).unwrap();
        }
        r.finish().unwrap();
        assert_eq!(r.ready(true).unwrap(), vec!["SAVE-1", "SAVE-2", "SAVE-3", "LAST_1"]);
        assert_eq!(r.result.refused, 3);
        let lines: Vec<i64> = r.result.refused_lines.iter().map(|l| l["line"].as_i64().unwrap()).collect();
        assert_eq!(lines, vec![5, 6, 7], "refused lines say where they are");
        let mut r = LineReader::default();
        r.feed(b"AAA1\nBBB2").unwrap();
        r.finish().unwrap();
        assert_eq!(r.ready(true).unwrap(), vec!["AAA1", "BBB2"], "no header row");
    }

    #[test]
    fn codes_are_plain_ascii() {
        assert_eq!(normalize("save-10_a"), Some("SAVE-10_A".into()));
        for bad in ["abc", "straße", "has space", "a,b", "ﬁve5", &"x".repeat(65)] {
            assert!(normalize(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn generated_codes_skip_look_alikes_and_are_long_enough() {
        let p = Pattern { prefix: "WELCOME-".into(), length: 10, alphabet: "letters_digits".into() };
        p.check().unwrap();
        let mut rng = rand::thread_rng();
        for _ in 0..200 {
            let c = p.make(&mut rng);
            assert!(c.starts_with("WELCOME-") && c.len() == 18 && valid_code(&c), "{c}");
            assert!(!c[8..].contains(['0', 'O', '1', 'I', 'L']), "{c}");
        }
        assert!(Pattern { prefix: "x y".into(), length: 8, alphabet: "letters".into() }.check().is_err());
        assert!(Pattern { prefix: "".into(), length: 4, alphabet: "digits".into() }.check().is_err(), "four digits are too easy to guess");
        assert!(Pattern { prefix: "".into(), length: 6, alphabet: "digits".into() }.check().is_err(), "at least 8 characters");
        assert!(Pattern { prefix: "AB".into(), length: 6, alphabet: "digits".into() }.check().is_ok());
        assert!(Pattern { prefix: "".into(), length: 8, alphabet: "emoji".into() }.check().is_err());
    }

    #[test]
    fn csv_cells_cant_become_formulas() {
        assert_eq!(csv_field("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(csv_field("-WELCOME"), "'-WELCOME");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("SAVE10"), "SAVE10");
    }
}

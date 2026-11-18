//! Moving reviews in and out: CSV import of existing reviews (from another reviews app), with a
//! dry run that shows how the columns map, run in the background in batches; and CSV export.
//!
//! Imported reviews keep the date, name, rating and verified flag the source gave, are marked as
//! imported in the dashboard, and are published or held as the import says. Photo addresses are
//! fetched through the same checked path as every other address a customer gives (public
//! addresses only, bounded redirects, timeouts and size caps) and re-encoded like uploads.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{photos, store};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Largest CSV taken.
pub const MAX_CSV: usize = 10 * 1024 * 1024;
pub const MAX_ROWS: usize = 20_000;
/// Rows per job run; the job queues itself again for the rest.
const BATCH: usize = 100;
/// Photos fetched per review, and the most a batch fetches.
const PHOTOS_PER_ROW: usize = 5;
const PHOTOS_PER_BATCH: usize = 50;
/// The whole of one batch's photo fetching.
const BATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
/// Imports one account may have queued or running at once.
const RUNNING_PER_ACCOUNT: i64 = 1;

/// Our fields and the column names other apps use for them.
pub const FIELDS: &[(&str, &[&str])] = &[
    ("product_id", &["product_id", "product id", "productid", "sku", "product_sku", "listing_id", "item_id"]),
    ("product_handle", &["product_handle", "handle", "product handle", "product_url"]),
    ("rating", &["rating", "stars", "score", "review_score", "review_rating"]),
    ("title", &["title", "review_title", "headline", "subject"]),
    ("body", &["body", "content", "review", "review_content", "text", "comment", "review_body"]),
    ("name", &["name", "reviewer_name", "author", "display_name", "reviewer"]),
    ("email", &["email", "reviewer_email", "author_email"]),
    ("date", &["date", "created_at", "review_date", "published_at", "submitted_at"]),
    ("verified", &["verified", "verified_buyer", "verified_purchase", "is_verified"]),
    ("photos", &["photos", "photo_urls", "pictures", "images", "picture_urls", "media"]),
    ("reply", &["reply", "response", "store_reply", "merchant_reply"]),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mapping(pub HashMap<String, String>);

fn norm(h: &str) -> String {
    h.trim().trim_start_matches('\u{feff}').to_lowercase()
}

/// Guess which column holds each field.
pub fn guess(headers: &[String]) -> Mapping {
    let mut m = HashMap::new();
    for (field, names) in FIELDS {
        if let Some(h) = headers.iter().find(|h| names.contains(&norm(h).as_str())) {
            m.insert(field.to_string(), h.clone());
        }
    }
    Mapping(m)
}

fn read(csv_text: &str) -> AppResult<(Vec<String>, Vec<csv::StringRecord>)> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(csv_text.as_bytes());
    let headers: Vec<String> = rdr.headers().map_err(|e| AppError::bad(format!("couldn't read the CSV header: {e}")))?.iter().map(String::from).collect();
    let mut rows = Vec::new();
    for r in rdr.records() {
        rows.push(r.map_err(|e| AppError::bad(format!("couldn't read the CSV: {e}")))?);
        if rows.len() > MAX_ROWS {
            return Err(AppError::bad(format!("import at most {MAX_ROWS} reviews at a time")));
        }
    }
    Ok((headers, rows))
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub product: String,
    pub by_handle: bool,
    pub rating: i16,
    pub title: String,
    pub body: String,
    pub name: String,
    pub email: Option<String>,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub verified: bool,
    pub photos: Vec<String>,
    pub reply: Option<String>,
}

fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S %Z", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Some(d.and_utc());
        }
    }
    for f in ["%Y-%m-%d", "%m/%d/%Y", "%d.%m.%Y"] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, f) {
            return d.and_hms_opt(12, 0, 0).map(|x| x.and_utc());
        }
    }
    None
}

fn truthy(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "verified" | "verified_buyer" | "verified buyer")
}

/// One CSV row as a review, or why it can't be imported.
pub fn parse_row(headers: &[String], m: &Mapping, rec: &csv::StringRecord) -> Result<Row, String> {
    let col = |f: &str| -> &str { m.0.get(f).and_then(|h| headers.iter().position(|x| x == h)).and_then(|i| rec.get(i)).unwrap_or("").trim() };
    let (product, by_handle) = match (col("product_id"), col("product_handle")) {
        (id, _) if !id.is_empty() => (id.to_string(), false),
        (_, h) if !h.is_empty() => (h.trim_end_matches('/').rsplit('/').next().unwrap_or(h).to_string(), true),
        _ => return Err("no product id or handle".into()),
    };
    if store::valid_key(&product).is_none() {
        return Err("the product id is too long".into());
    }
    let raw = col("rating");
    let rating = raw.parse::<f64>().ok().filter(|r| r.is_finite()).map(|r| r.round() as i16).filter(|r| (1..=5).contains(r)).ok_or_else(|| format!("rating {raw:?} isn't 1 to 5"))?;
    let email = Some(col("email")).filter(|e| !e.is_empty()).and_then(crate::models::normalize_email);
    let photos: Vec<String> = col("photos")
        .split([',', ' ', '\n', '|'])
        .map(str::trim)
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .take(PHOTOS_PER_ROW)
        .map(String::from)
        .collect();
    Ok(Row {
        product,
        by_handle,
        rating,
        title: store::plain(col("title"), store::MAX_TITLE).replace('\n', " "),
        body: store::plain(col("body"), store::MAX_BODY),
        name: Some(store::plain(col("name"), store::MAX_NAME).replace('\n', " ")).filter(|n| !n.is_empty()).unwrap_or_else(|| "A customer".into()),
        email,
        date: parse_date(col("date")),
        verified: truthy(col("verified")),
        photos,
        reply: Some(store::plain(col("reply"), store::MAX_REPLY)).filter(|r| !r.is_empty()),
    })
}

#[derive(Debug, Deserialize)]
pub struct ImportInput {
    pub csv: String,
    #[serde(default)]
    pub mapping: Option<HashMap<String, String>>,
    /// Publish the imported reviews (default true: they were public where they came from).
    #[serde(default)]
    pub publish: Option<bool>,
    #[serde(default)]
    pub confirm: bool,
}

/// Without `confirm`: the columns found, the mapping that would be used, the first rows as they'd
/// be imported and the rows that would be skipped. With it: queue the import.
pub async fn import(state: &AppState, account_id: Uuid, b: ImportInput) -> AppResult<Value> {
    if b.csv.len() > MAX_CSV {
        return Err(AppError::bad("the CSV can be at most 10 MB"));
    }
    let (headers, rows) = read(&b.csv)?;
    let mut mapping = guess(&headers);
    if let Some(given) = b.mapping {
        for (field, col) in given {
            if !FIELDS.iter().any(|(f, _)| *f == field) {
                return Err(AppError::bad(format!("unknown field {field:?} in the mapping")));
            }
            if col.is_empty() {
                mapping.0.remove(&field);
            } else if headers.contains(&col) {
                mapping.0.insert(field, col);
            } else {
                return Err(AppError::bad(format!("the CSV has no column {col:?}")));
            }
        }
    }
    if !mapping.0.contains_key("rating") || !(mapping.0.contains_key("product_id") || mapping.0.contains_key("product_handle")) {
        return Ok(json!({"dry_run": true, "ready": false, "headers": headers, "mapping": mapping.0, "fields": FIELDS.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            "note": "Choose which columns hold the product (id or handle) and the rating."}));
    }
    let mut ok = 0;
    let mut errors = Vec::new();
    let mut sample = Vec::new();
    for (i, rec) in rows.iter().enumerate() {
        match parse_row(&headers, &mapping, rec) {
            Ok(r) => {
                ok += 1;
                if sample.len() < 5 {
                    sample.push(json!(r));
                }
            }
            Err(e) if errors.len() < 50 => errors.push(json!({"row": i + 2, "error": e})),
            Err(_) => {}
        }
    }
    let publish = b.publish.unwrap_or(true);
    if !b.confirm {
        return Ok(json!({"dry_run": true, "ready": ok > 0, "headers": headers, "mapping": mapping.0, "fields": FIELDS.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            "rows": rows.len(), "importable": ok, "skipped": rows.len() - ok, "errors": errors, "sample": sample, "publish": publish,
            "note": format!("Nothing imported yet. {ok} review(s) would be imported{}. Call again with confirm=true once the user approves.", if publish { " and published" } else { " and held for moderation" })}));
    }
    if ok == 0 {
        return Err(AppError::bad("no rows can be imported"));
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 82))").bind(format!("rv-import:{account_id}")).execute(&mut *tx).await?;
    let running: i64 = sqlx::query_scalar("SELECT count(*) FROM review_imports WHERE account_id = $1 AND status IN ('queued', 'running')").bind(account_id).fetch_one(&mut *tx).await?;
    if running >= RUNNING_PER_ACCOUNT {
        return Err(AppError::Busy("An import is already running. Wait for it to finish.".into()));
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO review_imports (id, account_id, mapping, publish, csv, total) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(account_id)
        .bind(json!(mapping.0))
        .bind(publish)
        .bind(&b.csv)
        .bind(rows.len() as i32)
        .execute(&mut *tx)
        .await?;
    crate::jobs::enqueue_background(&mut *tx, "reviews_import", json!({"import_id": id}), None).await?;
    tx.commit().await?;
    get_import(state, account_id, id).await
}

pub async fn get_import(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let row: Option<(Uuid, String, i32, i32, i32, i32, Value, bool, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, status, total, next_row, imported, skipped, errors, publish, created_at, finished_at FROM review_imports WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let (id, status, total, done, imported, skipped, errors, publish, created_at, finished_at) = row.ok_or_else(|| AppError::not_found("import"))?;
    Ok(json!({"id": id, "status": status, "total": total, "processed": done, "imported": imported, "skipped": skipped, "errors": errors, "publish": publish, "created_at": created_at, "finished_at": finished_at}))
}

/// The catalog product a handle names (the last part of its page address).
async fn by_handle(state: &AppState, account_id: Uuid, handle: &str) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT external_id FROM catalog_products WHERE account_id = $1 AND rtrim(split_part(url, '?', 1), '/') LIKE '%/' || $2 LIMIT 1")
        .bind(account_id)
        .bind(handle.replace('%', "").replace('_', "\\_"))
        .fetch_optional(&state.db)
        .await?)
}

/// Job: import the next batch of rows. Each row is keyed by (import, row number), so a batch run
/// again after a crash skips what it already saved.
pub async fn run_import(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let err = |e: AppError| anyhow::anyhow!(e.public_message());
    let Some(id) = payload["import_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid, String, Value, bool, String, i32)> =
        sqlx::query_as("SELECT account_id, status, mapping, publish, csv, next_row FROM review_imports WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
    let Some((account_id, status, mapping, publish, csv_text, next)) = row else { return Ok(()) };
    if status == "done" || status == "failed" {
        return Ok(());
    }
    sqlx::query("UPDATE review_imports SET status = 'running' WHERE id = $1").bind(id).execute(&state.db).await?;
    let mapping: Mapping = Mapping(serde_json::from_value(mapping).unwrap_or_default());
    let (headers, rows) = match read(&csv_text) {
        Ok(x) => x,
        Err(e) => {
            sqlx::query("UPDATE review_imports SET status = 'failed', errors = $2, finished_at = now() WHERE id = $1").bind(id).bind(json!([{"error": e.public_message()}])).execute(&state.db).await?;
            return Ok(());
        }
    };
    let start = next.max(0) as usize;
    let end = (start + BATCH).min(rows.len());
    let deadline = tokio::time::Instant::now() + BATCH_DEADLINE;
    let mut fetched = 0usize;
    let (mut imported, mut skipped) = (0i32, 0i32);
    let mut errors: Vec<Value> = Vec::new();
    for (i, rec) in rows.iter().enumerate().take(end).skip(start) {
        let line = i as i32 + 2;
        let r = match parse_row(&headers, &mapping, rec) {
            Ok(r) => r,
            Err(e) => {
                skipped += 1;
                errors.push(json!({"row": line, "error": e}));
                continue;
            }
        };
        let key = if r.by_handle { by_handle(state, account_id, &r.product).await.map_err(err)?.unwrap_or(r.product.clone()) } else { r.product.clone() };
        let subject = match store::subject_for(&state.db, account_id, &key, &store::Hint { trusted: true, ..Default::default() }).await {
            Ok(s) => s,
            Err(e) => {
                skipped += 1;
                errors.push(json!({"row": line, "error": e.public_message()}));
                continue;
            }
        };
        let profile_id: Option<Uuid> = match &r.email {
            Some(e) => sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND email = $2").bind(account_id).bind(e).fetch_optional(&state.db).await?,
            None => None,
        };
        let s = store::settings(&state.db, account_id).await.map_err(err)?;
        let flags = store::flags(&format!("{}\n{}\n{}", r.title, r.body, r.name), &s.flag_words);
        let rid = Uuid::now_v7();
        let mut tx = state.db.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO reviews (id, account_id, subject_id, profile_id, status, rating, title, body, display_name, verified, source, flags, reply, replied_at,
                confirmed_at, reviewed_at, import_id, import_row)
             VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9, 'import', $10, $11, CASE WHEN $11::text IS NULL THEN NULL ELSE now() END, now(), COALESCE($12, now()), $13, $14)
             ON CONFLICT DO NOTHING",
        )
        .bind(rid)
        .bind(account_id)
        .bind(subject.id)
        .bind(profile_id)
        .bind(r.rating)
        .bind(&r.title)
        .bind(&r.body)
        .bind(&r.name)
        .bind(r.verified)
        .bind(&flags)
        .bind(&r.reply)
        .bind(r.date)
        .bind(id)
        .bind(line)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if inserted && publish {
            store::set_status(&mut tx, account_id, &[rid], "published", None).await.map_err(err)?;
        }
        tx.commit().await?;
        if !inserted {
            continue;
        }
        imported += 1;
        for (pos, url) in r.photos.iter().enumerate() {
            if fetched >= PHOTOS_PER_BATCH || tokio::time::Instant::now() >= deadline {
                errors.push(json!({"row": line, "error": "photo skipped: this batch's photo time ran out"}));
                break;
            }
            fetched += 1;
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let got = tokio::time::timeout(left, crate::flows::fetch_public(url, photos::MAX_UPLOAD + 1)).await;
            let bytes = match got {
                Ok(Ok(f)) if f.body.len() <= photos::MAX_UPLOAD => f.body,
                Ok(Ok(_)) => {
                    errors.push(json!({"row": line, "error": "photo skipped: bigger than 10 MB"}));
                    continue;
                }
                Ok(Err(e)) => {
                    errors.push(json!({"row": line, "error": format!("photo skipped: {}", e.public_message())}));
                    continue;
                }
                Err(_) => {
                    errors.push(json!({"row": line, "error": "photo skipped: took too long"}));
                    continue;
                }
            };
            match photos::reencode(bytes).await {
                Ok(enc) => {
                    sqlx::query("INSERT INTO review_photos (id, account_id, review_id, position, width, height, size, bytes) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                        .bind(Uuid::new_v4())
                        .bind(account_id)
                        .bind(rid)
                        .bind(pos as i16)
                        .bind(enc.width as i32)
                        .bind(enc.height as i32)
                        .bind(enc.bytes.len() as i32)
                        .bind(&enc.bytes)
                        .execute(&state.db)
                        .await?;
                }
                Err(e) => errors.push(json!({"row": line, "error": format!("photo skipped: {}", e.public_message())})),
            }
        }
    }
    let finished = end >= rows.len();
    sqlx::query(
        "UPDATE review_imports SET next_row = $2, imported = imported + $3, skipped = skipped + $4,
            errors = (SELECT COALESCE(jsonb_agg(e), '[]') FROM (SELECT e FROM jsonb_array_elements(errors || $5::jsonb) e LIMIT 200) x),
            status = CASE WHEN $6 THEN 'done' ELSE 'running' END, finished_at = CASE WHEN $6 THEN now() END WHERE id = $1",
    )
    .bind(id)
    .bind(end as i32)
    .bind(imported)
    .bind(skipped)
    .bind(json!(errors))
    .bind(finished)
    .execute(&state.db)
    .await?;
    if !finished {
        crate::jobs::enqueue_background(&state.db, "reviews_import", payload.clone(), None).await?;
    } else {
        // The CSV isn't needed once it's in.
        sqlx::query("UPDATE review_imports SET csv = '' WHERE id = $1").bind(id).execute(&state.db).await?;
    }
    Ok(())
}

fn cell(s: &str) -> String {
    // Cells a spreadsheet would read as a formula start with a quote instead.
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_string()
    }
}

/// Every review matching the filter as CSV (at most 50,000 rows). `with_people` adds the
/// reviewer's email (only for callers who may read profiles).
pub async fn export(state: &AppState, account_id: Uuid, tz: &str, q: &store::ListQuery, with_people: bool) -> AppResult<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    let mut head = vec!["review_id", "product_id", "product_name", "status", "rating", "title", "body", "name", "date", "verified", "source", "incentivized", "recommend", "reply", "photos", "flags"];
    if with_people {
        head.push("email");
    }
    w.write_record(&head).map_err(|e| AppError::Internal(e.into()))?;
    let mut offset = 0i64;
    loop {
        let page = store::ListQuery { limit: Some(200), offset: Some(offset), status: q.status.clone(), rating: q.rating, product_id: q.product_id.clone(), verified: q.verified, has_photos: q.has_photos, source: q.source.clone(), unconfirmed: q.unconfirmed, since: q.since.clone(), until: q.until.clone(), q: q.q.clone() };
        let (rows, _) = store::list(&state.db, account_id, tz, &page).await?;
        if rows.is_empty() {
            break;
        }
        let pids: Vec<Uuid> = rows.iter().filter_map(|(r, _, _)| r.profile_id).collect();
        let emails: HashMap<Uuid, String> = if with_people {
            sqlx::query_as::<_, (Uuid, Option<String>)>("SELECT id, email FROM profiles WHERE id = ANY($1)").bind(&pids).fetch_all(&state.db).await?.into_iter().filter_map(|(i, e)| e.map(|e| (i, e))).collect()
        } else {
            HashMap::new()
        };
        for (r, s, photos) in &rows {
            let mut rec = vec![
                r.id.to_string(),
                cell(&s.external_id),
                cell(&s.name),
                r.status.clone(),
                r.rating.to_string(),
                cell(&r.title),
                cell(&r.body),
                cell(&r.display_name),
                r.reviewed_at.to_rfc3339(),
                r.verified.to_string(),
                r.source.clone(),
                r.incentivized.to_string(),
                r.recommend.map(|b| b.to_string()).unwrap_or_default(),
                cell(r.reply.as_deref().unwrap_or("")),
                photos.to_string(),
                r.flags.join(" "),
            ];
            if with_people {
                rec.push(r.profile_id.and_then(|p| emails.get(&p).cloned()).unwrap_or_default());
            }
            w.write_record(&rec).map_err(|e| AppError::Internal(e.into()))?;
        }
        offset += rows.len() as i64;
        if offset >= 50_000 {
            break;
        }
    }
    String::from_utf8(w.into_inner().map_err(|e| AppError::Internal(anyhow::anyhow!("csv: {e}")))?).map_err(|e| AppError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_columns_and_parses_rows() {
        let text = "product_handle,rating,review_title,review_content,reviewer_name,review_date,verified_purchase,picture_urls\n\
                    https://shop.test/products/blue-mug,5,Great,Love it,Jane D.,2025-03-01,yes,https://img.test/a.jpg https://img.test/b.jpg\n\
                    blue-mug,9,Bad,,x,,no,\n";
        let (h, rows) = read(text).unwrap();
        let m = guess(&h);
        assert_eq!(m.0.get("product_handle").map(String::as_str), Some("product_handle"));
        assert_eq!(m.0.get("body").map(String::as_str), Some("review_content"));
        let r = parse_row(&h, &m, &rows[0]).unwrap();
        assert_eq!((r.product.as_str(), r.by_handle, r.rating, r.verified, r.photos.len()), ("blue-mug", true, 5, true, 2));
        assert!(r.date.is_some());
        assert!(parse_row(&h, &m, &rows[1]).unwrap_err().contains("rating"));
        assert_eq!(cell("=HYPERLINK(1)"), "'=HYPERLINK(1)");
    }
}

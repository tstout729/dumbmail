//! Object types: an account's own kinds of record (reservation, subscription ...) and their typed
//! fields. Field values are checked and put in one form here, before anything is written.

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// Most object types one account may have.
pub const MAX_TYPES: i64 = 50;
/// Most fields one type may have.
pub const MAX_FIELDS: usize = 100;
/// Longest text value.
pub const MAX_TEXT: usize = 2000;
/// Most items in a list field, and the longest item.
pub const MAX_LIST_ITEMS: usize = 100;
pub const MAX_LIST_ITEM: usize = 200;
/// Most options an enum field may have.
pub const MAX_OPTIONS: usize = 100;
/// Largest JSON for one object's fields.
pub const MAX_FIELDS_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Text,
    Number,
    Boolean,
    Date,
    Datetime,
    Enum,
    /// A list of text values (tags, amenities).
    List,
    /// An amount in the field's `currency` (major units: 120.5 is $120.50).
    Money,
}

impl Kind {
    pub fn is_date(self) -> bool {
        matches!(self, Kind::Date | Kind::Datetime)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(rename = "type")]
    pub kind: Kind,
    /// Enum fields: the allowed values (empty: any short text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Money fields: an ISO currency code such as USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Holds something private (a door code): shown only to people who can read profiles, and
    /// never in lists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sensitive: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ObjectType {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: Uuid,
    pub key: String,
    pub name: String,
    pub plural: String,
    #[sqlx(json)]
    pub fields: Vec<FieldDef>,
    pub title_field: Option<String>,
    /// `integer` or `timestamp`: how this type's writes say how new they are (`as_of`).
    pub version_kind: Option<String>,
    /// Sensitive fields taken off the type: a field with one of these names is sensitive again.
    #[serde(skip)]
    pub removed_sensitive: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const TYPE_COLS: &str = "id, account_id, key, name, plural, fields, title_field, version_kind, removed_sensitive, created_at, updated_at";

impl ObjectType {
    pub fn field(&self, key: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// Whether `key` is sensitive: marked so, or the name of a sensitive field taken off the type.
    pub fn is_sensitive(&self, key: &str) -> bool {
        self.field(key).is_some_and(|f| f.sensitive) || self.removed_sensitive.iter().any(|k| k == key)
    }

    pub fn sensitive(&self) -> Vec<&str> {
        self.fields.iter().filter(|f| f.sensitive).map(|f| f.key.as_str()).collect()
    }

    pub fn date_fields(&self) -> Vec<&str> {
        self.fields.iter().filter(|f| f.kind.is_date()).map(|f| f.key.as_str()).collect()
    }
}

/// The fields rules may test: those the type defines and doesn't mark sensitive. Trigger filters,
/// exit checks and condition splits only ever see these, so a rule can't be used to learn a
/// sensitive value.
pub fn rule_fields(t: &ObjectType, fields: &Map<String, Value>) -> Map<String, Value> {
    fields.iter().filter(|(k, _)| t.field(k).is_some_and(|f| !f.sensitive)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Rules (segment conditions, trigger filters and dates) may test only fields the type defines and
/// doesn't mark sensitive; anything else, or a type that doesn't exist, is refused.
pub async fn check_rule_fields(db: &sqlx::PgPool, account_id: Uuid, key: &str, fields: &[&str]) -> AppResult<()> {
    let t = require(db, account_id, key).await?;
    for f in fields {
        match t.field(f.trim()) {
            Some(d) if !d.sensitive => {}
            Some(_) => return Err(AppError::bad(format!("{f:?} is a sensitive field of {}; rules can't test it", t.key))),
            None => return Err(AppError::bad(format!("{} has no field {f:?}", t.key))),
        }
    }
    Ok(())
}

pub fn valid_type_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty() && b.len() <= 40 && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

pub fn valid_field_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphabetic() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn bad_type_key(k: &str) -> AppError {
    AppError::bad(format!("object type {k:?}: a type key is 1-40 lower-case letters, digits and _ starting with a letter (e.g. reservation)"))
}

/// Check a list of field definitions (for a new type or an edit).
pub fn check_fields(fields: &[FieldDef], title_field: Option<&str>) -> AppResult<()> {
    if fields.len() > MAX_FIELDS {
        return Err(AppError::bad(format!("a type can have at most {MAX_FIELDS} fields")));
    }
    let mut seen = std::collections::HashSet::new();
    for f in fields {
        if !valid_field_key(&f.key) {
            return Err(AppError::bad(format!("field {:?}: a field key is 1-64 letters, digits and _ starting with a letter", f.key)));
        }
        if !seen.insert(f.key.as_str()) {
            return Err(AppError::bad(format!("field {:?} is listed twice", f.key)));
        }
        if f.label.as_deref().is_some_and(|l| l.chars().count() > 100) {
            return Err(AppError::bad(format!("field {:?}: the label is over 100 characters", f.key)));
        }
        if f.options.len() > MAX_OPTIONS || f.options.iter().any(|o| o.trim().is_empty() || o.chars().count() > 100) {
            return Err(AppError::bad(format!("field {:?}: at most {MAX_OPTIONS} options, each 1-100 characters", f.key)));
        }
        if !f.options.is_empty() && f.kind != Kind::Enum {
            return Err(AppError::bad(format!("field {:?}: only enum fields take options", f.key)));
        }
        match (&f.currency, f.kind) {
            (Some(c), Kind::Money) if c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()) => {}
            (Some(_), Kind::Money) => return Err(AppError::bad(format!("field {:?}: currency is a 3-letter code such as USD", f.key))),
            (Some(_), _) => return Err(AppError::bad(format!("field {:?}: only money fields take a currency", f.key))),
            (None, _) => {}
        }
    }
    if let Some(t) = title_field {
        match fields.iter().find(|f| f.key == t) {
            Some(f) if matches!(f.kind, Kind::Text | Kind::Enum | Kind::Number | Kind::Date | Kind::Datetime) && !f.sensitive => {}
            Some(_) => return Err(AppError::bad(format!("title field {t:?} must be a text, enum, number or date field that isn't sensitive"))),
            None => return Err(AppError::bad(format!("title field {t:?} isn't one of the type's fields"))),
        }
    }
    Ok(())
}

fn date_only(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn datetime(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.with_timezone(&Utc))
}

/// The field type a value suggests, for a field nobody defined yet.
pub fn infer(v: &Value) -> Result<Kind, String> {
    Ok(match v {
        Value::Bool(_) => Kind::Boolean,
        Value::Number(_) => Kind::Number,
        Value::String(s) if date_only(s).is_some() => Kind::Date,
        Value::String(s) if datetime(s).is_some() => Kind::Datetime,
        Value::String(_) => Kind::Text,
        Value::Array(a) if a.iter().all(|x| x.is_string() || x.is_number()) => Kind::List,
        _ => return Err("fields hold plain values (text, numbers, true/false, dates) or lists of text, not objects".into()),
    })
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok())).filter(|n| n.is_finite())
}

/// One value in the form it is stored in, or why it doesn't fit the field.
pub fn normalize(def: &FieldDef, v: &Value) -> Result<Value, String> {
    let k = &def.key;
    Ok(match def.kind {
        Kind::Text => {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err(format!("{k} is a text field")),
            };
            if s.chars().count() > MAX_TEXT {
                return Err(format!("{k} is longer than {MAX_TEXT} characters"));
            }
            json!(s)
        }
        Kind::Number | Kind::Money => {
            let n = num(v).ok_or_else(|| format!("{k} must be a number"))?;
            // Whole numbers stay whole (3, not 3.0).
            if n.fract() == 0.0 && n.abs() < 9.0e15 { json!(n as i64) } else { json!(n) }
        }
        Kind::Boolean => match v {
            Value::Bool(b) => json!(b),
            Value::String(s) if s.eq_ignore_ascii_case("true") => json!(true),
            Value::String(s) if s.eq_ignore_ascii_case("false") => json!(false),
            _ => return Err(format!("{k} must be true or false")),
        },
        Kind::Date => {
            let d = v.as_str().and_then(date_only).ok_or_else(|| format!("{k} must be a date like 2026-11-20"))?;
            json!(d.format("%Y-%m-%d").to_string())
        }
        Kind::Datetime => {
            let d = v.as_str().and_then(datetime).ok_or_else(|| format!("{k} must be a date and time with its offset, like 2026-11-20T16:00:00Z"))?;
            json!(d.to_rfc3339_opts(SecondsFormat::Secs, true))
        }
        Kind::Enum => {
            let s = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err(format!("{k} must be one of its options")),
            };
            if def.options.is_empty() {
                if s.is_empty() || s.chars().count() > 100 {
                    return Err(format!("{k} must be 1-100 characters"));
                }
                json!(s)
            } else {
                let o = def.options.iter().find(|o| o.eq_ignore_ascii_case(&s)).ok_or_else(|| format!("{k} must be one of {}", def.options.join(", ")))?;
                json!(o)
            }
        }
        Kind::List => {
            let items = v.as_array().ok_or_else(|| format!("{k} must be a list"))?;
            if items.len() > MAX_LIST_ITEMS {
                return Err(format!("{k} can hold at most {MAX_LIST_ITEMS} items"));
            }
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                let s = match x {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return Err(format!("{k} is a list of text")),
                };
                if s.chars().count() > MAX_LIST_ITEM {
                    return Err(format!("{k}: each item is at most {MAX_LIST_ITEM} characters"));
                }
                out.push(json!(s));
            }
            Value::Array(out)
        }
    })
}

/// The error people see for a value that doesn't fit `def`: a sensitive field's says what kind of
/// value it takes without listing its options or echoing anything.
pub fn public_error(def: &FieldDef, e: String) -> String {
    if def.sensitive {
        format!("{} doesn't take that value (it's a {} field)", def.key, match def.kind {
            Kind::Enum => "choice",
            Kind::Date | Kind::Datetime => "date",
            Kind::Number | Kind::Money => "number",
            Kind::Boolean => "yes/no",
            Kind::List => "list",
            Kind::Text => "text",
        })
    } else {
        e
    }
}

/// A field for a key nobody defined, typed from its first value.
pub fn inferred(key: &str, v: &Value) -> Result<FieldDef, String> {
    if !valid_field_key(key) {
        return Err(format!("field {key:?}: a field key is 1-64 letters, digits and _ starting with a letter"));
    }
    Ok(FieldDef { key: key.to_string(), label: None, kind: infer(v).map_err(|e| format!("{key}: {e}"))?, options: Vec::new(), currency: None, sensitive: false })
}

/// Names for a type made on its first upsert: `reservation` → Reservation / Reservations.
pub fn default_names(key: &str) -> (String, String) {
    let words = key.replace('_', " ");
    let mut c = words.chars();
    let name = match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => key.to_string(),
    };
    let plural = if name.ends_with('s') { format!("{name}es") } else if name.ends_with('y') && !name.ends_with("ay") && !name.ends_with("ey") { format!("{}ies", &name[..name.len() - 1]) } else { format!("{name}s") };
    (name, plural)
}

pub async fn list(db: &sqlx::PgPool, account_id: Uuid) -> AppResult<Vec<(ObjectType, i64)>> {
    let q = format!("SELECT {TYPE_COLS}, (SELECT count(*) FROM objects o WHERE o.type_id = t.id) AS n FROM object_types t WHERE account_id = $1 ORDER BY name, key");
    let rows = sqlx::query(&q).bind(account_id).fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        use sqlx::{FromRow, Row};
        let n: i64 = r.try_get("n")?;
        out.push((ObjectType::from_row(&r)?, n));
    }
    Ok(out)
}

pub async fn by_key<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, key: &str) -> AppResult<Option<ObjectType>> {
    let q = format!("SELECT {TYPE_COLS} FROM object_types WHERE account_id = $1 AND key = $2");
    Ok(sqlx::query_as(&q).bind(account_id).bind(key.trim()).fetch_optional(db).await?)
}

pub async fn by_id<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, id: Uuid) -> AppResult<Option<ObjectType>> {
    let q = format!("SELECT {TYPE_COLS} FROM object_types WHERE account_id = $1 AND id = $2");
    Ok(sqlx::query_as(&q).bind(account_id).bind(id).fetch_optional(db).await?)
}

pub async fn require(db: &sqlx::PgPool, account_id: Uuid, key: &str) -> AppResult<ObjectType> {
    by_key(db, account_id, key).await?.ok_or_else(|| AppError::NotFound(format!("there's no object type {key:?}; list_object_types shows them")))
}

/// What `upsert_object_type` takes.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TypeInput {
    pub key: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub plural: Option<String>,
    /// Replaces the type's field list when given.
    #[serde(default)]
    pub fields: Option<Vec<FieldDef>>,
    /// The field shown as each object's title ("" to clear).
    #[serde(default)]
    pub title_field: Option<String>,
}

fn check_name(what: &str, s: &str) -> AppResult<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 100 {
        return Err(AppError::bad(format!("{what} must be 1-100 characters")));
    }
    Ok(s.to_string())
}

/// Create a type, or change one (names, field list, title field). The type count is checked under
/// the account's lock, in the transaction that adds the type, so two requests can't both pass it.
pub async fn upsert(db: &sqlx::PgPool, account_id: Uuid, b: TypeInput) -> AppResult<(ObjectType, bool)> {
    let key = b.key.trim().to_string();
    if !valid_type_key(&key) {
        return Err(bad_type_key(&key));
    }
    let mut tx = db.begin().await?;
    lock_types(&mut tx, account_id).await?;
    let q = format!("SELECT {TYPE_COLS} FROM object_types WHERE account_id = $1 AND key = $2 FOR UPDATE");
    let cur: Option<ObjectType> = sqlx::query_as(&q).bind(account_id).bind(&key).fetch_optional(&mut *tx).await?;
    let (dname, dplural) = default_names(&key);
    let name = match &b.name {
        Some(n) => check_name("name", n)?,
        None => cur.as_ref().map(|c| c.name.clone()).unwrap_or(dname),
    };
    let plural = match &b.plural {
        Some(n) => check_name("plural", n)?,
        None => cur.as_ref().map(|c| c.plural.clone()).unwrap_or(dplural),
    };
    let fields = b.fields.clone().or_else(|| cur.as_ref().map(|c| c.fields.clone())).unwrap_or_default();
    let title = match &b.title_field {
        Some(t) if t.trim().is_empty() => None,
        Some(t) => Some(t.trim().to_string()),
        None => cur.as_ref().and_then(|c| c.title_field.clone()).filter(|t| fields.iter().any(|f| &f.key == t)),
    };
    check_fields(&fields, title.as_deref())?;
    let created = cur.is_none();
    let row: ObjectType = match cur {
        Some(c) => {
            let q = format!("UPDATE object_types SET name = $2, plural = $3, fields = $4, title_field = $5, updated_at = now() WHERE id = $1 RETURNING {TYPE_COLS}");
            sqlx::query_as(&q).bind(c.id).bind(&name).bind(&plural).bind(json!(fields)).bind(&title).fetch_one(&mut *tx).await?
        }
        None => insert(&mut tx, account_id, &key, &name, &plural, &fields, title.as_deref()).await?,
    };
    tx.commit().await?;
    Ok((row, created))
}

/// Create a type unless the account already has one with this key (then it's left as it is).
/// Recipes use it for the types their flows need.
pub async fn create_if_missing(db: &sqlx::PgPool, account_id: Uuid, b: TypeInput) -> AppResult<ObjectType> {
    let key = b.key.trim().to_string();
    if !valid_type_key(&key) {
        return Err(bad_type_key(&key));
    }
    let fields = b.fields.clone().unwrap_or_default();
    let title = b.title_field.clone().filter(|t| !t.trim().is_empty());
    check_fields(&fields, title.as_deref())?;
    let mut tx = db.begin().await?;
    lock_types(&mut tx, account_id).await?;
    if let Some(t) = by_key(&mut *tx, account_id, &key).await? {
        return Ok(t);
    }
    let (dname, dplural) = default_names(&key);
    let name = match &b.name {
        Some(n) => check_name("name", n)?,
        None => dname,
    };
    let plural = match &b.plural {
        Some(n) => check_name("plural", n)?,
        None => dplural,
    };
    let t = insert(&mut tx, account_id, &key, &name, &plural, &fields, title.as_deref()).await?;
    tx.commit().await?;
    Ok(t)
}

/// Serialize type creation per account (the type limit is counted under it).
async fn lock_types(conn: &mut PgConnection, account_id: Uuid) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('object_types:' || $1::text, 0))").bind(account_id).execute(conn).await?;
    Ok(())
}

async fn insert(conn: &mut PgConnection, account_id: Uuid, key: &str, name: &str, plural: &str, fields: &[FieldDef], title: Option<&str>) -> AppResult<ObjectType> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM object_types WHERE account_id = $1").bind(account_id).fetch_one(&mut *conn).await?;
    if n >= MAX_TYPES {
        return Err(AppError::bad(format!("an account can have at most {MAX_TYPES} object types")));
    }
    let q = format!("INSERT INTO object_types (id, account_id, key, name, plural, fields, title_field) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {TYPE_COLS}");
    Ok(sqlx::query_as(&q).bind(Uuid::now_v7()).bind(account_id).bind(key).bind(name).bind(plural).bind(json!(fields)).bind(title).fetch_one(conn).await?)
}

/// The type for an upsert, created on first use from the values it brings, with new fields added
/// for keys nobody defined yet (typed from their values). Locks the type's row only when it has to
/// add fields.
pub async fn for_upsert(conn: &mut PgConnection, account_id: Uuid, key: &str, values: &Map<String, Value>) -> AppResult<ObjectType> {
    let key = key.trim();
    if !valid_type_key(key) {
        return Err(bad_type_key(key));
    }
    let existing = by_key(&mut *conn, account_id, key).await?;
    let needs = |t: &ObjectType| values.iter().any(|(k, v)| !v.is_null() && t.field(k).is_none());
    match existing {
        Some(t) if !needs(&t) => Ok(t),
        Some(_) => {
            let q = format!("SELECT {TYPE_COLS} FROM object_types WHERE account_id = $1 AND key = $2 FOR UPDATE");
            let mut t: ObjectType = sqlx::query_as(&q).bind(account_id).bind(key).fetch_one(&mut *conn).await?;
            add_inferred(&mut t.fields, values)?;
            let q = format!("UPDATE object_types SET fields = $2, updated_at = now() WHERE id = $1 RETURNING {TYPE_COLS}");
            Ok(sqlx::query_as(&q).bind(t.id).bind(json!(t.fields)).fetch_one(&mut *conn).await?)
        }
        None => {
            lock_types(&mut *conn, account_id).await?;
            // Someone may have made it while we waited for the lock.
            if by_key(&mut *conn, account_id, key).await?.is_some() {
                return Box::pin(for_upsert(conn, account_id, key, values)).await;
            }
            let mut fields = Vec::new();
            add_inferred(&mut fields, values)?;
            let (name, plural) = default_names(key);
            // A text field called name or title becomes the title.
            let title = ["name", "title"].iter().find(|k| fields.iter().any(|f| f.key == **k && f.kind == Kind::Text)).map(|k| k.to_string());
            insert(conn, account_id, key, &name, &plural, &fields, title.as_deref()).await
        }
    }
}

fn add_inferred(fields: &mut Vec<FieldDef>, values: &Map<String, Value>) -> AppResult<()> {
    let mut keys: Vec<&String> = values.keys().collect();
    keys.sort();
    for k in keys {
        let v = &values[k];
        if v.is_null() || fields.iter().any(|f| &f.key == k) {
            continue;
        }
        if fields.len() >= MAX_FIELDS {
            return Err(AppError::bad(format!("a type can have at most {MAX_FIELDS} fields; {k:?} would be one more")));
        }
        fields.push(inferred(k, v).map_err(AppError::bad)?);
    }
    Ok(())
}

/// Flows and segments that use a type (deleting it would break them).
pub async fn users(db: &sqlx::PgPool, account_id: Uuid, key: &str) -> AppResult<Vec<String>> {
    let flows: Vec<String> = sqlx::query_scalar("SELECT name FROM flows WHERE account_id = $1 AND trigger->>'type' IN ('object', 'object_date') AND trigger->>'object_type' = $2 ORDER BY name LIMIT 20")
        .bind(account_id)
        .bind(key)
        .fetch_all(db)
        .await?;
    let segments: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM segments WHERE account_id = $1 AND jsonb_path_exists(definition, '$.** ? (@.type == \"object\" && @.object_type == $k)', jsonb_build_object('k', $2::text)) ORDER BY name LIMIT 20",
    )
    .bind(account_id)
    .bind(key)
    .fetch_all(db)
    .await?;
    Ok(flows.into_iter().map(|n| format!("flow {n:?}")).chain(segments.into_iter().map(|n| format!("segment {n:?}"))).collect())
}

pub async fn delete(db: &sqlx::PgPool, t: &ObjectType) -> AppResult<()> {
    sqlx::query("DELETE FROM object_types WHERE id = $1 AND account_id = $2").bind(t.id).bind(t.account_id).execute(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(kind: Kind) -> FieldDef {
        FieldDef { key: "f".into(), label: None, kind, options: Vec::new(), currency: None, sensitive: false }
    }

    #[test]
    fn values_are_checked_and_normalized() {
        assert_eq!(normalize(&def(Kind::Date), &json!("2026-11-20")).unwrap(), json!("2026-11-20"));
        assert!(normalize(&def(Kind::Date), &json!("11/20/2026")).is_err());
        assert!(normalize(&def(Kind::Date), &json!("2026-11-20T10:00:00Z")).is_err());
        assert_eq!(normalize(&def(Kind::Datetime), &json!("2026-11-20T10:00:00-08:00")).unwrap(), json!("2026-11-20T18:00:00Z"));
        assert!(normalize(&def(Kind::Datetime), &json!("2026-11-20T10:00:00")).is_err());
        assert_eq!(normalize(&def(Kind::Number), &json!("12.5")).unwrap(), json!(12.5));
        assert_eq!(normalize(&def(Kind::Number), &json!(3.0)).unwrap(), json!(3));
        assert!(normalize(&def(Kind::Number), &json!("lots")).is_err());
        assert_eq!(normalize(&def(Kind::Boolean), &json!("TRUE")).unwrap(), json!(true));
        assert_eq!(normalize(&def(Kind::List), &json!(["a", 2])).unwrap(), json!(["a", "2"]));
        assert!(normalize(&def(Kind::List), &json!([{"x": 1}])).is_err());
        let mut e = def(Kind::Enum);
        e.options = vec!["confirmed".into(), "cancelled".into()];
        assert_eq!(normalize(&e, &json!("Cancelled")).unwrap(), json!("cancelled"));
        assert!(normalize(&e, &json!("maybe")).is_err());
        assert!(normalize(&def(Kind::Text), &json!("x".repeat(MAX_TEXT + 1))).is_err());
    }

    #[test]
    fn types_are_inferred_from_values() {
        assert_eq!(infer(&json!("2026-11-20")).unwrap(), Kind::Date);
        assert_eq!(infer(&json!("2026-11-20T16:00:00Z")).unwrap(), Kind::Datetime);
        assert_eq!(infer(&json!(3)).unwrap(), Kind::Number);
        assert_eq!(infer(&json!(false)).unwrap(), Kind::Boolean);
        assert_eq!(infer(&json!(["wifi"])).unwrap(), Kind::List);
        assert_eq!(infer(&json!("Ocean view")).unwrap(), Kind::Text);
        assert!(infer(&json!({"nested": 1})).is_err());
    }

    #[test]
    fn keys_and_definitions_are_checked() {
        assert!(valid_type_key("reservation"));
        assert!(!valid_type_key("Reservation"));
        assert!(!valid_type_key("1x"));
        assert!(valid_field_key("checkIn"));
        assert!(!valid_field_key("check-in"));
        let f = |k: &str, kind| FieldDef { key: k.into(), label: None, kind, options: Vec::new(), currency: None, sensitive: false };
        assert!(check_fields(&[f("a", Kind::Text), f("a", Kind::Date)], None).is_err());
        assert!(check_fields(&[f("a", Kind::Text)], Some("b")).is_err());
        let mut secret = f("code", Kind::Text);
        secret.sensitive = true;
        assert!(check_fields(&[secret], Some("code")).is_err(), "a sensitive field can't be the title");
        let mut m = f("total", Kind::Money);
        m.currency = Some("usd".into());
        assert!(check_fields(&[m], None).is_err());
        assert_eq!(default_names("reservation"), ("Reservation".into(), "Reservations".into()));
        assert_eq!(default_names("pet_policy"), ("Pet policy".into(), "Pet policies".into()));
    }
}

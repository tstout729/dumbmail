//! Klaviyo profiles → DumbMail profiles, a page at a time.
//!
//! One query finds the DumbMail profile for every Klaviyo profile on the page (by the saved Klaviyo
//! reference, then external id, email, phone). One statement merges all the matched ones and one `INSERT`
//! adds the new ones; only clashes fall back to a row at a time. The merge statement locks each profile
//! before it reads it (`FOR UPDATE`), so it decides on the row as it is at write time: a STOP or
//! unsubscribe that lands while the import runs is never overwritten. The same statement records what
//! consent the import itself set (`klaviyo_consent_writes`), so a later run can tell Klaviyo's opt-in
//! from the person's own DumbMail one. Then Klaviyo's opt-outs and suppressions are applied by address
//! and number, to whichever profile here has them. See `consent.rs` for the rule.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::consent::{self, EmailIn, SmsIn};
use super::runner::Run;
use super::store;

const MAX_PROFILE_PROPS: usize = 64 * 1024;

/// One Klaviyo profile, read and mapped, ready to write.
#[derive(Debug, Clone)]
pub struct KProfile {
    pub kid: String,
    pub label: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub ext: Option<String>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub props: Value,
    pub created: Option<DateTime<Utc>>,
    pub e_in: EmailIn,
    pub s_in: SmsIn,
    /// Klaviyo's own view, kept on the reference for the report.
    pub detail: Value,
    /// This run's entry for the profile's `klaviyo_consent_history`.
    pub hist: Value,
    /// Fingerprint of identifiers and consent: unchanged profiles are skipped on list pages.
    pub fp: String,
    /// Problems to record (invalid email, oversized properties).
    pub notes: Vec<String>,
    /// Klaviyo's `locale`, as a language tag (see `features::translations::locale`).
    pub locale: Option<String>,
}

fn st(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|x| !x.is_empty())
}

fn ts(v: &Value) -> Option<DateTime<Utc>> {
    st(v).and_then(|t| DateTime::parse_from_rfc3339(t).ok().map(|d| d.with_timezone(&Utc)))
}

/// Klaviyo stores free-text country names; DumbMail rules use ISO codes for the common ones.
pub fn country_code(c: &str) -> Option<&'static str> {
    const NAMES: &[(&str, &str)] = &[
        ("united states", "US"), ("united states of america", "US"), ("usa", "US"), ("us", "US"), ("canada", "CA"), ("united kingdom", "GB"), ("uk", "GB"),
        ("great britain", "GB"), ("england", "GB"), ("australia", "AU"), ("new zealand", "NZ"), ("ireland", "IE"), ("germany", "DE"), ("france", "FR"),
        ("spain", "ES"), ("italy", "IT"), ("netherlands", "NL"), ("belgium", "BE"), ("switzerland", "CH"), ("austria", "AT"), ("sweden", "SE"),
        ("norway", "NO"), ("denmark", "DK"), ("finland", "FI"), ("portugal", "PT"), ("poland", "PL"), ("mexico", "MX"), ("brazil", "BR"),
        ("argentina", "AR"), ("japan", "JP"), ("china", "CN"), ("india", "IN"), ("singapore", "SG"), ("south africa", "ZA"), ("israel", "IL"),
        ("united arab emirates", "AE"), ("south korea", "KR"), ("hong kong", "HK"), ("philippines", "PH"), ("czech republic", "CZ"), ("greece", "GR"),
    ];
    let l = c.trim().to_lowercase();
    if l.len() == 2 && l.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return NAMES.iter().map(|(_, code)| *code).find(|code| code.eq_ignore_ascii_case(&l));
    }
    NAMES.iter().find(|(n, _)| *n == l).map(|(_, code)| *code)
}

/// The fingerprint of what matters for matching and consent.
pub fn fingerprint(p: &Value) -> String {
    let a = &p["attributes"];
    let key = json!([a["email"], a["phone_number"], a["external_id"], a["subscriptions"]["email"]["marketing"], a["subscriptions"]["sms"]["marketing"]]).to_string();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// How many of Klaviyo's past consent records a profile keeps (`klaviyo_consent_history`), one per run.
pub const CONSENT_HISTORY: i32 = 5;

/// Read one Klaviyo profile resource. `Err(reason)`: it has nothing DumbMail can identify it by. `run`
/// names this import run in the consent history.
pub fn parse(p: &Value, hold_never: bool, run: uuid::Uuid) -> Result<KProfile, (String, String, Value)> {
    let kid = st(&p["id"]).unwrap_or("").to_string();
    let a = &p["attributes"];
    let raw_email = st(&a["email"]);
    let email = raw_email.and_then(crate::models::normalize_email);
    let phone = st(&a["phone_number"]).and_then(crate::models::normalize_phone);
    let ext: Option<String> = st(&a["external_id"]).map(|x| x.chars().take(200).collect());
    let label = raw_email.or(st(&a["phone_number"])).unwrap_or(&kid).chars().take(300).collect::<String>();
    let subs = &a["subscriptions"];
    let ke = consent::read_email(subs);
    let ks = consent::read_sms(subs);
    let e_in = consent::email_in(ke.as_ref(), hold_never);
    let s_in = consent::sms_in(ks.as_ref(), phone.is_some());
    let detail = json!({
        "email": ke.as_ref().map(|e| json!({"consent": e.consent, "can_receive": e.can_receive,
            "suppressed": e.suppressions.iter().any(|(r, _)| r != "UNSUBSCRIBE"), "state": e_in.state, "blocked": e_in.state == "unsubscribed" || e_in.suppressed.is_some()})),
        "sms": ks.as_ref().map(|k| json!({"consent": k.consent, "can_receive": k.can_receive, "state": s_in.state})),
        "has_email": email.is_some(), "has_phone": phone.is_some(),
        "list_suppressions": ke.as_ref().map(|e| e.list_suppressions.iter().map(|(l, _, _)| l.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "skipped": Value::Null,
    });
    if email.is_none() && phone.is_none() && ext.is_none() {
        let why = match raw_email {
            Some(e) => format!("Its email address ({}) isn't valid and it has no phone number or external id.", consent::mask_email(e)),
            None => "It has no email address, phone number or external id (often an anonymous website visitor).".to_string(),
        };
        let mut d = detail;
        d["skipped"] = json!(why);
        return Err((kid, label, d));
    }
    let mut notes = Vec::new();
    if raw_email.is_some() && email.is_none() {
        notes.push("Its email address isn't valid, so it was left off; the rest of the profile came across.".to_string());
    }
    let mut props: Map<String, Value> = a["properties"].as_object().cloned().unwrap_or_default();
    if serde_json::to_string(&props).map(|t| t.len()).unwrap_or(0) > MAX_PROFILE_PROPS {
        props.retain(|_, v| !v.is_object() && !v.is_array());
        notes.push("Its custom properties were too large; lists and nested values were left off.".to_string());
    }
    for k in ["organization", "title", "locale", "image"] {
        if let Some(v) = st(&a[k]) {
            props.insert(k.into(), json!(v));
        }
    }
    let loc = &a["location"];
    for k in ["address1", "address2", "city", "region", "zip", "timezone", "latitude", "longitude"] {
        if !loc[k].is_null() && loc[k] != "" {
            props.insert(k.into(), loc[k].clone());
        }
    }
    if let Some(c) = st(&loc["country"]) {
        props.insert("country".into(), json!(country_code(c).unwrap_or(c)));
    }
    props.insert("klaviyo_id".into(), json!(kid));
    let said = consent::provenance(ke.as_ref(), ks.as_ref());
    let mut hist = said.clone();
    hist["run"] = json!(run);
    hist["read_at"] = json!(Utc::now().to_rfc3339());
    props.insert("klaviyo_consent".into(), said);
    Ok(KProfile {
        hist,
        fp: fingerprint(p),
        kid,
        label,
        email,
        phone,
        ext,
        first: st(&a["first_name"]).map(|x| x.chars().take(200).collect()),
        last: st(&a["last_name"]).map(|x| x.chars().take(200).collect()),
        props: Value::Object(props),
        created: ts(&a["created"]),
        e_in,
        s_in,
        detail,
        notes,
        locale: st(&a["locale"]).and_then(crate::features::translations::locale::normalize),
    })
}

/// Set the language Klaviyo gave (a trusted source) on the profiles just written, in the same
/// transaction. Nothing runs when no profile on the page has one.
async fn write_locales(tx: &mut sqlx::PgConnection, ids: &[Uuid], locales: &[Option<String>]) -> sqlx::Result<()> {
    if locales.iter().all(Option::is_none) {
        return Ok(());
    }
    sqlx::query("UPDATE profiles p SET locale = x.loc FROM unnest($1::uuid[], $2::text[]) AS x(id, loc) WHERE p.id = x.id AND x.loc IS NOT NULL AND p.locale IS DISTINCT FROM x.loc")
        .bind(ids)
        .bind(locales)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// The merge, as SQL over `p` (the profile as it is now; the rows are locked by the statement before),
/// `k` (what Klaviyo says) and `w` (what the import itself wrote last time). "Same address" is the Klaviyo
/// email equal to the profile's, or the profile has none and this write adds it (`k.se`).
const SAME_E: &str = "COALESCE(k.ke IS NOT NULL AND (p.email = k.ke OR (p.email IS NULL AND k.se IS NOT NULL)), false)";
const SAME_S: &str = "COALESCE(k.kp IS NOT NULL AND (p.phone_number = k.kp OR (p.phone_number IS NULL AND k.sp IS NOT NULL)), false)";
/// Subscribed only because an earlier import set it, for this exact address or number.
const IMP_E: &str = "COALESCE(p.email_consent = 'subscribed' AND w.email_sub = COALESCE(p.email, k.se), false)";
const IMP_S: &str = "COALESCE(p.sms_consent = 'subscribed' AND w.sms_sub = COALESCE(p.phone_number, k.sp), false)";

/// Lock a page's matched profiles, in id order, before they're read and merged.
const LOCK_SQL: &str = "SELECT p.id FROM profiles p WHERE p.account_id = $1 AND p.id = ANY($2) ORDER BY p.id FOR UPDATE";

fn merge_sql() -> String {
    let (e, s, ie, is) = (SAME_E, SAME_S, IMP_E, IMP_S);
    format!(
        "WITH k AS (
            SELECT * FROM unnest($2::int[], $3::uuid[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::jsonb[],
                                 $12::text[], $13::text[], $14::bool[], $15::text[], $16::bool[], $17::jsonb[])
                 AS k(idx, pid, ke, se, kp, sp, sx, first, last, props, e_in, e_sup, e_hold, s_in, s_hold, hist)),
         m AS (
            SELECT k.*, p.id, p.email_consent AS was_e, p.sms_consent AS was_s, w.email_sub AS w_e, w.sms_sub AS w_s,
                   COALESCE(p.email, k.se) AS e_addr, COALESCE(p.phone_number, k.sp) AS s_addr,
                   {e} AS same_e, {s} AS same_s, {ie} AS imp_e, {is} AS imp_s,
                   -- An opt-out always applies; an opt-in only lifts 'never subscribed' with no suppression; a
                   -- subscription an earlier import set follows what Klaviyo says now; anything else stays.
                   CASE WHEN NOT {e} THEN p.email_consent
                        WHEN k.e_in = 'unsubscribed' THEN 'unsubscribed'
                        WHEN k.e_in = 'subscribed' AND p.email_consent = 'never_subscribed' AND p.email_suppressed_reason IS NULL THEN 'subscribed'
                        WHEN k.e_in <> 'subscribed' AND {ie} THEN 'never_subscribed'
                        ELSE p.email_consent END AS new_e,
                   CASE WHEN NOT {s} THEN p.sms_consent
                        WHEN k.s_in = 'unsubscribed' THEN 'unsubscribed'
                        WHEN k.s_in = 'subscribed' AND p.sms_consent = 'never_subscribed' THEN 'subscribed'
                        WHEN k.s_in <> 'subscribed' AND {is} THEN 'never_subscribed'
                        ELSE p.sms_consent END AS new_s
            FROM k JOIN profiles p ON p.id = k.pid AND p.account_id = $1
            LEFT JOIN klaviyo_consent_writes w ON w.profile_id = p.id),
         u AS (
            UPDATE profiles p SET
                email = COALESCE(p.email, m.se), phone_number = COALESCE(p.phone_number, m.sp), external_id = COALESCE(p.external_id, m.sx),
                first_name = COALESCE(m.first, p.first_name), last_name = COALESCE(m.last, p.last_name),
                -- Klaviyo's latest consent record replaces the last one; the history keeps one entry per run, the last few.
                properties = (p.properties || m.props) || jsonb_build_object('klaviyo_consent_history', (
                    SELECT COALESCE(jsonb_agg(h.e ORDER BY h.o), '[]'::jsonb) FROM (
                        SELECT x.e, x.o FROM jsonb_array_elements(
                            COALESCE((SELECT jsonb_agg(y.v ORDER BY y.i) FROM jsonb_array_elements(CASE WHEN jsonb_typeof(p.properties->'klaviyo_consent_history') = 'array'
                                THEN p.properties->'klaviyo_consent_history' ELSE '[]'::jsonb END) WITH ORDINALITY AS y(v, i) WHERE y.v->>'run' IS DISTINCT FROM m.hist->>'run'), '[]'::jsonb)
                            || jsonb_build_array(m.hist)) WITH ORDINALITY AS x(e, o)
                        ORDER BY x.o DESC LIMIT $18) h)),
                email_consent = m.new_e,
                -- A suppression already here is never removed; Klaviyo's is added.
                email_suppressed_reason = CASE WHEN m.same_e THEN COALESCE(p.email_suppressed_reason, m.e_sup) ELSE p.email_suppressed_reason END,
                sms_consent = m.new_s,
                -- Holds are only ever added here; someone who opted in through DumbMail itself isn't held.
                marketing_hold = p.marketing_hold OR (m.same_e AND m.e_hold AND (m.was_e <> 'subscribed' OR m.imp_e))
                                 OR (m.same_s AND m.s_hold AND (m.was_s <> 'subscribed' OR m.imp_s)),
                updated_at = now()
            FROM m WHERE p.id = m.id AND p.account_id = $1
            RETURNING m.idx, p.id, COALESCE(p.email = m.ke, false) AS ae, COALESCE(p.phone_number = m.kp, false) AS asm, p.email, p.phone_number,
                      p.email_consent, p.sms_consent,
                      -- What the import has written: the address or number it subscribed (Klaviyo's opt-in), else nothing. A
                      -- channel it couldn't apply keeps the old record.
                      CASE WHEN NOT m.same_e THEN m.w_e
                           WHEN m.new_e = 'subscribed' AND m.e_in = 'subscribed' AND (m.imp_e OR m.was_e = 'never_subscribed') THEN m.e_addr END AS rec_e,
                      CASE WHEN NOT m.same_s THEN m.w_s
                           WHEN m.new_s = 'subscribed' AND m.s_in = 'subscribed' AND (m.imp_s OR m.was_s = 'never_subscribed') THEN m.s_addr END AS rec_s),
         -- (A data-changing WITH always runs to completion, read or not.)
         rec AS (
            INSERT INTO klaviyo_consent_writes (profile_id, account_id, email_sub, sms_sub)
            SELECT u.id, $1, u.rec_e, u.rec_s FROM u
            ON CONFLICT (profile_id) DO UPDATE SET email_sub = EXCLUDED.email_sub, sms_sub = EXCLUDED.sms_sub, updated_at = now()
            RETURNING 1)
         SELECT u.idx, u.id, u.ae, u.asm, u.email, u.phone_number, u.email_consent, u.sms_consent FROM u"
    )
}

/// What one write did.
#[derive(Debug, Clone)]
struct Wrote {
    pid: Uuid,
    applied_e: bool,
    applied_s: bool,
    email_now: Option<String>,
    phone_now: Option<String>,
    email_consent: String,
    sms_consent: String,
}

type WroteRow = (i32, Uuid, bool, bool, Option<String>, Option<String>, String, String);

fn wrote(r: WroteRow) -> (usize, Wrote) {
    (r.0 as usize, Wrote { pid: r.1, applied_e: r.2, applied_s: r.3, email_now: r.4, phone_now: r.5, email_consent: r.6, sms_consent: r.7 })
}

pub struct Upserted {
    pub ids: HashMap<String, Uuid>,
    pub done: i64,
    pub skipped: i64,
}

type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

/// Did a statement fail on a unique index (an identifier belongs to someone else)?
fn clash(e: &anyhow::Error) -> bool {
    e.downcast_ref::<sqlx::Error>().and_then(|e| e.as_database_error()).and_then(|e| e.code()).as_deref() == Some("23505")
}

impl Run<'_> {
    fn hold_never(&self) -> bool {
        self.imp.options["never_subscribed"] == "hold"
    }

    /// Best DumbMail match for each row: saved reference, then external id, email, phone.
    async fn matches(&self, rows: &[(usize, &KProfile)]) -> sqlx::Result<HashMap<usize, Uuid>> {
        let idx: Vec<i32> = rows.iter().map(|(i, _)| *i as i32).collect();
        let kid: Vec<&str> = rows.iter().map(|(_, k)| k.kid.as_str()).collect();
        let email: Vec<Option<&str>> = rows.iter().map(|(_, k)| k.email.as_deref()).collect();
        let phone: Vec<Option<&str>> = rows.iter().map(|(_, k)| k.phone.as_deref()).collect();
        let ext: Vec<Option<&str>> = rows.iter().map(|(_, k)| k.ext.as_deref()).collect();
        let found: Vec<(i32, Uuid)> = sqlx::query_as(
            "WITH k AS (SELECT * FROM unnest($2::int[], $3::text[], $4::text[], $5::text[], $6::text[]) AS k(idx, kid, email, phone, ext)),
             c AS (
                SELECT k.idx, r.local_id AS pid, 0 AS pri FROM k JOIN klaviyo_refs r ON r.account_id = $1 AND r.kind = 'profile' AND r.klaviyo_id = k.kid AND r.local_id IS NOT NULL
                UNION ALL SELECT k.idx, p.id, 1 FROM k JOIN profiles p ON p.account_id = $1 AND p.external_id = k.ext
                UNION ALL SELECT k.idx, p.id, 2 FROM k JOIN profiles p ON p.account_id = $1 AND p.email = k.email
                UNION ALL SELECT k.idx, p.id, 3 FROM k JOIN profiles p ON p.account_id = $1 AND p.phone_number = k.phone)
             SELECT DISTINCT ON (c.idx) c.idx, c.pid FROM c JOIN profiles p ON p.id = c.pid AND p.account_id = $1 ORDER BY c.idx, c.pri",
        )
        .bind(self.imp.account_id)
        .bind(&idx)
        .bind(&kid)
        .bind(&email)
        .bind(&phone)
        .bind(&ext)
        .fetch_all(&self.state.db)
        .await?;
        Ok(found.into_iter().map(|(i, p)| (i as usize, p)).collect())
    }

    /// Merge rows into the profiles they matched, in one transaction that owns the run: lock them, then
    /// merge. `with_ids`: also fill in missing identifiers.
    async fn merge(&self, rows: &[(usize, &KProfile, Uuid)], with_ids: bool) -> anyhow::Result<Vec<(usize, Wrote)>> {
        let col = |f: &dyn Fn(&KProfile) -> Option<String>| rows.iter().map(|(_, k, _)| f(k)).collect::<Vec<Option<String>>>();
        let idx: Vec<i32> = rows.iter().map(|(i, _, _)| *i as i32).collect();
        let pid: Vec<Uuid> = rows.iter().map(|(_, _, p)| *p).collect();
        let ke = col(&|k| k.email.clone());
        let kp = col(&|k| k.phone.clone());
        let none: Vec<Option<String>> = vec![None; rows.len()];
        let (se, sp, sx) = if with_ids { (ke.clone(), kp.clone(), col(&|k| k.ext.clone())) } else { (none.clone(), none.clone(), none) };
        let mut tx: Tx = self.begin().await?;
        sqlx::query(LOCK_SQL).bind(self.imp.account_id).bind(&pid).execute(&mut *tx).await?;
        // A new statement after the lock: it reads every row as it is now, and nobody can change them until commit.
        let res: Vec<WroteRow> = sqlx::query_as(&merge_sql())
            .bind(self.imp.account_id)
            .bind(&idx)
            .bind(&pid)
            .bind(&ke)
            .bind(&se)
            .bind(&kp)
            .bind(&sp)
            .bind(&sx)
            .bind(col(&|k| k.first.clone()))
            .bind(col(&|k| k.last.clone()))
            .bind(rows.iter().map(|(_, k, _)| k.props.clone()).collect::<Vec<Value>>())
            .bind(rows.iter().map(|(_, k, _)| k.e_in.state).collect::<Vec<&str>>())
            .bind(rows.iter().map(|(_, k, _)| k.e_in.suppressed).collect::<Vec<Option<&str>>>())
            .bind(rows.iter().map(|(_, k, _)| k.e_in.hold).collect::<Vec<bool>>())
            .bind(rows.iter().map(|(_, k, _)| k.s_in.state).collect::<Vec<&str>>())
            .bind(rows.iter().map(|(_, k, _)| k.s_in.hold).collect::<Vec<bool>>())
            .bind(rows.iter().map(|(_, k, _)| k.hist.clone()).collect::<Vec<Value>>())
            .bind(CONSENT_HISTORY)
            .fetch_all(&mut *tx)
            .await?;
        write_locales(&mut tx, &pid, &rows.iter().map(|(_, k, _)| k.locale.clone()).collect::<Vec<_>>()).await?;
        tx.commit().await?;
        Ok(res.into_iter().map(wrote).collect())
    }

    /// Insert brand-new profiles; rows that clash with an existing one are left out (returned empty).
    async fn insert(&self, rows: &[(usize, &KProfile)]) -> anyhow::Result<Vec<(usize, Wrote)>> {
        let ids: Vec<Uuid> = rows.iter().map(|_| Uuid::now_v7()).collect();
        let by_id: HashMap<Uuid, usize> = ids.iter().zip(rows.iter()).map(|(id, (i, _))| (*id, *i)).collect();
        let ec: Vec<&str> = rows.iter().map(|(_, k)| if k.email.is_some() { consent::new_email_consent(&k.e_in) } else { "never_subscribed" }).collect();
        let sc: Vec<&str> = rows.iter().map(|(_, k)| if k.phone.is_some() { consent::new_sms_consent(&k.s_in) } else { "never_subscribed" }).collect();
        let sup: Vec<Option<&str>> = rows.iter().map(|(_, k)| k.email.as_ref().and(k.e_in.suppressed)).collect();
        let hold: Vec<bool> = rows.iter().map(|(_, k)| (k.email.is_some() && k.e_in.hold) || (k.phone.is_some() && k.s_in.hold)).collect();
        let props: Vec<Value> = rows
            .iter()
            .map(|(_, k)| {
                let mut p = k.props.clone();
                p["klaviyo_consent_history"] = json!([k.hist]);
                p
            })
            .collect();
        let mut tx: Tx = self.begin().await?;
        let res: Vec<(Uuid, Option<String>, Option<String>, String, String)> = sqlx::query_as(
            "WITH ins AS (
                INSERT INTO profiles (id, account_id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, marketing_hold, created_at)
                SELECT k.id, $1, k.email, k.phone, k.ext, k.first, k.last, k.props, k.ec, k.sc, k.sup, k.hold, COALESCE(k.created, now())
                FROM unnest($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::jsonb[], $9::text[], $10::text[], $11::text[], $12::bool[], $13::timestamptz[])
                     AS k(id, email, phone, ext, first, last, props, ec, sc, sup, hold, created)
                ON CONFLICT DO NOTHING
                RETURNING id, email, phone_number, email_consent, sms_consent),
             -- The subscriptions this import set, so a later run knows they were Klaviyo's opt-in.
             rec AS (
                INSERT INTO klaviyo_consent_writes (profile_id, account_id, email_sub, sms_sub)
                SELECT id, $1, CASE WHEN email_consent = 'subscribed' THEN email END, CASE WHEN sms_consent = 'subscribed' THEN phone_number END
                FROM ins WHERE email_consent = 'subscribed' OR sms_consent = 'subscribed'
                ON CONFLICT (profile_id) DO UPDATE SET email_sub = EXCLUDED.email_sub, sms_sub = EXCLUDED.sms_sub, updated_at = now()
                RETURNING 1)
             SELECT id, email, phone_number, email_consent, sms_consent FROM ins",
        )
        .bind(self.imp.account_id)
        .bind(&ids)
        .bind(rows.iter().map(|(_, k)| k.email.clone()).collect::<Vec<_>>())
        .bind(rows.iter().map(|(_, k)| k.phone.clone()).collect::<Vec<_>>())
        .bind(rows.iter().map(|(_, k)| k.ext.clone()).collect::<Vec<_>>())
        .bind(rows.iter().map(|(_, k)| k.first.clone()).collect::<Vec<_>>())
        .bind(rows.iter().map(|(_, k)| k.last.clone()).collect::<Vec<_>>())
        .bind(&props)
        .bind(&ec)
        .bind(&sc)
        .bind(&sup)
        .bind(&hold)
        .bind(rows.iter().map(|(_, k)| k.created).collect::<Vec<_>>())
        .fetch_all(&mut *tx)
        .await?;
        write_locales(&mut tx, &ids, &rows.iter().map(|(_, k)| k.locale.clone()).collect::<Vec<_>>()).await?;
        tx.commit().await?;
        Ok(res
            .into_iter()
            .filter_map(|(id, e, p, ec, sc)| {
                let i = *by_id.get(&id)?;
                Some((i, Wrote { pid: id, applied_e: e.is_some(), applied_s: p.is_some(), email_now: e, phone_now: p, email_consent: ec, sms_consent: sc }))
            })
            .collect())
    }

    /// One profile at a time, for rows the batch couldn't place (an identifier belongs to someone else).
    async fn one(&self, i: usize, k: &KProfile) -> anyhow::Result<Option<Wrote>> {
        for attempt in 0..3 {
            let m = self.matches(&[(i, k)]).await?;
            let res = match m.get(&i) {
                Some(pid) => self.merge(&[(i, k, *pid)], attempt == 0).await.map(|mut v| v.pop().map(|x| x.1)),
                None if attempt < 2 => self.insert(&[(i, k)]).await.map(|mut v| v.pop().map(|x| x.1)),
                None => break,
            };
            match res {
                Ok(Some(w)) => return Ok(Some(w)),
                // Inserted nothing: someone else took the identifier meanwhile; look again.
                Ok(None) => continue,
                Err(e) if clash(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Klaviyo's opt-outs and suppressions apply to the address or number itself: to every profile here
    /// that has it now, whichever profile the import matched. Restrictions only; nothing is lifted.
    async fn restrict_by_address(&self, ks: &[KProfile]) -> anyhow::Result<()> {
        let (mut emails, mut unsub, mut sups) = (Vec::new(), Vec::new(), Vec::new());
        let mut phones = Vec::new();
        for k in ks {
            if let Some(e) = &k.email {
                if k.e_in.state == "unsubscribed" || k.e_in.suppressed.is_some() {
                    emails.push(e.clone());
                    unsub.push(k.e_in.state == "unsubscribed");
                    sups.push(k.e_in.suppressed);
                }
            }
            if let Some(p) = &k.phone {
                if k.s_in.state == "unsubscribed" {
                    phones.push(p.clone());
                }
            }
        }
        if emails.is_empty() && phones.is_empty() {
            return Ok(());
        }
        let mut tx: Tx = self.begin().await?;
        if !emails.is_empty() {
            sqlx::query(
                "WITH up AS (
                    UPDATE profiles p SET email_consent = CASE WHEN k.unsub THEN 'unsubscribed' ELSE p.email_consent END,
                        email_suppressed_reason = COALESCE(p.email_suppressed_reason, k.sup), updated_at = now()
                    FROM unnest($2::text[], $3::bool[], $4::text[]) AS k(addr, unsub, sup)
                    WHERE p.account_id = $1 AND p.email = k.addr
                      AND ((k.unsub AND p.email_consent <> 'unsubscribed') OR (k.sup IS NOT NULL AND p.email_suppressed_reason IS NULL))
                    RETURNING p.id, p.email_consent)
                 -- An import-set subscription that's now off is no longer one.
                 UPDATE klaviyo_consent_writes w SET email_sub = NULL, updated_at = now() FROM up WHERE w.profile_id = up.id AND up.email_consent <> 'subscribed'",
            )
            .bind(self.imp.account_id)
            .bind(&emails)
            .bind(&unsub)
            .bind(&sups)
            .execute(&mut *tx)
            .await?;
        }
        if !phones.is_empty() {
            sqlx::query(
                "WITH up AS (
                    UPDATE profiles p SET sms_consent = 'unsubscribed', updated_at = now()
                    WHERE p.account_id = $1 AND p.phone_number = ANY($2) AND p.sms_consent <> 'unsubscribed' RETURNING p.id)
                 UPDATE klaviyo_consent_writes w SET sms_sub = NULL, updated_at = now() FROM up WHERE w.profile_id = up.id",
            )
            .bind(self.imp.account_id)
            .bind(&phones)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Create or update the DumbMail profiles for a page of Klaviyo profile resources (with
    /// `subscriptions`). Returns Klaviyo id → DumbMail id for those that came across.
    pub async fn upsert_profiles(&self, resources: &[Value]) -> anyhow::Result<Upserted> {
        let acct = self.imp.account_id;
        let hold_never = self.hold_never();
        let mut ks: Vec<KProfile> = Vec::new();
        let mut refs: Vec<(String, Option<Uuid>, String, Value)> = Vec::new();
        let mut issues: Vec<(String, Option<String>, Option<Uuid>, String, String)> = Vec::new();
        let mut skipped = 0i64;
        for r in resources {
            if r["id"].as_str().is_none() {
                continue;
            }
            match parse(r, hold_never, self.imp.id) {
                Ok(k) => ks.push(k),
                Err((kid, label, mut d)) => {
                    if self.in_profile_scan {
                        d["scan"] = json!(self.imp.id);
                    }
                    let why = d["skipped"].as_str().unwrap_or("").to_string();
                    issues.push(("skipped".into(), Some(kid.clone()), None, mask_text(&label), why));
                    refs.push((kid, None, label, d));
                    skipped += 1;
                }
            }
        }
        // Several Klaviyo profiles on one page can be one DumbMail profile; write those one at a time.
        let rows: Vec<(usize, &KProfile)> = ks.iter().enumerate().collect();
        let matched = self.matches(&rows).await?;
        let mut first_for: HashMap<Uuid, usize> = HashMap::new();
        let (mut batch, mut later, mut fresh) = (Vec::new(), Vec::new(), Vec::new());
        for (i, k) in &rows {
            match matched.get(i) {
                Some(pid) if !first_for.contains_key(pid) => {
                    first_for.insert(*pid, *i);
                    batch.push((*i, *k, *pid));
                }
                Some(_) => later.push(*i),
                None => fresh.push((*i, *k)),
            }
        }
        let mut out: HashMap<usize, Wrote> = HashMap::new();
        if !batch.is_empty() {
            match self.merge(&batch, true).await {
                Ok(w) => out.extend(w),
                Err(e) if clash(&e) => later.extend(batch.iter().map(|(i, _, _)| *i)),
                Err(e) => return Err(e),
            }
        }
        if !fresh.is_empty() {
            let inserted = self.insert(&fresh).await?;
            let done: std::collections::HashSet<usize> = inserted.iter().map(|(i, _)| *i).collect();
            later.extend(fresh.iter().map(|(i, _)| *i).filter(|i| !done.contains(i)));
            out.extend(inserted);
        }
        later.sort_unstable();
        for i in later {
            if let Some(w) = self.one(i, &ks[i]).await? {
                out.insert(i, w);
            }
        }
        self.restrict_by_address(&ks).await?;
        let mut ids = HashMap::new();
        let mut done = 0i64;
        for (i, k) in ks.iter().enumerate() {
            for n in &k.notes {
                issues.push(("skipped".into(), Some(k.kid.clone()), None, masked_label(k), n.clone()));
            }
            let mut d = k.detail.clone();
            d["fp"] = json!(k.fp);
            if self.in_profile_scan {
                d["scan"] = json!(self.imp.id);
            }
            let Some(w) = out.get(&i) else {
                let why = "Its email, phone number or external id belongs to another profile that couldn't be merged with it. Klaviyo's opt-outs for its address and number were still applied.";
                d["skipped"] = json!(why);
                issues.push(("skipped".into(), Some(k.kid.clone()), None, masked_label(k), why.into()));
                refs.push((k.kid.clone(), None, k.label.clone(), d));
                skipped += 1;
                continue;
            };
            let mut conflicts = Vec::new();
            let e_restrict = k.e_in.state == "unsubscribed" || k.e_in.suppressed.is_some();
            if let (Some(ke), false) = (&k.email, w.applied_e) {
                conflicts.push(match &w.email_now {
                    Some(here) => format!("Email: DumbMail has {}, Klaviyo has {}.", consent::mask_email(here), consent::mask_email(ke)),
                    None => format!("Email: Klaviyo's address {} belongs to another DumbMail profile.", consent::mask_email(ke)),
                } + if e_restrict { " Klaviyo's opt-out or suppression for Klaviyo's address was applied to whichever profile has it." } else { "" });
            }
            if let (Some(kp), false) = (&k.phone, w.applied_s) {
                conflicts.push(match &w.phone_now {
                    Some(here) => format!("Phone: DumbMail has {}, Klaviyo has {}.", consent::mask_phone(here), consent::mask_phone(kp)),
                    None => format!("Phone: Klaviyo's number {} belongs to another DumbMail profile.", consent::mask_phone(kp)),
                } + if k.s_in.state == "unsubscribed" { " Klaviyo's opt-out for Klaviyo's number was applied to whichever profile has it." } else { "" });
            }
            if !conflicts.is_empty() {
                let msg = format!("{} Klaviyo's opt-in for that address or number wasn't applied to this profile. Check which is right and update the profile.", conflicts.join(" "));
                issues.push(("conflict".into(), Some(k.kid.clone()), Some(w.pid), masked_label(k), msg));
            }
            d["applied_email"] = json!(w.applied_e);
            d["applied_sms"] = json!(w.applied_s);
            d["conflict"] = if conflicts.is_empty() { Value::Null } else { json!(conflicts) };
            // Consent given through DumbMail that Klaviyo had no opt-in for: kept, and shown in the report.
            d["kept_dumbmail_email"] = json!(w.applied_e && matches!(k.e_in.state, "never" | "unknown") && w.email_consent == "subscribed");
            d["kept_dumbmail_sms"] = json!(w.applied_s && !matches!(k.s_in.state, "subscribed" | "unsubscribed") && w.sms_consent == "subscribed");
            refs.push((k.kid.clone(), Some(w.pid), k.label.clone(), d));
            ids.insert(k.kid.clone(), w.pid);
            done += 1;
        }
        let mut tx: Tx = self.begin().await?;
        store::set_refs(&mut tx, acct, "profile", &refs).await?;
        for (kind, kid, local, label, msg) in &issues {
            store::issue(&mut tx, &self.imp, "profiles", kind, label, kid.as_deref(), *local, msg).await?;
        }
        tx.commit().await?;
        Ok(Upserted { ids, done, skipped })
    }
}

/// A profile's label with its email and phone partly hidden: issues and the report never show whole
/// addresses.
fn masked_label(k: &KProfile) -> String {
    match (&k.email, &k.phone) {
        (Some(e), _) => consent::mask_email(e),
        (None, Some(p)) => consent::mask_phone(p),
        _ => mask_text(&k.label),
    }
}

/// A raw label (an email, a phone number or a Klaviyo id), partly hidden if it's an address.
fn mask_text(label: &str) -> String {
    if label.contains('@') {
        consent::mask_email(label)
    } else if label.starts_with('+') || label.chars().filter(|c| c.is_ascii_digit()).count() >= 7 {
        consent::mask_phone(label)
    } else {
        label.to_string()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn countries() {
        assert_eq!(super::country_code("United States"), Some("US"));
        assert_eq!(super::country_code("gb"), Some("GB"));
        assert_eq!(super::country_code("Narnia"), None);
    }
}

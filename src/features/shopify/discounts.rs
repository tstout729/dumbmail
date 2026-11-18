//! Single-use Shopify discount codes for one person, made through the Admin API from a template.
//!
//! The coupon feature calls [`create_discount_code`] (one code, for a form or a message being
//! prepared) or [`create_discount_codes`] (a bounded batch made ahead of a campaign send, so the
//! send never waits on Shopify's rate limit). Codes are derived from a stable key with the account's
//! secret, so a retry after a crash (Shopify made the code, we died before saving it) makes the same
//! code, finds it already exists, and keeps it: one person, one grant, one code.
//!
//! Redemption is read from order webhooks (`orders::redeem`), which come from Shopify's servers.

use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use super::client::{self, ApiError};
use super::shop;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Most codes one batch call makes.
pub const MAX_BATCH: usize = 25;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DiscountValue {
    /// 0–100.
    Percentage(f64),
    /// In the store's currency.
    FixedAmount(f64),
}

/// What each code gives. `key` names the template (the coupon's id): codes from different templates
/// never collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscountTemplate {
    pub key: String,
    /// Shown to the merchant in Shopify and to the buyer at checkout.
    pub title: String,
    pub value: DiscountValue,
    /// Smallest order subtotal the code works on.
    #[serde(default)]
    pub minimum_subtotal: Option<f64>,
    /// Days the code lasts from when it's made.
    #[serde(default)]
    pub expires_after_days: Option<i32>,
    /// A fixed end instead.
    #[serde(default)]
    pub ends_at: Option<DateTime<Utc>>,
    /// Shopify collection ids it applies to; empty means everything.
    #[serde(default)]
    pub collection_ids: Vec<String>,
    /// Start of each code, e.g. WELCOME (letters and digits, up to 12).
    #[serde(default)]
    pub prefix: String,
    /// Whether it can be used with other discounts.
    #[serde(default)]
    pub combines_with_other_discounts: bool,
}

impl DiscountTemplate {
    pub fn check(&self) -> AppResult<()> {
        match self.value {
            DiscountValue::Percentage(p) if !(p > 0.0 && p <= 100.0) => return Err(AppError::bad("a percentage discount must be more than 0 and at most 100")),
            DiscountValue::FixedAmount(a) if !(a > 0.0 && a < 1_000_000.0) => return Err(AppError::bad("a fixed discount must be more than 0")),
            _ => {}
        }
        if self.key.is_empty() || self.key.len() > 100 {
            return Err(AppError::bad("template key must be 1-100 characters"));
        }
        if self.title.trim().is_empty() || self.title.len() > 255 {
            return Err(AppError::bad("give the discount a title (up to 255 characters)"));
        }
        if self.prefix.len() > 12 || !self.prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(AppError::bad("the code prefix can have up to 12 letters and digits"));
        }
        if self.minimum_subtotal.is_some_and(|m| !(m >= 0.0 && m.is_finite())) {
            return Err(AppError::bad("minimum subtotal can't be negative"));
        }
        if self.expires_after_days.is_some_and(|d| !(1..=3650).contains(&d)) {
            return Err(AppError::bad("expires_after_days must be 1-3650"));
        }
        if self.collection_ids.len() > 100 || self.collection_ids.iter().any(|c| super::customers::num_id(&json!(c)).is_none()) {
            return Err(AppError::bad("collection_ids must be up to 100 Shopify collection ids"));
        }
        Ok(())
    }

    fn ends(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.ends_at.or_else(|| self.expires_after_days.map(|d| now + Duration::days(i64::from(d))))
    }
}

/// The code for one grant: prefix + 10 characters from an HMAC of the account, store, template,
/// person and grant key with the server's secret (unguessable, and the same on every retry). A
/// different store, template, person or grant always gets a different code.
pub fn code_for(secret: &str, account_id: Uuid, shop: &str, template: &DiscountTemplate, profile_id: Uuid, grant_key: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut mac = Hmac::<Sha256>::new_from_slice(format!("shopify-discount:{secret}").as_bytes()).expect("hmac key");
    mac.update(format!("{account_id}|{shop}|{}|{profile_id}|{grant_key}", template.key).as_bytes());
    let bytes = mac.finalize().into_bytes();
    let tail: String = bytes.iter().take(10).map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect();
    format!("{}{tail}", template.prefix.to_ascii_uppercase())
}

fn input(t: &DiscountTemplate, code: &str, now: DateTime<Utc>) -> Value {
    let value = match t.value {
        DiscountValue::Percentage(p) => json!({"percentage": (p / 100.0 * 10000.0).round() / 10000.0}),
        DiscountValue::FixedAmount(a) => json!({"discountAmount": {"amount": format!("{a:.2}"), "appliesOnEachItem": false}}),
    };
    let items = if t.collection_ids.is_empty() {
        json!({"all": true})
    } else {
        json!({"collections": {"add": t.collection_ids.iter().filter_map(|c| super::customers::num_id(&json!(c))).map(|c| super::customers::gid("Collection", c)).collect::<Vec<_>>()}})
    };
    let mut v = json!({
        "title": t.title,
        "code": code,
        "startsAt": now.to_rfc3339(),
        "endsAt": t.ends(now).map(|e| e.to_rfc3339()),
        "context": {"all": "ALL"},
        "customerGets": {"value": value, "items": items},
        "usageLimit": 1,
        "appliesOncePerCustomer": true,
        "combinesWith": {"orderDiscounts": t.combines_with_other_discounts, "productDiscounts": t.combines_with_other_discounts, "shippingDiscounts": t.combines_with_other_discounts},
    });
    if let Some(m) = t.minimum_subtotal.filter(|m| *m > 0.0) {
        v["minimumRequirement"] = json!({"subtotal": {"greaterThanOrEqualToSubtotal": format!("{m:.2}")}});
    }
    v
}

/// Is the discount Shopify has under this code the one this template makes: single use, once per
/// customer, the template's title, value and stacking?
fn matches_template(node: &Value, t: &DiscountTemplate) -> bool {
    let d = &node["codeDiscount"];
    let value_ok = match t.value {
        DiscountValue::Percentage(p) => d["customerGets"]["value"]["percentage"].as_f64().is_some_and(|x| (x - p / 100.0).abs() < 1e-6),
        DiscountValue::FixedAmount(a) => super::customers::amount(&d["customerGets"]["value"]["amount"]).is_some_and(|x| (x - a).abs() < 0.005),
    };
    let c = t.combines_with_other_discounts;
    d["title"].as_str() == Some(t.title.as_str())
        && d["usageLimit"].as_i64() == Some(1)
        && d["appliesOncePerCustomer"].as_bool() == Some(true)
        && value_ok
        && d["combinesWith"]["orderDiscounts"].as_bool() == Some(c)
        && d["combinesWith"]["productDiscounts"].as_bool() == Some(c)
        && d["combinesWith"]["shippingDiscounts"].as_bool() == Some(c)
}

/// Make (or find again) the code for one grant. Called with no database lock held; it waits on
/// Shopify. `grant_key` identifies the grant (for example the message id), so asking twice for the
/// same person, template and grant in the same store returns the same code, and nothing else ever
/// does.
pub async fn create_discount_code(state: &AppState, account_id: Uuid, template: &DiscountTemplate, profile_id: Uuid, grant_key: &str) -> AppResult<String> {
    template.check()?;
    if grant_key.is_empty() || grant_key.len() > 200 {
        return Err(AppError::bad("grant key must be 1-200 characters"));
    }
    let s = shop::connected(&state.db, account_id).await?;
    let owned: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM profiles WHERE id = $1 AND account_id = $2)").bind(profile_id).bind(account_id).fetch_one(&state.db).await?;
    if !owned {
        return Err(AppError::not_found("profile"));
    }
    // The store and its install: a reinstall (or another store) never gets an earlier install's code.
    let code = code_for(&state.cfg.secret_key, account_id, &format!("{}#{}", s.shop_domain, s.generation), template, profile_id, grant_key);
    // A saved code counts only if it was made for this store, template, person and grant.
    let known: Option<(Option<String>, String, Option<Uuid>, Option<String>)> =
        sqlx::query_as("SELECT shop_domain, template_key, profile_id, grant_key FROM shopify_discount_codes WHERE account_id = $1 AND code = $2")
            .bind(account_id)
            .bind(&code)
            .fetch_optional(&state.db)
            .await?;
    let mine = |k: &(Option<String>, String, Option<Uuid>, Option<String>)| k.0.as_deref() == Some(s.shop_domain.as_str()) && k.1 == template.key && k.2 == Some(profile_id) && k.3.as_deref() == Some(grant_key);
    match &known {
        Some(k) if mine(k) => return Ok(code),
        Some(_) => return Err(AppError::Conflict("that discount code belongs to another grant".into())),
        None => {}
    }
    let now = Utc::now();
    let d = client::graphql(
        state,
        &s,
        "mutation($d: DiscountCodeBasicInput!) { discountCodeBasicCreate(basicCodeDiscount: $d) { codeDiscountNode { id } userErrors { field message code } } }",
        json!({"d": input(template, &code, now)}),
    )
    .await
    .map_err(api_err)?;
    let p = &d["discountCodeBasicCreate"];
    let id = match (p["codeDiscountNode"]["id"].as_str(), client::user_error(p)) {
        (Some(id), None) => Some(id.to_string()),
        // Made by an earlier try that didn't get to save it. It's ours only if this store has it under
        // this code with this template's terms (the code itself is derived from this store, template,
        // person and grant).
        (_, Some(e)) if e.contains("taken") || e.contains("unique") || e.contains("exists") => {
            let f = client::graphql(
                state,
                &s,
                "query($c: String!) { codeDiscountNodeByCode(code: $c) { id codeDiscount { ... on DiscountCodeBasic { title usageLimit appliesOncePerCustomer
                    combinesWith { orderDiscounts productDiscounts shippingDiscounts }
                    customerGets { value { ... on DiscountPercentage { percentage } ... on DiscountAmount { amount { amount } } } } } } } }",
                json!({"c": code}),
            )
            .await
            .map_err(api_err)?;
            let node = &f["codeDiscountNodeByCode"];
            if !matches_template(node, template) {
                return Err(AppError::Conflict("Shopify already has a different discount under that code".into()));
            }
            Some(node["id"].as_str().ok_or_else(|| AppError::Conflict("Shopify says that code exists but couldn't find it".into()))?.to_string())
        }
        (_, Some(e)) => return Err(AppError::bad(format!("Shopify refused the discount ({e}); check the template"))),
        (None, None) => return Err(AppError::Internal(anyhow::anyhow!("Shopify returned no discount"))),
    };
    let saved: Option<String> = sqlx::query_scalar(
        "INSERT INTO shopify_discount_codes (account_id, code, template_key, profile_id, discount_id, expires_at, shop_domain, grant_key, generation) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (account_id, code) DO UPDATE SET discount_id = COALESCE(shopify_discount_codes.discount_id, EXCLUDED.discount_id)
            WHERE shopify_discount_codes.shop_domain = EXCLUDED.shop_domain AND shopify_discount_codes.template_key = EXCLUDED.template_key
              AND shopify_discount_codes.profile_id = EXCLUDED.profile_id AND shopify_discount_codes.grant_key = EXCLUDED.grant_key
              AND shopify_discount_codes.generation IS NOT DISTINCT FROM EXCLUDED.generation
         RETURNING code",
    )
    .bind(account_id)
    .bind(&code)
    .bind(&template.key)
    .bind(profile_id)
    .bind(id)
    .bind(template.ends(now))
    .bind(&s.shop_domain)
    .bind(grant_key)
    .bind(s.generation)
    .fetch_optional(&state.db)
    .await?;
    saved.map(|_| code).ok_or_else(|| AppError::Conflict("that discount code belongs to another grant".into()))
}

fn api_err(e: ApiError) -> AppError {
    match e {
        ApiError::Throttled(_) | ApiError::Busy => AppError::Busy("Shopify is rate-limiting discount codes right now; try again shortly".into()),
        ApiError::Forbidden(_) => AppError::Forbidden("DumbMail doesn't have permission to make discounts in this store (reconnect it to grant write_discounts)".into()),
        other => AppError::Conflict(other.to_string()),
    }
}

/// Make codes for up to [`MAX_BATCH`] grants, in order, ahead of a send. Stops early at Shopify's
/// rate limit: the result has one entry per grant attempted, and the caller runs the rest later.
pub async fn create_discount_codes(state: &AppState, account_id: Uuid, template: &DiscountTemplate, grants: &[(Uuid, String)]) -> AppResult<Vec<(Uuid, AppResult<String>)>> {
    if grants.len() > MAX_BATCH {
        return Err(AppError::bad(format!("make at most {MAX_BATCH} codes per call")));
    }
    let mut out = Vec::new();
    for (profile_id, key) in grants {
        let r = create_discount_code(state, account_id, template, *profile_id, key).await;
        let busy = matches!(r, Err(AppError::Busy(_)));
        out.push((*profile_id, r));
        if busy {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> DiscountTemplate {
        DiscountTemplate {
            key: "coupon-1".into(),
            title: "Welcome 10%".into(),
            value: DiscountValue::Percentage(10.0),
            minimum_subtotal: Some(50.0),
            expires_after_days: Some(14),
            ends_at: None,
            collection_ids: vec!["gid://shopify/Collection/5".into()],
            prefix: "WELCOME".into(),
            combines_with_other_discounts: false,
        }
    }

    #[test]
    fn codes_are_stable_and_distinct() {
        let a = Uuid::now_v7();
        let p = Uuid::now_v7();
        let c1 = code_for("k", a, "s.myshopify.com", &t(), p, "m1");
        assert_eq!(c1, code_for("k", a, "s.myshopify.com", &t(), p, "m1"));
        assert!(c1.starts_with("WELCOME") && c1.len() == 17);
        assert_ne!(c1, code_for("k", a, "s.myshopify.com", &t(), p, "m2"));
        assert_ne!(c1, code_for("k", a, "other.myshopify.com", &t(), p, "m1"));
        assert_ne!(c1, code_for("k", a, "s.myshopify.com", &t(), Uuid::now_v7(), "m1"));
        assert_ne!(c1, code_for("k", Uuid::now_v7(), "s.myshopify.com", &t(), p, "m1"));
        assert_ne!(c1, code_for("other", a, "s.myshopify.com", &t(), p, "m1"));
    }

    #[test]
    fn mutation_input() {
        let now = DateTime::parse_from_rfc3339("2026-11-17T00:00:00Z").unwrap().with_timezone(&Utc);
        let v = input(&t(), "WELCOMEABC", now);
        assert_eq!(v["usageLimit"], 1);
        assert_eq!(v["appliesOncePerCustomer"], true);
        assert_eq!(v["customerGets"]["value"]["percentage"], 0.1);
        assert_eq!(v["customerGets"]["items"]["collections"]["add"][0], "gid://shopify/Collection/5");
        assert_eq!(v["minimumRequirement"]["subtotal"]["greaterThanOrEqualToSubtotal"], "50.00");
        assert_eq!(v["endsAt"], "2026-12-01T00:00:00+00:00");
        assert_eq!(v["combinesWith"], json!({"orderDiscounts": false, "productDiscounts": false, "shippingDiscounts": false}));
        let mut bad = t();
        bad.value = DiscountValue::Percentage(150.0);
        assert!(bad.check().is_err());
        bad = t();
        bad.prefix = "NO SPACES".into();
        assert!(bad.check().is_err());
        assert!(t().check().is_ok());
    }
}

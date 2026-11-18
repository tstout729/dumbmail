//! Coupons whose codes are made in the connected store.
//!
//! A coupon (Content → Coupons) is linked to a discount template here. When a message that shows the
//! coupon is about to be sent and the coupon has no free code for that person, the coupons module
//! calls its `CreateInStore` hook (`features::coupons::give_code`), which this module answers with
//! [`super::create_discount_code`]: a real, single-use Shopify code for that person, keyed by the
//! store, the coupon (template), the person and the grant (the send, when the coupon gives a new code
//! each time). The code is saved as that person's code in the coupon, so the send, its retries and
//! redemption tracking all use the coupons module's one path. Nothing else hands out codes.

use serde_json::{json, Value};
use uuid::Uuid;

use super::discounts::{create_discount_code, DiscountTemplate};
use crate::error::{AppError, AppResult};
use crate::features::coupons::store::Coupon;
use crate::features::coupons::StoreCodeFuture;
use crate::state::AppState;

/// The template a coupon's codes are made from, if the coupon is linked to the store.
pub async fn template_for(db: &sqlx::PgPool, coupon: &Coupon) -> AppResult<Option<DiscountTemplate>> {
    let t: Option<Value> = sqlx::query_scalar("SELECT template FROM shopify_coupon_templates WHERE coupon_id = $1 AND account_id = $2")
        .bind(coupon.id)
        .bind(coupon.account_id)
        .fetch_optional(db)
        .await?;
    let Some(t) = t else { return Ok(None) };
    let mut tpl: DiscountTemplate = serde_json::from_value(t).map_err(|e| AppError::Internal(e.into()))?;
    tpl.key = format!("coupon:{}", coupon.id);
    // The coupon's own expiry, unless the template sets one.
    if tpl.ends_at.is_none() && tpl.expires_after_days.is_none() {
        tpl.ends_at = coupon.expires_at;
        tpl.expires_after_days = coupon.expires_after_days;
    }
    Ok(Some(tpl))
}

/// The grant a code is for: the send (for a coupon that gives a new code each time), otherwise the
/// coupon itself (one code per person).
pub fn grant(coupon: &Coupon, send: &str) -> String {
    if coupon.new_code_each_time && !send.is_empty() {
        format!("send:{send}")
    } else {
        "coupon".into()
    }
}

/// Make the person's code in the store (the coupons module's `CreateInStore`). Called with no
/// database lock held.
pub fn make<'a>(state: &AppState, template: &DiscountTemplate, send: &str) -> impl Fn(&'a Coupon, Uuid) -> StoreCodeFuture<'a> + Send + Sync + 'static {
    let (state, template, send) = (state.clone(), template.clone(), send.to_string());
    move |c: &'a Coupon, profile_id: Uuid| -> StoreCodeFuture<'a> {
        let (state, template, grant) = (state.clone(), template.clone(), grant(c, &send));
        Box::pin(async move { create_discount_code(&state, c.account_id, &template, profile_id, &grant).await.map_err(|e| anyhow::anyhow!(e.public_message())) })
    }
}

/// Link a coupon to the store (or change its template). The body is a discount template without
/// `key`: `title`, `value` (`{"percentage": 10}` or `{"fixed_amount": 5}`), `minimum_subtotal`,
/// `collection_ids`, `prefix`, `combines_with_other_discounts`, and optionally `expires_after_days`
/// or `ends_at` (else the coupon's own expiry).
pub async fn set(state: &AppState, account_id: Uuid, coupon_id: Uuid, body: &Value) -> AppResult<Value> {
    super::shop::connected(&state.db, account_id).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM coupons WHERE id = $1 AND account_id = $2)").bind(coupon_id).bind(account_id).fetch_one(&state.db).await?;
    if !exists {
        return Err(AppError::not_found("coupon"));
    }
    let mut b = body.clone();
    let Some(o) = b.as_object_mut() else { return Err(AppError::bad("send the discount template as an object")) };
    o.insert("key".into(), json!(format!("coupon:{coupon_id}")));
    let tpl: DiscountTemplate = serde_json::from_value(b.clone()).map_err(|e| AppError::bad(format!("discount template: {e}")))?;
    tpl.check()?;
    let saved = serde_json::to_value(&tpl).map_err(|e| AppError::Internal(e.into()))?;
    sqlx::query(
        "INSERT INTO shopify_coupon_templates (coupon_id, account_id, template) VALUES ($1, $2, $3)
         ON CONFLICT (coupon_id) DO UPDATE SET template = EXCLUDED.template, updated_at = now()",
    )
    .bind(coupon_id)
    .bind(account_id)
    .bind(&saved)
    .execute(&state.db)
    .await?;
    Ok(json!({"coupon_id": coupon_id, "template": saved, "note": "When this coupon has no free code for someone, each person gets their own single-use Shopify code from this template."}))
}

/// Unlink a coupon from the store. Codes already given stay theirs.
pub async fn remove(state: &AppState, account_id: Uuid, coupon_id: Uuid) -> AppResult<Value> {
    let n = sqlx::query("DELETE FROM shopify_coupon_templates WHERE coupon_id = $1 AND account_id = $2").bind(coupon_id).bind(account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("Shopify coupon"));
    }
    Ok(json!({"ok": true, "coupon_id": coupon_id}))
}

/// Linked coupons, for status.
pub async fn list(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('coupon_id', t.coupon_id, 'name', c.name, 'template', t.template - 'key', 'updated_at', t.updated_at)
         FROM shopify_coupon_templates t JOIN coupons c ON c.id = t.coupon_id WHERE t.account_id = $1 ORDER BY c.name LIMIT 100",
    )
    .bind(account_id)
    .fetch_all(db)
    .await
}

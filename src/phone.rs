//! Phone numbers: claim a number through the platform's Telnyx account (toll-free verification included),
//! or connect your own Telnyx/Twilio account. Also picks the SMS provider for each account.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::billing;
use crate::error::{AppError, AppResult};
use crate::models::normalize_phone;
use crate::providers::sms::{SmsProvider, TelnyxSms, TwilioSms};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PhoneNumberRow {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: Uuid,
    pub phone_number: String,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub kind: String,
    pub source: String,
    pub provider: String,
    #[serde(skip)]
    pub provider_id: Option<String>,
    pub status: String,
    #[serde(skip)]
    pub verification_id: Option<String>,
    pub verification_note: Option<String>,
    #[serde(skip)]
    pub monthly_price_micros: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub fn row_json(r: &PhoneNumberRow) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or_default();
    v["monthly_price"] = json!(billing::to_usd(r.monthly_price_micros));
    v
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ByoCredentials {
    Telnyx { api_key: String, messaging_profile_id: Option<String> },
    Twilio { account_sid: String, auth_token: String },
}

fn platform_key(state: &AppState) -> Option<&str> {
    state.cfg.telnyx_api_key.as_deref()
}

/// Fake number inventory for local development only (log SMS provider on a local URL).
fn dev_inventory(state: &AppState) -> bool {
    state.sms.name() == "log" && ["http://localhost", "http://127.0.0.1", "https://pl.test"].iter().any(|p| state.cfg.public_url().starts_with(p))
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Vec<PhoneNumberRow>> {
    Ok(sqlx::query_as("SELECT * FROM phone_numbers WHERE account_id = $1 AND status <> 'released' ORDER BY created_at DESC")
        .bind(account_id)
        .fetch_all(&state.db)
        .await?)
}

/// The provider and sending number for an account, or why it can't text yet.
pub async fn sender(state: &AppState, account_id: Uuid) -> AppResult<(Arc<dyn SmsProvider>, String)> {
    let (mode, enc, from): (String, Option<String>, Option<String>) = sqlx::query_as("SELECT sms_mode, sms_byo_credentials, sms_from FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&state.db)
        .await?;
    let from = from.ok_or_else(|| AppError::bad("set up a texting number in Settings → SMS first"))?;
    if mode == "byo" {
        let enc = enc.ok_or_else(|| AppError::bad("reconnect your SMS provider in Settings → SMS"))?;
        let creds: ByoCredentials = serde_json::from_str(&crate::crypto::decrypt(&state.cfg.secret_key, &enc)?)
            .map_err(|_| AppError::bad("reconnect your SMS provider in Settings → SMS"))?;
        let p: Arc<dyn SmsProvider> = match creds {
            ByoCredentials::Telnyx { api_key, messaging_profile_id } => Arc::new(TelnyxSms::new(state.http.clone(), api_key, messaging_profile_id)),
            ByoCredentials::Twilio { account_sid, auth_token } => Arc::new(TwilioSms::new(state.http.clone(), account_sid, auth_token)),
        };
        return Ok((p, from));
    }
    if state.cfg.hosted && state.sms.name() != "log" {
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM phone_numbers WHERE account_id = $1 AND phone_number = $2 AND status <> 'released'")
            .bind(account_id)
            .bind(&from)
            .fetch_optional(&state.db)
            .await?;
        match status.as_deref() {
            Some("verified" | "active") => {}
            Some("needs_verification" | "verification_pending") => {
                return Err(AppError::bad("your toll-free number is waiting on carrier verification; texts can send once it's approved"))
            }
            _ => return Err(AppError::bad("claim or connect a texting number in Settings → SMS first")),
        }
    }
    Ok((state.sms.clone(), from))
}

pub async fn sms_setup_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let (mode, provider, from, token): (String, Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT sms_mode, sms_byo_provider, sms_from, webhook_token FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let can_send = sender(state, account_id).await.is_ok();
    let prov = if mode == "byo" { provider.clone().unwrap_or_default() } else { state.sms.name().to_string() };
    let hook_provider = if prov == "twilio" { "twilio" } else { "telnyx" };
    Ok(json!({
        "mode": mode,
        "provider": prov,
        "from_number": from,
        "can_send": can_send,
        "inbound_webhook_url": format!("{}/webhooks/{hook_provider}/{}", state.cfg.public_url(), token.unwrap_or_default()),
        "platform_available": platform_key(state).is_some() || dev_inventory(state),
    }))
}

async fn telnyx(state: &AppState, key: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> AppResult<Value> {
    let mut req = state.http.request(method, format!("https://api.telnyx.com/v2/{path}")).bearer_auth(key);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.map_err(|e| AppError::Internal(e.into()))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        let detail = v["errors"][0]["detail"].as_str().or(v["errors"][0]["title"].as_str()).unwrap_or("request failed");
        return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
            AppError::bad("Telnyx rejected the API key")
        } else if status.is_client_error() {
            AppError::BadRequest(format!("Telnyx: {detail}"))
        } else {
            AppError::Internal(anyhow::anyhow!("Telnyx {status}: {detail}"))
        });
    }
    Ok(v)
}

/// Search numbers the platform can claim.
pub async fn available(state: &AppState, kind: &str, area_code: Option<&str>) -> AppResult<Value> {
    let price = billing::to_usd(billing::prices(state).number_monthly);
    if kind != "toll_free" && platform_key(state).is_some() {
        return Err(AppError::bad("local numbers need 10DLC brand registration; claim a toll-free number, or connect your own Telnyx or Twilio account to use a local number"));
    }
    let Some(key) = platform_key(state) else {
        if dev_inventory(state) {
            // Dev mode: fake inventory so the flow can be tried end to end.
            let prefix = if kind == "toll_free" { "+1833555".to_string() } else { format!("+1{}555", area_code.unwrap_or("503")) };
            let taken: Vec<String> = sqlx::query_scalar("SELECT phone_number FROM phone_numbers WHERE status <> 'released'").fetch_all(&state.db).await?;
            let data: Vec<Value> = (1..=200)
                .map(|i| format!("{prefix}{:04}", 100 + i))
                .filter(|n| !taken.contains(n))
                .take(5)
                .map(|n| json!({"phone_number": n, "type": kind, "region": null, "monthly_price": price}))
                .collect();
            return Ok(json!({"data": data}));
        }
        return Err(AppError::bad("claiming numbers isn't available on this server; connect your own Telnyx or Twilio account instead"));
    };
    let mut q = format!("available_phone_numbers?filter[country_code]=US&filter[features][]=sms&filter[limit]=10&filter[phone_number_type]={}", if kind == "toll_free" { "toll_free" } else { "local" });
    if let Some(ac) = area_code.filter(|a| a.len() == 3 && a.chars().all(|c| c.is_ascii_digit())) {
        q.push_str(&format!("&filter[national_destination_code]={ac}"));
    }
    let v = telnyx(state, key, reqwest::Method::GET, &q, None).await?;
    let data: Vec<Value> = v["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|n| {
            let region = n["region_information"].as_array().and_then(|r| r.iter().find(|x| x["region_type"] == "rate_center" || x["region_type"] == "state")).and_then(|x| x["region_name"].as_str()).map(String::from);
            json!({"phone_number": n["phone_number"], "type": kind, "region": region, "monthly_price": price})
        })
        .collect();
    Ok(json!({"data": data}))
}

/// Claim a number: order it from Telnyx, charge the first month, make it the account's SMS number.
pub async fn claim(state: &AppState, account_id: Uuid, phone: &str) -> AppResult<PhoneNumberRow> {
    let phone = normalize_phone(phone).ok_or_else(|| AppError::bad("not a valid phone number"))?;
    let kind = if ["+1800", "+1833", "+1844", "+1855", "+1866", "+1877", "+1888"].iter().any(|p| phone.starts_with(p)) { "toll_free" } else { "local" };
    if kind != "toll_free" && platform_key(state).is_some() {
        return Err(AppError::bad("local numbers need 10DLC brand registration; claim a toll-free number, or connect your own account to use a local number"));
    }
    let price = billing::prices(state).number_monthly;
    billing::ensure_funds(state, account_id, price, "A phone number").await?;
    let provider_id = match platform_key(state) {
        Some(key) => {
            let mut body = json!({"phone_numbers": [{"phone_number": phone}]});
            if let Some(p) = &state.cfg.telnyx_messaging_profile_id {
                body["messaging_profile_id"] = json!(p);
            }
            let order = telnyx(state, key, reqwest::Method::POST, "number_orders", Some(body)).await?;
            order["data"]["id"].as_str().map(String::from)
        }
        None if dev_inventory(state) => Some(format!("dev-{}", Uuid::now_v7())),
        None => return Err(AppError::bad("claiming numbers isn't available on this server")),
    };
    // Toll-free numbers must pass carrier verification before they can text; local numbers are usable
    // in dev, and in production need 10DLC registration handled by the platform's brand.
    let status = if kind == "toll_free" && state.sms.name() != "log" { "needs_verification" } else { "active" };
    let id = Uuid::now_v7();
    let row: PhoneNumberRow = sqlx::query_as(
        "INSERT INTO phone_numbers (id, account_id, phone_number, type, source, provider, provider_id, status, monthly_price_micros, next_charge_at)
         VALUES ($1, $2, $3, $4, 'purchased', 'telnyx', $5, $6, $7, now() + interval '1 month') RETURNING *",
    )
    .bind(id)
    .bind(account_id)
    .bind(&phone)
    .bind(kind)
    .bind(&provider_id)
    .bind(status)
    .bind(price)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match AppError::from(e) {
        AppError::Conflict(_) => AppError::Conflict("that number is already taken".into()),
        other => other,
    })?;
    if state.cfg.hosted {
        billing::post_entry(&state.db, account_id, "number", -price, &format!("Phone number {phone} (first month)"), &format!("{id}:first")).await?;
    }
    sqlx::query("UPDATE accounts SET sms_from = $2, sms_mode = 'platform' WHERE id = $1").bind(account_id).bind(&phone).execute(&state.db).await?;
    Ok(row)
}

pub async fn release(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let row: PhoneNumberRow = sqlx::query_as("SELECT * FROM phone_numbers WHERE id = $1 AND account_id = $2 AND status <> 'released'")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("phone number"))?;
    if row.source == "purchased" {
        if let Some(key) = platform_key(state) {
            let found = telnyx(state, key, reqwest::Method::GET, &format!("phone_numbers?filter[phone_number]={}", urlenc(&row.phone_number)), None).await?;
            if let Some(nid) = found["data"][0]["id"].as_str() {
                telnyx(state, key, reqwest::Method::DELETE, &format!("phone_numbers/{nid}"), None).await?;
            }
        }
    }
    sqlx::query("UPDATE phone_numbers SET status = 'released', released_at = now() WHERE id = $1").bind(id).execute(&state.db).await?;
    sqlx::query("UPDATE accounts SET sms_from = NULL WHERE id = $1 AND sms_from = $2").bind(account_id).bind(&row.phone_number).execute(&state.db).await?;
    if row.source == "connected" {
        sqlx::query("UPDATE accounts SET sms_mode = 'platform', sms_byo_provider = NULL, sms_byo_credentials = NULL WHERE id = $1").bind(account_id).execute(&state.db).await?;
    }
    Ok(())
}

fn urlenc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Debug, Deserialize)]
pub struct TollFreeVerification {
    pub business_name: String,
    pub website: String,
    pub address1: String,
    #[serde(default)]
    pub address2: Option<String>,
    pub city: String,
    pub state: String,
    pub zip: String,
    pub contact_first_name: String,
    pub contact_last_name: String,
    pub contact_email: String,
    pub contact_phone: String,
    pub use_case: String,
    pub use_case_summary: String,
    pub sample_message: String,
    pub opt_in_description: String,
    pub opt_in_image_url: String,
    pub monthly_volume: String,
}

/// Submit carrier verification for a claimed toll-free number.
pub async fn submit_verification(state: &AppState, account_id: Uuid, id: Uuid, v: TollFreeVerification) -> AppResult<PhoneNumberRow> {
    let row: PhoneNumberRow = sqlx::query_as("SELECT * FROM phone_numbers WHERE id = $1 AND account_id = $2 AND status <> 'released'")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("phone number"))?;
    if row.kind != "toll_free" || row.source != "purchased" {
        return Err(AppError::bad("only toll-free numbers claimed through Postlark need this verification"));
    }
    for (name, val) in [("business name", &v.business_name), ("website", &v.website), ("address", &v.address1), ("city", &v.city), ("state", &v.state), ("ZIP", &v.zip),
        ("contact name", &v.contact_first_name), ("contact email", &v.contact_email), ("contact phone", &v.contact_phone), ("use case summary", &v.use_case_summary),
        ("sample message", &v.sample_message), ("opt-in description", &v.opt_in_description), ("opt-in screenshot URL", &v.opt_in_image_url)]
    {
        if val.trim().is_empty() {
            return Err(AppError::bad(format!("{name} is required")));
        }
    }
    if !v.sample_message.to_lowercase().contains("stop") {
        return Err(AppError::bad("the sample message must include opt-out language like \"Reply STOP to opt out\""));
    }
    let contact_phone = normalize_phone(&v.contact_phone).ok_or_else(|| AppError::bad("contact phone isn't a valid number"))?;
    let verification_id = match platform_key(state) {
        Some(key) => {
            let body = json!({
                "businessName": v.business_name, "corporateWebsite": v.website,
                "businessAddr1": v.address1, "businessAddr2": v.address2.clone().unwrap_or_default(),
                "businessCity": v.city, "businessState": v.state, "businessZip": v.zip,
                "businessContactFirstName": v.contact_first_name, "businessContactLastName": v.contact_last_name,
                "businessContactEmail": v.contact_email, "businessContactPhone": contact_phone,
                "messageVolume": v.monthly_volume, "phoneNumbers": [{"phoneNumber": row.phone_number}],
                "useCase": v.use_case, "useCaseSummary": v.use_case_summary, "productionMessageContent": v.sample_message,
                "optInWorkflow": v.opt_in_description, "optInWorkflowImageURLs": [{"url": v.opt_in_image_url}],
                "additionalInformation": "Marketing messages to customers who opted in through the business's own website sign-up form.",
                "isvReseller": "Postlark",
            });
            let res = telnyx(state, key, reqwest::Method::POST, "messaging_tollfree/verification/requests", Some(body)).await?;
            res["id"].as_str().or(res["data"]["id"].as_str()).map(String::from)
        }
        None => Some(format!("dev-{}", Uuid::now_v7())),
    };
    let status = if platform_key(state).is_some() { "verification_pending" } else { "verified" };
    Ok(sqlx::query_as("UPDATE phone_numbers SET status = $2, verification_id = $3, verification_note = NULL WHERE id = $1 RETURNING *")
        .bind(id)
        .bind(status)
        .bind(verification_id)
        .fetch_one(&state.db)
        .await?)
}

/// Scheduler: poll pending toll-free verifications.
pub async fn poll_verifications(state: &AppState) -> anyhow::Result<()> {
    let Some(key) = platform_key(state) else { return Ok(()) };
    let pending: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, verification_id FROM phone_numbers WHERE status = 'verification_pending' AND verification_id IS NOT NULL LIMIT 50")
        .fetch_all(&state.db)
        .await?;
    for (id, vid) in pending {
        let Ok(v) = telnyx(state, key, reqwest::Method::GET, &format!("messaging_tollfree/verification/requests/{vid}"), None).await else { continue };
        let st = v["verificationStatus"].as_str().or(v["data"]["verificationStatus"].as_str()).unwrap_or("");
        let (status, note) = match st {
            "Verified" => ("verified", None),
            "Rejected" => ("rejected", v["reason"].as_str().or(v["data"]["reason"].as_str()).map(String::from)),
            _ => continue,
        };
        sqlx::query("UPDATE phone_numbers SET status = $2, verification_note = $3 WHERE id = $1").bind(id).bind(status).bind(note).execute(&state.db).await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ConnectBody {
    pub provider: String,
    pub phone_number: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub messaging_profile_id: Option<String>,
    #[serde(default)]
    pub account_sid: Option<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
}

/// Connect the customer's own provider: verify the credentials own the number, then store them encrypted.
pub async fn connect(state: &AppState, account_id: Uuid, b: ConnectBody) -> AppResult<()> {
    let phone = normalize_phone(&b.phone_number).ok_or_else(|| AppError::bad("enter the number in a format like +15035550100"))?;
    let creds = match b.provider.as_str() {
        "telnyx" => {
            let key = b.api_key.filter(|k| !k.trim().is_empty()).ok_or_else(|| AppError::bad("enter your Telnyx API key"))?;
            let found = telnyx(state, key.trim(), reqwest::Method::GET, &format!("phone_numbers?filter[phone_number]={}", urlenc(&phone)), None).await?;
            if found["data"].as_array().is_none_or(|a| a.is_empty()) {
                return Err(AppError::bad(format!("{phone} isn't in that Telnyx account")));
            }
            let profile = b.messaging_profile_id.filter(|p| !p.trim().is_empty()).or_else(|| found["data"][0]["messaging_profile_id"].as_str().map(String::from));
            ByoCredentials::Telnyx { api_key: key.trim().to_string(), messaging_profile_id: profile }
        }
        "twilio" => {
            let sid = b.account_sid.filter(|s| s.starts_with("AC")).ok_or_else(|| AppError::bad("enter your Twilio Account SID (starts with AC)"))?;
            let token = b.auth_token.filter(|t| !t.trim().is_empty()).ok_or_else(|| AppError::bad("enter your Twilio auth token"))?;
            let res = state
                .http
                .get(format!("https://api.twilio.com/2010-04-01/Accounts/{sid}/IncomingPhoneNumbers.json"))
                .query(&[("PhoneNumber", phone.as_str())])
                .basic_auth(&sid, Some(token.trim()))
                .send()
                .await
                .map_err(|e| AppError::Internal(e.into()))?;
            if res.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(AppError::bad("Twilio rejected the Account SID or auth token"));
            }
            let v: Value = res.json().await.unwrap_or_default();
            if v["incoming_phone_numbers"].as_array().is_none_or(|a| a.is_empty()) {
                return Err(AppError::bad(format!("{phone} isn't in that Twilio account")));
            }
            ByoCredentials::Twilio { account_sid: sid, auth_token: token.trim().to_string() }
        }
        _ => return Err(AppError::bad("provider must be telnyx or twilio")),
    };
    let enc = crate::crypto::encrypt(&state.cfg.secret_key, &serde_json::to_string(&creds)?)?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE phone_numbers SET status = 'released', released_at = now() WHERE account_id = $1 AND source = 'connected' AND status <> 'released'").bind(account_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO phone_numbers (id, account_id, phone_number, type, source, provider, status) VALUES ($1, $2, $3, $4, 'connected', $5, 'active')",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(&phone)
    .bind(if phone.starts_with("+18") { "toll_free" } else { "local" })
    .bind(&b.provider)
    .execute(&mut *tx)
    .await
    .map_err(|e| match AppError::from(e) {
        AppError::Conflict(_) => AppError::Conflict("that number is already connected to another account".into()),
        other => other,
    })?;
    sqlx::query("UPDATE accounts SET sms_mode = 'byo', sms_byo_provider = $2, sms_byo_credentials = $3, sms_from = $4 WHERE id = $1")
        .bind(account_id)
        .bind(&b.provider)
        .bind(enc)
        .bind(&phone)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn disconnect(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let released: Vec<String> = sqlx::query_scalar("UPDATE phone_numbers SET status = 'released', released_at = now() WHERE account_id = $1 AND source = 'connected' AND status <> 'released' RETURNING phone_number")
        .bind(account_id)
        .fetch_all(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET sms_mode = 'platform', sms_byo_provider = NULL, sms_byo_credentials = NULL, sms_from = CASE WHEN sms_from = ANY($2) THEN NULL ELSE sms_from END WHERE id = $1")
        .bind(account_id)
        .bind(&released)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

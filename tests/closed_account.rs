//! A closed account (its only owner's login was given up by "start fresh") does nothing and costs
//! nothing: no texts or emails on any channel, no jobs, no rent, no renewals, and a late payment
//! doesn't reopen it. Opt-outs still count. Needs TEST_DATABASE_URL (see tests/common).

// Every test here builds its own configured harness, so the shared `harness!` goes unused.
#![allow(unused_macros)]

#[macro_use]
mod common;

use axum::http::StatusCode;
use base64::Engine;
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use uuid::Uuid;

fn telnyx_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A harness whose platform Telnyx key is set, so inbound texts are verified.
async fn harness_signed() -> Option<Harness> {
    Harness::with(|c| c.telnyx_public_key = Some(b64(&telnyx_key().verifying_key().to_bytes()))).await
}

/// A signed Telnyx `message.received` webhook (Telnyx's documented shape and Ed25519 signature).
async fn text_in(h: &Harness, id: &str, from: &str, to: &str, text: &str) {
    let body = json!({"data": {"event_type": "message.received", "id": format!("evt-{id}"), "record_type": "event", "payload": {
        "id": id, "direction": "inbound", "type": "SMS", "text": text, "received_at": chrono::Utc::now().to_rfc3339(),
        "from": {"phone_number": from}, "to": [{"phone_number": to}], "media": [], "record_type": "message"
    }}, "meta": {"attempt": 1}})
    .to_string();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = b64(&telnyx_key().sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let (s, _, out) = h
        .raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json"), ("telnyx-timestamp", &ts), ("telnyx-signature-ed25519", &sig)], &body)
        .await;
    assert_eq!(s, StatusCode::OK, "{out}");
}

fn texts_to(h: &Harness, phone: &str) -> Vec<String> {
    h.state.sms.sent().into_iter().filter(|s| s.to == phone).map(|s| s.body).collect()
}

/// A phone number in a zone where it's the middle of the day, so texts aren't held for quiet hours.
fn daytime_number(n: u32) -> String {
    [format!("+1808555{n:04}"), format!("+1503555{n:04}"), format!("+1212555{n:04}"), format!("+4420794{n:05}"), format!("+9198123{n:05}"), format!("+8190123{n:05}")]
        .into_iter()
        .find(|c| dumbmail::features::sendtime::zone::phone_zone(c).is_some_and(|tz| (11..17).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz)))))
        .expect("some zone is in the middle of the day")
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// Close the account the way "start fresh" does: give up its only owner's login.
async fn close(h: &Harness, account: Uuid) {
    let owner: Uuid = sqlx::query_scalar("SELECT user_id FROM memberships WHERE account_id = $1 AND role = 'owner'").bind(account).fetch_one(&h.state.db).await.unwrap();
    let mut tx = h.state.db.begin().await.unwrap();
    let closed = dumbmail::features::team::google_link::give_up_login(&mut tx, owner).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(closed, vec![account]);
}

/// Run the main job lane only (the background lane would release the closed account's numbers).
async fn run_main_jobs(h: &Harness) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE lane = $1 AND run_at <> 'infinity'").bind(dumbmail::jobs::MAIN).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::MAIN).await.unwrap();
}

#[tokio::test]
async fn texts_to_a_closed_account_get_no_reply_but_opt_outs_still_count() {
    let Some(mut h) = harness_signed().await else { return };
    h.signup().await;
    h.credit(5.0).await;
    let ours = h.claim_number().await;
    let acct = account_id(&h).await;
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"program_name": "Acme", "message_frequency": "Up to 4 msgs/month", "support_contact": "help@acme.test",
        "default_reply_enabled": true, "default_reply": "Thanks, we'll get back to you.", "open_hour": 0, "close_hour": 24, "open_days": [1, 2, 3, 4, 5, 6, 7]}))).await;
    // While open, a text gets the automatic reply.
    let (a, b, c) = (daytime_number(1), daytime_number(2), daytime_number(3));
    text_in(&h, "in-1", &a, &ours, "Do you ship to Canada?").await;
    run_main_jobs(&h).await;
    assert_eq!(texts_to(&h, &a).len(), 1, "the reply works before closing");

    close(&h, acct).await;
    text_in(&h, "in-2", &b, &ours, "Hello?").await;
    run_main_jobs(&h).await;
    assert!(texts_to(&h, &b).is_empty(), "no reply from a closed account");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound WHERE provider_message_id = 'in-2'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0, "no conversation for a closed account");
    // STOP still counts.
    text_in(&h, "in-3", &c, &ours, "STOP").await;
    let opted_out: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_opt_outs WHERE phone_number = $1)").bind(&c).fetch_one(&h.state.db).await.unwrap();
    assert!(opted_out);
    assert!(texts_to(&h, &c).is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn a_queued_campaign_and_a_waiting_flow_run_send_nothing() {
    let Some(mut h) = harness_signed().await else { return };
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await;
    let acct = account_id(&h).await;
    // A text campaign started but not yet sent.
    let camp = daytime_number(11);
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Campaign"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": camp, "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "Sale today. Reply STOP to opt out."}}))).await;
    let cid: Uuid = c["id"].as_str().unwrap().parse().unwrap();
    run_main_jobs(&h).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    // A flow run waiting behind a delay before its text.
    let wait = daytime_number(12);
    let fl = h.ok("POST", "/api/lists", Some(json!({"name": "Flow"}))).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Later", "trigger": {"type": "list", "list_id": fl["id"]}, "definition": {"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "hours", "next": "s"},
        {"id": "s", "type": "sms", "content": {"body": "Big sale today! Reply STOP to opt out."}, "next": null}
    ]}}))).await;
    let fid: Uuid = flow["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": wait, "sms_consent": "subscribed", "list_ids": [fl["id"]]}))).await;

    close(&h, acct).await;
    // Even if the campaign and the flow were still on (closing stops them; this checks the rule
    // that holds whatever their state), nothing goes out.
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(cid).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE flows SET status = 'live' WHERE id = $1").bind(fid).execute(&h.state.db).await.unwrap();
    run_main_jobs(&h).await;
    run_main_jobs(&h).await;
    assert!(texts_to(&h, &camp).is_empty(), "{:?}", texts_to(&h, &camp));
    assert!(texts_to(&h, &wait).is_empty(), "{:?}", texts_to(&h, &wait));
    // The send path itself refuses a closed account, tests included.
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    assert!(account.closed_at.is_some());
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = $1").bind(&camp).fetch_one(&h.state.db).await.unwrap();
    let profile = dumbmail::features::inbox::replies::load_profile(&h.state.db, acct, pid).await.unwrap();
    let content = json!({"body": "hi"});
    let spec = |is_test: bool| dumbmail::sending::SendSpec {
        account: &account,
        profile: &profile,
        channel: "sms",
        content: &content,
        campaign_id: None,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test,
        message_id: None,
        to_override: None,
    };
    assert!(matches!(dumbmail::sending::send(&h.state, spec(false)).await.unwrap(), dumbmail::sending::Outcome::Skipped("account closed")));
    assert!(dumbmail::sending::send(&h.state, spec(true)).await.is_err());
    assert!(texts_to(&h, &camp).is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn a_closed_account_pays_no_rent_or_renewal_and_a_late_payment_does_not_reopen_it() {
    let Some(mut h) = Harness::with(|c| {
        c.stripe_secret_key = Some("sk_test_closed".into());
        // Nothing should reach Stripe; if anything tried, it would fail here.
        c.stripe_api_base = "http://127.0.0.1:9/v1".into();
    })
    .await
    else {
        return;
    };
    h.signup().await;
    h.credit(10.0).await;
    let number = h.claim_number().await;
    let acct = account_id(&h).await;
    sqlx::query(
        "UPDATE accounts SET email_plan_tier = 1, email_plan_period_start = now() - interval '31 days', email_plan_period_end = now() - interval '1 minute',
                stripe_customer_id = 'cus_test', stripe_payment_method_id = 'pm_test'
         WHERE id = $1",
    )
    .bind(acct)
    .execute(&h.state.db)
    .await
    .unwrap();
    close(&h, acct).await;
    let before = dumbmail::billing::balance(&h.state.db, acct).await.unwrap();

    // Rent comes due: nothing is charged.
    sqlx::query("UPDATE phone_numbers SET next_charge_at = now() - interval '1 minute' WHERE phone_number = $1").bind(&number).execute(&h.state.db).await.unwrap();
    dumbmail::billing::charge_numbers(&h.state).await.unwrap();
    assert_eq!(dumbmail::billing::balance(&h.state.db, acct).await.unwrap(), before, "no rent");
    // Renewal comes due, even with the cancellation somehow cleared: no attempt is made.
    sqlx::query("UPDATE accounts SET email_plan_cancel_at_period_end = false WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::subscription::run_renewals(&h.state).await.unwrap();
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM plan_renewals WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(attempts, 0, "no renewal attempt");
    sqlx::query("UPDATE accounts SET email_plan_cancel_at_period_end = true, email_plan_tier = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();

    // A Checkout completion that arrives after closure is kept as credit and doesn't restart a plan.
    let session = json!({"id": "cs_late", "amount_total": 1000, "metadata": {"tier": "1", "from_tier": "0"}});
    dumbmail::subscription::apply_checkout(&h.state.db, acct, &session).await.unwrap();
    let (tier, cancel): (Option<i16>, bool) =
        sqlx::query_as("SELECT email_plan_tier, email_plan_cancel_at_period_end FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((tier, cancel), (None, true), "the plan stays ended and cancelled");
    let credited: bool = sqlx::query_scalar("SELECT credited_to_balance FROM plan_payments WHERE stripe_ref = 'cs_late'").fetch_one(&h.state.db).await.unwrap();
    assert!(credited, "recorded for reconciliation");

    // Its numbers are released in the background, once.
    dumbmail::jobs::drain(&h.state).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM phone_numbers WHERE phone_number = $1").bind(&number).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "released");
    h.cleanup().await;
}

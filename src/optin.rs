//! Double opt-in. Signing up through a form to a double opt-in list sends a confirmation email. The
//! person joins the list, with the "Subscribed to List" event that starts welcome flows, only after
//! confirming. Typos, fake addresses and bots never reach a campaign, so they can't bounce or complain.
//!
//! Confirmation emails aren't billed and don't count toward the warm-up limit (or raise it). They do count
//! toward the bounce and complaint rates. One address gets at most one every 24 hours and an account at most
//! `MAX_PER_HOUR` (one visitor `MAX_PER_IP_HOUR`, one inbox `MAX_PER_INBOX_DAY` a day however the address is
//! spelled), so a bot filling in a form can't use DumbMail to flood someone's inbox. Addresses that
//! bounced or complained before get none. When no confirmation can be sent, the sign-up is refused rather
//! than let through unconfirmed.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::audience::{self, NewEvent};
use crate::error::AppResult;
use crate::models::Account;
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

/// How long a confirmation link works.
pub const LINK_DAYS: i32 = 7;
/// An address waiting to confirm isn't sent another email for this long.
const RESEND_AFTER_HOURS: i32 = 24;
/// Most confirmations one inbox (`email_key`) gets from an account in a day, however the address is spelled.
const MAX_PER_INBOX_DAY: i64 = 3;
/// Most confirmation emails one account sends in an hour.
const MAX_PER_HOUR: i64 = 200;
/// Most confirmation emails one visitor (IP address) can trigger in an hour, across all accounts.
const MAX_PER_IP_HOUR: i64 = 5;

pub enum Started {
    /// The person has to confirm (an email went out now or in the last day).
    Pending,
    /// No confirmation can be sent (no verified sender, or sending is paused). The sign-up is refused.
    Unavailable(String),
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn token(state: &AppState, id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("c{}", id.simple()))
}

pub fn id_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    crate::auth::verify(&state.cfg.secret_key, token)?.strip_prefix('c')?.parse().ok()
}

/// The inbox an address most likely reaches: lowercased, `+tag` removed and, for Gmail, dots removed too,
/// so `v.ictim+1@gmail.com`, `victim+2@shop.test` and `victim@googlemail.com` count as their inbox. Used
/// only to cap how many confirmations one inbox gets (`MAX_PER_INBOX_DAY`); whether a confirmation is
/// already waiting is judged on the exact address, so a made-up variant can't hold back the real one.
pub fn email_key(email: &str) -> String {
    let e = email.trim().to_lowercase();
    let Some((local, domain)) = e.rsplit_once('@') else { return e };
    let domain = if domain == "googlemail.com" { "gmail.com" } else { domain };
    let local = local.split('+').next().unwrap_or(local);
    let local = if domain == "gmail.com" { local.replace('.', "") } else { local.to_string() };
    format!("{local}@{domain}")
}

/// Ask `email` to confirm joining `list_id`. `properties` are kept for the sign-up event; `ip` is the
/// visitor's address, for the per-visitor limit.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    state: &AppState,
    account: &Account,
    profile_id: Uuid,
    email: &str,
    list_id: Uuid,
    form_id: Uuid,
    properties: Value,
    ip: Option<&str>,
) -> AppResult<Started> {
    if account.sending_paused_at.is_some() {
        return Ok(Started::Unavailable("email sending is paused".into()));
    }
    let Some(from_email) = account.from_email.clone() else { return Ok(Started::Unavailable("no sender email".into())) };
    if state.email.name() == "ses" {
        if let Err(e) = crate::sending::check_sender_domain_name(account, &from_email) {
            return Ok(Started::Unavailable(e));
        }
    }
    // An address that hard bounced or reported spam gets no more email, a confirmation included.
    let suppressed: bool = sqlx::query_scalar("SELECT email_suppressed_reason IS NOT NULL FROM profiles WHERE id = $1").bind(profile_id).fetch_one(&state.db).await?;
    if suppressed {
        return Ok(Started::Pending);
    }
    let key = email_key(email);
    let ip_hash = ip.filter(|s| !s.is_empty()).map(crate::auth::sha256_hex);

    // Checked and recorded under locks on the account (its inbox and hourly checks) and the visitor, always
    // taken in that order, so simultaneous sign-ups can't get past a check together.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("optin:{}", account.id)).execute(&mut *tx).await?;
    if let Some(h) = &ip_hash {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("optin-ip:{h}")).execute(&mut *tx).await?;
    }
    let waiting: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM optin_confirmations WHERE account_id = $1 AND email = $2 AND confirmed_at IS NULL AND created_at > now() - make_interval(hours => $3))",
    )
    .bind(account.id)
    .bind(email)
    .bind(RESEND_AFTER_HOURS)
    .fetch_one(&mut *tx)
    .await?;
    if waiting {
        return Ok(Started::Pending);
    }
    let to_inbox: i64 = sqlx::query_scalar("SELECT count(*) FROM optin_confirmations WHERE account_id = $1 AND email_key = $2 AND created_at > now() - interval '24 hours'")
        .bind(account.id)
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;
    if to_inbox >= MAX_PER_INBOX_DAY {
        tracing::warn!(account = %account.id, "double opt-in: per-inbox daily limit reached; not sending another");
        return Ok(Started::Pending);
    }
    if let Some(h) = &ip_hash {
        let from_ip: i64 = sqlx::query_scalar("SELECT count(*) FROM optin_confirmations WHERE ip_hash = $1 AND created_at > now() - interval '1 hour'")
            .bind(h)
            .fetch_one(&mut *tx)
            .await?;
        if from_ip >= MAX_PER_IP_HOUR {
            tracing::warn!(account = %account.id, "double opt-in: per-visitor limit reached; not sending another");
            return Ok(Started::Pending);
        }
    }
    let last_hour: i64 = sqlx::query_scalar("SELECT count(*) FROM optin_confirmations WHERE account_id = $1 AND created_at > now() - interval '1 hour'")
        .bind(account.id)
        .fetch_one(&mut *tx)
        .await?;
    if last_hour >= MAX_PER_HOUR {
        // Tell the visitor to try later rather than claim an email is on its way.
        return Ok(Started::Unavailable("hourly confirmation limit reached".into()));
    }

    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO optin_confirmations (id, account_id, profile_id, list_id, form_id, email, email_key, ip_hash, properties) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(account.id)
    .bind(profile_id)
    .bind(list_id)
    .bind(form_id)
    .bind(email)
    .bind(&key)
    .bind(&ip_hash)
    .bind(&properties)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let url = format!("{}/c/{}", crate::domains::link_base(state, account), token(state, id));
    let message_id = Uuid::now_v7();
    let (subject, html, text) = confirmation_email(account, &url);
    let outgoing = OutgoingEmail {
        message_id,
        to: email.to_string(),
        from_name: account.from_name.clone().unwrap_or_else(|| account.name.clone()),
        from_email,
        reply_to: account.reply_to.clone(),
        subject: subject.clone(),
        html,
        text,
        unsubscribe_url: None,
    };
    crate::sending::throttle_email(state).await;
    match state.email.send(&outgoing).await {
        Ok(provider_id) => {
            // Kept like any sent email, so a bounce or complaint suppresses the address and counts.
            sqlx::query(
                "INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject, status, provider_message_id, sent_at)
                 VALUES ($1, $2, $3, 'email', $4, $5, 'sent', $6, now())",
            )
            .bind(message_id)
            .bind(account.id)
            .bind(profile_id)
            .bind(email)
            .bind(&subject)
            .bind(&provider_id)
            .execute(&state.db)
            .await?;
            crate::deliverability::record_confirmation_send(&state.db, message_id, &provider_id, account.id).await?;
            Ok(Started::Pending)
        }
        Err(e) => {
            sqlx::query("DELETE FROM optin_confirmations WHERE id = $1").bind(id).execute(&state.db).await?;
            Ok(Started::Unavailable(format!("couldn't send the confirmation email: {e:#}")))
        }
    }
}

/// Subject, HTML and text of a confirmation email.
pub fn confirmation_email(account: &Account, url: &str) -> (String, String, String) {
    let name = esc(&account.name);
    let color = crate::render::safe_color(&account.brand_color, "#1f6f5c");
    let address = account.address.as_deref().map(esc).map(|a| format!(" · {a}")).unwrap_or_default();
    let logo = account
        .logo_url
        .as_deref()
        .filter(|u| u.starts_with("https://"))
        .map(|u| format!(r#"<img src="{}" alt="{name}" style="max-height:48px;max-width:200px;margin:0 0 20px;display:block">"#, esc(u)))
        .unwrap_or_default();
    let subject = format!("Confirm your subscription to {}", account.name);
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"></head>
<body style="margin:0;background:#f4f4f5;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:32px 16px">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:480px;background:#ffffff;border-radius:12px"><tr><td style="padding:32px">
{logo}<h1 style="font-size:22px;line-height:1.3;margin:0 0 12px;color:#111827">Confirm your subscription</h1>
<p style="font-size:16px;line-height:1.5;color:#4b5563;margin:0 0 24px">Tap the button to start getting emails from {name}.</p>
<a href="{url}" style="display:inline-block;background:{color};color:#ffffff;text-decoration:none;font-weight:600;font-size:16px;padding:12px 24px;border-radius:8px">Yes, subscribe me</a>
<p style="font-size:13px;line-height:1.5;color:#6b7280;margin:24px 0 0">If you didn't sign up, ignore this email and you won't hear from us. The link works for {LINK_DAYS} days.</p>
</td></tr></table>
<p style="font-size:12px;color:#9ca3af;margin:16px 0 0">{name}{address}</p>
</td></tr></table></body></html>"#,
        url = esc(url),
    );
    let text = format!(
        "Confirm your subscription\n\nOpen this link to start getting emails from {}:\n{url}\n\nIf you didn't sign up, ignore this email and you won't hear from us. The link works for {LINK_DAYS} days.\n\n--\n{}{}\n",
        account.name,
        account.name,
        account.address.as_deref().map(|a| format!(", {a}")).unwrap_or_default(),
    );
    (subject, html, text)
}

pub enum Confirmed {
    /// Joined the list now.
    Joined(Uuid),
    /// Still waiting for the person to confirm.
    Waiting(Uuid),
    /// Confirmed before.
    Already(Uuid),
    Expired(Uuid),
    NotFound,
}

/// Whether confirmation `id` is still waiting, without changing anything (for the confirm page).
pub async fn status(state: &AppState, id: Uuid) -> AppResult<Confirmed> {
    let row: Option<(Uuid, bool, bool)> = sqlx::query_as(
        "SELECT account_id, confirmed_at IS NOT NULL, created_at < now() - make_interval(days => $2) FROM optin_confirmations WHERE id = $1",
    )
    .bind(id)
    .bind(LINK_DAYS)
    .fetch_optional(&state.db)
    .await?;
    Ok(match row {
        None => Confirmed::NotFound,
        Some((a, true, _)) => Confirmed::Already(a),
        Some((a, false, true)) => Confirmed::Expired(a),
        Some((a, false, false)) => Confirmed::Waiting(a),
    })
}

/// The person confirmed: subscribe them to email, record the sign-up and add them to the list.
pub async fn confirm(state: &AppState, id: Uuid) -> AppResult<Confirmed> {
    let row: Option<(Uuid, Uuid, Uuid, Value)> = sqlx::query_as(
        "UPDATE optin_confirmations SET confirmed_at = now()
         WHERE id = $1 AND confirmed_at IS NULL AND created_at >= now() - make_interval(days => $2)
         RETURNING account_id, profile_id, list_id, properties",
    )
    .bind(id)
    .bind(LINK_DAYS)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, profile_id, list_id, mut props)) = row else { return status(state, id).await };
    sqlx::query("UPDATE profiles SET email_consent = 'subscribed', updated_at = now() WHERE id = $1").bind(profile_id).execute(&state.db).await?;
    crate::features::webhooks::consent_changed(&state.db, account_id, profile_id, "email", true, "double_opt_in").await;
    // Waiting confirmations for the same address are done too.
    sqlx::query("UPDATE optin_confirmations SET confirmed_at = now() WHERE profile_id = $1 AND list_id = $2 AND confirmed_at IS NULL")
        .bind(profile_id)
        .bind(list_id)
        .execute(&state.db)
        .await?;
    if let Some(o) = props.as_object_mut() {
        o.insert("double_opt_in".into(), json!(true));
    }
    audience::record_event(
        &state.db,
        NewEvent { account_id, profile_id, metric: "Subscribed to List", properties: props, value: None, occurred_at: None, unique_id: Some(format!("optin:{id}")), message_id: None },
    )
    .await?;
    audience::add_to_list(&state.db, account_id, list_id, &[profile_id]).await?;
    Ok(Confirmed::Joined(account_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_inbox_one_key() {
        assert_eq!(email_key("V.ictim+1@Gmail.com"), "victim@gmail.com");
        assert_eq!(email_key("victim@googlemail.com"), "victim@gmail.com");
        assert_eq!(email_key("jane+news@outlook.com"), "jane@outlook.com");
        assert_eq!(email_key("jane.doe+news@shop.test"), "jane.doe@shop.test");
        assert_ne!(email_key("jane.doe@shop.test"), email_key("janedoe@shop.test"), "dots only matter outside Gmail");
    }

    #[test]
    fn confirmation_email_escapes_tenant_text() {
        let mut a = test_account();
        a.name = "Ann & <Co>".into();
        a.address = Some("1 Main St".into());
        let (subject, html, text) = confirmation_email(&a, "https://pl.test/c/abc?x=1&y=2");
        assert_eq!(subject, "Confirm your subscription to Ann & <Co>");
        assert!(html.contains("Ann &amp; &lt;Co&gt;"));
        assert!(!html.contains("<Co>"));
        assert!(html.contains("https://pl.test/c/abc?x=1&amp;y=2"));
        assert!(text.contains("https://pl.test/c/abc?x=1&y=2"));
        assert!(text.contains("Ann & <Co>, 1 Main St"));
    }

    fn test_account() -> Account {
        Account {
            id: Uuid::nil(),
            public_key: String::new(),
            name: "A".into(),
            website: None,
            from_name: None,
            from_email: None,
            reply_to: None,
            sms_from: None,
            timezone: "America/Los_Angeles".into(),
            address: None,
            brand_color: "#000000".into(),
            logo_url: None,
            social_links: json!({}),
            plan: "free".into(),
            onboarding: json!({}),
            conversion_metric: "Placed Order".into(),
            created_at: chrono::Utc::now(),
            sending_paused_at: None,
            sending_paused_reason: None,
            email_daily_limit_override: None,
            email_sunset_enabled: true,
            link_domain: None,
            link_domain_status: "not_started".into(),
            link_domain_records: json!([]),
            verified_domains: Vec::new(),
        }
    }
}

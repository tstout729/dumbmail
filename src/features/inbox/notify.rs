//! Background work: copying MMS images before the provider's links expire, and email digests of
//! new texts for the members who asked for them.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppResult;
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

/// Largest attachment copied; bigger ones stay as the provider's link.
pub const MAX_MEDIA_BYTES: usize = 5 * 1024 * 1024;

/// Job: copy an inbound text's images into `sms_media` through the SSRF-safe fetch (public
/// addresses only, a few re-checked redirects, a size cap). Only PNG, JPEG, GIF and WebP are
/// copied, checked by their bytes; other files (video, contact cards) stay as links.
pub async fn copy_media(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["inbound_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid, Value, String, String, bool)> =
        sqlx::query_as("SELECT account_id, media, provider, provider_message_id, verified FROM sms_inbound WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
    let Some((account_id, mut media, provider, message_sid, verified)) = row else { return Ok(()) };
    // Twilio's media links need the account's credentials; only for a verified Twilio text.
    let twilio = if provider == "twilio" && verified { twilio_credentials(state, account_id).await? } else { None };
    let Some(items) = media.as_array_mut() else { return Ok(()) };
    for item in items.iter_mut().take(super::store::MAX_MEDIA) {
        if item.get("media_id").is_some() {
            continue;
        }
        let Some(url) = item["url"].as_str().map(str::to_string) else { continue };
        let fetched = if is_twilio_media(&url) {
            match &twilio {
                Some((sid, token)) if twilio_media_belongs(&url, sid, &message_sid) => fetch_twilio(&url, sid, token).await,
                _ => {
                    tracing::info!(inbound = %id, "a Twilio attachment can't be copied without that Twilio account's credentials");
                    continue;
                }
            }
        } else {
            crate::flows::fetch_public(&url, MAX_MEDIA_BYTES + 1).await
        };
        let fetched = match fetched {
            Ok(f) => f,
            Err(e) => {
                tracing::info!(inbound = %id, error = %e.public_message(), "couldn't copy a texted attachment; keeping the link");
                continue;
            }
        };
        if fetched.body.len() > MAX_MEDIA_BYTES {
            continue;
        }
        let Some(content_type) = crate::api::images::sniff(&fetched.body) else { continue };
        let media_id = Uuid::now_v7();
        sqlx::query("INSERT INTO sms_media (id, account_id, inbound_id, content_type, size, bytes) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(media_id)
            .bind(account_id)
            .bind(id)
            .bind(content_type)
            .bind(fetched.body.len() as i32)
            .bind(&fetched.body)
            .execute(&state.db)
            .await?;
        item["media_id"] = json!(media_id);
        item["content_type"] = json!(content_type);
    }
    sqlx::query("UPDATE sms_inbound SET media = $2 WHERE id = $1").bind(id).bind(&media).execute(&state.db).await?;
    Ok(())
}

pub const TWILIO_API_HOST: &str = "api.twilio.com";

/// A Twilio media link (it needs the account's credentials to open).
pub fn is_twilio_media(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|u| u.host_str() == Some(TWILIO_API_HOST))
}

/// The link is exactly this account's media for this message:
/// https://api.twilio.com/2010-04-01/Accounts/{sid}/Messages/{message sid}/Media/{media sid}
pub fn twilio_media_belongs(url: &str, account_sid: &str, message_sid: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    if u.scheme() != "https" || u.host_str() != Some(TWILIO_API_HOST) || u.port().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let parts: Vec<&str> = u.path().trim_end_matches('/').split('/').collect();
    let id_ok = |s: &str, prefix: &str| s.starts_with(prefix) && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric());
    matches!(parts.as_slice(), ["", "2010-04-01", "Accounts", sid, "Messages", msg, "Media", media]
        if *sid == account_sid && *msg == message_sid && id_ok(media, "ME"))
        && id_ok(account_sid, "AC")
}

/// The Twilio account an account's texts come in on: its own connection, or the platform's.
async fn twilio_credentials(state: &AppState, account_id: Uuid) -> anyhow::Result<Option<(String, String)>> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT sms_mode, sms_byo_credentials FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await?;
    if let Some((mode, Some(enc))) = row {
        if mode == "byo" {
            let creds: Option<crate::phone::ByoCredentials> = serde_json::from_str(&crate::crypto::decrypt(&state.cfg.secret_key, &enc)?).ok();
            return Ok(match creds {
                Some(crate::phone::ByoCredentials::Twilio { account_sid, auth_token }) => Some((account_sid, auth_token)),
                _ => None,
            });
        }
    }
    Ok(state.cfg.twilio_account_sid.clone().zip(state.cfg.twilio_auth_token.clone()))
}

/// Download Twilio media: the credentials go only to api.twilio.com, with redirects not followed;
/// Twilio answers with a redirect to a short-lived file link, fetched without credentials through
/// the SSRF-safe path.
async fn fetch_twilio(url: &str, sid: &str, token: &str) -> AppResult<crate::flows::Fetched> {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default()
    });
    let res = CLIENT.get(url).basic_auth(sid, Some(token)).send().await.map_err(|e| crate::error::AppError::Internal(e.into()))?;
    let status = res.status();
    if status.is_redirection() {
        let next = res.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
        let next = next.and_then(|n| url::Url::parse(url).ok()?.join(&n).ok()).ok_or_else(|| crate::error::AppError::bad("Twilio's media redirect had no location"))?;
        return crate::flows::fetch_public(next.as_str(), MAX_MEDIA_BYTES + 1).await;
    }
    if !status.is_success() {
        return Err(crate::error::AppError::bad(format!("Twilio answered {status} for the attachment")));
    }
    let content_type = res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let mut body = Vec::new();
    let mut res = res;
    while let Some(chunk) = res.chunk().await.map_err(|e| crate::error::AppError::Internal(e.into()))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_MEDIA_BYTES {
            break;
        }
    }
    Ok(crate::flows::Fetched { url: url.to_string(), content_type, body })
}

pub async fn media(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Option<(String, Vec<u8>)>> {
    Ok(sqlx::query_as("SELECT content_type, bytes FROM sms_media WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?)
}

/// Scheduler: queue a digest for each account with new texts whose last digest is 30+ minutes old.
pub async fn queue_digests(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE sms_inbox_settings s SET last_digest_at = now()
         WHERE s.account_id IN (
             SELECT s2.account_id FROM sms_inbox_settings s2
             WHERE cardinality(s2.notify_user_ids) > 0
               AND (s2.last_digest_at IS NULL OR s2.last_digest_at < now() - interval '30 minutes')
               AND EXISTS (SELECT 1 FROM sms_inbound i WHERE i.account_id = s2.account_id AND i.received_at > COALESCE(s2.notified_through, s2.updated_at))
             LIMIT 200)
         RETURNING s.account_id",
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in due {
        crate::jobs::enqueue(&state.db, "inbox_digest", json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Job: one email per chosen member listing the people who texted since the last digest.
pub async fn send_digest(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(account_id) = payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, "no SUPPORT_EMAIL set; inbox digest not emailed");
        return Ok(());
    };
    let since: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT COALESCE(notified_through, updated_at) FROM sms_inbox_settings WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(since) = since else { return Ok(()) };
    let rows: Vec<(Option<String>, Option<String>, Option<String>, i64, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT p.first_name, p.last_name, p.phone_number, count(*), (array_agg(i.body ORDER BY i.received_at DESC))[1], max(i.received_at)
         FROM sms_inbound i JOIN profiles p ON p.id = i.profile_id
         WHERE i.account_id = $1 AND i.received_at > $2
         GROUP BY p.id, p.first_name, p.last_name, p.phone_number ORDER BY max(i.received_at) DESC LIMIT 21",
    )
    .bind(account_id)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let through = rows.iter().map(|r| r.5).max().unwrap_or(since);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound WHERE account_id = $1 AND received_at > $2 AND received_at <= $3")
        .bind(account_id)
        .bind(since)
        .bind(through)
        .fetch_one(&state.db)
        .await?;
    let account_name: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let link = format!("{}/inbox", state.cfg.public_url());
    let mut lines = Vec::new();
    for (first, last, phone, n, body, _) in rows.iter().take(20) {
        let who = [first.as_deref(), last.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let who = if who.is_empty() { phone.clone().unwrap_or_else(|| "Someone".into()) } else { who };
        let said = super::store::preview(body, 0);
        let said = if said.is_empty() { "(an attachment)".to_string() } else { said };
        lines.push((who, *n, said));
    }
    let subject = if total == 1 { format!("1 new text for {account_name}") } else { format!("{total} new texts for {account_name}") };
    let mut text = format!("{subject}.\n\n");
    let mut html = format!("<div style=\"font-family:system-ui,sans-serif;font-size:15px;line-height:1.5\"><p>{}.</p><ul>", esc(&subject));
    for (who, n, said) in &lines {
        let count = if *n > 1 { format!(" ({n} texts)") } else { String::new() };
        text.push_str(&format!("- {who}{count}: {said}\n"));
        html.push_str(&format!("<li><b>{}</b>{}: {}</li>", esc(who), esc(&count), esc(said)));
    }
    if rows.len() > 20 {
        text.push_str("- and more\n");
        html.push_str("<li>and more</li>");
    }
    text.push_str(&format!("\nReply in your inbox: {link}\nYou get this because you chose texting digests in your DumbMail inbox settings.\n"));
    html.push_str(&format!(
        "</ul><p><a href=\"{}\">Open your inbox</a></p><p style=\"color:#71717a;font-size:13px\">You get this because you chose texting digests in your DumbMail inbox settings.</p></div>",
        esc(&link)
    ));
    let recipients: Vec<String> = sqlx::query_scalar(
        "SELECT u.email FROM sms_inbox_settings s JOIN memberships m ON m.account_id = s.account_id AND m.user_id = ANY(s.notify_user_ids)
         JOIN users u ON u.id = m.user_id WHERE s.account_id = $1 LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    for to in recipients {
        let email = OutgoingEmail {
            message_id: Uuid::now_v7(),
            to,
            from_name: "DumbMail".into(),
            from_email: from.to_string(),
            reply_to: None,
            subject: subject.clone(),
            html: html.clone(),
            text: text.clone(),
            unsubscribe_url: None,
        };
        if let Err(e) = state.email.send(&email).await {
            tracing::warn!(account = %account_id, error = %format!("{e:#}"), "couldn't email an inbox digest");
        }
    }
    sqlx::query("UPDATE sms_inbox_settings SET notified_through = $2 WHERE account_id = $1").bind(account_id).bind(through).execute(&state.db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twilio_media_is_bound_to_the_account_and_message() {
        let ok = "https://api.twilio.com/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789";
        assert!(is_twilio_media(ok));
        assert!(twilio_media_belongs(ok, "AC123", "MM456"));
        assert!(!twilio_media_belongs(ok, "AC999", "MM456"), "another Twilio account");
        assert!(!twilio_media_belongs(ok, "AC123", "MM000"), "another message");
        for bad in [
            "http://api.twilio.com/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789",
            "https://api.twilio.com.evil.test/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789",
            "https://user:pw@api.twilio.com/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789",
            "https://api.twilio.com:8443/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789",
            "https://api.twilio.com/2010-04-01/Accounts/AC123/Messages/MM456/Media/ME789/../../../Calls",
            "https://api.twilio.com/2010-04-01/Accounts/AC123/Calls",
        ] {
            assert!(!twilio_media_belongs(bad, "AC123", "MM456"), "{bad}");
        }
        assert!(!is_twilio_media("https://media.telnyx.example/a.png"));
    }
}

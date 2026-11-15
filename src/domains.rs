//! Custom domains.
//! - Sending domain: DKIM (from the email provider), a custom MAIL FROM subdomain for SPF/DMARC alignment,
//!   and a recommended DMARC record. Each record is checked against public DNS so people see exactly
//!   what's missing or mistyped.
//! - Link domain (e.g. links.brand.com): tracking links, unsubscribe pages and hosted forms on the
//!   customer's domain. On Railway this provisions the domain and its TLS certificate automatically;
//!   self-hosters point a CNAME at their server and terminate TLS in their proxy.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

/// Look up DNS records over HTTPS (Cloudflare resolver). Returns normalized record data.
pub async fn lookup(state: &AppState, name: &str, rtype: &str) -> Vec<String> {
    let res = state
        .http
        .get("https://cloudflare-dns.com/dns-query")
        .query(&[("name", name), ("type", rtype)])
        .header("accept", "application/dns-json")
        .send()
        .await;
    let Ok(res) = res else { return Vec::new() };
    let v: Value = res.json().await.unwrap_or_default();
    v["Answer"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["data"].as_str())
                .map(|d| d.trim_matches('"').replace("\" \"", "").trim_end_matches('.').to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn rec(kind: &str, name: &str, value: &str, purpose: &str, priority: Option<u16>) -> Value {
    let mut r = json!({"type": kind, "name": name, "value": value, "purpose": purpose, "status": "pending", "found": null});
    if let Some(p) = priority {
        r["priority"] = json!(p);
    }
    r
}

/// Fill in `status` and `found` for each record from public DNS.
pub async fn annotate(state: &AppState, records: &mut [Value]) {
    for r in records.iter_mut() {
        let kind = r["type"].as_str().unwrap_or("").to_string();
        let name = r["name"].as_str().unwrap_or("").to_string();
        let want = r["value"].as_str().unwrap_or("").trim_end_matches('.').to_lowercase();
        let purpose = r["purpose"].as_str().unwrap_or("").to_string();
        let found = lookup(state, &name, &kind).await;
        let ok = match (kind.as_str(), purpose.as_str()) {
            ("MX", _) => found.iter().any(|f| f.ends_with(&want)),
            ("TXT", "dmarc") => found.iter().any(|f| f.starts_with("v=dmarc1")),
            ("TXT", "spf") => found.iter().any(|f| f.starts_with("v=spf1") && f.contains("amazonses.com")),
            ("TXT", _) => found.iter().any(|f| f.eq_ignore_ascii_case(&want)),
            _ => found.iter().any(|f| f == &want),
        };
        let shown = if kind == "TXT" && purpose == "dmarc" { found.iter().find(|f| f.starts_with("v=dmarc1")).cloned() } else { found.first().cloned() };
        r["found"] = json!(shown);
        r["status"] = json!(if ok {
            "verified"
        } else if purpose == "dmarc" {
            "recommended"
        } else if found.is_empty() {
            "missing"
        } else {
            "pending"
        });
    }
}

pub fn mail_from_for(domain: &str) -> String {
    format!("send.{domain}")
}

/// Per-account proof that this account controls the domain (the email provider's identity is shared
/// by every account that adds the same domain, so its "verified" status alone proves nothing).
pub fn ownership_token(state: &AppState, account_id: Uuid, domain: &str) -> String {
    let sig = crate::auth::sign(&state.cfg.secret_key, &format!("own:{account_id}:{domain}"));
    format!("dumbmail-verify={}", sig.rsplit_once('.').map(|(_, s)| s).unwrap_or_default())
}

/// Domains verified before the rename used a `_postlark` record; it still proves ownership.
async fn legacy_ownership(state: &AppState, account_id: Uuid, domain: &str) -> bool {
    let want = ownership_token(state, account_id, domain).replacen("dumbmail-verify=", "postlark-verify=", 1);
    lookup(state, &format!("_postlark.{domain}"), "TXT").await.iter().any(|f| f.eq_ignore_ascii_case(&want))
}

fn sending_records(state: &AppState, account_id: Uuid, domain: &str, provider_records: &[crate::providers::DnsRecord]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![rec("TXT", &format!("_dumbmail.{domain}"), &ownership_token(state, account_id, domain), "verification", None)];
    out.extend(provider_records
        .iter()
        .filter(|r| r.kind == "CNAME")
        .map(|r| rec("CNAME", &r.name, &r.value, "dkim", None)));
    let mf = mail_from_for(domain);
    out.push(rec("MX", &mf, &format!("feedback-smtp.{}.amazonses.com", state.cfg.aws_region), "mail_from", Some(10)));
    out.push(rec("TXT", &mf, "v=spf1 include:amazonses.com ~all", "spf", None));
    out.push(rec("TXT", &format!("_dmarc.{domain}"), &format!("v=DMARC1; p=none; rua=mailto:dmarc@{domain}"), "dmarc", None));
    out
}

fn clean_domain(raw: &str) -> AppResult<String> {
    let d = raw.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').trim_end_matches('.').to_lowercase();
    let valid = d.contains('.') && d.len() < 254 && d.split('.').all(|l| !l.is_empty() && l.len() < 64 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !l.starts_with('-'));
    if !valid {
        return Err(AppError::bad("enter a domain like yourbrand.com"));
    }
    Ok(d)
}

/// Another account has already proven ownership of this domain, a parent or a subdomain of it.
async fn verified_elsewhere(state: &AppState, account_id: Uuid, domain: &str) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM accounts WHERE id <> $2 AND sending_domain_status = 'verified' AND sending_domain IS NOT NULL
           AND (sending_domain = $1 OR $1 LIKE '%.' || sending_domain OR sending_domain LIKE '%.' || $1))",
    )
    .bind(domain)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?)
}

pub async fn set_sending_domain(state: &AppState, account_id: Uuid, raw: &str) -> AppResult<()> {
    let domain = clean_domain(raw)?;
    if verified_elsewhere(state, account_id, &domain).await? {
        return Err(AppError::Conflict("that domain is already verified by another account".into()));
    }
    let st = state.email.add_domain(&domain).await.map_err(|e| AppError::bad(format!("could not start domain setup: {e:#}")))?;
    let mf = mail_from_for(&domain);
    // Configure the shared provider identity only when no other account is using this domain.
    let shared: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE sending_domain = $1 AND id <> $2)").bind(&domain).bind(account_id).fetch_one(&state.db).await?;
    if !shared {
        if let Err(e) = state.email.set_mail_from(&domain, &mf).await {
            tracing::warn!(domain = %domain, error = %e, "could not set custom MAIL FROM");
        }
    }
    let mut records = sending_records(state, account_id, &domain, &st.records);
    annotate(state, &mut records).await;
    sqlx::query("UPDATE accounts SET sending_domain = $2, sending_domain_status = $3, sending_domain_records = $4, mail_from_domain = $5 WHERE id = $1")
        .bind(account_id)
        .bind(&domain)
        .bind(&st.status)
        .bind(Value::Array(records))
        .bind(&mf)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn verify_sending_domain(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let a = crate::campaigns::load_account(&state.db, account_id).await?;
    let domain = a.sending_domain.clone().ok_or_else(|| AppError::bad("add a sending domain first"))?;
    let st = state.email.domain_state(&domain).await.map_err(|e| AppError::bad(format!("could not check the domain: {e:#}")))?;
    let mut records = sending_records(state, account_id, &domain, &st.records);
    annotate(state, &mut records).await;
    let owns = records.iter().any(|r| r["purpose"] == "verification" && r["status"] == "verified") || legacy_ownership(state, account_id, &domain).await;
    let dkim_ok = {
        let dkim: Vec<&Value> = records.iter().filter(|r| r["purpose"] == "dkim").collect();
        !dkim.is_empty() && dkim.iter().all(|r| r["status"] == "verified")
    };
    // Verified = this account proved ownership AND the provider (or, in dev, public DNS) accepts DKIM.
    let provider_ok = if state.email.name() == "log" { dkim_ok } else { st.status == "verified" };
    let status = if owns && provider_ok && !verified_elsewhere(state, account_id, &domain).await? {
        "verified".to_string()
    } else if st.status == "failed" {
        "failed".to_string()
    } else {
        "pending".to_string()
    };
    sqlx::query("UPDATE accounts SET sending_domain_status = $2, sending_domain_records = $3 WHERE id = $1")
        .bind(account_id)
        .bind(&status)
        .bind(Value::Array(records))
        .execute(&state.db)
        .await?;
    if status == "verified" {
        sqlx::query("UPDATE accounts SET onboarding = onboarding || '{\"sender\": true}' WHERE id = $1").bind(account_id).execute(&state.db).await?;
    }
    Ok(())
}

pub async fn remove_sending_domain(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let a = crate::campaigns::load_account(&state.db, account_id).await?;
    if let Some(d) = &a.sending_domain {
        // Only delete the provider identity if no other account uses the same domain.
        let shared: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE sending_domain = $1 AND id <> $2)").bind(d).bind(account_id).fetch_one(&state.db).await?;
        if !shared {
            if let Err(e) = state.email.remove_domain(d).await {
                tracing::warn!(domain = %d, error = %e, "could not remove domain identity");
            }
        }
    }
    sqlx::query("UPDATE accounts SET sending_domain = NULL, sending_domain_status = 'not_started', sending_domain_records = '[]', mail_from_domain = NULL WHERE id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// ---------- Link domains ----------

fn public_host(state: &AppState) -> String {
    url::Url::parse(state.cfg.public_url()).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()
}

fn railway(state: &AppState) -> Option<(&str, &str, &str, &str)> {
    let c = &state.cfg;
    Some((c.railway_api_token.as_deref()?, c.railway_project_id.as_deref()?, c.railway_environment_id.as_deref()?, c.railway_service_id.as_deref()?))
}

async fn railway_gql(state: &AppState, token: &str, query: &str, vars: Value) -> AppResult<Value> {
    let res = state
        .http
        .post("https://backboard.railway.com/graphql/v2")
        // A project token scoped to this Railway project and environment.
        .header("Project-Access-Token", token)
        .json(&json!({"query": query, "variables": vars}))
        .send()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let v: Value = res.json().await.map_err(|e| AppError::Internal(e.into()))?;
    if let Some(err) = v["errors"][0]["message"].as_str() {
        return Err(AppError::bad(format!("domain setup failed: {err}")));
    }
    Ok(v["data"].clone())
}

const DOMAIN_FIELDS: &str = "id domain status { verified certificateStatus dnsRecords { recordType hostlabel fqdn zone requiredValue currentValue status purpose } verificationDnsHost verificationToken }";

fn railway_records(d: &Value) -> (Vec<Value>, bool) {
    let st = &d["status"];
    let mut out: Vec<Value> = st["dnsRecords"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let kind = r["recordType"].as_str().unwrap_or("").trim_start_matches("DNS_RECORD_TYPE_").to_string();
            json!({
                "type": kind, "name": r["fqdn"], "value": r["requiredValue"], "purpose": "link",
                "status": if r["status"].as_str().unwrap_or("").contains("PROPAGATED") { "verified" } else if r["currentValue"].as_str().unwrap_or("").is_empty() { "missing" } else { "pending" },
                "found": r["currentValue"].as_str().filter(|s| !s.is_empty()),
            })
        })
        .collect();
    if let (Some(host), Some(token)) = (st["verificationDnsHost"].as_str(), st["verificationToken"].as_str()) {
        if !host.is_empty() && !token.is_empty() {
            // Railway gives the host relative to the DNS zone; show the full name.
            let zone = st["dnsRecords"][0]["zone"].as_str().unwrap_or("");
            let name = if zone.is_empty() || host.ends_with(zone) { host.to_string() } else { format!("{host}.{zone}") };
            out.push(json!({"type": "TXT", "name": name, "value": token, "purpose": "verification", "status": if st["verified"] == true { "verified" } else { "pending" }, "found": null}));
        }
    }
    let cert = st["certificateStatus"].as_str().unwrap_or("");
    let active = st["verified"] == true && (cert.ends_with("_VALID") || cert.ends_with("_ISSUED"));
    (out, active)
}

pub async fn set_link_domain(state: &AppState, account_id: Uuid, raw: &str) -> AppResult<()> {
    let domain = clean_domain(raw)?;
    if domain == public_host(state) {
        return Err(AppError::bad("use a domain you own, like links.yourbrand.com"));
    }
    // Active domains are exclusive; an unfinished claim by someone else lapses after 48 hours.
    let holder: Option<(Uuid, String, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, link_domain_status, link_domain_set_at FROM accounts WHERE link_domain = $1 AND id <> $2 LIMIT 1")
            .bind(&domain)
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?;
    if let Some((other, status, set_at)) = holder {
        let stale = set_at.is_none_or(|t| t < chrono::Utc::now() - chrono::Duration::hours(48));
        if status == "active" || !stale {
            return Err(AppError::Conflict("that domain is being connected by another account".into()));
        }
        remove_link_domain(state, other).await?;
    }
    remove_link_domain(state, account_id).await?;
    let (records, provider_id) = match railway(state) {
        Some((token, project, env, service)) => {
            let q = format!("mutation($input: CustomDomainCreateInput!) {{ customDomainCreate(input: $input) {{ {DOMAIN_FIELDS} }} }}");
            let d = railway_gql(state, token, &q, json!({"input": {"domain": domain, "projectId": project, "environmentId": env, "serviceId": service}})).await?;
            let d = &d["customDomainCreate"];
            (railway_records(d).0, d["id"].as_str().map(String::from))
        }
        None => {
            let mut r = vec![rec("CNAME", &domain, &public_host(state), "link", None)];
            annotate(state, &mut r).await;
            (r, None)
        }
    };
    sqlx::query("UPDATE accounts SET link_domain = $2, link_domain_status = 'pending', link_domain_records = $3, link_domain_provider_id = $4, link_domain_set_at = now() WHERE id = $1")
        .bind(account_id)
        .bind(&domain)
        .bind(Value::Array(records))
        .bind(provider_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn verify_link_domain(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let (domain, provider_id): (Option<String>, Option<String>) = sqlx::query_as("SELECT link_domain, link_domain_provider_id FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let domain = domain.ok_or_else(|| AppError::bad("add a link domain first"))?;
    let (records, mut active) = match (railway(state), provider_id) {
        (Some((token, project, _, _)), Some(id)) => {
            let q = format!("query($id: String!, $projectId: String!) {{ customDomain(id: $id, projectId: $projectId) {{ {DOMAIN_FIELDS} }} }}");
            let d = railway_gql(state, token, &q, json!({"id": id, "projectId": project})).await?;
            railway_records(&d["customDomain"])
        }
        _ => {
            let mut r = vec![rec("CNAME", &domain, &public_host(state), "link", None)];
            annotate(state, &mut r).await;
            let ok = r.iter().all(|x| x["status"] == "verified");
            (r, ok)
        }
    };
    // Final check: the domain actually serves this app over HTTPS.
    if active {
        let probe = state.http.get(format!("https://{domain}/healthz")).send().await;
        active = matches!(probe, Ok(r) if r.status().is_success());
    }
    sqlx::query("UPDATE accounts SET link_domain_status = $2, link_domain_records = $3 WHERE id = $1")
        .bind(account_id)
        .bind(if active { "active" } else { "pending" })
        .bind(Value::Array(records))
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn remove_link_domain(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let provider_id: Option<String> = sqlx::query_scalar("SELECT link_domain_provider_id FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    if let (Some((token, _, _, _)), Some(id)) = (railway(state), provider_id) {
        if let Err(e) = railway_gql(state, token, "mutation($id: String!) { customDomainDelete(id: $id) }", json!({"id": id})).await {
            tracing::warn!(error = %e.public_message(), "could not delete Railway custom domain");
        }
    }
    sqlx::query("UPDATE accounts SET link_domain = NULL, link_domain_status = 'not_started', link_domain_records = '[]', link_domain_provider_id = NULL WHERE id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Base URL for links in this account's messages and forms: the branded domain when active.
pub fn link_base(state: &AppState, account: &Account) -> String {
    match (&account.link_domain, account.link_domain_status.as_str()) {
        (Some(d), "active") => format!("https://{d}"),
        _ => state.cfg.public_url().to_string(),
    }
}

pub fn sending_json(state: &AppState, a: &Account) -> Value {
    json!({
        "email": {
            "provider": state.email.name(),
            "domain": a.sending_domain,
            "status": a.sending_domain_status,
            "mail_from_domain": a.mail_from_domain,
            "dns_records": a.sending_domain_records,
            "from_email": a.from_email,
        },
        "sms": {
            "provider": state.sms.name(),
            "from_number": a.sms_from,
            "status": if a.sms_from.is_some() { "ready" } else { "not_started" },
        },
        "link_domain": {
            "domain": a.link_domain,
            "status": a.link_domain_status,
            "dns_records": a.link_domain_records,
            "url": a.link_domain.as_ref().filter(|_| a.link_domain_status == "active").map(|d| format!("https://{d}")),
        },
    })
}

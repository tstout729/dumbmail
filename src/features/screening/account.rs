//! The first check of a `pending` account, run when it first tries to send for real (a launch,
//! a flow turned on or a flow step coming due) or asks for a review. Rules first, then one Claude
//! call; a model `pass` never overrides a rules `review` or `block`, and with no model answer a
//! rules `pass` becomes `review` (a person decides).

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::ai::{self, NoJudgment};
use super::fetch;
use super::redact;
use super::rules;
use super::standing::{self, New, Standing};
use super::Verdict;
use crate::state::AppState;

pub const JOB: &str = "screening_account_check";
/// A domain registered more recently than this is a reason for a person to look.
pub const NEW_DOMAIN_DAYS: i64 = 30;
/// A claim on the check lapses after this (a crashed run).
const CLAIM_MINUTES: i32 = 5;

/// The customer's answers before the first check.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    /// What the business does.
    pub business: String,
    /// How the people on the list signed up.
    pub list_source: String,
    /// Expected emails per month.
    pub monthly_volume: i64,
}

impl Answers {
    pub fn checked(self) -> Result<Self, String> {
        let business = self.business.trim().to_string();
        let list_source = self.list_source.trim().to_string();
        if business.chars().count() < 3 || list_source.chars().count() < 3 {
            return Err("Tell us what your business does and how the people on your list signed up.".into());
        }
        if business.chars().count() > 1000 || list_source.chars().count() > 1000 {
            return Err("Keep each answer under 1,000 characters.".into());
        }
        if !(0..=100_000_000).contains(&self.monthly_volume) {
            return Err("The monthly volume must be a number of emails from 0 to 100,000,000.".into());
        }
        Ok(Self { business, list_source, monthly_volume: self.monthly_volume })
    }
}

/// Start a check soon (a flow step came due for a pending account). Never waits.
pub fn kick(state: &AppState, account_id: Uuid) {
    if !super::gate::kick_due(account_id) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::jobs::enqueue_background(&state.db, JOB, json!({"account_id": account_id}), None).await {
            tracing::warn!(account = %account_id, error = %e, "couldn't queue the account check");
        }
    });
}

pub async fn run_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    run(state, id, None, Duration::from_secs(120)).await?;
    Ok(())
}

/// Scheduler: pending accounts that answered the questions and were waiting on a verified sending
/// domain (or whose last check is older than their newest verified domain) get checked.
pub async fn sweep_waiting(state: &AppState) -> anyhow::Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM accounts a
         WHERE a.sending_review = 'pending' AND a.sending_review_answers IS NOT NULL AND a.closed_at IS NULL
           AND (a.sending_review_checking_at IS NULL OR a.sending_review_checking_at < now() - make_interval(mins => $1))
           AND EXISTS (SELECT 1 FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified'
               AND NOT EXISTS (SELECT 1 FROM screenings s WHERE s.account_id = a.id AND s.kind = 'account' AND s.created_at > COALESCE(d.verified_at, d.created_at)))
         LIMIT 50",
    )
    .bind(CLAIM_MINUTES)
    .fetch_all(&state.db)
    .await?;
    for id in ids {
        if super::gate::kick_due(id) {
            crate::jobs::enqueue_background(&state.db, JOB, json!({"account_id": id}), None).await?;
        }
    }
    Ok(())
}

/// What the check knows about the account (read before any network call).
struct Facts {
    name: String,
    website: Option<String>,
    owner_domain: Option<String>,
    owner_disposable: bool,
    answers: Option<Value>,
    sending: Vec<(String, DateTime<Utc>)>,
    link_domain: Option<String>,
    earlier: Vec<(String, String, Value, Option<String>)>,
    imports: Vec<Value>,
    other_sources: Value,
    first_content: Option<Value>,
    brands: Vec<String>,
}

async fn facts(state: &AppState, account_id: Uuid, first_content: Option<Value>) -> anyhow::Result<Facts> {
    let db = &state.db;
    let (name, website, answers, link_domain, brands): (String, Option<String>, Option<Value>, Option<String>, Vec<String>) =
        sqlx::query_as("SELECT name, website, sending_review_answers, link_domain, sending_review_brands FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?;
    let owner = standing::owner_email(db, account_id).await;
    let owner_domain = owner.as_deref().and_then(|e| e.rsplit_once('@')).map(|(_, d)| d.to_ascii_lowercase());
    let owner_disposable = owner.as_deref().and_then(crate::models::normalize_email).is_some_and(|e| crate::deliverability::risky_address(&e) == Some("disposable"));
    let sending: Vec<(String, DateTime<Utc>)> =
        sqlx::query_as("SELECT domain, created_at FROM sending_domains WHERE account_id = $1 AND status = 'verified' ORDER BY created_at LIMIT 10").bind(account_id).fetch_all(db).await?;
    let earlier: Vec<(String, String, Value, Option<String>)> = sqlx::query_as(
        "SELECT kind, verdict, reasons, subject_name FROM screenings WHERE account_id = $1 AND kind IN ('import', 'domain', 'content') ORDER BY created_at DESC LIMIT 20",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let imports: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('file', subject_name, 'verdict', verdict, 'stats', signals->'stats') FROM screenings WHERE account_id = $1 AND kind = 'import' ORDER BY created_at DESC LIMIT 5")
        .bind(account_id)
        .fetch_all(db)
        .await?;
    // Contacts that came in other ways than a CSV (Klaviyo import, integrations): counts only.
    let other_sources: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'profiles', (SELECT count(*) FROM profiles WHERE account_id = $1),
            'klaviyo_imports', (SELECT COALESCE(jsonb_agg(status), '[]') FROM (SELECT status FROM klaviyo_imports WHERE account_id = $1 ORDER BY created_at DESC LIMIT 5) k),
            'integrations', (SELECT COALESCE(jsonb_agg(COALESCE(recipe, 'custom')), '[]') FROM (SELECT recipe FROM integrations WHERE account_id = $1 LIMIT 10) i))",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let first_content = match first_content {
        Some(c) => Some(c),
        None => super::content::latest_material(state, account_id).await?,
    };
    Ok(Facts { name, website, owner_domain, owner_disposable, answers, sending, link_domain, earlier, imports, other_sources, first_content, brands })
}

/// A claim on running the account's check (one at a time); give it back with [`release`].
pub struct Claim(Uuid);

/// Claim the check for a pending account that has answered. None: not pending, not answered, or
/// another check is running.
pub async fn claim(state: &AppState, account_id: Uuid) -> anyhow::Result<Option<Claim>> {
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE accounts SET sending_review_checking_at = now()
         WHERE id = $1 AND sending_review = 'pending' AND sending_review_answers IS NOT NULL AND closed_at IS NULL
           AND (sending_review_checking_at IS NULL OR sending_review_checking_at < now() - make_interval(mins => $2)) RETURNING id",
    )
    .bind(account_id)
    .bind(CLAIM_MINUTES)
    .fetch_optional(&state.db)
    .await?;
    Ok(claimed.map(Claim))
}

pub async fn release(state: &AppState, c: Claim) -> anyhow::Result<()> {
    sqlx::query("UPDATE accounts SET sending_review_checking_at = NULL WHERE id = $1").bind(c.0).execute(&state.db).await?;
    Ok(())
}

/// Run the check now if the account is pending, has answered and isn't being checked already, and
/// write its decision. Returns the standing afterwards. Holds no database connection while waiting
/// on the network.
pub async fn run(state: &AppState, account_id: Uuid, first_content: Option<Value>, deadline: Duration) -> anyhow::Result<Standing> {
    if !super::on(state) {
        return Ok(Standing::Approved);
    }
    let start = Instant::now();
    let Some(c) = claim(state, account_id).await? else { return Ok(standing::read(state, account_id).await?) };
    let out = async {
        if let Some(d) = assess(state, account_id, first_content, deadline, start).await? {
            decide(state, account_id, d).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    release(state, c).await?;
    out?;
    Ok(standing::read(state, account_id).await?)
}

/// The check's outcome, not written yet.
pub struct Decision {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub signals: Value,
    pub judged: Option<ai::Judgment>,
}

/// Run the check (rules, then the model) under the caller's claim, without writing anything.
/// None: it waits for a verified sending domain (the customer is told).
pub async fn assess(state: &AppState, account_id: Uuid, first_content: Option<Value>, deadline: Duration, start: Instant) -> anyhow::Result<Option<Decision>> {
    let f = facts(state, account_id, first_content).await?;
    if f.sending.is_empty() {
        sqlx::query("UPDATE accounts SET sending_review_note = $2 WHERE id = $1 AND sending_review = 'pending'")
            .bind(account_id)
            .bind("Next step: verify a sending domain in Settings → Domains. We check your account as soon as it's verified.")
            .execute(&state.db)
            .await?;
        return Ok(None);
    }
    let mut block: Vec<String> = Vec::new();
    let mut review: Vec<String> = Vec::new();
    if f.owner_disposable {
        block.push("The account was created with a throwaway email address.".into());
    }
    let site_host = f.website.as_deref().and_then(fetch::website_host);
    let mut domains: Vec<(String, &'static str)> = f.sending.iter().map(|(d, _)| (d.clone(), "sending")).collect();
    if let Some(l) = &f.link_domain {
        domains.push((l.clone(), "link"));
    }
    // Proof of belonging to a brand: a verified sending domain on it, or an operator's word.
    let verified: Vec<String> = f.sending.iter().map(|(d, _)| d.clone()).collect();
    let proof = rules::Affiliation { verified_domains: &verified, brands: &f.brands };
    for (d, what) in &domains {
        if let Some(brand) = rules::impersonated_brand(d, &proof) {
            block.push(format!("The {what} domain {d} looks like it impersonates {brand}."));
        }
    }
    // The website and the domains' ages, looked up at the same time.
    let site = async {
        match &site_host {
            Some(h) => Some(fetch::website(h).await),
            None => None,
        }
    };
    let ages = futures_util::future::join_all(domains.iter().map(|(d, _)| fetch::registered_at(d)));
    let (site, ages) = tokio::time::timeout(deadline.saturating_sub(start.elapsed()).min(Duration::from_secs(15)), async { tokio::join!(site, ages) })
        .await
        .unwrap_or((Some(Err(fetch::SiteError::NotAnswering("took too long"))), vec![None; domains.len()]));
    let mut page: Option<fetch::Page> = None;
    match (&site_host, site) {
        (None, _) => review.push("The account has no website.".into()),
        (Some(h), Some(Err(fetch::SiteError::NotAnswering(why)))) => review.push(format!("The website {h} {why}.")),
        (Some(h), Some(Ok(p))) => {
            if rules::free_host(h) {
                review.push("The website is on a free hosting address, not the business's own domain.".into());
            }
            if let Some(why) = rules::website_problem(&p.title, &p.text) {
                review.push(why.into());
            }
            page = Some(p);
        }
        (Some(_), None) => {}
    }
    let mut domain_ages = Vec::new();
    for ((d, what), at) in domains.iter().zip(ages) {
        if let Some(at) = at {
            let days = (Utc::now() - at).num_days();
            domain_ages.push(json!({"domain": d, "use": what, "registered_days_ago": days}));
            if days < NEW_DOMAIN_DAYS {
                review.push(format!("The {what} domain {d} was registered {days} days ago."));
            }
        } else {
            domain_ages.push(json!({"domain": d, "use": what, "registered_days_ago": null}));
        }
    }
    // Three names with nothing in common: who signed up, the website, the sending domain.
    let names: Vec<String> = [f.owner_domain.clone().filter(|d| !rules::is_consumer(d)), site_host.clone(), f.sending.first().map(|(d, _)| d.clone())].into_iter().flatten().collect();
    if names.len() == 3 && rules::unrelated(&names[0], &names[1]) && rules::unrelated(&names[0], &names[2]) && rules::unrelated(&names[1], &names[2]) {
        review.push("The sign-up email's domain, the website and the sending domain are three unrelated names.".into());
    }
    for (kind, verdict, reasons, _) in &f.earlier {
        if verdict == "review" && (kind == "import" || kind == "domain") {
            let r: Vec<String> = serde_json::from_value(reasons.clone()).unwrap_or_default();
            review.extend(r.into_iter().take(2));
        }
    }
    review.dedup();
    let rules_verdict = if !block.is_empty() { Verdict::Block } else if !review.is_empty() { Verdict::Review } else { Verdict::Pass };
    let rule_reasons: Vec<String> = block.iter().chain(&review).cloned().collect();
    let signals_base = json!({
        "rules": {"verdict": rules_verdict.as_str(), "block": block, "review": review},
        "owner_domain": f.owner_domain,
        "website": site_host,
        "website_title": page.as_ref().map(|p| p.title.clone()),
        "domains": domain_ages,
        "answers": f.answers,
        "imports": f.imports,
        "other_sources": f.other_sources,
    });
    // Rules that block decide on their own: the model can't change it.
    if rules_verdict == Verdict::Block {
        return Ok(Some(Decision { verdict: Verdict::Block, reasons: rule_reasons, signals: signals_base, judged: None }));
    }
    // Only these fields go to the model, free text with addresses and numbers replaced.
    let answers = f.answers.as_ref().map(|a| {
        json!({
            "business": redact::text(a["business"].as_str().unwrap_or("")),
            "list_source": redact::text(a["list_source"].as_str().unwrap_or("")),
            "monthly_volume": a["monthly_volume"].as_i64(),
        })
    });
    let website_page = page.as_ref().map(|p| json!({"title": redact::text(&p.title), "description": redact::text(&p.description), "text": redact::text(&p.text)}));
    let imports: Vec<Value> = f.imports.iter().map(|i| json!({"file": i["file"].as_str().map(redact::text), "verdict": i["verdict"], "stats": i["stats"]})).collect();
    let material = json!({
        "account_name": redact::text(&crate::deliverability::plain(&f.name, 200)),
        "website": site_host,
        "website_page": website_page,
        "answers": answers,
        "signup_email_domain": f.owner_domain,
        "domains": domain_ages,
        "imports": imports,
        "contacts_from_other_sources": f.other_sources,
        "first_message": f.first_content,
    });
    let task = "Task: decide whether this new DumbMail account is a real business that will send to people who signed up with it. \
                You get the account's name, its website's text, its own answers about the business, how its list signed up and its expected monthly volume, \
                the domain of the email address it signed up with, its sending and link domains with their age, summaries of the files it imported (counts only), \
                and the first email or text it wants to send, if any. Pass a clear, real business with an honest list. Review anything unclear or unusual \
                (a mismatch between the answers and the website, a very high volume for a small new business, a vague list source). Block clear cold outreach, bought lists, phishing, impersonation or a prohibited category.";
    let left = deadline.saturating_sub(start.elapsed());
    Ok(Some(match ai::judge(state, account_id, task, &material, left).await {
        Ok(j) => {
            let mut reasons = rule_reasons;
            reasons.extend(j.reasons.iter().cloned());
            let mut signals = signals_base;
            signals["model"] = json!({"verdict": j.verdict.as_str(), "codes": j.codes, "notes": j.notes});
            Decision { verdict: j.verdict.max(rules_verdict), reasons, signals, judged: Some(j) }
        }
        Err(e) => {
            let mut reasons = rule_reasons;
            reasons.push(match e {
                NoJudgment::Refused => "This needs a person's check.".into(),
                _ => "A person will check your account (our automatic check couldn't finish).".into(),
            });
            let mut signals = signals_base;
            signals["model"] = json!({"unavailable": e.describe()});
            Decision { verdict: rules_verdict.max(Verdict::Review), reasons, signals, judged: None }
        }
    }))
}

/// Write the check's one decision: the screening and the account's standing (from pending).
pub async fn decide(state: &AppState, account_id: Uuid, d: Decision) -> anyhow::Result<()> {
    let Decision { verdict, mut reasons, signals, judged } = d;
    reasons.dedup();
    let mut s = New::rules("account", verdict, &reasons, signals.clone());
    if let Some(j) = &judged {
        s = s.judged(j);
    }
    standing::record(&state.db, account_id, s).await?;
    let (to, note) = match verdict {
        Verdict::Pass => (Standing::Approved, String::new()),
        Verdict::Review => (Standing::Review, reasons.join(" ")),
        Verdict::Block => (Standing::Blocked, reasons.join(" ")),
    };
    let why = format!(
        "Account check: {}. {}{}",
        verdict.as_str(),
        reasons.join(" "),
        signals["model"]["unavailable"].as_str().map(|u| format!(" The AI check didn't answer ({u}), so a person decides.")).unwrap_or_default()
    );
    standing::move_to(state, account_id, &[Standing::Pending], to, &note, &why).await?;
    Ok(())
}

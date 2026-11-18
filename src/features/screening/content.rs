//! What a campaign or flow sends, checked when a campaign launches or is scheduled, when a flow is
//! turned on, and when a live flow gets new email or text content: rules first, then one Claude
//! call against the acceptable use rules. The same content (by the hash of what went to the model)
//! is judged once per account.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::ai::{self, NoJudgment};
use super::fetch;
use super::rules;
use super::standing::{self, New, Standing};
use super::Verdict;
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, FlowRow};
use crate::state::AppState;

/// How long a launch (or turning a flow on) waits for the checks.
pub const DEADLINE: Duration = Duration::from_secs(45);
/// The most visible text per message that goes into a check.
const TEXT_CHARS: usize = 6000;
/// The most messages in one check (a flow's steps, a campaign's versions).
const MAX_MESSAGES: usize = 25;
const MAX_LINKS: usize = 40;
/// The most live flows one saved block edit checks (more: the edit is refused).
const MAX_FLOWS_PER_BLOCK: i64 = 10;
/// Established accounts (rules decide when the model can't be reached): approved, this many real
/// emails over at least this many days, and no pause in the last 30 days.
pub const ESTABLISHED_EMAILS: i64 = 1000;
pub const ESTABLISHED_DAYS: i32 = 14;
pub const ESTABLISHED_NO_PAUSE_DAYS: i32 = 30;

pub const HELD_MESSAGE: &str = "A person is checking this before it goes out. We'll email you when it's done, usually within a day.";

static ANCHOR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<a\s[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap());
static BARE_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)https?://[^\s<>"')]+"#).unwrap());

fn host(u: &str) -> Option<String> {
    url::Url::parse(u.trim()).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
}

/// Where a link goes, as judged: scheme, host, path and query, without link tags (`utm_…`, added
/// at send time) or the fragment, so any other change to a destination is checked again.
fn destination(href: &str) -> String {
    let Ok(mut u) = url::Url::parse(href.trim()) else { return href.chars().take(300).collect() };
    let kept: Vec<(String, String)> = u.query_pairs().filter(|(k, _)| !k.to_ascii_lowercase().starts_with("utm_")).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    u.set_fragment(None);
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.to_string().chars().take(300).collect()
}

/// One rendered message, reduced to what a check looks at.
fn message(channel: &str, rendered: &Value) -> Value {
    if channel == "sms" {
        let body: String = rendered["body"].as_str().unwrap_or("").chars().take(TEXT_CHARS).collect();
        let links: Vec<Value> = BARE_URL.find_iter(&body).take(MAX_LINKS).filter_map(|m| host(m.as_str())).map(|d| json!({"text": "", "domain": d})).collect();
        return json!({"channel": "sms", "body": body, "links": links});
    }
    let html = rendered["html"].as_str().unwrap_or("");
    let links: Vec<Value> = ANCHOR
        .captures_iter(html)
        .take(MAX_LINKS)
        .filter_map(|c| {
            let href = c[1].replace("&amp;", "&");
            let d = host(&href)?;
            Some(json!({"text": fetch::visible_text(&c[2], 120), "to": destination(&href), "domain": d}))
        })
        .collect();
    json!({
        "channel": "email",
        "subject": rendered["subject"].as_str().unwrap_or("").chars().take(300).collect::<String>(),
        "preview_text": rendered["preheader"].as_str().unwrap_or("").chars().take(300).collect::<String>(),
        "text": fetch::visible_text(html, TEXT_CHARS),
        "links": links,
    })
}

async fn render(state: &AppState, account_id: Uuid, channel: &str, content: &Value, flow: bool) -> AppResult<Value> {
    let event = flow.then(|| json!({"properties": {}}));
    let r = crate::api::messaging::render_preview(state, account_id, crate::api::messaging::RenderBody { channel: channel.to_string(), content: content.clone(), profile_id: None, event }).await?;
    Ok(message(channel, &r))
}

/// What a check is about.
pub struct Material {
    pub subject: &'static str,
    pub subject_id: Uuid,
    pub name: String,
    pub first_campaign: bool,
    pub messages: Vec<Value>,
}

pub async fn campaign_material(state: &AppState, c: &CampaignRow) -> AppResult<Material> {
    let mut versions = vec![c.content.clone()];
    versions.extend(crate::campaigns::ab_test(c).into_iter().flat_map(|t| t.variants).map(|v| v.content));
    let mut messages = Vec::new();
    for v in versions.iter().take(MAX_MESSAGES) {
        messages.push(render(state, c.account_id, &c.channel, v, false).await?);
    }
    let earlier: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM campaigns WHERE account_id = $1 AND id <> $2 AND status IN ('sending', 'sent', 'testing'))")
        .bind(c.account_id)
        .bind(c.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Material { subject: "campaign", subject_id: c.id, name: c.name.clone(), first_campaign: !earlier, messages })
}

/// The email and text steps of a flow definition, by step id.
fn steps(def: &Value) -> BTreeMap<String, (String, Value)> {
    let Ok(d) = crate::flows::parse_definition(def) else { return BTreeMap::new() };
    d.nodes.into_iter().filter(|n| matches!(n.kind.as_str(), "email" | "sms")).map(|n| (n.id.clone(), (n.kind.clone(), n.data.get("content").cloned().unwrap_or(json!({}))))).collect()
}

async fn flow_material(state: &AppState, f: &FlowRow, only: &BTreeMap<String, (String, Value)>) -> AppResult<Material> {
    let mut messages = Vec::new();
    for (_, (channel, content)) in only.iter().take(MAX_MESSAGES) {
        messages.push(render(state, f.account_id, channel, content, true).await?);
    }
    Ok(Material { subject: "flow", subject_id: f.id, name: f.name.clone(), first_campaign: false, messages })
}

/// The newest campaign's or flow's messages, for an account check with no send in hand.
pub async fn latest_material(state: &AppState, account_id: Uuid) -> anyhow::Result<Option<Value>> {
    let c: Option<CampaignRow> = sqlx::query_as("SELECT * FROM campaigns WHERE account_id = $1 AND channel IN ('email', 'sms') ORDER BY updated_at DESC LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    if let Some(c) = c {
        if let Ok(m) = campaign_material(state, &c).await {
            return Ok(Some(json!(m.messages)));
        }
    }
    let f: Option<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 ORDER BY updated_at DESC LIMIT 1").bind(account_id).fetch_optional(&state.db).await?;
    if let Some(f) = f {
        if let Ok(m) = flow_material(state, &f, &steps(&f.definition)).await {
            if !m.messages.is_empty() {
                return Ok(Some(json!(m.messages)));
            }
        }
    }
    Ok(None)
}

/// The account's own domains (registrable): its website, sending and link domains, its sender
/// address, and DumbMail's own hosts.
async fn own_domains(state: &AppState, account: &Account) -> AppResult<HashSet<String>> {
    let mut own: HashSet<String> = account.verified_domains.iter().map(|d| rules::registrable(d)).collect();
    let sending: Vec<String> = sqlx::query_scalar("SELECT domain FROM sending_domains WHERE account_id = $1").bind(account.id).fetch_all(&state.db).await?;
    own.extend(sending.iter().map(|d| rules::registrable(d)));
    own.extend(account.website.as_deref().and_then(fetch::website_host).map(|h| rules::registrable(&h)));
    own.extend(account.link_domain.as_deref().map(rules::registrable));
    own.extend(account.from_email.as_deref().and_then(|e| e.rsplit_once('@')).map(|(_, d)| rules::registrable(d)));
    own.extend(crate::features::content::check::configured_own_hosts(&state.cfg).iter().map(|h| rules::registrable(h)));
    Ok(own)
}

/// The rules' verdict on the messages.
fn judge_rules(m: &Material, own: &HashSet<String>) -> (Verdict, Vec<String>) {
    let mut block = Vec::new();
    let mut review = Vec::new();
    let mut texts = String::new();
    let mut foreign = false;
    let mut shorteners = HashSet::new();
    for msg in &m.messages {
        for k in ["subject", "preview_text", "text", "body"] {
            texts.push_str(msg[k].as_str().unwrap_or(""));
            texts.push('\n');
        }
        if m.subject == "campaign" && rules::fake_reply(msg["subject"].as_str().unwrap_or("")) {
            review.push("The subject line starts with \"Re:\" or \"Fwd:\" but this isn't a reply. Use a subject that says what the email is.".to_string());
        }
        for l in msg["links"].as_array().into_iter().flatten() {
            let d = l["domain"].as_str().unwrap_or("");
            if !own.contains(&rules::registrable(d)) {
                foreign = true;
            }
            if crate::features::content::check::SHORTENERS.contains(&d) {
                shorteners.insert(d.to_string());
            }
            let text = l["text"].as_str().unwrap_or("");
            if !d.is_empty() && crate::features::content::check::mismatched(text, &format!("https://{d}/")) {
                review.push(format!("A link shows {} but goes to {d}. Make each link's text match where it goes.", crate::deliverability::plain(text, 80)));
            }
        }
    }
    if rules::asks_for_credentials(&texts) && foreign {
        block.push("The message asks people to sign in or confirm account details, with a link to a site that isn't yours.".to_string());
    }
    if m.first_campaign && !shorteners.is_empty() {
        let mut s: Vec<String> = shorteners.into_iter().collect();
        s.sort();
        review.push(format!("Your first campaign uses a link shortener ({}). Use full links to your own site.", s.join(", ")));
    }
    review.sort();
    review.dedup();
    if !block.is_empty() {
        block.extend(review);
        (Verdict::Block, block)
    } else if !review.is_empty() {
        (Verdict::Review, review)
    } else {
        (Verdict::Pass, Vec::new())
    }
}

/// Approved, with at least [`ESTABLISHED_EMAILS`] real emails over at least [`ESTABLISHED_DAYS`]
/// days and no pause in the last [`ESTABLISHED_NO_PAUSE_DAYS`].
pub async fn established(state: &AppState, account_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT a.sending_review = 'approved' AND a.sending_paused_at IS NULL
            AND (a.sending_window_start IS NULL OR a.sending_window_start < now() - make_interval(days => $3))
            AND (SELECT count(*) FROM (SELECT 1 FROM messages m WHERE m.account_id = a.id AND m.channel = 'email' AND NOT m.is_test AND m.sent_at IS NOT NULL LIMIT $2) x) >= $2
            AND COALESCE((SELECT m.created_at FROM messages m WHERE m.account_id = a.id AND m.channel = 'email' AND NOT m.is_test AND m.sent_at IS NOT NULL ORDER BY m.created_at LIMIT 1) < now() - make_interval(days => $4), false)
         FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .bind(ESTABLISHED_EMAILS)
    .bind(ESTABLISHED_NO_PAUSE_DAYS)
    .bind(ESTABLISHED_DAYS)
    .fetch_one(&state.db)
    .await
}

/// A content check's result.
#[derive(Debug, Clone)]
pub struct Checked {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub screening_id: Uuid,
    /// A verdict from earlier for the same content (no new call, no new alert).
    pub reused: bool,
}

/// Check the messages (rules, then the model), or reuse the verdict on the same content.
pub async fn check(state: &AppState, account: &Account, m: &Material, deadline: Duration) -> AppResult<Checked> {
    let own = own_domains(state, account).await?;
    let material = json!({
        "account_name": crate::deliverability::plain(&account.name, 200),
        "website": account.website.as_deref().and_then(fetch::website_host),
        "own_domains": own.iter().collect::<std::collections::BTreeSet<_>>(),
        "kind": m.subject,
        "name": crate::deliverability::plain(&m.name, 200),
        "first_campaign": m.first_campaign,
        "messages": m.messages,
    });
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(&material).unwrap_or_default()));
    let earlier: Option<(Uuid, String, Value, String)> = sqlx::query_as(
        "SELECT id, verdict, reasons, source FROM screenings WHERE account_id = $1 AND kind = 'content' AND content_hash = $2
           AND (source IN ('ai', 'operator') OR verdict = 'block' OR (source = 'rules' AND verdict = 'pass'))
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account.id)
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?;
    if let Some((id, verdict, reasons, _)) = earlier {
        return Ok(Checked { verdict: Verdict::parse(&verdict).unwrap_or(Verdict::Review), reasons: serde_json::from_value(reasons).unwrap_or_default(), screening_id: id, reused: true });
    }
    let (rules_verdict, mut reasons) = judge_rules(m, &own);
    let mut signals = json!({"rules": rules_verdict.as_str(), "messages": m.messages.len(), "link_domains": m.messages.iter().flat_map(|x| x["links"].as_array().cloned().unwrap_or_default()).filter_map(|l| l["domain"].as_str().map(str::to_string)).collect::<std::collections::BTreeSet<_>>()});
    let mut judged = None;
    let verdict = if rules_verdict == Verdict::Block {
        Verdict::Block
    } else {
        let task = "Task: judge this email or text content (a campaign or the steps of an automated flow) against DumbMail's acceptable use rules. \
                    You get the business's name, website and own domains, and each message's subject, preview text, visible text and links. \
                    Pass ordinary marketing from a business to its subscribers. Review anything unclear or borderline. Block clear cold outreach, phishing, impersonation, deception or a prohibited category.";
        match ai::judge(state, account.id, task, &material, deadline).await {
            Ok(j) => {
                signals["model"] = json!({"verdict": j.verdict.as_str(), "notes": j.notes});
                reasons.extend(j.reasons.iter().cloned());
                let v = j.verdict.max(rules_verdict);
                judged = Some(j);
                v
            }
            Err(e) => {
                let est = established(state, account.id).await?;
                signals["model"] = json!({"unavailable": e.describe(), "established": est});
                match e {
                    NoJudgment::Unavailable(_) if est => {
                        if rules_verdict == Verdict::Pass {
                            reasons.push("Checked by our rules only: our AI check couldn't be reached, and your account's sending history let it go ahead.".into());
                        }
                        rules_verdict
                    }
                    NoJudgment::Refused => {
                        reasons.push("This needs a person's check.".into());
                        Verdict::Review.max(rules_verdict)
                    }
                    _ => {
                        reasons.push("A person will check this (our automatic check couldn't finish).".into());
                        Verdict::Review.max(rules_verdict)
                    }
                }
            }
        }
    };
    let mut row = New::rules("content", verdict, &reasons, signals);
    row.subject = Some(m.subject);
    row.subject_id = Some(m.subject_id);
    row.subject_name = Some(m.name.clone());
    row.content_hash = Some(hash);
    if let Some(j) = &judged {
        row = row.judged(j);
    }
    let id = standing::record(&state.db, account.id, row).await?;
    Ok(Checked { verdict, reasons, screening_id: id, reused: false })
}

/// What a check's result does: a pass goes on; a review holds it (the operator is alerted); a
/// block refuses it and puts the account in review.
async fn apply(state: &AppState, account_id: Uuid, m: &Material, r: &Checked) -> AppResult<()> {
    let what = format!("{} \"{}\"", m.subject, crate::deliverability::plain(&m.name, 80));
    let reasons = r.reasons.join(" ");
    match r.verdict {
        Verdict::Pass => Ok(()),
        Verdict::Review => {
            if !r.reused {
                standing::alert_held(state, account_id, &what, r.screening_id, &reasons).await;
            }
            Err(AppError::Conflict(refusal(r.verdict, &r.reasons)))
        }
        Verdict::Block => {
            if !r.reused {
                let why = format!("Content refused ({what}): {reasons}");
                standing::move_to(state, account_id, &[Standing::Pending, Standing::Approved], Standing::Review, "Something you tried to send needs a person's check.", &why)
                    .await
                    .map_err(AppError::Internal)?;
            }
            Err(AppError::Conflict(refusal(r.verdict, &r.reasons)))
        }
    }
}

async fn answered(state: &AppState, account_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT sending_review_answers IS NOT NULL FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?)
}

/// What a pending account is told after its check didn't approve it yet.
async fn pending_message(state: &AppState, account_id: Uuid, now: Standing) -> AppResult<String> {
    if now == Standing::Pending {
        let note: Option<String> = sqlx::query_scalar("SELECT sending_review_note FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
        if let Some(n) = note.filter(|n| n.starts_with("Next step")) {
            return Ok(n);
        }
    }
    Ok(standing::hold_message(now).to_string())
}

/// Run the account's first check (when pending) and the content check together, within
/// [`DEADLINE`]. The account's standing comes first: content is judged only for an approved one.
async fn with_account_check(state: &AppState, account: &Account, m: &Material, block_unless_approved: bool) -> AppResult<()> {
    let start = Instant::now();
    let st = standing::of(state, account);
    match st {
        Standing::Blocked => return Err(AppError::Conflict(standing::BLOCKED_MESSAGE.into())),
        Standing::Review if block_unless_approved => return Err(AppError::Conflict(standing::REVIEW_MESSAGE.into())),
        Standing::Pending if !answered(state, account.id).await? => return Err(AppError::Conflict(standing::ANSWERS_NEEDED.into())),
        _ => {}
    }
    if st == Standing::Pending && block_unless_approved && account.verified_domains.is_empty() {
        // The check waits for a verified sending domain, and a campaign can't go before it: no
        // content call yet.
        let now = super::account::run(state, account.id, None, DEADLINE).await.map_err(AppError::Internal)?;
        return Err(AppError::Conflict(pending_message(state, account.id, now).await?));
    }
    if st == Standing::Pending {
        let (now, content) = tokio::join!(super::account::run(state, account.id, Some(json!(m.messages)), DEADLINE), check(state, account, m, DEADLINE));
        let now = now.map_err(AppError::Internal)?;
        if now != Standing::Approved && (block_unless_approved || now == Standing::Blocked) {
            return Err(AppError::Conflict(pending_message(state, account.id, now).await?));
        }
        return apply(state, account.id, m, &content?).await;
    }
    let r = check(state, account, m, DEADLINE.saturating_sub(start.elapsed())).await?;
    apply(state, account.id, m, &r).await
}

/// Before a campaign launches or is scheduled (`api::messaging::launch`).
pub async fn before_launch(state: &AppState, account: &Account, c: &CampaignRow) -> AppResult<()> {
    if !super::on(state) || c.channel == "push" {
        return Ok(());
    }
    let st = standing::of(state, account);
    if st != Standing::Approved && st != Standing::Pending {
        return Err(AppError::Conflict(standing::hold_message(st).into()));
    }
    let m = campaign_material(state, c).await?;
    with_account_check(state, account, &m, true).await
}

/// Before a flow is turned on (`api::messaging::set_flow_status`). A flow can go live while the
/// account isn't approved yet; its messages wait (see `gate`).
pub async fn before_flow_live(state: &AppState, account: &Account, f: &FlowRow) -> AppResult<()> {
    if !super::on(state) {
        return Ok(());
    }
    let all = steps(&f.definition);
    if all.is_empty() {
        return Ok(());
    }
    let m = flow_material(state, f, &all).await?;
    with_account_check(state, account, &m, false).await
}

/// Before a live flow's definition is replaced (`api::messaging::update_flow`): its new or changed
/// email and text steps are checked.
pub async fn before_flow_edit(state: &AppState, account_id: Uuid, cur: &FlowRow, new_def: Option<&Value>) -> AppResult<()> {
    let Some(new_def) = new_def else { return Ok(()) };
    if !super::on(state) || cur.status != "live" {
        return Ok(());
    }
    let old = steps(&cur.definition);
    let changed: BTreeMap<String, (String, Value)> = steps(new_def).into_iter().filter(|(id, v)| old.get(id) != Some(v)).collect();
    if changed.is_empty() {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    if standing::of(state, &account) == Standing::Blocked {
        return Err(AppError::Conflict(standing::BLOCKED_MESSAGE.into()));
    }
    let m = flow_material(state, cur, &changed).await?;
    let r = check(state, &account, &m, DEADLINE).await?;
    apply(state, account_id, &m, &r).await
}

static VERIFIED: Lazy<std::sync::Mutex<HashSet<(Uuid, String)>>> = Lazy::new(|| std::sync::Mutex::new(HashSet::new()));

/// A campaign about to send (its batch job), from an approved account: what it sends now must be
/// content that passed. A scheduled campaign can be edited after it was checked, and saved blocks
/// can change; then it's checked again here (the same content is never judged twice). Some((the
/// customer's message, the skip reason)): don't send it.
pub async fn verify_at_send(state: &AppState, account: &Account, c: &CampaignRow) -> anyhow::Result<Option<(String, &'static str)>> {
    let key = (c.id, hex::encode(Sha256::digest(serde_json::to_vec(&json!([c.content, c.ab_test])).unwrap_or_default())));
    if VERIFIED.lock().is_ok_and(|v| v.contains(&key)) {
        return Ok(None);
    }
    let m = campaign_material(state, c).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let r = check(state, account, &m, DEADLINE).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if r.verdict == Verdict::Pass {
        if let Ok(mut v) = VERIFIED.lock() {
            if v.len() > 10_000 {
                v.clear();
            }
            v.insert(key);
        }
        return Ok(None);
    }
    // Stopped with its own reason first, then the account's standing follows (a block puts it in review).
    let reason = if r.verdict == Verdict::Block { "content refused" } else { "content held for review" };
    let msg = refusal(r.verdict, &r.reasons);
    standing::stop_campaign(&state.db, c.id, &msg, reason).await?;
    let _ = apply(state, account.id, &m, &r).await;
    Ok(Some((msg, reason)))
}

fn refusal(verdict: Verdict, reasons: &[String]) -> String {
    let reasons = reasons.join(" ");
    match verdict {
        Verdict::Block => format!("This can't be sent: {reasons}"),
        _ => format!("{HELD_MESSAGE} {reasons}").trim().to_string(),
    }
}

/// `content` with every use of saved block `id` replaced by `blocks` (as it will read once saved).
fn with_saved(content: &Value, id: &str, blocks: &Value) -> Value {
    match content {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .flat_map(|x| {
                    if x["type"] == "saved" && x["saved_block_id"].as_str() == Some(id) {
                        blocks.as_array().cloned().unwrap_or_default()
                    } else {
                        vec![with_saved(x, id, blocks)]
                    }
                })
                .collect(),
        ),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), with_saved(v, id, blocks))).collect()),
        other => other.clone(),
    }
}

/// Before saved block `id` changes to `blocks` (`content::saved::update`): live flows that use it
/// would send the new version with nobody launching anything, so their steps are checked with it
/// first. (Scheduled campaigns are checked again when they send: see [`verify_at_send`].)
pub async fn before_saved_block_edit(state: &AppState, account_id: Uuid, id: Uuid, blocks: &Value) -> AppResult<()> {
    if !super::on(state) {
        return Ok(());
    }
    // Every live flow that uses it is checked; past the cap the edit is refused rather than any skipped.
    let flows: Vec<FlowRow> = sqlx::query_as(
        "SELECT * FROM flows WHERE account_id = $1 AND status = 'live' AND jsonb_path_exists(definition, '$.** ? (@.type == \"saved\" && @.saved_block_id == $id)'::jsonpath, jsonb_build_object('id', $2::text)) ORDER BY id LIMIT $3",
    )
    .bind(account_id)
    .bind(id.to_string())
    .bind(MAX_FLOWS_PER_BLOCK + 1)
    .fetch_all(&state.db)
    .await?;
    if flows.is_empty() {
        return Ok(());
    }
    if flows.len() as i64 > MAX_FLOWS_PER_BLOCK {
        return Err(AppError::Conflict(format!(
            "This saved block is used by more than {MAX_FLOWS_PER_BLOCK} live flows, too many to check at once. Turn some of them off or detach the block, then save again."
        )));
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let mut materials = Vec::new();
    for f in flows {
        let uses: BTreeMap<String, (String, Value)> = steps(&f.definition)
            .into_iter()
            .filter(|(_, (_, c))| c.to_string().contains(&id.to_string()))
            .map(|(k, (ch, c))| (k, (ch, with_saved(&c, &id.to_string(), blocks))))
            .collect();
        if !uses.is_empty() {
            materials.push(flow_material(state, &f, &uses).await?);
        }
    }
    // Checked side by side (the model calls are limited server-wide), all within the deadline.
    let checks = futures_util::future::join_all(materials.iter().map(|m| check(state, &account, m, DEADLINE)));
    let results = tokio::time::timeout(DEADLINE + Duration::from_secs(5), checks)
        .await
        .map_err(|_| AppError::Busy("Checking the live flows that use this block took too long. Try saving again in a minute.".into()))?;
    // Every result takes effect (a block's standing change first); the most severe one is answered.
    let mut done: Vec<(&Material, Checked)> = Vec::new();
    for (m, r) in materials.iter().zip(results) {
        done.push((m, r?));
    }
    done.sort_by(|a, b| b.1.verdict.cmp(&a.1.verdict));
    let mut first = None;
    for (m, r) in &done {
        if let Err(e) = apply(state, account_id, m, r).await {
            first.get_or_insert(e);
        }
    }
    if let Some(e) = first {
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mat(subject: &str, html: &str, first: bool) -> Material {
        Material {
            subject: "campaign",
            subject_id: Uuid::nil(),
            name: "x".into(),
            first_campaign: first,
            messages: vec![message("email", &json!({"subject": subject, "preheader": "", "html": html}))],
        }
    }

    #[test]
    fn destinations_keep_all_but_link_tags() {
        assert_eq!(destination("https://acme.test/fall?utm_source=dm&utm_campaign=x#top"), "https://acme.test/fall");
        assert_eq!(destination("https://acme.test/p?id=7&utm_medium=email"), "https://acme.test/p?id=7");
        assert_ne!(destination("https://acme.test/a"), destination("https://acme.test/b"));
    }

    #[test]
    fn saved_blocks_are_swapped_in() {
        let id = Uuid::now_v7();
        let c = json!({"subject": "s", "blocks": [{"type": "text", "html": "a"}, {"type": "saved", "saved_block_id": id.to_string()},
            {"type": "columns", "columns": [[{"type": "saved", "saved_block_id": id.to_string()}]]}]});
        let out = with_saved(&c, &id.to_string(), &json!([{"type": "text", "html": "NEW"}, {"type": "text", "html": "NEW2"}]));
        assert_eq!(out["blocks"].as_array().unwrap().len(), 4);
        assert_eq!(out["blocks"][1]["html"], "NEW");
        assert_eq!(out["blocks"][3]["columns"][0][1]["html"], "NEW2");
        assert!(!out.to_string().contains("saved_block_id"));
    }

    #[test]
    fn content_rules() {
        let own: HashSet<String> = ["acme.test".to_string()].into();
        assert_eq!(judge_rules(&mat("Fall sale", "<p>Shop <a href=\"https://acme.test/sale\">now</a></p>", true), &own).0, Verdict::Pass);
        let phish = mat("Action needed", "<p>We saw an unusual sign-in. <a href=\"https://login.evil.test\">Verify your account</a></p>", false);
        assert_eq!(judge_rules(&phish, &own).0, Verdict::Block);
        let ok_reset = mat("Hi", "<p>Reset your password at <a href=\"https://acme.test/reset\">acme</a></p>", false);
        assert_eq!(judge_rules(&ok_reset, &own).0, Verdict::Pass, "credential words with only the sender's own links");
        assert_eq!(judge_rules(&mat("Re: our call", "<p>hi</p>", false), &own).0, Verdict::Review);
        assert_eq!(judge_rules(&mat("Hi", "<a href=\"https://evil.test/x\">acme.test</a>", false), &own).0, Verdict::Review, "link text names another domain");
        assert_eq!(judge_rules(&mat("Hi", "<a href=\"https://bit.ly/x\">go</a>", true), &own).0, Verdict::Review, "a shortener in a first campaign");
        assert_eq!(judge_rules(&mat("Hi", "<a href=\"https://bit.ly/x\">go</a>", false), &own).0, Verdict::Pass);
    }
}

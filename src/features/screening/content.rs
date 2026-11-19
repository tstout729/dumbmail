//! What a campaign or flow sends: rules first, then one Claude call against the acceptable use
//! rules.
//!
//! What's judged is the content exactly as committed, every way it can go out: each A/B version,
//! each language version, every block whatever its display rules, both sides of every template
//! condition (the text is read as written, not one sample person's render), the saved blocks it
//! uses, and review-request steps. Each message variant gets its own fingerprint (`part_hashes` on
//! the screening), and sending checks the message it's about to send against those: a campaign in
//! its batch job, a flow message in the send path (see `gate`). Content that changed since its
//! check (an edit while scheduled or as a draft, a saved block, a translation) waits for a check on
//! the background lane; the sending lane never calls Claude. More than the check can cover is held
//! for a person, never cut short.
//!
//! The same content (the hash of what was judged) is judged once per account, except a pass the
//! rules gave alone while Claude was unavailable, which is judged again next time.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::ai::{self, NoJudgment};
use super::fetch;
use super::redact;
use super::rules;
use super::standing::{self, New, Standing};
use super::Verdict;
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, FlowRow};
use crate::state::AppState;

/// How long a launch (or turning a flow on, or saving) may take with its checks, all of it:
/// rendering, the model, the quota and the notices.
pub const DEADLINE: Duration = Duration::from_secs(45);
/// The most text in one message variant.
pub const TEXT_CHARS: usize = 20_000;
/// The most message variants (versions × languages × steps) one check covers.
pub const MAX_MESSAGES: usize = 60;
/// The most links in one message variant.
pub const MAX_LINKS: usize = 80;
/// The most a check may send to the model.
pub const MAX_MATERIAL_BYTES: usize = 200 * 1024;
/// The most live flows one saved block edit checks (more: the edit is refused).
const MAX_FLOWS_PER_BLOCK: i64 = 10;
/// Established accounts (rules decide when the model can't be reached): approved, this many real
/// emails over at least this many days, and no pause in the last 30 days.
pub const ESTABLISHED_EMAILS: i64 = 1000;
pub const ESTABLISHED_DAYS: i32 = 14;
pub const ESTABLISHED_NO_PAUSE_DAYS: i32 = 30;
/// A campaign whose content can't get a verdict in this long after it was due is held for a person.
const SEND_CHECK_WAIT: chrono::Duration = chrono::Duration::minutes(30);

pub const HELD_MESSAGE: &str = "A person is checking this before it goes out. We'll email you when it's done, usually within a day.";
pub const TOO_BIG: &str = "This is too large for our automatic check (too many versions, languages or steps, or too much text), so a person will look at it.";
pub const JOB: &str = "screening_content_check";

static ANCHOR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<a\s[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap());
static BARE_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)https?://[^\s<>"')]+"#).unwrap());

/// Fields that never show as words: a block's kind and ids, layout and colours, display rules.
/// Every other string a block holds is read (as text, or as a link when it's a web address), so a
/// new kind of block or field can't slip past the check.
const NOT_SHOWN: &[&str] = &[
    "type", "id", "k", "key", "saved_block_id", "name", "color", "colour", "background", "background_color", "bg", "text_color", "button_color", "link_color", "border",
    "border_color", "align", "width", "height", "padding", "margin", "font", "font_family", "font_size", "font_weight", "size", "style", "radius", "layout", "variant",
    "display", "rules", "condition", "conditions", "spacing", "weight", "icon", "network", "ratio", "position", "format", "theme", "coupon", "coupon_id", "product_id",
    "list_id", "segment_id", "metric", "channel", "currency", "locale", "lang", "src", "image", "logo", "thumbnail", "image_url", "video_thumbnail",
];

fn shown(s: &str) -> bool {
    static NOISE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(#[0-9a-fA-F]{3,8}|[0-9.]+(px|%|em|rem)?|true|false)$").unwrap());
    !s.trim().is_empty() && !NOISE.is_match(s.trim())
}

fn host(u: &str) -> Option<String> {
    url::Url::parse(u.trim()).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
}

/// Where a link goes, as judged: scheme, host, path and query, without link tags (`utm_…`, added
/// at send time) or the fragment, so any other change to a destination is judged again.
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

#[derive(Default)]
struct Collected {
    text: Vec<String>,
    /// (link text, href)
    links: Vec<(String, String)>,
}

impl Collected {
    fn html(&mut self, html: &str) {
        for c in ANCHOR.captures_iter(html) {
            self.links.push((fetch::visible_text(&c[2], 120), c[1].replace("&amp;", "&")));
        }
        let text = fetch::visible_text(html, usize::MAX);
        // Addresses written out in the text (email apps make them links).
        for m in BARE_URL.find_iter(&text) {
            self.links.push((String::new(), m.as_str().to_string()));
        }
        self.text.push(text);
    }

    /// One string a block holds: always read as text (markup, anchors and written-out addresses
    /// included); a value that is itself just a web address is also a link, with the block's own
    /// words as its text.
    fn string(&mut self, s: &str, link_text: &str) {
        let t = s.trim();
        if !shown(t) {
            return;
        }
        let bare_url = (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace) && !t.contains('<') && url::Url::parse(t).is_ok();
        if bare_url {
            self.links.push((fetch::visible_text(link_text, 120), t.to_string()));
        }
        self.html(t);
    }

    /// Every string a block holds, whatever its display rules, template conditions included: text
    /// fields, table cells, captions, links. Only fields that never show as words are left out.
    fn walk(&mut self, v: &Value, link_text: &str) {
        match v {
            Value::String(s) => self.string(s, link_text),
            Value::Array(a) => a.iter().for_each(|x| self.walk(x, link_text)),
            Value::Object(o) => {
                let own_text = o.get("text").or_else(|| o.get("button_text")).or_else(|| o.get("title")).or_else(|| o.get("alt")).and_then(|t| t.as_str()).unwrap_or(link_text);
                for (k, x) in o {
                    if k.starts_with('_') || k == "i18n" || k == "translations" || NOT_SHOWN.contains(&k.as_str()) {
                        continue;
                    }
                    self.walk(x, own_text);
                }
            }
            _ => {}
        }
    }
}

/// One message variant as judged (the full form: rules and fingerprints use it; the model gets
/// [`for_model`]). Over-long text is marked, not cut.
fn variant(channel: &str, c: &Value, label: &str, language: &str) -> (Value, bool) {
    let mut col = Collected::default();
    let (subject, preview) = if channel == "sms" {
        let body = c["body"].as_str().unwrap_or("");
        col.text.push(body.to_string());
        for m in BARE_URL.find_iter(body) {
            col.links.push((String::new(), m.as_str().to_string()));
        }
        if let Some(m) = c["media_url"].as_str().filter(|m| !m.is_empty()) {
            col.links.push((String::new(), m.to_string()));
        }
        (String::new(), String::new())
    } else {
        if let Some(h) = c["html"].as_str() {
            col.html(h);
        }
        if let Some(b) = c.get("blocks") {
            col.walk(b, "");
        }
        (c["subject"].as_str().unwrap_or("").to_string(), c["preheader"].as_str().unwrap_or("").to_string())
    };
    let text = col.text.iter().filter(|t| !t.trim().is_empty()).cloned().collect::<Vec<_>>().join("\n");
    let mut too_big = text.chars().count() > TEXT_CHARS || subject.chars().count() > 1000 || col.links.len() > MAX_LINKS;
    let mut seen = HashSet::new();
    let links: Vec<Value> = col
        .links
        .iter()
        .filter_map(|(t, h)| {
            let d = host(h)?;
            let to = destination(h);
            seen.insert((t.clone(), to.clone())).then(|| json!({"text": t, "to": to, "domain": d}))
        })
        .collect();
    too_big |= links.len() > MAX_LINKS;
    (json!({"channel": channel, "label": label, "language": language, "subject": subject, "preview_text": preview, "text": text, "links": links}), too_big)
}

/// A variant's fingerprint: what people get, not where it sits.
fn part_hash(v: &Value) -> String {
    let core = json!([v["channel"], v["subject"], v["preview_text"], v["text"], v["links"]]);
    hex::encode(Sha256::digest(serde_json::to_vec(&core).unwrap_or_default()))
}

/// The saved blocks `content` uses.
fn collect_saved_ids(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect_saved_ids(x, out)),
        Value::Object(o) => {
            if o.get("type").and_then(|t| t.as_str()) == Some("saved") {
                if let Some(id) = o.get("saved_block_id").and_then(|i| i.as_str()) {
                    out.insert(id.to_string());
                }
            }
            o.values().for_each(|x| collect_saved_ids(x, out));
        }
        _ => {}
    }
}

/// `content` with every saved block it uses swapped for that block's current content.
async fn with_saved_blocks(state: &AppState, account_id: Uuid, content: &Value) -> AppResult<Value> {
    let mut ids = BTreeSet::new();
    collect_saved_ids(content, &mut ids);
    if ids.is_empty() {
        return Ok(content.clone());
    }
    let uuids: Vec<Uuid> = ids.iter().filter_map(|i| i.parse().ok()).collect();
    let rows: Vec<(Uuid, Value)> = sqlx::query_as("SELECT id, blocks FROM saved_blocks WHERE account_id = $1 AND id = ANY($2) AND deleted_at IS NULL").bind(account_id).bind(&uuids).fetch_all(&state.db).await?;
    let mut out = content.clone();
    for (id, blocks) in rows {
        out = with_saved(&out, &id.to_string(), &blocks);
    }
    Ok(out)
}

/// Every variant of one message: its default text and each language version.
async fn variants(state: &AppState, account: &Account, channel: &str, content: &Value, label: &str) -> AppResult<Vec<(Value, bool)>> {
    let content = with_saved_blocks(state, account.id, content).await?;
    let (_, mut langs) = crate::features::translations::fields::languages(&content);
    if let Some(b) = content.get("blocks") {
        crate::features::translations::fields::blocks_languages(b, &mut langs);
    }
    langs.sort();
    langs.dedup();
    let own = crate::features::translations::own_hosts(state, Some(account));
    let mut out = vec![variant(channel, &content, label, "")];
    for l in langs {
        out.push(variant(channel, &crate::features::translations::fields::localize(&content, &l, &own), label, &l));
    }
    Ok(out)
}

/// What a check is about.
pub struct Material {
    pub subject: &'static str,
    pub subject_id: Uuid,
    pub name: String,
    pub first_campaign: bool,
    pub messages: Vec<Value>,
    pub parts: Vec<String>,
    /// More than a check can cover: a person decides.
    pub too_big: bool,
}

impl Material {
    fn new(subject: &'static str, subject_id: Uuid, name: &str, first_campaign: bool, all: Vec<(Value, bool)>) -> Self {
        let too_big = all.len() > MAX_MESSAGES || all.iter().any(|(_, b)| *b);
        let messages: Vec<Value> = all.into_iter().map(|(v, _)| v).collect();
        let mut parts: Vec<String> = messages.iter().map(part_hash).collect();
        parts.sort();
        parts.dedup();
        Self { subject, subject_id, name: name.to_string(), first_campaign, messages, parts, too_big }
    }
}

pub async fn campaign_material(state: &AppState, account: &Account, c: &CampaignRow) -> AppResult<Material> {
    let mut versions = vec![("version a".to_string(), c.content.clone())];
    versions.extend(crate::campaigns::ab_test(c).into_iter().flat_map(|t| t.variants).map(|v| (format!("version {}", v.id), v.content)));
    let mut all = Vec::new();
    for (label, v) in &versions {
        all.extend(variants(state, account, &c.channel, v, label).await?);
    }
    let earlier: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM campaigns WHERE account_id = $1 AND id <> $2 AND status IN ('sending', 'sent', 'testing'))")
        .bind(c.account_id)
        .bind(c.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Material::new("campaign", c.id, &c.name, !earlier, all))
}

/// The steps of a flow definition that send email or texts (review requests included), by id:
/// (channel, content).
fn steps(def: &Value) -> BTreeMap<String, (String, Value)> {
    let Ok(d) = crate::flows::parse_definition(def) else { return BTreeMap::new() };
    d.nodes
        .into_iter()
        .filter_map(|n| {
            let channel = match n.kind.as_str() {
                "email" | "sms" => n.kind.clone(),
                "review_request" => n.data.get("channel").and_then(|c| c.as_str()).unwrap_or("email").to_string(),
                _ => return None,
            };
            Some((n.id.clone(), (channel, n.data.get("content").cloned().unwrap_or(json!({})))))
        })
        .collect()
}

async fn flow_material(state: &AppState, account: &Account, f: &FlowRow, only: &BTreeMap<String, (String, Value)>) -> AppResult<Material> {
    let mut all = Vec::new();
    for (id, (channel, content)) in only {
        all.extend(variants(state, account, channel, content, &format!("step {id}")).await?);
    }
    Ok(Material::new("flow", f.id, &f.name, false, all))
}

/// The messages as the model sees them: free text with addresses and phone numbers replaced, links
/// as domain and path with anything identifying replaced.
fn for_model(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let links: Vec<Value> = m["links"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| json!({"text": redact::text(l["text"].as_str().unwrap_or("")), "to": redact::link(l["to"].as_str().unwrap_or("")), "domain": l["domain"]}))
                .collect();
            json!({
                "channel": m["channel"], "label": m["label"], "language": m["language"],
                "subject": redact::text(m["subject"].as_str().unwrap_or("")),
                "preview_text": redact::text(m["preview_text"].as_str().unwrap_or("")),
                "text": redact::text(m["text"].as_str().unwrap_or("")),
                "links": links,
            })
        })
        .collect()
}

/// The newest campaign's or flow's messages (as the model may see them), for an account check
/// with no send in hand.
pub async fn latest_material(state: &AppState, account_id: Uuid) -> anyhow::Result<Option<Value>> {
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let c: Option<CampaignRow> = sqlx::query_as("SELECT * FROM campaigns WHERE account_id = $1 AND channel IN ('email', 'sms') ORDER BY updated_at DESC LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    if let Some(c) = c {
        if let Ok(m) = campaign_material(state, &account, &c).await {
            return Ok(Some(json!(for_model(&m.messages[..m.messages.len().min(MAX_MESSAGES)]))));
        }
    }
    let f: Option<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 ORDER BY updated_at DESC LIMIT 1").bind(account_id).fetch_optional(&state.db).await?;
    if let Some(f) = f {
        if let Ok(m) = flow_material(state, &account, &f, &steps(&f.definition)).await {
            if !m.messages.is_empty() {
                return Ok(Some(json!(for_model(&m.messages[..m.messages.len().min(MAX_MESSAGES)]))));
            }
        }
    }
    Ok(None)
}

/// The messages for the account's first check (as the model may see them).
pub fn model_messages(m: &Material) -> Value {
    json!(for_model(&m.messages[..m.messages.len().min(MAX_MESSAGES)]))
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
        for k in ["subject", "preview_text", "text"] {
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
    let own_sorted: BTreeSet<&String> = own.iter().collect();
    let judged = json!({"account": account.id, "own_domains": own_sorted, "kind": m.subject, "first_campaign": m.first_campaign, "messages": m.messages});
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(&judged).unwrap_or_default()));
    // A pass the rules gave alone (Claude unavailable) isn't reused: it's judged again.
    let earlier: Option<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT id, verdict, reasons FROM screenings WHERE account_id = $1 AND kind = 'content' AND content_hash = $2 AND (source IN ('ai', 'operator') OR verdict = 'block')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account.id)
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?;
    if let Some((id, verdict, reasons)) = earlier {
        return Ok(Checked { verdict: Verdict::parse(&verdict).unwrap_or(Verdict::Review), reasons: serde_json::from_value(reasons).unwrap_or_default(), screening_id: id, reused: true });
    }
    let (rules_verdict, mut reasons) = judge_rules(m, &own);
    let link_domains: BTreeSet<String> = m.messages.iter().flat_map(|x| x["links"].as_array().cloned().unwrap_or_default()).filter_map(|l| l["domain"].as_str().map(str::to_string)).collect();
    let mut signals = json!({"rules": rules_verdict.as_str(), "messages": m.messages.len(), "link_domains": link_domains});
    let mut judged_by = None;
    let model_material = json!({
        "account_name": redact::text(&crate::deliverability::plain(&account.name, 200)),
        "website": account.website.as_deref().and_then(fetch::website_host),
        "own_domains": own_sorted,
        "kind": m.subject,
        "name": redact::text(&crate::deliverability::plain(&m.name, 200)),
        "first_campaign": m.first_campaign,
        "messages": for_model(&m.messages),
    });
    let too_big = m.too_big || serde_json::to_vec(&model_material).map_or(true, |b| b.len() > MAX_MATERIAL_BYTES);
    let verdict = if rules_verdict == Verdict::Block {
        Verdict::Block
    } else if too_big {
        signals["too_big"] = json!(true);
        reasons.push(TOO_BIG.into());
        Verdict::Review
    } else {
        let task = "Task: judge this email or text content (a campaign with all its versions and languages, or the steps of an automated flow) against DumbMail's acceptable use rules. \
                    You get the business's name, website and own domains, and each message's subject, preview text, text as written (template conditions and all blocks included) and links. \
                    Pass ordinary marketing from a business to its subscribers. Review anything unclear or borderline. Block clear cold outreach, phishing, impersonation, deception or a prohibited category.";
        match ai::judge(state, account.id, task, &model_material, deadline).await {
            Ok(j) => {
                signals["model"] = json!({"verdict": j.verdict.as_str(), "codes": j.codes, "notes": j.notes});
                reasons.extend(j.reasons.iter().cloned());
                let v = j.verdict.max(rules_verdict);
                judged_by = Some(j);
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
    reasons.dedup();
    let mut row = New::rules("content", verdict, &reasons, signals);
    row.subject = Some(m.subject);
    row.subject_id = Some(m.subject_id);
    row.subject_name = Some(m.name.clone());
    row.content_hash = Some(hash);
    row.part_hashes = m.parts.clone();
    if let Some(j) = &judged_by {
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

/// For tests: a shorter deadline than [`DEADLINE`] for one account.
#[doc(hidden)]
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use once_cell::sync::Lazy;
    use uuid::Uuid;

    pub(super) static DEADLINES: Lazy<Mutex<HashMap<Uuid, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    pub fn deadline(account_id: Uuid, ms: u64) {
        if let Ok(mut d) = DEADLINES.lock() {
            d.insert(account_id, ms);
        }
    }
}

/// The deadline for this account's checks ([`DEADLINE`], unless a test set its own).
pub fn deadline(account_id: Uuid) -> Duration {
    testing::DEADLINES.lock().ok().and_then(|d| d.get(&account_id).copied()).map(Duration::from_millis).unwrap_or(DEADLINE)
}

/// All of `f` within the deadline (rendering, the checks, the quota, the notices).
async fn within<T>(account_id: Uuid, f: impl std::future::Future<Output = AppResult<T>>) -> AppResult<T> {
    tokio::time::timeout(deadline(account_id), f).await.map_err(|_| AppError::Busy("Checking this took too long. Try again in a minute.".into()))?
}

/// Run the account's first check (when pending) and the content check side by side. One decision
/// about the account is written once both are done: a content block keeps a pending account from
/// being approved, and nothing is published in between.
async fn with_account_check(state: &AppState, account: &Account, m: &Material, block_unless_approved: bool, start: Instant) -> AppResult<()> {
    let left = || deadline(account.id).saturating_sub(start.elapsed());
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
        let now = super::account::run(state, account.id, None, left()).await.map_err(AppError::Internal)?;
        return Err(AppError::Conflict(pending_message(state, account.id, now).await?));
    }
    if st == Standing::Pending {
        let Some(claim) = super::account::claim(state, account.id).await.map_err(AppError::Internal)? else {
            return Err(AppError::Conflict(standing::PENDING_MESSAGE.into()));
        };
        let (assessed, content) = tokio::join!(super::account::assess(state, account.id, Some(model_messages(m)), left(), start), check(state, account, m, left()));
        let decided = async {
            let content = content?;
            if let Some(mut d) = assessed.map_err(AppError::Internal)? {
                if content.verdict == Verdict::Block && d.verdict == Verdict::Pass {
                    d.verdict = Verdict::Review;
                    d.reasons.push("Something you tried to send needs a person's check.".into());
                }
                super::account::decide(state, account.id, d).await.map_err(AppError::Internal)?;
            }
            Ok::<_, AppError>(content)
        }
        .await;
        super::account::release(state, claim).await.map_err(AppError::Internal)?;
        let content = decided?;
        let now = standing::read(state, account.id).await?;
        if now != Standing::Approved && (block_unless_approved || now == Standing::Blocked) {
            return Err(AppError::Conflict(pending_message(state, account.id, now).await?));
        }
        return apply(state, account.id, m, &content).await;
    }
    let r = check(state, account, m, left()).await?;
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
    let start = Instant::now();
    within(account.id, async {
        let m = campaign_material(state, account, c).await?;
        with_account_check(state, account, &m, true, start).await
    })
    .await
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
    let start = Instant::now();
    within(account.id, async {
        let m = flow_material(state, account, f, &all).await?;
        with_account_check(state, account, &m, false, start).await
    })
    .await
}

/// Before a live flow's definition is replaced (`api::messaging::update_flow`): its new or changed
/// sending steps are checked.
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
    within(account_id, async {
        let m = flow_material(state, &account, cur, &changed).await?;
        let r = check(state, &account, &m, deadline(account_id)).await?;
        apply(state, account_id, &m, &r).await
    })
    .await
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
/// first. (Campaigns, and anything that changes a block another way, are checked when they send.)
pub async fn before_saved_block_edit(state: &AppState, account_id: Uuid, id: Uuid, blocks: &Value) -> AppResult<()> {
    if !super::on(state) {
        return Ok(());
    }
    content_changed(account_id);
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
    within(account_id, async {
        let mut materials = Vec::new();
        for f in flows {
            let uses: BTreeMap<String, (String, Value)> = steps(&f.definition)
                .into_iter()
                .filter(|(_, (_, c))| c.to_string().contains(&id.to_string()))
                .map(|(k, (ch, c))| (k, (ch, with_saved(&c, &id.to_string(), blocks))))
                .collect();
            if !uses.is_empty() {
                materials.push(flow_material(state, &account, &f, &uses).await?);
            }
        }
        // Checked side by side (the model calls are limited server-wide).
        let results = futures_util::future::join_all(materials.iter().map(|m| check(state, &account, m, deadline(account_id)))).await;
        // Every result takes effect (a block's standing change first); the most severe one is answered.
        let mut done: Vec<(&Material, Checked)> = Vec::new();
        for (m, r) in materials.iter().zip(results) {
            done.push((m, r?));
        }
        done.sort_by_key(|d| std::cmp::Reverse(d.1.verdict));
        let mut first = None;
        for (m, r) in &done {
            if let Err(e) = apply(state, account_id, m, r).await {
                first.get_or_insert(e);
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })
    .await
}

// ---------- At send time ----------

/// What sending does with a message whose content was (or wasn't) checked.
#[derive(Debug, Clone, PartialEq)]
pub enum AtSend {
    Go,
    /// Not checked yet (a check is on its way) or held for a person: try again then.
    Wait(chrono::DateTime<Utc>, &'static str),
    /// Refused (`content refused`) or, for a campaign, held: don't send it. With the customer's message.
    Stop(&'static str, String),
}

/// Per account: bumped when content may have changed outside a check (a saved block, a
/// translation), so remembered verdicts are looked up again.
static EPOCH: Lazy<Mutex<HashMap<Uuid, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// (account, the content as sent) → (epoch, when) for content found to have passed.
static PASSED: Lazy<Mutex<HashMap<(Uuid, String), (u64, Instant)>>> = Lazy::new(|| Mutex::new(HashMap::new()));
const PASSED_FOR: Duration = Duration::from_secs(600);

/// Content of this account may have changed outside a check (`translations::store`, saved blocks).
pub fn content_changed(account_id: Uuid) {
    if let Ok(mut e) = EPOCH.lock() {
        *e.entry(account_id).or_default() += 1;
    }
}

fn epoch(account_id: Uuid) -> u64 {
    EPOCH.lock().ok().and_then(|e| e.get(&account_id).copied()).unwrap_or(0)
}

fn remembered(account_id: Uuid, key: &str) -> bool {
    let ep = epoch(account_id);
    PASSED.lock().is_ok_and(|p| p.get(&(account_id, key.to_string())).is_some_and(|(e, t)| *e == ep && t.elapsed() < PASSED_FOR))
}

fn remember(account_id: Uuid, key: String) {
    let ep = epoch(account_id);
    if let Ok(mut p) = PASSED.lock() {
        if p.len() > 20_000 {
            p.clear();
        }
        p.insert((account_id, key), (ep, Instant::now()));
    }
}

/// The latest verdict for every part; None when any part has none.
async fn verdict_for(state: &AppState, account_id: Uuid, parts: &[String]) -> sqlx::Result<Option<Verdict>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (p) p, s.verdict FROM screenings s, unnest(s.part_hashes) p
         WHERE s.account_id = $1 AND s.kind = 'content' AND s.part_hashes && $2 AND p = ANY($2)
         ORDER BY p, s.created_at DESC",
    )
    .bind(account_id)
    .bind(parts)
    .fetch_all(&state.db)
    .await?;
    if rows.len() < parts.len() {
        return Ok(None);
    }
    Ok(rows.iter().filter_map(|(_, v)| Verdict::parse(v)).max())
}

static KICKED: Lazy<Mutex<HashMap<String, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Queue a content check on the background lane (at most once a minute per subject per server).
async fn kick(state: &AppState, payload: Value) -> sqlx::Result<()> {
    let key = payload.to_string();
    let due = KICKED.lock().is_ok_and(|mut m| {
        m.retain(|_, t| t.elapsed() < Duration::from_secs(600));
        match m.get(&key) {
            Some(t) if t.elapsed() < Duration::from_secs(60) => false,
            _ => {
                m.insert(key.clone(), Instant::now());
                true
            }
        }
    });
    if due {
        crate::jobs::enqueue_background(&state.db, JOB, payload, None).await?;
    }
    Ok(())
}

/// A flow message about to send (`gate`): its content, exactly as it will go out, must have passed.
pub async fn flow_at_send(state: &AppState, account: &Account, flow_id: Uuid, channel: &str, content: &Value) -> AppResult<AtSend> {
    // The saved blocks it uses, as they are now, are part of what was checked.
    let saved: Vec<(Uuid, chrono::DateTime<Utc>)> = if content.to_string().contains("saved_block_id") {
        let mut ids = BTreeSet::new();
        collect_saved_ids(content, &mut ids);
        let uuids: Vec<Uuid> = ids.iter().filter_map(|i| i.parse().ok()).collect();
        sqlx::query_as("SELECT id, updated_at FROM saved_blocks WHERE account_id = $1 AND id = ANY($2) ORDER BY id").bind(account.id).bind(&uuids).fetch_all(&state.db).await?
    } else {
        Vec::new()
    };
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&json!([channel, content, saved])).unwrap_or_default()));
    if remembered(account.id, &key) {
        return Ok(AtSend::Go);
    }
    let all = variants(state, account, channel, content, "").await?;
    let mut parts: Vec<String> = all.iter().map(|(v, _)| part_hash(v)).collect();
    parts.sort();
    parts.dedup();
    let soon = Utc::now() + chrono::Duration::minutes(2);
    match verdict_for(state, account.id, &parts).await? {
        Some(Verdict::Pass) => {
            remember(account.id, key);
            Ok(AtSend::Go)
        }
        Some(Verdict::Block) => Ok(AtSend::Stop("content refused", refusal(Verdict::Block, &[]))),
        Some(Verdict::Review) => Ok(AtSend::Wait(Utc::now() + chrono::Duration::minutes(15), "content held for review")),
        None => {
            kick(state, json!({"account_id": account.id, "flow_id": flow_id})).await?;
            Ok(AtSend::Wait(soon, "content not checked"))
        }
    }
}

/// A campaign batch about to send, from an approved account: what it sends now must have passed
/// (it may have changed since it was scheduled). Unchecked content is checked on the background
/// lane while the batch waits; held or refused content stops the campaign.
pub async fn campaign_at_send(state: &AppState, account: &Account, c: &CampaignRow) -> anyhow::Result<AtSend> {
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&json!(["campaign", c.content, c.ab_test])).unwrap_or_default()));
    if remembered(account.id, &key) {
        return Ok(AtSend::Go);
    }
    let m = campaign_material(state, account, c).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let stop = |v: Verdict, reasons: &[String]| -> AtSend {
        let reason = if v == Verdict::Block { "content refused" } else { "content held for review" };
        AtSend::Stop(reason, refusal(v, reasons))
    };
    match verdict_for(state, account.id, &m.parts).await? {
        Some(Verdict::Pass) => {
            remember(account.id, key);
            Ok(AtSend::Go)
        }
        Some(v) => {
            let reasons: Option<Value> = sqlx::query_scalar("SELECT reasons FROM screenings WHERE account_id = $1 AND kind = 'content' AND part_hashes && $2 AND verdict = $3 ORDER BY created_at DESC LIMIT 1")
                .bind(account.id)
                .bind(&m.parts)
                .bind(v.as_str())
                .fetch_optional(&state.db)
                .await?;
            let reasons: Vec<String> = reasons.and_then(|r| serde_json::from_value(r).ok()).unwrap_or_default();
            Ok(stop(v, &reasons))
        }
        None if c.send_at.is_some_and(|t| t < Utc::now() - SEND_CHECK_WAIT) => {
            // No verdict in all this time: a person decides.
            let why = [TOO_BIG_WAIT.to_string()];
            let id = standing::record(&state.db, account.id, {
                let mut s = New::rules("content", Verdict::Review, &why, json!({"waited": true}));
                s.subject = Some("campaign");
                s.subject_id = Some(c.id);
                s.subject_name = Some(c.name.clone());
                s.part_hashes = m.parts.clone();
                s
            })
            .await?;
            standing::alert_held(state, account.id, &format!("campaign \"{}\"", crate::deliverability::plain(&c.name, 80)), id, TOO_BIG_WAIT).await;
            Ok(stop(Verdict::Review, &[TOO_BIG_WAIT.to_string()]))
        }
        None => {
            kick(state, json!({"account_id": account.id, "campaign_id": c.id})).await?;
            Ok(AtSend::Wait(Utc::now() + chrono::Duration::seconds(60), "content not checked"))
        }
    }
}

const TOO_BIG_WAIT: &str = "Our automatic check couldn't finish in time, so a person will look at this.";

/// Job (background lane): check a flow's or campaign's content as it is now.
pub async fn run_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let id = |k: &str| payload[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let Some(account_id) = id("account_id") else { return Ok(()) };
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let m = if let Some(fid) = id("flow_id") {
        let Some(f) = sqlx::query_as::<_, FlowRow>("SELECT * FROM flows WHERE id = $1 AND account_id = $2").bind(fid).bind(account_id).fetch_optional(&state.db).await? else { return Ok(()) };
        flow_material(state, &account, &f, &steps(&f.definition)).await
    } else if let Some(cid) = id("campaign_id") {
        let Some(c) = sqlx::query_as::<_, CampaignRow>("SELECT * FROM campaigns WHERE id = $1 AND account_id = $2").bind(cid).bind(account_id).fetch_optional(&state.db).await? else { return Ok(()) };
        campaign_material(state, &account, &c).await
    } else {
        return Ok(());
    }
    .map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let r = check(state, &account, &m, DEADLINE).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    // A campaign that didn't pass stops with its own reason first; then the effects (an alert, the
    // account's review). A flow message waits or stops when it next tries to send.
    if m.subject == "campaign" && r.verdict != Verdict::Pass {
        let reason = if r.verdict == Verdict::Block { "content refused" } else { "content held for review" };
        standing::stop_campaign(&state.db, m.subject_id, &refusal(r.verdict, &r.reasons), reason).await?;
    }
    let _ = apply(state, account_id, &m, &r).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mat(subject: &str, html: &str, first: bool) -> Material {
        let c = json!({"subject": subject, "preheader": "", "blocks": [{"type": "text", "html": html}]});
        Material::new("campaign", Uuid::nil(), "x", first, vec![variant("email", &c, "", "")])
    }

    #[test]
    fn destinations_keep_all_but_link_tags() {
        assert_eq!(destination("https://acme.test/fall?utm_source=dm&utm_campaign=x#top"), "https://acme.test/fall");
        assert_eq!(destination("https://acme.test/p?id=7&utm_medium=email"), "https://acme.test/p?id=7");
        assert_ne!(destination("https://acme.test/a"), destination("https://acme.test/b"));
    }

    #[test]
    fn every_block_and_both_sides_of_a_condition_are_read() {
        let c = json!({"subject": "Hi", "blocks": [
            {"type": "text", "html": "<p>{% if person.vip %}VIP deal{% else %}Plain deal{% endif %}</p>", "display": {"rules": [{"field": "x"}]}},
            {"type": "button", "text": "Go", "href": "https://acme.test/a?utm_source=x"},
            {"type": "columns", "columns": [[{"type": "text", "html": "<p>Hidden <a href=\"https://evil.test/z\">here</a></p>"}]]},
            {"type": "text", "html": "x", "i18n": {"fr": {"html": {"text": "<p>SECRET FR</p>"}}}}
        ]});
        let (v, too_big) = variant("email", &c, "", "");
        assert!(!too_big);
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("VIP deal") && text.contains("Plain deal") && text.contains("Hidden"), "{text}");
        assert!(!text.contains("SECRET FR"), "language versions are their own variants");
        let to: Vec<&str> = v["links"].as_array().unwrap().iter().map(|l| l["to"].as_str().unwrap()).collect();
        assert!(to.contains(&"https://acme.test/a") && to.contains(&"https://evil.test/z"), "{to:?}");
        // The same content gets the same fingerprint, whatever link tags it carries.
        let mut tagged = c.clone();
        tagged["link_tags"] = json!({"params": [["utm_source", "dm"]]});
        assert_eq!(part_hash(&variant("email", &tagged, "", "").0), part_hash(&v));
    }

    #[test]
    fn table_cells_and_new_fields_count() {
        let t = |cell: &str| json!({"subject": "s", "blocks": [{"type": "table", "rows": [["Item", cell]], "footer": "Total", "color": "#ff0000"}, {"type": "newthing", "blurb": "Fresh"}]});
        let (a, _) = variant("email", &t("$10"), "", "");
        let (b, _) = variant("email", &t("VERDICT_REVIEW free money"), "", "");
        assert!(b["text"].as_str().unwrap().contains("free money") && a["text"].as_str().unwrap().contains("Fresh"), "{b}");
        assert!(!a["text"].as_str().unwrap().contains("ff0000"));
        assert_ne!(part_hash(&a), part_hash(&b), "a table cell change changes the fingerprint");
    }

    #[test]
    fn too_much_is_flagged_not_cut() {
        let long = "word ".repeat(TEXT_CHARS);
        let (_, too_big) = variant("email", &json!({"subject": "s", "blocks": [{"type": "text", "html": long}]}), "", "");
        assert!(too_big);
        let many: Vec<(Value, bool)> = (0..=MAX_MESSAGES).map(|i| variant("sms", &json!({"body": format!("hi {i}")}), "", "")).collect();
        assert!(Material::new("flow", Uuid::nil(), "f", false, many).too_big);
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
    fn the_model_gets_no_addresses_numbers_or_tokens() {
        let c = json!({"subject": "Hi jane@x.test", "blocks": [{"type": "text", "html": "<p>Call +1 503 555 0199. <a href=\"https://acme.test/r/9f8e7d6c5b4a3f2e1d0c?t=SECRET\">go</a></p>"}]});
        let (v, _) = variant("email", &c, "", "");
        let sent = serde_json::to_string(&for_model(&[v])).unwrap();
        for bad in ["jane@x.test", "555 0199", "SECRET", "9f8e7d6c5b4a3f2e1d0c"] {
            assert!(!sent.contains(bad), "{bad} in {sent}");
        }
        assert!(sent.contains("acme.test/r/:id"), "{sent}");
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

//! Short links on the way out: which links in a text are shortened, the target rows for them, the
//! message's reference, and the sample links previews show.
//!
//! Cost: a text without a link costs nothing. A flow text, a reply, a keyword answer or a test with
//! links runs one statement ([`ensure_targets`]) for all of its links. A campaign text runs none: its
//! targets were set up once, when its audience was fixed ([`freeze_campaign`]), and are read once per
//! batch of recipients ([`campaign_targets`]). The message's reference is written by the statement that already
//! writes the message row (`sending::send`).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::code::{self, PATH_LEN, TARGET_LEN};
use super::runtime;
use crate::error::AppResult;
use crate::models::Account;
use crate::render::{LinkHook, SmsLinks};
use crate::sending::SendSpec;
use crate::state::AppState;

/// Where a campaign's targets ride on the content the send sees: `{"targets": {destination: code}}`.
/// Only a real campaign send reads it, and the campaign sender always sets or removes it from the
/// launch's targets ([`campaign_content`]), so a value typed into content by hand never decides
/// what's sent.
pub const CONTENT_KEY: &str = "short_links";
/// At most this many different links in one text are shortened; the rest keep the long form.
pub const MAX_LINKS: usize = 10;
/// Longer links keep the long form.
pub const MAX_URL: usize = 2000;
/// What previews show after `/s/`: the real length, so the segment count is right.
pub const SAMPLE_PATH: &str = "Ab12Cd34Ef56Gh78Jk90Lm12";

static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"https?://[^\s]+"#).unwrap());
/// Template tags: a link inside or touching one is built by the template, not written literally.
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{\{.*?\}\}|\{%.*?%\}|\{#.*?#\}").unwrap());
/// Words in a link's host or path that mean it's about opting out: never hidden behind a short link.
const OPT_OUT_WORDS: [&str; 5] = ["unsubscribe", "optout", "opt-out", "opt_out", "stop"];

/// DumbMail's own hosts for this account: its links (unsubscribe, forms, tracking) are never shortened.
pub fn own_hosts(state: &AppState, link_domain: Option<&str>) -> HashSet<String> {
    let mut own = crate::features::content::check::configured_own_hosts(&state.cfg);
    own.extend(link_domain.map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase()));
    own
}

/// Whether a link written in a text may be shortened: a web link of at most [`MAX_URL`] bytes, not
/// to DumbMail's own hosts and not about opting out.
fn shortenable(link: &str, own: &HashSet<String>) -> bool {
    if link.len() > MAX_URL {
        return false;
    }
    let Ok(u) = url::Url::parse(link) else { return false };
    let Some(host) = u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()) else { return false };
    let where_to = format!("{host}{}", u.path()).to_ascii_lowercase();
    !own.contains(&host) && !OPT_OUT_WORDS.iter().any(|w| where_to.contains(w))
}

pub use crate::render::sms_link_len as link_len;

/// Where in a text's template each link a send would shorten is written: the link the URL pattern
/// found, without trailing sentence punctuation ([`link_len`]), outside template tags, at most
/// [`MAX_LINKS`] different links. These exact characters are what the short link replaces and where
/// it goes; the punctuation stays in the text after it.
fn literal_spans(template: &str, own: &HashSet<String>) -> Vec<std::ops::Range<usize>> {
    let tags: Vec<std::ops::Range<usize>> = TAG.find_iter(template).map(|m| m.range()).collect();
    let mut distinct: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for m in URL.find_iter(template) {
        let found = m.range();
        if tags.iter().any(|t| t.start < found.end && found.start < t.end) {
            continue;
        }
        let r = found.start..found.start + link_len(m.as_str());
        let link = &template[r.clone()];
        if !shortenable(link, own) {
            continue;
        }
        if !distinct.contains(&link) {
            if distinct.len() >= MAX_LINKS {
                continue;
            }
            distinct.push(link);
        }
        out.push(r);
    }
    out
}

/// The links a send would shorten in a text's template, once each, as written.
pub fn written_links(template: &str, own: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in literal_spans(template, own) {
        let l = &template[r];
        if !out.iter().any(|o| o == l) {
            out.push(l.to_string());
        }
    }
    out
}

/// `template` with each link a send would shorten replaced by `short(link)` (when it gives one).
fn replace_literals<'a>(template: &'a str, own: &HashSet<String>, short: impl Fn(&str) -> Option<String>) -> Cow<'a, str> {
    let spans = literal_spans(template, own);
    if spans.is_empty() {
        return Cow::Borrowed(template);
    }
    let mut out = String::with_capacity(template.len());
    let mut at = 0;
    for r in spans {
        if let Some(s) = short(&template[r.clone()]) {
            out.push_str(&template[at..r.start]);
            out.push_str(&s);
            at = r.end;
        }
    }
    out.push_str(&template[at..]);
    Cow::Owned(out)
}

/// Each written link and where it goes: the link with the message's link tags added, or as written
/// when the tags would take it past [`MAX_URL`]. Whether a link is shortened never depends on its
/// tags, so previews and estimates (which don't know them) decide exactly as the send does.
fn destinations(links: Vec<String>, hook: Option<&dyn LinkHook>) -> Vec<(String, String)> {
    links
        .into_iter()
        .map(|l| {
            let tagged = hook.and_then(|h| h.destination(&l)).filter(|d| (d.starts_with("https://") || d.starts_with("http://")) && d.len() <= MAX_URL);
            let dest = tagged.unwrap_or_else(|| l.clone());
            (l, dest)
        })
        .collect()
}

/// The short links of one text.
pub struct TextLinks {
    /// The references the message may take; its links are written with the first.
    refs: Vec<String>,
    /// Link as written → its short link.
    links: HashMap<String, String>,
    issued: HashSet<String>,
    own: HashSet<String>,
}

impl TextLinks {
    /// The references the message may take, in order: the statement that writes its row picks the
    /// first one not already used in its account.
    pub fn ref_candidates(&self) -> &[String] {
        &self.refs
    }

    /// `body`, rendered with the first candidate, with the reference the row actually got.
    pub fn with_ref(&self, body: &str, chosen: &str) -> String {
        if chosen == self.refs[0] {
            return body.to_string();
        }
        let mut out = body.to_string();
        for link in &self.issued {
            // `.../s/<target><ref><tag>`: only the reference changes.
            let at = link.len() - code::TAG_LEN - code::REF_LEN;
            let rekeyed = format!("{}{chosen}{}", &link[..at], &link[at + code::REF_LEN..]);
            out = out.replace(link.as_str(), &rekeyed);
        }
        out
    }
}

impl SmsLinks for TextLinks {
    fn source<'a>(&self, template: &'a str) -> Cow<'a, str> {
        replace_literals(template, &self.own, |l| self.links.get(l).cloned())
    }

    fn issued(&self, link: &str) -> bool {
        self.issued.contains(link)
    }
}

/// The short links for a text about to be rendered, or None (the long form, as before): not a text,
/// no links the sender wrote, short links off, or the index isn't in place yet. `content` is the
/// content as prepared for this person; `hook` adds the message's link tags.
pub async fn for_send(state: &AppState, spec: &SendSpec<'_>, content: &Value, hook: Option<&dyn LinkHook>, message_id: Uuid, link_base: &str) -> AppResult<Option<TextLinks>> {
    if spec.channel != "sms" || !runtime::issuing(state) {
        return Ok(None);
    }
    let Some(body) = content.get("body").and_then(Value::as_str) else { return Ok(None) };
    let own = own_hosts(state, spec.account.link_domain.as_deref());
    let links = written_links(body, &own);
    if links.is_empty() {
        return Ok(None);
    }
    let dests = destinations(links, hook);
    let codes = if spec.campaign_id.is_some() && spec.message_id.is_some() && !spec.is_test {
        // A campaign's were set up when its audience was fixed: no statement per recipient.
        frozen(spec.content)
    } else {
        let urls: Vec<String> = dests.iter().map(|(_, d)| d.clone()).collect();
        let mut conn = state.db.acquire().await?;
        ensure_targets(&mut conn, spec.account.id, &urls).await?
    };
    let refs = code::ref_candidates(&state.cfg.secret_key, message_id);
    let message_ref = refs[0].clone();
    let base = link_base.trim_end_matches('/');
    let links: HashMap<String, String> = dests
        .into_iter()
        .filter_map(|(l, d)| codes.get(&d).map(|c| (l, format!("{base}/s/{}", code::path(&state.cfg.secret_key, c, &d, &message_ref, message_id)))))
        .collect();
    if links.is_empty() {
        return Ok(None);
    }
    let issued = links.values().cloned().collect();
    Ok(Some(TextLinks { refs, links, issued, own }))
}

/// The targets riding on a campaign's content (codes checked again: content can be typed by hand).
fn frozen(content: &Value) -> HashMap<String, String> {
    content
        .get(CONTENT_KEY)
        .and_then(|v| v.get("targets"))
        .and_then(Value::as_object)
        .map(|o| o.iter().filter_map(|(u, c)| c.as_str().filter(|c| code::is_code(c, TARGET_LEN)).map(|c| (u.clone(), c.to_string()))).collect())
        .unwrap_or_default()
}

/// Create or reuse the account's targets for `urls` in one statement, and return destination →
/// code. Empty when the account has short links off or is closed. A destination seen for the first
/// time gets a random code; one already used keeps its code (and is marked used, at most once a
/// day). A new code that collides with another account's, or a destination another send created at
/// the same moment, is tried again (rare); the first attempt is the only statement otherwise.
pub async fn ensure_targets(conn: &mut PgConnection, account_id: Uuid, urls: &[String]) -> sqlx::Result<HashMap<String, String>> {
    const SQL: &str = "WITH enabled AS (SELECT 1 FROM accounts WHERE id = $1 AND sms_short_links AND closed_at IS NULL),
        ins AS (INSERT INTO short_link_targets (code, account_id, url)
                SELECT w.code, $1, w.url FROM unnest($2::text[], $3::text[]) AS w(url, code) WHERE EXISTS (SELECT 1 FROM enabled)
                ON CONFLICT DO NOTHING RETURNING code, url),
        touched AS (UPDATE short_link_targets t SET last_used_at = now()
                    WHERE t.account_id = $1 AND t.url = ANY($2) AND t.last_used_at < now() - interval '1 day' AND EXISTS (SELECT 1 FROM enabled) RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM enabled),
               COALESCE((SELECT jsonb_object_agg(x.url, x.code) FROM (SELECT code, url FROM ins
                         UNION ALL SELECT t.code, t.url FROM short_link_targets t WHERE t.account_id = $1 AND t.url = ANY($2)) x), '{}')";
    let mut out: HashMap<String, String> = HashMap::new();
    let mut want: Vec<String> = urls.iter().filter(|u| !u.is_empty()).cloned().collect::<HashSet<_>>().into_iter().collect();
    for _ in 0..3 {
        if want.is_empty() {
            break;
        }
        let codes: Vec<String> = want.iter().map(|_| code::random(TARGET_LEN)).collect();
        let (enabled, found): (bool, Value) = sqlx::query_as(SQL).bind(account_id).bind(&want).bind(&codes).fetch_one(&mut *conn).await?;
        if !enabled {
            return Ok(HashMap::new());
        }
        if let Some(o) = found.as_object() {
            out.extend(o.iter().filter_map(|(u, c)| c.as_str().map(|c| (u.clone(), c.to_string()))));
        }
        want.retain(|u| !out.contains_key(u));
    }
    Ok(out)
}

/// When a text campaign's audience is fixed (its first launch, and again at every relaunch), on the
/// connection that holds the campaign lock: create its targets (every version's links, with their
/// link tags) and keep them in `short_link_campaigns`, out of the content anyone can edit. A launch
/// with nothing to shorten removes the row.
pub async fn freeze_campaign(state: &AppState, conn: &mut PgConnection, campaign_id: Uuid) -> sqlx::Result<()> {
    let (account_id, content, ab_test, link_domain): (Uuid, Value, Option<Value>, Option<String>) =
        sqlx::query_as("SELECT c.account_id, c.content, c.ab_test, a.link_domain FROM campaigns c JOIN accounts a ON a.id = c.account_id WHERE c.id = $1")
            .bind(campaign_id)
            .fetch_one(&mut *conn)
            .await?;
    let own = own_hosts(state, link_domain.as_deref());
    let dests_of = |c: &Value| -> Vec<String> {
        let links = c.get("body").and_then(Value::as_str).map(|b| written_links(b, &own)).unwrap_or_default();
        let tagger = crate::features::campaigns_plus::utm::Tagger::from_content_with(state, link_domain.as_deref(), c);
        destinations(links, tagger.as_ref().map(|t| t as &dyn LinkHook)).into_iter().map(|(_, d)| d).collect()
    };
    let mut all: HashSet<String> = dests_of(&content).into_iter().collect();
    if let Some(vs) = ab_test.as_ref().and_then(|t| t.get("variants")).and_then(Value::as_array) {
        all.extend(vs.iter().filter_map(|v| v.get("content")).flat_map(dests_of));
    }
    let all: Vec<String> = all.into_iter().collect();
    let codes = if all.is_empty() || !runtime::issuing(state) { HashMap::new() } else { ensure_targets(conn, account_id, &all).await? };
    if codes.is_empty() {
        sqlx::query("DELETE FROM short_link_campaigns WHERE campaign_id = $1").bind(campaign_id).execute(&mut *conn).await?;
    } else {
        let targets: Map<String, Value> = codes.into_iter().map(|(d, c)| (d, json!(c))).collect();
        sqlx::query(
            "INSERT INTO short_link_campaigns (campaign_id, targets) VALUES ($1, $2)
             ON CONFLICT (campaign_id) DO UPDATE SET targets = EXCLUDED.targets, frozen_at = now()",
        )
        .bind(campaign_id)
        .bind(Value::Object(targets))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A text campaign's targets from its last launch (one read per batch of recipients).
pub async fn campaign_targets(db: &sqlx::PgPool, campaign_id: Uuid) -> sqlx::Result<Option<Value>> {
    sqlx::query_scalar("SELECT targets FROM short_link_campaigns WHERE campaign_id = $1").bind(campaign_id).fetch_optional(db).await
}

/// A campaign version's content as the send sees it: with the launch's targets under
/// [`CONTENT_KEY`], and never a value typed into the content by hand.
pub fn campaign_content<'a>(content: &'a Value, targets: Option<&Value>) -> Cow<'a, Value> {
    if targets.is_none() && content.get(CONTENT_KEY).is_none() {
        return Cow::Borrowed(content);
    }
    let mut out = content.clone();
    if let Some(o) = out.as_object_mut() {
        o.remove(CONTENT_KEY);
        if let Some(t) = targets {
            o.insert(CONTENT_KEY.into(), json!({"targets": t}));
        }
    }
    Cow::Owned(out)
}

/// Sample short links for a preview: every link a real send would shorten, as a short link of the
/// real length. None when the body has no such link, the account has short links off or this
/// server doesn't issue short links yet (see [`runtime::issuing`]).
pub struct Sample {
    own: HashSet<String>,
    short: String,
}

impl Sample {
    /// How many links in a rendered preview are (sample) short links.
    pub fn count_in(&self, body: &str) -> usize {
        body.matches(self.short.as_str()).count()
    }
}

impl SmsLinks for Sample {
    fn source<'a>(&self, template: &'a str) -> Cow<'a, str> {
        replace_literals(template, &self.own, |_| Some(self.short.clone()))
    }

    fn issued(&self, link: &str) -> bool {
        link == self.short
    }
}

/// See [`Sample`]. One statement, only when the body has a link the sender wrote.
pub async fn preview(state: &AppState, account: &Account, body: &str) -> AppResult<Option<Sample>> {
    if written_links(body, &own_hosts(state, account.link_domain.as_deref())).is_empty() {
        return Ok(None);
    }
    sample_for(state, account).await
}

/// The sample for any text of this account (one statement), for estimates over several texts:
/// [`SmsLinks::source`] on it gives a template with every link the send would shorten replaced by
/// a short link of the real length. None when the send wouldn't shorten anything now.
pub async fn sample_for(state: &AppState, account: &Account) -> AppResult<Option<Sample>> {
    // Until this server issues short links, sends use the long form, so estimates do too.
    if !runtime::issuing(state) {
        return Ok(None);
    }
    let on: bool = sqlx::query_scalar("SELECT sms_short_links FROM accounts WHERE id = $1").bind(account.id).fetch_optional(&state.db).await?.unwrap_or(false);
    if !on {
        return Ok(None);
    }
    Ok(Some(Sample { own: own_hosts(state, account.link_domain.as_deref()), short: sample_link(state, account) }))
}

/// A short link of the real length on this account's link domain.
pub fn sample_link(state: &AppState, account: &Account) -> String {
    debug_assert_eq!(SAMPLE_PATH.len(), PATH_LEN);
    format!("{}/s/{SAMPLE_PATH}", crate::domains::link_base(state, account).trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_sentence_punctuation_isnt_part_of_the_link() {
        let cut = |f: &'static str| &f[..link_len(f)];
        assert_eq!(cut("https://shop.example/sale."), "https://shop.example/sale");
        assert_eq!(cut("https://shop.example/a,"), "https://shop.example/a");
        assert_eq!(cut("https://shop.example/go!"), "https://shop.example/go");
        assert_eq!(cut("https://shop.example/q?"), "https://shop.example/q");
        assert_eq!(cut("https://shop.example/x?a=1;"), "https://shop.example/x?a=1");
        assert_eq!(cut("https://shop.example/y:"), "https://shop.example/y");
        assert_eq!(cut("https://shop.example/sale)"), "https://shop.example/sale", "a `)` with no `(` in the link closes the sentence's bracket");
        assert_eq!(cut("https://en.wikipedia.org/wiki/Foo_(bar)"), "https://en.wikipedia.org/wiki/Foo_(bar)", "a balanced `)` is part of the link");
        assert_eq!(cut("https://en.wikipedia.org/wiki/Foo_(bar))."), "https://en.wikipedia.org/wiki/Foo_(bar)");
        assert_eq!(cut("https://shop.example/c\""), "https://shop.example/c");
        assert_eq!(cut("https://shop.example/d'"), "https://shop.example/d");
        assert_eq!(cut("https://shop.example/e!)."), "https://shop.example/e");
        assert_eq!(cut("https://shop.example/"), "https://shop.example/");
        assert_eq!(cut("https://shop.example/?a=b"), "https://shop.example/?a=b");
    }

    #[test]
    fn only_links_written_literally_are_shortened() {
        let own: HashSet<String> = ["pl.test".to_string()].into_iter().collect();
        let body = "Acme: sale https://shop.test/a! Also (https://shop.test/a) and https://shop.test/p?u={{ email }} and {{ product_url }} \
                    and {{ 'https://shop.test/a' }} and https://pl.test/f/abc and https://shop.test/{% if x %}b{% endif %} http://shop.test/c. \
                    https://shop.test/unsubscribe https://shop.test/x/STOP https://stop.shop.test/ https://shop.test/opt-out?y=1";
        assert_eq!(written_links(body, &own), vec!["https://shop.test/a".to_string(), "http://shop.test/c".to_string()]);
        // The punctuation stays in the text after what's replaced.
        let out = replace_literals(body, &own, |l| Some(format!("<{l}>")));
        assert!(out.starts_with("Acme: sale <https://shop.test/a>! Also (<https://shop.test/a>) and https://shop.test/p?u={{ email }}"), "{out}");
        assert!(out.contains("{{ 'https://shop.test/a' }}"), "a link inside a template tag is left to the template: {out}");
        assert!(out.contains(" <http://shop.test/c>. https://shop.test/unsubscribe https://shop.test/x/STOP https://stop.shop.test/ https://shop.test/opt-out?y=1"), "{out}");
        let many: String = (0..15).map(|i| format!("https://shop.test/{i} ")).collect();
        assert_eq!(written_links(&many, &own).len(), MAX_LINKS);
        let long = format!("https://shop.test/{}", "a".repeat(MAX_URL));
        assert!(written_links(&long, &own).is_empty());
    }

    #[test]
    fn sample_path_has_the_real_length() {
        assert_eq!(SAMPLE_PATH.len(), PATH_LEN);
        assert!(code::split(SAMPLE_PATH).is_some());
    }
}

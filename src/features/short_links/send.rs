//! Short links on the way out: which links in a text are shortened, the target rows for them, the
//! message's reference, and the sample links previews show.
//!
//! Cost: a text without a link costs nothing. A flow text, a reply, a keyword answer or a test with
//! links runs one statement ([`ensure_targets`]) for all of its links. A campaign text runs none: its
//! targets were set up once, when its audience was fixed ([`freeze_campaign`]), and ride on its
//! content under [`CONTENT_KEY`]. The message's reference is written by the statement that already
//! writes the message row (`sending::send`).

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::code::{self, PATH_LEN, REF_LEN, TARGET_LEN};
use super::runtime;
use crate::error::AppResult;
use crate::models::Account;
use crate::render::{LinkHook, SmsLinks};
use crate::sending::SendSpec;
use crate::state::AppState;

/// Where a campaign's targets ride on its content: `{"targets": {destination: code}}`. Only a real
/// campaign send reads it, and `freeze_campaign` always sets or removes it when the send starts, so
/// a value typed into content by hand never decides what's sent.
pub const CONTENT_KEY: &str = "short_links";
/// At most this many different links in one text are shortened; the rest keep the long form.
pub const MAX_LINKS: usize = 10;
/// Longer links keep the long form.
pub const MAX_URL: usize = 2000;
/// What previews show after `/s/`: the real length, so the segment count is right.
pub const SAMPLE_PATH: &str = "Ab12Cd34Ef56Gh78Jk90Lm12";

static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"https?://[^\s]+"#).unwrap());

/// A link as the URL pattern found it, split into the link and punctuation that ends the sentence
/// after it ("Shop now: https://x.co/sale." keeps its full stop outside the link).
pub fn trim_link(found: &str) -> (&str, &str) {
    let mut end = found.len();
    loop {
        let link = &found[..end];
        let Some(last) = link.chars().last() else { break };
        let unbalanced = |open: char, close: char| last == close && link.matches(close).count() > link.matches(open).count();
        if matches!(last, '.' | ',' | '!' | '?' | ';' | ':' | '\'' | '"') || unbalanced('(', ')') || unbalanced('[', ']') {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    (&found[..end], &found[end..])
}

/// DumbMail's own hosts for this account: its links (unsubscribe, forms, tracking) are never shortened.
pub fn own_hosts(state: &AppState, link_domain: Option<&str>) -> HashSet<String> {
    let mut own = crate::features::content::check::configured_own_hosts(&state.cfg);
    own.extend(link_domain.map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase()));
    own
}

/// The links the sender wrote in a text's template, as written, once each: web links with no
/// template tag in them (a link built from a variable can differ per person, so it keeps the long
/// form), not DumbMail's own, at most [`MAX_URL`] long, at most [`MAX_LINKS`].
pub fn written_links(body: &str, own: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in URL.find_iter(body) {
        let (link, _) = trim_link(m.as_str());
        if out.len() >= MAX_LINKS || link.len() > MAX_URL || out.iter().any(|l| l == link) {
            continue;
        }
        if ["{{", "}}", "{%", "%}", "{#", "#}"].iter().any(|t| link.contains(t)) {
            continue;
        }
        let Some(host) = url::Url::parse(link).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())) else { continue };
        if own.contains(&host) {
            continue;
        }
        out.push(link.to_string());
    }
    out
}

/// Each written link and where it goes: the link with the message's link tags added.
fn destinations(links: Vec<String>, hook: Option<&dyn LinkHook>) -> Vec<(String, String)> {
    links
        .into_iter()
        .map(|l| {
            let dest = hook.and_then(|h| h.destination(&l)).unwrap_or_else(|| l.clone());
            (l, dest)
        })
        .filter(|(_, d)| (d.starts_with("https://") || d.starts_with("http://")) && d.len() <= MAX_URL)
        .collect()
}

/// The short links of one text.
pub struct TextLinks {
    message_ref: String,
    /// Link as written → its short link.
    links: HashMap<String, String>,
}

impl TextLinks {
    /// The message's reference, written on its row.
    pub fn message_ref(&self) -> &str {
        &self.message_ref
    }
}

impl SmsLinks for TextLinks {
    fn short(&self, found: &str) -> Option<String> {
        let (link, rest) = trim_link(found);
        self.links.get(link).map(|s| format!("{s}{rest}"))
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
    let links = written_links(body, &own_hosts(state, spec.account.link_domain.as_deref()));
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
    let message_ref = code::random(REF_LEN);
    let base = link_base.trim_end_matches('/');
    let links: HashMap<String, String> = dests
        .into_iter()
        .filter_map(|(l, d)| codes.get(&d).map(|c| (l, format!("{base}/s/{}", code::path(&state.cfg.secret_key, c, &d, &message_ref, message_id)))))
        .collect();
    if links.is_empty() {
        return Ok(None);
    }
    Ok(Some(TextLinks { message_ref, links }))
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

/// When a text campaign starts, on the connection that holds the campaign lock: create its targets
/// (every version's links, with their link tags) and write them onto its content, or take stale
/// ones off. Called only the first time its audience is fixed.
pub async fn freeze_campaign(state: &AppState, conn: &mut PgConnection, campaign_id: Uuid) -> sqlx::Result<()> {
    let (account_id, mut content, mut ab_test, link_domain): (Uuid, Value, Option<Value>, Option<String>) =
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
    let mut per: Vec<Vec<String>> = vec![dests_of(&content)];
    let variants = ab_test.as_mut().and_then(|t| t.get_mut("variants")).and_then(Value::as_array_mut);
    if let Some(vs) = &variants {
        per.extend(vs.iter().map(|v| v.get("content").map(dests_of).unwrap_or_default()));
    }
    let all: Vec<String> = per.iter().flatten().cloned().collect::<HashSet<_>>().into_iter().collect();
    let codes = if all.is_empty() || !runtime::issuing(state) { HashMap::new() } else { ensure_targets(conn, account_id, &all).await? };
    let set = |c: &mut Value, dests: &[String]| -> bool {
        let Some(o) = c.as_object_mut() else { return false };
        let targets: Map<String, Value> = dests.iter().filter_map(|d| codes.get(d).map(|code| (d.clone(), json!(code)))).collect();
        let before = o.get(CONTENT_KEY).cloned();
        if targets.is_empty() {
            o.remove(CONTENT_KEY);
        } else {
            o.insert(CONTENT_KEY.into(), json!({"targets": targets}));
        }
        o.get(CONTENT_KEY).cloned() != before
    };
    let mut changed = set(&mut content, &per[0]);
    if let Some(vs) = variants {
        for (v, dests) in vs.iter_mut().zip(per.iter().skip(1)) {
            if let Some(vc) = v.get_mut("content") {
                changed |= set(vc, dests);
            }
        }
    }
    if changed {
        sqlx::query("UPDATE campaigns SET content = $2, ab_test = $3 WHERE id = $1").bind(campaign_id).bind(&content).bind(&ab_test).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Sample short links for a preview: every link a real send would shorten, as a short link of the
/// real length. None when the body has no such link, the account has short links off or short
/// this server doesn't issue short links yet (see [`runtime::issuing`]).
pub struct Sample {
    links: HashSet<String>,
    short: String,
}

impl SmsLinks for Sample {
    fn short(&self, found: &str) -> Option<String> {
        let (link, rest) = trim_link(found);
        self.links.contains(link).then(|| format!("{}{rest}", self.short))
    }
}

/// See [`Sample`]. One statement, only when the body has a link the sender wrote.
pub async fn preview(state: &AppState, account: &Account, body: &str) -> AppResult<Option<Sample>> {
    // Until the index is in place, sends use the long form, so the preview does too.
    if !runtime::issuing(state) {
        return Ok(None);
    }
    let links = written_links(body, &own_hosts(state, account.link_domain.as_deref()));
    if links.is_empty() {
        return Ok(None);
    }
    let on: bool = sqlx::query_scalar("SELECT sms_short_links FROM accounts WHERE id = $1").bind(account.id).fetch_optional(&state.db).await?.unwrap_or(false);
    if !on {
        return Ok(None);
    }
    Ok(Some(Sample { links: links.into_iter().collect(), short: sample_link(state, account) }))
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
    fn sentence_punctuation_stays_outside_the_link() {
        assert_eq!(trim_link("https://x.co/sale."), ("https://x.co/sale", "."));
        assert_eq!(trim_link("https://x.co/a?b=1!)"), ("https://x.co/a?b=1", "!)"));
        assert_eq!(trim_link("https://en.wikipedia.org/wiki/Foo_(bar)"), ("https://en.wikipedia.org/wiki/Foo_(bar)", ""));
        assert_eq!(trim_link("https://x.co/"), ("https://x.co/", ""));
    }

    #[test]
    fn only_links_the_sender_wrote_are_shortened() {
        let own: HashSet<String> = ["pl.test".to_string()].into_iter().collect();
        let body = "Acme: sale https://shop.test/a. Also https://shop.test/a and https://shop.test/p?u={{ email }} and {{ product_url }} \
                    and https://pl.test/f/abc and https://shop.test/{% if x %}b{% endif %} http://shop.test/c";
        assert_eq!(written_links(body, &own), vec!["https://shop.test/a".to_string(), "http://shop.test/c".to_string()]);
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

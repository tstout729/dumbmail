//! The marketing site: the landing page (`/` on SITE_URL's host, `/home` when there's one host),
//! `/pricing`, and the files they use under `/site/`. Prices come from the running config, never
//! from this file, so the page always matches what the server charges.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use super::{esc, money, request_host, thousands};
use crate::config::Config;
use crate::state::AppState;
use crate::subscription;

pub const GITHUB: &str = "https://github.com/tstout729/dumbmail";

/// Klaviyo's email plan prices by contact count, as the README compares them.
const KLAVIYO: [(i64, i64); 2] = [(10_000, 150), (50_000, 720)];
/// Emails per contact per month in the comparison (the README's example).
const EMAILS_PER_CONTACT: i64 = 8;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(root))
        .route("/home", get(home))
        .route("/pricing", get(pricing))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/site/{file}", get(asset))
}

// ---------- Static files ----------

struct Asset {
    name: &'static str,
    mime: &'static str,
    bytes: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset { name: "archivo.woff2", mime: "font/woff2", bytes: include_bytes!("assets/archivo.woff2") },
    Asset { name: "hero.webp", mime: "image/webp", bytes: include_bytes!("assets/hero.webp") },
    Asset { name: "flow.webp", mime: "image/webp", bytes: include_bytes!("assets/flow.webp") },
    Asset { name: "form.webp", mime: "image/webp", bytes: include_bytes!("assets/form.webp") },
    Asset { name: "segment.webp", mime: "image/webp", bytes: include_bytes!("assets/segment.webp") },
    Asset { name: "analytics.webp", mime: "image/webp", bytes: include_bytes!("assets/analytics.webp") },
    Asset { name: "og.png", mime: "image/png", bytes: include_bytes!("assets/og.png") },
    Asset { name: "apple-touch-icon.png", mime: "image/png", bytes: include_bytes!("assets/apple-touch-icon.png") },
    Asset { name: "icon.svg", mime: "image/svg+xml", bytes: ICON_SVG.as_bytes() },
];

const CSS: &str = include_str!("assets/site.css");
const CALC_JS: &str = include_str!("assets/calc.js");

pub(super) const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#115f42"/><rect x="5.5" y="8.5" width="21" height="15" rx="2.5" fill="#fff"/><path d="M6.6 9.8 16 16.2l9.4-6.4" fill="none" stroke="#115f42" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><circle cx="12" cy="19.6" r="2.3" fill="#115f42"/><circle cx="20.2" cy="19.6" r="2.3" fill="#115f42"/><circle cx="12.9" cy="20.3" r="0.9" fill="#fff"/><circle cx="19.4" cy="18.8" r="0.9" fill="#fff"/></svg>"##;

/// Content hashes, so asset URLs change when the file does and can be cached for a year.
static VERSIONS: LazyLock<HashMap<&'static str, String>> = LazyLock::new(|| {
    ASSETS
        .iter()
        .map(|a| {
            let hash = a.bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3));
            (a.name, format!("{:08x}", hash >> 32))
        })
        .collect()
});

static STYLE: LazyLock<String> = LazyLock::new(|| CSS.replace("__FONT__", &asset_url("archivo.woff2")));

pub(super) fn asset_url(name: &str) -> String {
    format!("/site/{name}?v={}", VERSIONS.get(name).map(String::as_str).unwrap_or("0"))
}

async fn asset(Path(file): Path<String>) -> Response {
    match ASSETS.iter().find(|a| a.name == file) {
        Some(a) => ([(header::CONTENT_TYPE, a.mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], a.bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---------- Where links go ----------

/// Link bases. With SITE_URL set, the site and the app are on different hosts, so links between them
/// are absolute; with one host they stay relative, so any hostname works (dev, self-hosting).
pub(super) struct Links {
    /// Prefix for site pages ("" or SITE_URL).
    pub site: String,
    /// Prefix for app pages ("" or PUBLIC_URL).
    pub app: String,
    /// The landing page.
    pub home: String,
    /// Absolute base for canonical URLs.
    pub canonical: String,
}

pub(super) fn links(cfg: &Config) -> Links {
    match cfg.site_url() {
        Some(site) => Links { site: site.into(), app: cfg.public_url().into(), home: format!("{site}/"), canonical: site.into() },
        None => Links { site: String::new(), app: String::new(), home: "/home".into(), canonical: cfg.public_url().into() },
    }
}

fn on_site_host(state: &AppState, headers: &HeaderMap) -> bool {
    state.cfg.site_host().is_some_and(|h| h == request_host(headers))
}

// ---------- Page chrome (shared with About, Privacy and Terms) ----------

pub(super) struct Meta<'a> {
    pub title: &'a str,
    pub description: &'a str,
    /// Path of this page on the site, for the canonical URL.
    pub path: &'a str,
}

pub(super) fn document(cfg: &Config, meta: &Meta, main: &str, script: &str) -> Html<String> {
    let l = links(cfg);
    let canonical = format!("{}{}", l.canonical, meta.path);
    let og_image = format!("{}{}", l.canonical, asset_url("og.png"));
    let mut out = String::with_capacity(main.len() + STYLE.len() + 4096);
    let _ = write!(
        out,
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<link rel="canonical" href="{canonical}">
<link rel="icon" href="{icon}" type="image/svg+xml">
<link rel="apple-touch-icon" href="{touch}">
<link rel="preload" href="{font}" as="font" type="font/woff2" crossorigin>
<meta name="theme-color" content="#f4f7f4" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#0c1a14" media="(prefers-color-scheme: dark)">
<meta property="og:type" content="website">
<meta property="og:site_name" content="DumbMail">
<meta property="og:title" content="{title}">
<meta property="og:description" content="{desc}">
<meta property="og:url" content="{canonical}">
<meta property="og:image" content="{og_image}">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<style>{style}</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="wrap top">
<a class="mark" href="{home}" aria-label="DumbMail home">{logo}DumbMail</a>
<nav aria-label="Main">
<a class="opt" href="{home}#features">Features</a>
<a class="opt2" href="{site}/pricing">Pricing</a>
<a class="opt" href="{github}">GitHub</a>
<a href="{app}/login">Log in</a>
<a class="btn small" href="{app}/signup">Start free</a>
</nav>
</header>
<main id="main">
{main}
</main>
<footer class="wrap foot">
<a class="mark" href="{home}" aria-label="DumbMail home">{logo}DumbMail</a>
<a href="{site}/pricing">Pricing</a>
<a href="{site}/about">About</a>
<a href="{site}/privacy">Privacy</a>
<a href="{site}/terms">Terms</a>
<a href="{github}">GitHub</a>
<span class="sp">{contact}</span>
</footer>
{script}
</body>
</html>"##,
        title = esc(meta.title),
        desc = esc(meta.description),
        canonical = esc(&canonical),
        icon = asset_url("icon.svg"),
        touch = asset_url("apple-touch-icon.png"),
        font = asset_url("archivo.woff2"),
        og_image = esc(&og_image),
        style = STYLE.as_str(),
        home = esc(&l.home),
        site = esc(&l.site),
        app = esc(&l.app),
        github = GITHUB,
        logo = ICON_SVG.replacen("<svg ", "<svg aria-hidden=\"true\" ", 1),
        contact = contact_line(cfg),
        main = main,
        script = script,
    );
    Html(out)
}

fn contact_line(cfg: &Config) -> String {
    match cfg.support_email.as_deref().map(str::trim).filter(|e| e.contains('@')) {
        Some(e) => format!("Questions? <a href=\"mailto:{0}\">{0}</a>", esc(e)),
        None => String::new(),
    }
}

// ---------- Prices ----------

/// Dollars with cents and thousands separators: $31.60, $1,234.00.
fn dollars(v: f64) -> String {
    let cents = (v * 100.0).round() as i64;
    format!("${}.{:02}", thousands(cents / 100), cents % 100)
}

/// Whole dollars when there are no cents ($20), else with cents.
fn plan_dollars(cents: i64) -> String {
    if cents % 100 == 0 { format!("${}", thousands(cents / 100)) } else { dollars(cents as f64 / 100.0) }
}

/// The cheapest way to send `emails` in a month: the free allowance first, then a plan's emails,
/// then credit at the pay-as-you-go price (the order billing uses). Returns (total USD, plan tier).
fn best_cost(cfg: &Config, emails: i64) -> (f64, Option<i16>) {
    let per = cfg.price_email_per_1000 / 1000.0;
    let billable = (emails - cfg.free_emails_per_month.max(0)).max(0);
    let mut best = (billable as f64 * per, None);
    for t in 1..=subscription::TIERS {
        let total = subscription::price_cents(t) as f64 / 100.0 + (billable - subscription::emails(t)).max(0) as f64 * per;
        if total < best.0 - 1e-9 {
            best = (total, Some(t));
        }
    }
    best
}

fn payg_cost(cfg: &Config, emails: i64) -> f64 {
    (emails - cfg.free_emails_per_month.max(0)).max(0) as f64 * cfg.price_email_per_1000 / 1000.0
}

/// How much less a plan's emails cost than pay-as-you-go, in whole percent.
fn plan_savings(cfg: &Config) -> i64 {
    let plan_per_email = subscription::price_cents(1) as f64 / 100.0 / subscription::emails(1) as f64;
    let payg = cfg.price_email_per_1000 / 1000.0;
    if payg <= 0.0 { 0 } else { ((1.0 - plan_per_email / payg) * 100.0).round().max(0.0) as i64 }
}

fn how_text(cfg: &Config, emails: i64) -> String {
    let (total, tier) = best_cost(cfg, emails);
    let free = cfg.free_emails_per_month.max(0);
    match tier {
        None if emails <= free => format!("{} emails a month, all inside the free {}.", thousands(emails), thousands(free)),
        None => format!("{} emails a month, paid as you go.", thousands(emails)),
        Some(t) => {
            let price = subscription::price_cents(t) as f64 / 100.0;
            let extra = total - price;
            let tail = if extra > 0.004 { format!(" plus {} of credit.", dollars(extra)) } else { ".".into() };
            format!("{} emails a month, on the {} plan ({} emails){tail}", thousands(emails), plan_dollars(subscription::price_cents(t)), thousands(subscription::emails(t)))
        }
    }
}

// ---------- Handlers ----------

/// `/`: the landing page on the site host; the dashboard everywhere else.
async fn root(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if on_site_host(&state, &headers) {
        return landing_page(&state).into_response();
    }
    crate::web::serve(State(state), uri).await
}

/// `/home`: the landing page when everything is on one host; otherwise it lives at SITE_URL's root.
async fn home(State(state): State<AppState>) -> Response {
    match state.cfg.site_url() {
        Some(site) => Redirect::to(&format!("{site}/")).into_response(),
        None => landing_page(&state).into_response(),
    }
}

async fn pricing(State(state): State<AppState>) -> Html<String> {
    let c = &state.cfg;
    let main = format!(
        r#"<div class="wrap hero" style="grid-template-columns:1fr;padding-bottom:24px">
<div><h1>Pay for what you send.</h1>
<p class="lede" style="margin-top:22px">No contact tiers and no seats. Keep as many subscribers as you like; the bill only moves when you send.</p></div>
</div>
{pricing}
{faq}"#,
        pricing = pricing_section(c, false),
        faq = faq_section(&state),
    );
    document(
        c,
        &Meta {
            title: "Pricing · DumbMail",
            description: &format!(
                "DumbMail pricing: {} per 1,000 emails, {} per text segment, no per-contact fees. Monthly email plans from {}.",
                money(c.price_email_per_1000),
                money(c.price_sms_segment),
                plan_dollars(subscription::price_cents(1))
            ),
            path: "/pricing",
        },
        &main,
        &format!("<script>{CALC_JS}</script>"),
    )
}

async fn robots(State(state): State<AppState>) -> Response {
    let l = links(&state.cfg);
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("User-agent: *\nAllow: /\nSitemap: {}/sitemap.xml\n", l.canonical)).into_response()
}

async fn sitemap(State(state): State<AppState>) -> Response {
    let l = links(&state.cfg);
    let home = if state.cfg.site_url().is_some() { "/" } else { "/home" };
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#);
    for p in [home, "/pricing", "/about", "/privacy", "/terms"] {
        let _ = write!(xml, "<url><loc>{}{p}</loc></url>", esc(&l.canonical));
    }
    xml.push_str("</urlset>");
    ([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], xml).into_response()
}

// ---------- The landing page ----------

fn shot(name: &str, w: u32, h: u32, alt: &str, label: &str, eager: bool) -> String {
    format!(
        r#"<figure class="shot"><div class="bar" aria-hidden="true"><i></i><i></i><i></i><span>{label}</span></div><img src="{src}" width="{w}" height="{h}" alt="{alt}"{lazy}></figure>"#,
        label = esc(label),
        src = asset_url(name),
        alt = esc(alt),
        lazy = if eager { r#" fetchpriority="high""# } else { r#" loading="lazy" decoding="async""# },
    )
}

/// Screenshot sizes (pixels) of the files in src/site/assets.
const SHOTS: [(&str, u32, u32); 5] = [("hero.webp", 1600, 991), ("flow.webp", 960, 1005), ("form.webp", 1200, 923), ("segment.webp", 1200, 923), ("analytics.webp", 1200, 923)];

fn shot_size(name: &str) -> (u32, u32) {
    SHOTS.iter().find(|s| s.0 == name).map(|s| (s.1, s.2)).unwrap_or((1600, 1000))
}

fn landing_page(state: &AppState) -> Html<String> {
    let c = &state.cfg;
    let l = links(c);
    // The screenshots are of the hosted app, so the frame names its host whatever this server is called.
    let app_host = "app.dumbmail.ai";
    let free = c.free_emails_per_month.max(0);
    let free_line = if free > 0 { format!("The first {} emails every month are free. No card needed to start.", thousands(free)) } else { "No card needed to start.".into() };
    let shot_of = |name: &str, alt: &str, eager: bool| {
        let (w, h) = shot_size(name);
        shot(name, w, h, alt, app_host, eager)
    };

    let hero = format!(
        r##"<div class="wrap hero">
<div>
<h1>Postage, not rent.</h1>
<p class="lede">DumbMail does the core of what Klaviyo does for online stores: email campaigns, automated flows, sign-up forms, segments and text messages. You pay for the messages you send, never for the contacts you keep.</p>
<div class="ctas"><a class="btn" href="{app}/signup">Start free</a><a class="btn ghost" href="#pricing">See pricing</a></div>
<p class="fine">{free_line}</p>
</div>
<div class="sheet" aria-label="Prices">
<div class="stamp a"><div><span class="v num">{email}</span><span class="u">per 1,000 emails</span><span class="k" aria-hidden="true"><span>DumbMail</span><span>Email</span></span></div></div>
<div class="stamp b"><div><span class="v num">{sms}</span><span class="u">per text segment, carrier fees included</span><span class="k" aria-hidden="true"><span>DumbMail</span><span>SMS</span></span></div></div>
<div class="stamp c"><div><span class="v num">$0</span><span class="u">per contact, however many you have</span><span class="k" aria-hidden="true"><span>DumbMail</span><span>List</span></span></div></div>
<svg class="postmark" viewBox="0 0 220 110" aria-hidden="true"><circle cx="55" cy="55" r="44" fill="none" stroke="currentColor" stroke-width="3"/><circle cx="55" cy="55" r="34" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M38 52h34M38 60h34" stroke="currentColor" stroke-width="3" stroke-linecap="round"/><path d="M104 30c12-8 24 8 36 0s24 8 36 0 24 8 36 0M104 55c12-8 24 8 36 0s24 8 36 0 24 8 36 0M104 80c12-8 24 8 36 0s24 8 36 0 24 8 36 0" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round"/></svg>
</div>
</div>
<div class="wrap showcase">{shot}</div>"##,
        app = esc(&l.app),
        email = money(c.price_email_per_1000),
        sms = money(c.price_sms_segment),
        shot = shot_of("hero.webp", "The DumbMail campaign editor with an email draft and its preview", true),
    );

    let closing = format!(
        r#"<section class="sec"><div class="wrap closing"><h2>Your list is the asset. Stop paying rent on it.</h2><div class="ctas"><a class="btn" href="{app}/signup">Start free</a><a class="btn ghost" href="{github}">Read the code</a></div></div></section>"#,
        app = esc(&l.app),
        github = GITHUB
    );
    let main = format!(
        "{hero}\n{bills}\n{features}\n{pricing}\n{faq}\n{closing}",
        bills = bills_section(c),
        features = features_section(c, &shot_of),
        pricing = pricing_section(c, true),
        faq = faq_section(state),
    );
    document(
        c,
        &Meta {
            title: "DumbMail: email and text marketing priced by what you send",
            description: &format!(
                "Email campaigns, automated flows, sign-up forms and SMS for online stores, like Klaviyo but billed by what you send: {} per 1,000 emails and no per-contact fees. Open source.",
                money(c.price_email_per_1000)
            ),
            path: if c.site_url().is_some() { "/" } else { "/home" },
        },
        &main,
        &format!("<script>{CALC_JS}</script>"),
    )
}

fn receipt_row(label: &str, amount: &str) -> String {
    format!(r#"<div class="row"><dt>{label}</dt><span class="lead" aria-hidden="true"></span><dd>{amount}</dd></div>"#)
}

fn bills_section(c: &Config) -> String {
    let (contacts, klaviyo) = KLAVIYO[0];
    let emails = contacts * EMAILS_PER_CONTACT;
    let free = c.free_emails_per_month.max(0).min(emails);
    let payg = payg_cost(c, emails);
    let (best, tier) = best_cost(c, emails);
    let plan_note = match tier {
        Some(t) if best + 0.004 < payg => format!(
            r#"<p class="note">Or {} with the {} monthly plan.</p>"#,
            dollars(best),
            plan_dollars(subscription::price_cents(t))
        ),
        _ => String::new(),
    };
    let mut rows = receipt_row("Emails sent", &thousands(emails));
    if free > 0 {
        rows.push_str(&receipt_row(&format!("First {} free", thousands(free)), "$0.00"));
    }
    rows.push_str(&receipt_row(&format!("{} × {} per 1,000", thousands(emails - free), money(c.price_email_per_1000)), &dollars(payg)));
    rows.push_str(&receipt_row(&format!("{} contacts kept", thousands(contacts)), "$0.00"));

    let (big_contacts, big_klaviyo) = KLAVIYO[1];
    let big_emails = big_contacts * EMAILS_PER_CONTACT;
    let (big_best, _) = best_cost(c, big_emails);
    let big_best_s = dollars(big_best.round());
    let big_best_s = big_best_s.trim_end_matches(".00");
    format!(
        r#"<section class="sec" id="compare" aria-labelledby="compare-h"><div class="wrap">
<div class="sec-head"><h2 id="compare-h">Same store, two bills.</h2>
<p class="lede">A store with {contacts_s} subscribers sends each of them about {per} emails a month. Klaviyo prices by how many contacts you have. DumbMail prices by how many emails go out.</p></div>
<div class="bills">
<div class="receipt them"><h3>Klaviyo <small>monthly</small></h3><dl>
{k_row}
<div class="row total"><dt>Total</dt><span class="lead" aria-hidden="true"></span><dd class="num">${klaviyo}</dd></div>
</dl><p class="note">Priced by how many contacts you have.</p></div>
<div class="receipt us"><h3>DumbMail <small>monthly</small></h3><dl>
{rows}
<div class="row total"><dt>Total</dt><span class="lead" aria-hidden="true"></span><dd class="num">{payg_s}</dd></div>
</dl>{plan_note}</div>
</div>
<p class="bills-foot">Klaviyo's email plan is ${klaviyo} a month for {contacts_s} contacts and ${big_klaviyo} for {big_contacts_s}. At {big_contacts_s} contacts and the same {per} emails each, DumbMail comes to about {big_best_s}.</p>
</div></section>"#,
        contacts_s = thousands(contacts),
        per = EMAILS_PER_CONTACT,
        k_row = receipt_row(&format!("Email plan, {} contacts", thousands(contacts)), &format!("${klaviyo}.00")),
        big_contacts_s = thousands(big_contacts),
        payg_s = dollars(payg),
    )
}

fn features_section(c: &Config, shot_of: &dyn Fn(&str, &str, bool) -> String) -> String {
    let sms_line = format!(
        "Texts cost {} per segment and {} per picture message, carrier fees included. Claim a toll-free number for {} a month, or connect your own Telnyx or Twilio account and DumbMail doesn't charge for texts at all.",
        money(c.price_sms_segment),
        money(c.price_mms),
        money(c.price_number_monthly)
    );
    format!(
        r#"<section class="sec" id="features" aria-labelledby="features-h"><div class="wrap">
<div class="sec-head"><h2 id="features-h">The marketing a store actually runs.</h2>
<p class="lede">Campaigns, flows, forms, segments and texts in one place, with the reports to see what made money.</p></div>

<div class="feature">
<div><h3>Flows that follow up for you</h3>
<p>Start from a blueprint and edit it, or build your own from triggers, delays, emails, texts and conditional splits.</p>
<ul><li>Welcome series, abandoned checkout, browse abandonment, post-purchase, winback, back in stock, price drop and birthday blueprints</li><li>Triggers on events, list joins, segment entry or dates</li><li>Exit conditions, re-entry rules and smart sending</li></ul></div>
{flow}
</div>

<div class="feature flip">
<div><h3>Sign-up forms that grow the list</h3>
<p>Popups, flyouts, embedded forms and hosted pages, installed with one script tag. Show them by delay, scroll depth, exit intent, page, device and frequency.</p>
<ul><li>A/B test headlines, copy, buttons and images</li><li>Optional SMS consent checkbox</li><li>Hosted pages for links in your bio or a QR code</li></ul></div>
{form}
</div>

<div class="feature">
<div><h3>Segments that stay current</h3>
<p>Rules on profile properties, events over time windows, list membership and consent, nested with AND and OR. Counts update live, and campaigns and flows use the same rules.</p>
<ul><li>Bought twice in 90 days, viewed but didn't buy, engaged but quiet</li><li>Exclude segments from any campaign</li></ul></div>
{segment}
</div>

<div class="feature flip">
<div><h3>Reports that follow the money</h3>
<p>Revenue is attributed to the campaign or flow that earned it: the last email open or click within 5 days, or a text click within 24 hours. Open, click, unsubscribe and bounce rates sit next to it, by day in your time zone.</p>
<ul><li>Campaign A/B tests that send the winner automatically</li><li>A metric explorer for any event you track</li></ul></div>
{analytics}
</div>

<h3 class="box-h">Also in the box</h3>
<dl class="box">
<div><dt>Text messages</dt><dd>{sms_line} Quiet hours and STOP replies are handled for you.</dd></div>
<div><dt>Website tracking and Shopify</dt><dd>One snippet tracks visits, product views, carts, checkouts and orders, and a Shopify pixel covers checkout. Guides for Shopify, WordPress, Squarespace, Wix and more.</dd></div>
<div><dt>Deliverability guardrails</dt><dd>DKIM, MAIL FROM and DMARC checked live, a warm-up for new senders, automatic suppression of bounces and complaints, and skipping people who never engage.</dd></div>
<div><dt>An MCP server</dt><dd>Claude, ChatGPT or Cursor can build segments, draft and preview emails, create flows and pull reports. Anything that reaches real people needs your confirmation first.</dd></div>
<div><dt>Your brand, pulled in</dt><dd>Your logo, colors and links come from your website when you sign up, and emails go out from your own domain.</dd></div>
<div><dt>Open source</dt><dd>AGPL-3.0, one Rust binary and Postgres. <a href="{github}">Read the code</a> or run it yourself.</dd></div>
</dl>
</div></section>"#,
        flow = shot_of("flow.webp", "The flow builder showing an abandoned checkout flow: delays, a reminder email, then a text or a last-call email", false),
        form = shot_of("form.webp", "The sign-up form editor with a live popup preview", false),
        segment = shot_of("segment.webp", "The segment builder with event rules, a list rule, a nested OR group and a live count of matching profiles", false),
        analytics = shot_of("analytics.webp", "The metrics explorer charting Placed Order revenue by day", false),
        sms_line = sms_line,
        github = GITHUB,
    )
}

fn pricing_section(c: &Config, with_heading: bool) -> String {
    let free = c.free_emails_per_month.max(0);
    let head = if with_heading {
        r#"<div class="sec-head"><h2 id="pricing-h">The name is dumb. So is the pricing.</h2>
<p class="lede">Messages sent times the price. That's the whole model. Add credit with a card and it's used as you send, or take a monthly email plan and pay less per email.</p></div>"#
    } else {
        r#"<h2 id="pricing-h" class="skip">Prices</h2>"#
    };
    let free_row = if free > 0 { format!(r#"<tr><th scope="row">Free emails</th><td class="big">{}<small> every month</small></td></tr>"#, thousands(free)) } else { String::new() };
    let mut plans = String::new();
    for t in 1..=subscription::TIERS {
        let price = subscription::price_cents(t);
        let emails = subscription::emails(t);
        let _ = write!(
            plans,
            r#"<tr><th scope="row">{} emails</th><td>{}</td><td class="big">{}<small> /mo</small></td></tr>"#,
            thousands(emails),
            dollars(emails as f64 * c.price_email_per_1000 / 1000.0),
            plan_dollars(price)
        );
    }
    let tiers: Vec<_> = (1..=subscription::TIERS).map(|t| serde_json::json!({"price": subscription::price_cents(t) as f64 / 100.0, "emails": subscription::emails(t)})).collect();
    let data = serde_json::json!({"per1000": c.price_email_per_1000, "free": free, "tiers": tiers}).to_string();
    let default_emails = KLAVIYO[0].0 * EMAILS_PER_CONTACT;
    format!(
        r#"<section class="sec" id="pricing" aria-labelledby="pricing-h"><div class="wrap">
{head}
<div class="rates">
<div class="card"><h3>Pay as you go</h3><p>Prepaid credit, used as you send. Turn on auto-recharge so it never runs dry.</p>
<table><tbody>
<tr><th scope="row">Email</th><td class="big">{email}<small> per 1,000</small></td></tr>
{free_row}
<tr><th scope="row">Text message (SMS)</th><td class="big">{sms}<small> per segment</small></td></tr>
<tr><th scope="row">Picture message (MMS)</th><td class="big">{mms}<small> each</small></td></tr>
<tr><th scope="row">Texting number</th><td class="big">{num}<small> per month</small></td></tr>
<tr><th scope="row">Contacts</th><td class="big">$0<small> any number</small></td></tr>
</tbody></table></div>
<div class="card"><h3>Monthly email plans</h3><p>A set number of emails each month for about {savings}% less per email. Each plan doubles the one before it. Cancel any time.</p>
<table><thead><tr><th scope="col">Emails a month</th><th scope="col">Pay as you go</th><th scope="col">Price</th></tr></thead><tbody>
{plans}
</tbody></table></div>
</div>
<form class="calc" id="calc" data-pricing="{data}">
<div><h3>What would you pay?</h3>
<label for="contacts">Subscribers</label><input id="contacts" name="contacts" type="number" inputmode="numeric" min="0" step="100" value="{contacts}">
<label for="per">Emails to each per month</label><input id="per" name="per" type="number" inputmode="decimal" min="0" step="1" value="{per}">
</div>
<div aria-live="polite"><output for="contacts per">{best}</output><p class="how">{how}</p></div>
</form>
<p class="selfhost">Running it yourself? Self-hosted DumbMail has no billing at all. You pay Amazon SES and your texting provider directly.</p>
</div></section>"#,
        email = money(c.price_email_per_1000),
        sms = money(c.price_sms_segment),
        mms = money(c.price_mms),
        num = money(c.price_number_monthly),
        savings = plan_savings(c),
        data = esc(&data),
        contacts = KLAVIYO[0].0,
        per = EMAILS_PER_CONTACT,
        best = dollars(best_cost(c, default_emails).0),
        how = esc(&how_text(c, default_emails)),
    )
}

fn faq_section(state: &AppState) -> String {
    let c = &state.cfg;
    let l = links(c);
    format!(
        r#"<section class="sec" id="faq" aria-labelledby="faq-h"><div class="wrap faq">
<h2 id="faq-h" style="margin-bottom:28px">Questions</h2>
<details><summary>Is it really cheaper than Klaviyo?</summary><div><p>For most stores, by a lot, because contacts cost nothing to keep and the bill follows what you send. A {contacts} subscriber store sending {per} emails to each a month pays about {example} here, against Klaviyo's ${klaviyo} for that list size.</p><p>If you email a small list many times a day, run your numbers in the calculator above.</p></div></details>
<details><summary>Why is it called DumbMail?</summary><div><p>Because the pricing is dumb on purpose: messages sent times a price you can read. No contact tiers and no seat counts.</p></div></details>
<details><summary>How does billing work?</summary><div><p>Add credit with a card through Stripe and it's used as you send. Auto-recharge tops it up when it runs low. Or choose a monthly email plan: it's charged when you start it and on the same date each month until you cancel in Settings. Phone numbers are billed monthly from your credit.</p></div></details>
<details><summary>Can I bring my own texting provider?</summary><div><p>Yes. Connect your own Telnyx or Twilio account and number. Texts then go through your provider, who bills you directly, and DumbMail doesn't charge for them. STOP replies still sync.</p></div></details>
<details><summary>Can I move my list over?</summary><div><p>Yes. Import contacts from a CSV (you confirm they opted in), then add the snippet to your site so DumbMail sees visits, carts and orders. Lists can use double opt-in for new sign-ups.</p></div></details>
<details><summary>Who owns my data?</summary><div><p>You do. Profiles, lists, events and campaigns are all reachable through the REST API and the MCP server, and we never sell data or use it for ads. To get a full copy or close your account, email us. See the <a href="{site}/privacy">privacy policy</a>.</p></div></details>
<details><summary>Can I self-host it?</summary><div><p>Yes. DumbMail is open source under the AGPL-3.0 license: one Rust binary and Postgres, with Docker Compose to start. Billing is off when you run it yourself. <a href="{github}">See the README on GitHub</a>.</p></div></details>
</div></section>"#,
        contacts = thousands(KLAVIYO[0].0),
        per = EMAILS_PER_CONTACT,
        example = dollars(payg_cost(c, KLAVIYO[0].0 * EMAILS_PER_CONTACT)),
        klaviyo = KLAVIYO[0].1,
        site = esc(&l.site),
        github = GITHUB,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formats() {
        assert_eq!(dollars(31.6), "$31.60");
        assert_eq!(dollars(1234.5), "$1,234.50");
        assert_eq!(plan_dollars(2_000), "$20");
        assert_eq!(plan_dollars(1_050), "$10.50");
    }
}

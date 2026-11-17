//! Public pages served without the dashboard: the landing page, Pricing, About, Privacy and Terms.
//! Google's sign-in consent screen links to these, so they're plain server-rendered HTML anyone (or
//! any crawler) can read.
//!
//! With SITE_URL set, these pages live on the marketing host and the dashboard on PUBLIC_URL's host;
//! `host_split` sends each page visit to the right one (see `Config::site_url`).

mod landing;

use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

const UPDATED: &str = "November 16, 2026";

/// Pages served on the marketing host, which PUBLIC_URL's host sends there.
const SITE_PAGES: [&str; 4] = ["/about", "/privacy", "/terms", "/pricing"];

pub fn routes() -> Router<AppState> {
    Router::new().route("/about", get(about)).route("/privacy", get(privacy)).route("/terms", get(terms)).merge(landing::routes())
}

/// The request's hostname, lowercase and without a port.
pub(crate) fn request_host(headers: &HeaderMap) -> String {
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_lowercase()
}

/// With SITE_URL set: on its host, the site pages and everything that must keep answering there (API,
/// MCP, webhooks, tracking links, hosted forms, the snippet, Google sign-in and verification) stay,
/// and other page visits go to the app at PUBLIC_URL. On PUBLIC_URL's host, the site pages go to
/// SITE_URL. Both redirects are 302s that keep the path and query. `www.` + the site host moves
/// permanently (301) to SITE_URL.
pub async fn host_split(State(state): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let (Some(site_url), Some(site_host)) = (state.cfg.site_url(), state.cfg.site_host()) else {
        return next.run(req).await;
    };
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return next.run(req).await;
    }
    let host = request_host(req.headers());
    let path = req.uri().path();
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let found = |to: String| (StatusCode::FOUND, [(header::LOCATION, to)]).into_response();
    if host == site_host {
        let stays = path == "/"
            || path == "/home"
            || SITE_PAGES.contains(&path)
            || path.starts_with("/site/")
            || matches!(path, "/robots.txt" | "/sitemap.xml" | "/favicon.svg")
            || path.starts_with("/api/")
            || crate::stays_on_old_host(path);
        if !stays {
            return found(format!("{}{path_and_query}", state.cfg.public_url()));
        }
    } else if host.strip_prefix("www.") == Some(site_host.as_str()) {
        return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, format!("{site_url}{path_and_query}"))]).into_response();
    } else if Some(host) == state.cfg.public_host() && SITE_PAGES.contains(&path) {
        return found(format!("{site_url}{path_and_query}"));
    }
    next.run(req).await
}

/// Google Search Console's HTML-file check (GOOGLE_SITE_VERIFICATION=google0123abcd.html).
/// Returns None when the path isn't that file, so the dashboard fallback serves it as usual.
pub fn site_verification(state: &AppState, uri: &Uri) -> Option<Response> {
    let file = state.cfg.google_site_verification.as_deref()?.trim();
    let valid = file.starts_with("google") && file.ends_with(".html") && file.len() < 100 && file.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
    if !valid || uri.path().strip_prefix('/') != Some(file) {
        return None;
    }
    Some(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], format!("google-site-verification: {file}")).into_response())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn contact(state: &AppState) -> String {
    match state.cfg.support_email.as_deref().map(str::trim).filter(|e| e.contains('@')) {
        Some(e) => format!("<a href=\"mailto:{0}\">{0}</a>", esc(e)),
        None => "the contact address in your dashboard".into(),
    }
}

/// A text page (About, Privacy, Terms) in the site's chrome.
fn page(state: &AppState, path: &str, title: &str, description: &str, body: &str) -> Html<String> {
    landing::document(&state.cfg, &landing::Meta { title, description, path }, &format!("<article class=\"wrap doc\">\n{body}\n</article>"), "")
}

pub(crate) fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn money(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0');
    let s = if s.ends_with('.') { format!("{s}00") } else if s.split('.').nth(1).map(str::len) == Some(1) { format!("{s}0") } else { s.to_string() };
    format!("${s}")
}

async fn about(State(state): State<AppState>) -> Html<String> {
    let c = &state.cfg;
    let l = landing::links(c);
    let free = if c.free_emails_per_month > 0 { format!(" The first {} emails each month are free.", thousands(c.free_emails_per_month)) } else { String::new() };
    page(
        &state,
        "/about",
        "About DumbMail",
        "DumbMail is open-source email and text marketing for online stores, priced by what you send.",
        &format!(
            r#"<h1>About DumbMail</h1>
<p>DumbMail sends a store's newsletters, automated flows and text messages. Sign-up forms and a website snippet grow the list; flows follow up on browsing, carts and orders; reports show which messages earned money. You pay only for what you send, never for how many contacts you keep.</p>
<p class="ctas"><a class="btn" href="{app}/signup">Create an account</a><a class="btn ghost" href="{app}/login">Log in</a></p>
<h2>How pricing works</h2>
<p>Email is {email} per 1,000 and texts are {sms} per segment.{free} Add credit with a card and it's used as you send, or choose a monthly email plan. There are no contact tiers. See <a href="{site}/pricing">all prices</a>.</p>
<h2>Open source</h2>
<p>DumbMail is open source under the AGPL-3.0 license, so you can read the code and run it yourself: <a href="{github}">{github}</a>.</p>
<h2>Contact</h2>
<p>Questions: {contact}.</p>"#,
            app = esc(&l.app),
            site = esc(&l.site),
            email = money(c.price_email_per_1000),
            sms = money(c.price_sms_segment),
            github = landing::GITHUB,
            contact = contact(&state),
        ),
    )
}

async fn privacy(State(state): State<AppState>) -> Html<String> {
    page(
        &state,
        "/privacy",
        "Privacy Policy · DumbMail",
        "How DumbMail collects, uses and protects information.",
        &format!(
            r#"<h1>Privacy Policy</h1>
<p class="muted">Last updated {UPDATED}</p>
<p>This policy explains what information DumbMail collects, how we use it and the choices you have. "DumbMail", "we" and "us" mean the operator of this service. "You" means a person who has a DumbMail account. "Subscribers" means the people our customers send email and text messages to.</p>

<h2>Information we collect from account holders</h2>
<ul>
<li><strong>Account details:</strong> your name, email address, company or brand name, website and time zone, and a hashed password if you set one.</li>
<li><strong>Google sign-in:</strong> if you choose "Continue with Google", Google shares your name, email address, whether that address is verified, and a Google account ID. We use them only to create your account and sign you in.</li>
<li><strong>Brand details:</strong> your logo, colors, mailing address, sender address and social links. If you ask us to import them from your website, we read that site's public pages.</li>
<li><strong>Billing:</strong> payments are handled by Stripe. We store your credit balance and payment history, but never your full card number.</li>
<li><strong>Usage and security logs:</strong> sign-ins, API requests, IP addresses and error logs, used to run and protect the service.</li>
</ul>

<h2>Information our customers store about their subscribers</h2>
<p>Customers upload or collect subscriber details such as email addresses, phone numbers, names, consent records, and activity like opens, clicks, page views and orders. Our customers decide what they collect and why. We process that data on their behalf and only to provide the service to them. If you're a subscriber, contact the business that messaged you first; you can also unsubscribe with the link in any email or by replying STOP to any text.</p>

<h2>Website snippet</h2>
<p>Customers can add DumbMail's snippet to their own websites. It shows sign-up forms and records page views and events for that customer. It stores a visitor identifier in the browser's local storage on the customer's site. It doesn't track people across different websites.</p>

<h2>How we use information</h2>
<ul>
<li>To provide the service: sending messages, running flows, showing reports and billing for usage.</li>
<li>To keep it secure and reliable, prevent abuse and spam, and meet legal obligations.</li>
<li>To contact you about your account, such as receipts, security notices and important changes.</li>
</ul>
<p>We don't sell personal information, and we don't use it for advertising.</p>

<h2>Google user data</h2>
<p>DumbMail's use of information received from Google APIs adheres to the <a href="https://developers.google.com/terms/api-services-user-data-policy">Google API Services User Data Policy</a>, including the Limited Use requirements. We request only the basic sign-in scopes (openid, email and profile). That data is used only to sign you in and set up your account. It isn't sold, used for advertising, shared with third parties except as needed to run the service or required by law, or used to train AI models.</p>

<h2>Service providers</h2>
<p>We share information with providers only as needed to run DumbMail:</p>
<ul>
<li>Railway: hosting and database.</li>
<li>Amazon Web Services (Amazon SES): email delivery.</li>
<li>Telnyx: text messages and phone numbers.</li>
<li>Stripe: payments.</li>
<li>Google: sign-in, if you use it.</li>
</ul>
<p>We may also disclose information if the law requires it, to protect people or the service, or as part of a merger or sale of the business, in which case this policy continues to apply.</p>

<h2>Cookies</h2>
<p>The dashboard uses a session cookie to keep you signed in, and a short-lived cookie to complete Google sign-in securely. We don't use advertising or third-party tracking cookies.</p>

<h2>Retention and deletion</h2>
<p>We keep account data while your account is open. Customers can delete subscriber profiles at any time. To close your account and delete its data, or to get a copy of your information, email {contact}. We delete it within 30 days, except records we must keep for legal, tax or fraud-prevention reasons.</p>

<h2>Security</h2>
<p>Data is encrypted in transit, passwords are hashed, and provider credentials are encrypted at rest. No system is perfectly secure, but we work to protect your information and will tell you about a breach that affects you as the law requires.</p>

<h2>Children</h2>
<p>DumbMail is a business service and isn't meant for anyone under 16.</p>

<h2>Changes</h2>
<p>If we change this policy, we'll update the date above and, for significant changes, let account holders know by email.</p>

<h2>Contact</h2>
<p>Questions or requests: {contact}.</p>"#,
            contact = contact(&state),
        ),
    )
}

async fn terms(State(state): State<AppState>) -> Html<String> {
    page(
        &state,
        "/terms",
        "Terms of Service · DumbMail",
        "The terms for using the hosted DumbMail service.",
        &format!(
            r#"<h1>Terms of Service</h1>
<p class="muted">Last updated {UPDATED}</p>
<p>These terms apply when you create a DumbMail account or use the hosted service. By using it, you agree to them. If you use DumbMail for a business, you agree on that business's behalf.</p>

<h2>Your account</h2>
<p>Give accurate information and keep your password, API keys and sign-in secure. You're responsible for activity on your account, including by people and tools you give access to.</p>

<h2>Sending rules</h2>
<p>You may only message people who have agreed to hear from you, and you must follow the laws that apply to your messages, including CAN-SPAM, the TCPA, CASL and GDPR where relevant. In particular:</p>
<ul>
<li>Don't send spam or use purchased, rented or scraped lists.</li>
<li>Honor unsubscribes and STOP replies. DumbMail adds unsubscribe links and handles STOP for you; don't remove or work around them.</li>
<li>Include your real business name and mailing address in marketing email.</li>
<li>Get express written consent before sending marketing texts, and respect quiet hours.</li>
<li>Don't send illegal, deceptive, harassing or harmful content, phishing or malware.</li>
</ul>
<p>We may pause sending, hold messages or suspend an account to protect deliverability, recipients or the service, for example after high bounce or complaint rates. We'll tell you why when we can.</p>

<h2>Prices and payment</h2>
<p>The hosted service is pay as you go. You add credit and it's used as you send, at the prices on our <a href="/pricing">Pricing page</a>. We may change prices with at least 30 days' notice by email; changes don't apply to messages already sent.</p>
<p>You can also choose a monthly email plan: a set number of emails each month for a fixed price. It's charged to your card when you start it and again each month on the same date (Pacific time) until you cancel. Cancel any time in Settings → Billing; the plan stays active until the end of the month you've paid for and then stops renewing. If a renewal payment is already failing, cancelling ends the plan right away. Plan emails left at the end of a monthly period don't carry over. Moving to a bigger plan is charged right away for the difference in price; moving to a smaller one starts at your next renewal. If a renewal can't be charged, we try three times, a day apart, and then end the plan.</p>
<p>Phone numbers are billed monthly from your credit and may be released if their rent goes unpaid. Taxes may apply. For billing questions or refund requests, contact us.</p>

<h2>Your content</h2>
<p>You own your content and subscriber data. You give us permission to store and process it only to provide the service. You're responsible for having the rights and consents needed to upload and message it.</p>

<h2>Open source</h2>
<p>DumbMail's code is available under the AGPL-3.0 license. These terms cover the hosted service; the license covers the code.</p>

<h2>Availability and changes</h2>
<p>We work to keep DumbMail running, but it may sometimes be unavailable. We may change or discontinue features. If we shut down the hosted service, we'll give at least 30 days' notice so you can export your data.</p>

<h2>Ending your account</h2>
<p>You can stop using DumbMail at any time and ask us to close your account. We may suspend or close accounts that break these terms.</p>

<h2>Disclaimers and limits</h2>
<p>The service is provided "as is", without warranties of any kind, to the extent the law allows. We aren't liable for indirect or consequential losses, lost profits or lost data. Our total liability for any claim is limited to the amount you paid us in the 12 months before the claim.</p>

<h2>Indemnity</h2>
<p>You agree to cover claims against us that arise from your messages, your content, or your breaking these terms or the law.</p>

<h2>Governing law</h2>
<p>These terms are governed by the laws of the State of Oregon, USA.</p>

<h2>Changes to these terms</h2>
<p>We may update these terms. For significant changes, we'll email account holders before they take effect. Continuing to use DumbMail after that means you accept the new terms.</p>

<h2>Contact</h2>
<p>Questions: {contact}.</p>"#,
            contact = contact(&state),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{money, thousands};

    #[test]
    fn prices_read_naturally() {
        assert_eq!(money(0.40), "$0.40");
        assert_eq!(money(0.015), "$0.015");
        assert_eq!(money(0.03), "$0.03");
        assert_eq!(money(2.20), "$2.20");
        assert_eq!(money(2.0), "$2.00");
        assert_eq!(thousands(25), "25");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}

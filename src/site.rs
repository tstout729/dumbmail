//! Public pages served without the dashboard: About, Privacy and Terms. Google's sign-in consent
//! screen links to these, so they're plain server-rendered HTML anyone (or any crawler) can read.

use axum::extract::State;
use axum::http::{header, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

const UPDATED: &str = "November 15, 2026";

pub fn routes() -> Router<AppState> {
    Router::new().route("/about", get(about)).route("/privacy", get(privacy)).route("/terms", get(terms))
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

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<style>
:root {{ color-scheme: light dark; --fg:#18181b; --muted:#52525b; --bg:#ffffff; --line:#e4e4e7; --brand:#0f5132; --link:#0f5132; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg:#f4f4f5; --muted:#a1a1aa; --bg:#09090b; --line:#27272a; --link:#6ee7b7; }} }}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:16px/1.6 system-ui,-apple-system,"Segoe UI",sans-serif; }}
header, main, footer {{ max-width:720px; margin:0 auto; padding:0 16px; }}
header {{ display:flex; align-items:center; justify-content:space-between; gap:12px; padding-top:20px; padding-bottom:20px; border-bottom:1px solid var(--line); }}
.mark {{ display:flex; align-items:center; gap:10px; font-weight:700; font-size:18px; color:var(--fg); text-decoration:none; }}
.mark svg {{ width:28px; height:28px; }}
nav a {{ margin-left:16px; color:var(--muted); text-decoration:none; font-size:14px; }}
nav a:hover {{ color:var(--fg); }}
main {{ padding-top:32px; padding-bottom:48px; }}
h1 {{ font-size:32px; line-height:1.2; margin:0 0 8px; }}
h2 {{ font-size:20px; margin:32px 0 8px; }}
p, li {{ color:var(--fg); }}
.muted {{ color:var(--muted); font-size:14px; }}
a {{ color:var(--link); }}
ul {{ padding-left:20px; }}
table {{ border-collapse:collapse; width:100%; margin:8px 0; }}
td {{ border-bottom:1px solid var(--line); padding:8px 0; }}
td:last-child {{ text-align:right; font-variant-numeric:tabular-nums; }}
.cta {{ display:inline-block; background:var(--brand); color:#fff; padding:10px 18px; border-radius:8px; text-decoration:none; font-weight:600; margin-right:8px; }}
.cta.secondary {{ background:transparent; color:var(--fg); border:1px solid var(--line); }}
footer {{ border-top:1px solid var(--line); padding-top:20px; padding-bottom:40px; color:var(--muted); font-size:14px; }}
footer a {{ color:var(--muted); margin-right:16px; }}
</style>
</head>
<body>
<header>
<a class="mark" href="/about"><svg viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="8" fill="#0f5132"/><path d="M8 21c4-1 7-4 9-9l7-3-3 7c-5 2-8 5-9 9z" fill="#fff"/></svg>Postlark</a>
<nav><a href="/login">Log in</a><a href="/signup">Sign up</a></nav>
</header>
<main>
{body}
</main>
<footer><a href="/about">About</a><a href="/privacy">Privacy</a><a href="/terms">Terms</a></footer>
</body>
</html>"##
    ))
}

fn money(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0');
    let s = if s.ends_with('.') { format!("{s}00") } else if s.split('.').nth(1).map(str::len) == Some(1) { format!("{s}0") } else { s.to_string() };
    format!("${s}")
}

async fn about(State(state): State<AppState>) -> Html<String> {
    let c = &state.cfg;
    let free = if c.free_emails_per_month > 0 { format!("<p>The first {} emails each month are free.</p>", c.free_emails_per_month) } else { String::new() };
    page(
        "Postlark: email and text marketing you pay for as you go",
        &format!(
            r#"<h1>Email and text marketing, without the monthly plan.</h1>
<p>Postlark sends your store's newsletters, automated flows and text messages. Sign-up forms and a website snippet grow your list; flows follow up on browsing, carts and orders. You pay only for what you send.</p>
<p><a class="cta" href="/signup">Create an account</a><a class="cta secondary" href="/login">Log in</a></p>
<h2>What it does</h2>
<ul>
<li>Email campaigns and automated flows, sent from your own domain.</li>
<li>Text messages from your own phone number.</li>
<li>Sign-up forms and a snippet for your website.</li>
<li>Lists, segments and profiles for every subscriber.</li>
<li>Pulls your logo, colors and links from your website when you sign up.</li>
<li>An MCP server so AI assistants can run your marketing with you.</li>
</ul>
<h2>Pricing</h2>
<table>
<tr><td>Email</td><td>{email} per 1,000</td></tr>
<tr><td>Text message (SMS)</td><td>{sms} per segment</td></tr>
<tr><td>Picture message (MMS)</td><td>{mms} each</td></tr>
<tr><td>Phone number</td><td>{num} per month</td></tr>
</table>
{free}
<p class="muted">Add credit with a card and it's used as you send. No subscription.</p>
<h2>Open source</h2>
<p>Postlark is open source under the AGPL-3.0 license, so you can also run it yourself.</p>
<h2>Contact</h2>
<p>Questions: {contact}.</p>"#,
            email = money(c.price_email_per_1000),
            sms = money(c.price_sms_segment),
            mms = money(c.price_mms),
            num = money(c.price_number_monthly),
            contact = contact(&state),
        ),
    )
}

async fn privacy(State(state): State<AppState>) -> Html<String> {
    page(
        "Privacy Policy · Postlark",
        &format!(
            r#"<h1>Privacy Policy</h1>
<p class="muted">Last updated {UPDATED}</p>
<p>This policy explains what information Postlark collects, how we use it and the choices you have. "Postlark", "we" and "us" mean the operator of this service. "You" means a person who has a Postlark account. "Subscribers" means the people our customers send email and text messages to.</p>

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
<p>Customers can add Postlark's snippet to their own websites. It shows sign-up forms and records page views and events for that customer. It stores a visitor identifier in the browser's local storage on the customer's site. It doesn't track people across different websites.</p>

<h2>How we use information</h2>
<ul>
<li>To provide the service: sending messages, running flows, showing reports and billing for usage.</li>
<li>To keep it secure and reliable, prevent abuse and spam, and meet legal obligations.</li>
<li>To contact you about your account, such as receipts, security notices and important changes.</li>
</ul>
<p>We don't sell personal information, and we don't use it for advertising.</p>

<h2>Google user data</h2>
<p>Postlark's use of information received from Google APIs adheres to the <a href="https://developers.google.com/terms/api-services-user-data-policy">Google API Services User Data Policy</a>, including the Limited Use requirements. We request only the basic sign-in scopes (openid, email and profile). That data is used only to sign you in and set up your account. It isn't sold, used for advertising, shared with third parties except as needed to run the service or required by law, or used to train AI models.</p>

<h2>Service providers</h2>
<p>We share information with providers only as needed to run Postlark:</p>
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
<p>Postlark is a business service and isn't meant for anyone under 16.</p>

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
        "Terms of Service · Postlark",
        &format!(
            r#"<h1>Terms of Service</h1>
<p class="muted">Last updated {UPDATED}</p>
<p>These terms apply when you create a Postlark account or use the hosted service. By using it, you agree to them. If you use Postlark for a business, you agree on that business's behalf.</p>

<h2>Your account</h2>
<p>Give accurate information and keep your password, API keys and sign-in secure. You're responsible for activity on your account, including by people and tools you give access to.</p>

<h2>Sending rules</h2>
<p>You may only message people who have agreed to hear from you, and you must follow the laws that apply to your messages, including CAN-SPAM, the TCPA, CASL and GDPR where relevant. In particular:</p>
<ul>
<li>Don't send spam or use purchased, rented or scraped lists.</li>
<li>Honor unsubscribes and STOP replies. Postlark adds unsubscribe links and handles STOP for you; don't remove or work around them.</li>
<li>Include your real business name and mailing address in marketing email.</li>
<li>Get express written consent before sending marketing texts, and respect quiet hours.</li>
<li>Don't send illegal, deceptive, harassing or harmful content, phishing or malware.</li>
</ul>
<p>We may pause sending, hold messages or suspend an account to protect deliverability, recipients or the service, for example after high bounce or complaint rates. We'll tell you why when we can.</p>

<h2>Prices and payment</h2>
<p>The hosted service is pay as you go. You add credit and it's used as you send, at the prices on our <a href="/about">About page</a>. We may change prices with at least 30 days' notice by email; changes don't apply to messages already sent. Phone numbers are billed monthly from your credit and may be released if their rent goes unpaid. Taxes may apply. For billing questions or refund requests, contact us.</p>

<h2>Your content</h2>
<p>You own your content and subscriber data. You give us permission to store and process it only to provide the service. You're responsible for having the rights and consents needed to upload and message it.</p>

<h2>Open source</h2>
<p>Postlark's code is available under the AGPL-3.0 license. These terms cover the hosted service; the license covers the code.</p>

<h2>Availability and changes</h2>
<p>We work to keep Postlark running, but it may sometimes be unavailable. We may change or discontinue features. If we shut down the hosted service, we'll give at least 30 days' notice so you can export your data.</p>

<h2>Ending your account</h2>
<p>You can stop using Postlark at any time and ask us to close your account. We may suspend or close accounts that break these terms.</p>

<h2>Disclaimers and limits</h2>
<p>The service is provided "as is", without warranties of any kind, to the extent the law allows. We aren't liable for indirect or consequential losses, lost profits or lost data. Our total liability for any claim is limited to the amount you paid us in the 12 months before the claim.</p>

<h2>Indemnity</h2>
<p>You agree to cover claims against us that arise from your messages, your content, or your breaking these terms or the law.</p>

<h2>Governing law</h2>
<p>These terms are governed by the laws of the State of Oregon, USA.</p>

<h2>Changes to these terms</h2>
<p>We may update these terms. For significant changes, we'll email account holders before they take effect. Continuing to use Postlark after that means you accept the new terms.</p>

<h2>Contact</h2>
<p>Questions: {contact}.</p>"#,
            contact = contact(&state),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::money;

    #[test]
    fn prices_read_naturally() {
        assert_eq!(money(0.40), "$0.40");
        assert_eq!(money(0.012), "$0.012");
        assert_eq!(money(0.03), "$0.03");
        assert_eq!(money(2.20), "$2.20");
        assert_eq!(money(2.0), "$2.00");
    }
}

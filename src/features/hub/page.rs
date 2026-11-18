//! The hub's HTML: plain server-rendered pages with ordinary forms, in the store's colours and
//! font, usable with a keyboard and a screen reader and at phone width. Every value from the
//! store or the shopper's data is escaped; links are only ever http(s) or mailto.

use super::data::{EmailStatus, Order, Prefs, Status, View};
use super::settings::Settings;
use crate::models::Account;

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The page's surroundings: the store's look, where links and forms go, and the form token.
pub struct Frame<'a> {
    pub name: String,
    pub logo: Option<String>,
    pub accent: String,
    pub font: &'static str,
    /// The hub's path, e.g. `/hub/<key>`.
    pub base: String,
    /// The session's form token (signed in only).
    pub token: Option<String>,
    /// Drawn inside the store's panel (no outer margin).
    pub embed: bool,
    /// The dashboard's preview: nothing works, links go nowhere.
    pub preview: bool,
    pub settings: &'a Settings,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl<'a> Frame<'a> {
    pub fn new(account: &Account, settings: &'a Settings, embed: bool) -> Self {
        Frame {
            name: account.name.clone(),
            logo: super::data::web_url(account.logo_url.clone()).filter(|u| u.starts_with("https://")),
            accent: crate::render::safe_color(settings.accent_color.as_deref().unwrap_or(&account.brand_color), "#1f6f5c"),
            font: settings.font.css(),
            base: super::session::cookie_path(account),
            token: None,
            embed,
            preview: false,
            settings,
            notice: None,
            error: None,
        }
    }

    pub fn preview(account: &Account, settings: &'a Settings) -> Self {
        Frame { preview: true, ..Frame::new(account, settings, false) }
    }

    /// A link inside the hub, keeping the panel mode.
    fn href(&self, path: &str) -> String {
        if self.preview {
            return "#".into();
        }
        let sep = if path.contains('?') { '&' } else { '?' };
        let p = format!("{}{path}", self.base);
        esc(&if self.embed { format!("{p}{sep}embed=1") } else { p })
    }

    fn form(&self, path: &str, inner: &str, class: &str) -> String {
        let token = self.token.as_deref().map(|t| format!(r#"<input type="hidden" name="t" value="{}">"#, esc(t))).unwrap_or_default();
        let action = if self.preview { String::new() } else { format!(r#" action="{}""#, self.href(path)) };
        format!(r#"<form method="post"{action} class="{class}">{token}{inner}</form>"#)
    }
}

const CSS: &str = "*{box-sizing:border-box}body{margin:0;font-family:var(--font);background:#f6f6f7;color:#111827;line-height:1.5;font-size:16px}
a{color:var(--accent)}a:focus-visible,button:focus-visible,input:focus-visible,summary:focus-visible{outline:3px solid var(--accent);outline-offset:2px}
.skip{position:absolute;left:-999px;top:0;background:#fff;padding:8px 12px}.skip:focus{left:8px;z-index:9}
.wrap{max-width:760px;margin:0 auto;padding:24px 16px 48px}.embed .wrap{padding-top:16px}
header{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:20px}.brand{display:flex;align-items:center;gap:10px;font-weight:700;font-size:18px;min-width:0}
.brand img{max-height:40px;max-width:160px;object-fit:contain}.brand span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
nav ul{display:flex;flex-wrap:wrap;gap:6px;list-style:none;margin:0 0 20px;padding:0}nav a{display:inline-block;padding:6px 12px;border-radius:999px;background:#fff;border:1px solid #e5e7eb;text-decoration:none;color:#111827;font-size:14px}
section{background:#fff;border:1px solid #e5e7eb;border-radius:12px;padding:20px;margin:0 0 16px}h1{font-size:24px;margin:0 0 4px}h2{font-size:19px;margin:0 0 12px}h3{font-size:16px;margin:0}
.muted{color:#6b7280;font-size:14px}.empty{color:#6b7280;margin:0}
.btn{display:inline-block;background:var(--accent);color:#fff;border:0;border-radius:8px;padding:10px 16px;font:inherit;font-weight:600;cursor:pointer;text-decoration:none}
.btn.secondary{background:#fff;color:#111827;border:1px solid #d1d5db}.btn.small{padding:6px 12px;font-size:14px}.btn[disabled]{opacity:.6;cursor:default}
label{display:block;font-size:14px;font-weight:600;margin:0 0 6px}input[type=email],input[type=text]{width:100%;padding:12px;border:1px solid #d1d5db;border-radius:8px;font:inherit;margin:0 0 12px}
.code{letter-spacing:6px;font-size:22px;text-align:center}
.notice{background:#ecfdf5;color:#065f46;border-radius:8px;padding:10px 12px;margin:0 0 16px}.error{background:#fef2f2;color:#991b1b;border-radius:8px;padding:10px 12px;margin:0 0 16px}
.order{border-top:1px solid #f0f0f1;padding:14px 0}.order:first-of-type{border-top:0;padding-top:0}.row{display:flex;justify-content:space-between;align-items:flex-start;gap:12px;flex-wrap:wrap}
.pill{display:inline-block;font-size:12px;font-weight:600;border-radius:999px;padding:2px 10px;background:#f3f4f6;color:#374151}.pill.good{background:#ecfdf5;color:#065f46}.pill.bad{background:#fef2f2;color:#991b1b}
.thumbs{display:flex;gap:6px;margin-top:8px}.thumbs img,.item img{width:48px;height:48px;object-fit:cover;border-radius:6px;background:#f3f4f6}
.item{display:flex;gap:12px;align-items:center;padding:8px 0;border-top:1px solid #f0f0f1}.item:first-child{border-top:0}.item .grow{flex:1;min-width:0}
.ship{background:#f9fafb;border-radius:8px;padding:10px 12px;margin-top:8px;font-size:14px}
dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 16px;margin:8px 0 0;font-size:14px}dt{color:#6b7280}dd{margin:0;overflow-wrap:anywhere}
.record{border-top:1px solid #f0f0f1;padding:12px 0}.record:first-of-type{border-top:0;padding-top:0}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:12px}.card{border:1px solid #e5e7eb;border-radius:10px;overflow:hidden;text-decoration:none;color:#111827;display:block}
.card img,.card .ph{width:100%;aspect-ratio:1;object-fit:cover;background:#f3f4f6;display:block}.card div{padding:8px 10px;font-size:14px}
.check{display:flex;gap:10px;align-items:flex-start;margin:0 0 12px;font-weight:400}.check input{width:18px;height:18px;margin-top:3px;accent-color:var(--accent)}
.actions{display:flex;gap:8px;flex-wrap:wrap;margin-top:12px}details{border-top:1px solid #f0f0f1;padding:10px 0}details:first-of-type{border-top:0}summary{cursor:pointer;font-weight:600}
footer{text-align:center;margin-top:24px;font-size:12px;color:#9ca3af}footer a{color:#9ca3af}
.banner{background:#fffbeb;color:#92400e;text-align:center;font-size:13px;padding:6px}
@media (max-width:480px){.wrap{padding:16px 12px 40px}section{padding:16px}h1{font-size:21px}dl{grid-template-columns:1fr}dt{margin-top:6px}}";

fn document(f: &Frame<'_>, title: &str, body: &str) -> String {
    let brand = match &f.logo {
        Some(l) => format!(r#"<img src="{}" alt="{}">"#, esc(l), esc(&f.name)),
        None => format!("<span>{}</span>", esc(&f.name)),
    };
    let banner = if f.preview { r#"<div class="banner">Preview with a made-up shopper. Nothing here works.</div>"# } else { "" };
    let sign_out = match (&f.token, f.preview) {
        (Some(_), _) | (None, true) => f.form("/signout", r#"<button class="btn secondary small" type="submit">Sign out</button>"#, ""),
        _ => String::new(),
    };
    let notice = f.notice.as_deref().map(|n| format!(r#"<p class="notice" role="status">{}</p>"#, esc(n))).unwrap_or_default();
    let error = f.error.as_deref().map(|n| format!(r#"<p class="error" role="alert">{}</p>"#, esc(n))).unwrap_or_default();
    format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><title>{t}</title>
<style>:root{{--accent:{accent};--font:{font}}}{CSS}</style></head><body class="{cls}">{banner}<a class="skip" href="#main">Skip to content</a>
<div class="wrap"><header><div class="brand">{brand}</div>{sign_out}</header><main id="main">{notice}{error}{body}</main>
<footer>Powered by <a href="https://dumbmail.ai" target="_blank" rel="noopener">DumbMail</a></footer></div></body></html>"##,
        t = esc(title),
        accent = f.accent,
        font = f.font,
        cls = if f.embed { "embed" } else { "" },
    )
}

/// The first sign-in step: their email.
pub fn sign_in_page(f: &Frame<'_>, email: &str) -> String {
    let form = f.form(
        "/code",
        &format!(
            r#"<label for="email">Email</label><input id="email" name="email" type="email" autocomplete="email" required maxlength="254" value="{}"><button class="btn" type="submit">Email me a code</button>"#,
            esc(email)
        ),
        "",
    );
    let body = format!(
        r#"<section><h1>Your account</h1><p class="muted">See your orders, track parcels and choose what we send you. Enter your email and we'll send you a code to sign in.</p>{form}</section>"#
    );
    document(f, &format!("Sign in · {}", f.name), &body)
}

/// The second step: the code.
pub fn code_page(f: &Frame<'_>, email: &str) -> String {
    let form = f.form(
        "/verify",
        &format!(
            r#"<input type="hidden" name="email" value="{}"><label for="code">6-digit code</label><input id="code" class="code" name="code" type="text" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9 ]*" maxlength="7" required autofocus><button class="btn" type="submit">Sign in</button>"#,
            esc(email)
        ),
        "",
    );
    let again = f.form("/code", &format!(r#"<input type="hidden" name="email" value="{}"><button class="btn secondary small" type="submit">Send a new code</button>"#, esc(email)), "actions");
    let body = format!(
        r#"<section><h1>Check your email</h1><p class="muted">If we have {} on file, we've sent a code to it. It works for {} minutes.</p>{form}{again}<p class="muted"><a href="{}">Use a different email</a></p></section>"#,
        esc(email),
        super::session::CODE_MINUTES,
        f.href("")
    );
    document(f, &format!("Enter your code · {}", f.name), &body)
}

/// The hub isn't on (or the link was wrong).
pub fn unavailable_page(f: &Frame<'_>) -> String {
    document(f, &f.name, r#"<section><h1>Not available</h1><p class="muted">This account page isn't available right now.</p></section>"#)
}

fn status_pill(o: &Order) -> String {
    let (label, class) = match o.status {
        Status::Placed => ("Order placed", ""),
        Status::Paid => ("Paid", ""),
        Status::Fulfilled if o.partial => ("Partly shipped", "good"),
        Status::Fulfilled => ("Shipped", "good"),
        Status::Delivered => ("Delivered", "good"),
        Status::Refunded => ("Refunded", "bad"),
        Status::Cancelled => ("Cancelled", "bad"),
    };
    format!(r#"<span class="pill {class}" data-status="{}">{label}</span>"#, o.status.key())
}

fn item_count(o: &Order) -> String {
    match o.item_count {
        0 => String::new(),
        1 => "1 item".into(),
        n => format!("{n} items"),
    }
}

fn shipments_html(o: &Order) -> String {
    o.shipments
        .iter()
        .map(|s| {
            let mut parts = Vec::new();
            if let Some(st) = s.status {
                parts.push(format!("<strong>{}</strong>", esc(st)));
            }
            if let Some(c) = &s.carrier {
                parts.push(esc(c));
            }
            if let Some(n) = &s.tracking_number {
                parts.push(format!("Tracking {}", esc(n)));
            }
            if let Some(e) = &s.expected {
                parts.push(format!("Expected {}", esc(e)));
            }
            let link = s.tracking_url.as_deref().map(|u| format!(r#" · <a href="{}" target="_blank" rel="noopener noreferrer">Track parcel</a>"#, esc(u))).unwrap_or_default();
            format!(r#"<div class="ship">{}{link}</div>"#, parts.join(" · "))
        })
        .collect()
}

fn orders_html(f: &Frame<'_>, orders: &[Order], more: bool, all: bool) -> String {
    let mut h = String::from(r#"<section id="orders" aria-labelledby="orders-h"><h2 id="orders-h">Orders</h2>"#);
    if orders.is_empty() {
        h.push_str(r#"<p class="empty">No orders in the last two years.</p>"#);
    }
    for o in orders {
        let thumbs: String = o.items.iter().filter_map(|i| i.image_url.as_deref()).take(4).map(|u| format!(r#"<img src="{}" alt="" loading="lazy">"#, esc(u))).collect();
        let total = o.total_text.as_deref().map(esc).unwrap_or_default();
        let refunded = o.refunded_text.as_deref().map(|r| format!(r#"<div class="muted">{} refunded</div>"#, esc(r))).unwrap_or_default();
        h.push_str(&format!(
            r#"<article class="order"><div class="row"><div><h3>Order {num}</h3><div class="muted">{date}{sep}{count}</div></div><div style="text-align:right">{pill}<div><strong>{total}</strong></div>{refunded}</div></div>{thumbs}{ships}<p style="margin:8px 0 0"><a href="{href}">View order {num}</a></p></article>"#,
            num = esc(&o.number),
            date = esc(&o.placed),
            sep = if o.item_count > 0 { " · " } else { "" },
            count = item_count(o),
            pill = status_pill(o),
            thumbs = if thumbs.is_empty() { String::new() } else { format!(r#"<div class="thumbs">{thumbs}</div>"#) },
            ships = shipments_html(o),
            href = f.href(&format!("/orders/{}", urlencoding(&o.id))),
        ));
    }
    if more && !all {
        h.push_str(&format!(r#"<p><a href="{}">See all orders</a></p>"#, f.href("?orders=all#orders")));
    }
    h.push_str("</section>");
    h
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

fn records_html(records: &[super::data::Record]) -> String {
    let mut h = String::from(r#"<section id="bookings" aria-labelledby="bookings-h"><h2 id="bookings-h">Bookings and subscriptions</h2>"#);
    if records.is_empty() {
        h.push_str(r#"<p class="empty">Nothing here yet.</p>"#);
    }
    for r in records {
        let dl: String = r.fields.iter().map(|(k, v)| format!("<dt>{}</dt><dd>{}</dd>", esc(k), esc(v))).collect();
        h.push_str(&format!(
            r#"<article class="record"><h3>{}</h3><div class="muted">{}</div>{}</article>"#,
            esc(&r.title),
            esc(&r.type_name),
            if dl.is_empty() { String::new() } else { format!("<dl>{dl}</dl>") }
        ));
    }
    h.push_str("</section>");
    h
}

fn picks_html(f: &Frame<'_>, picks: &[super::data::Pick], stock: &[super::data::StockRequest]) -> String {
    let mut h = String::from(r#"<section id="picks" aria-labelledby="picks-h"><h2 id="picks-h">Picked for you</h2>"#);
    if picks.is_empty() {
        h.push_str(r#"<p class="empty">We'll have picks for you after your first visits and orders.</p>"#);
    } else {
        h.push_str(r#"<div class="grid">"#);
        for p in picks {
            let img = p.image_url.as_deref().map(|u| format!(r#"<img src="{}" alt="" loading="lazy">"#, esc(u))).unwrap_or_else(|| r#"<span class="ph"></span>"#.into());
            let inner = format!("{img}<div><strong>{}</strong><br>{}</div>", esc(&p.title), p.price_text.as_deref().map(esc).unwrap_or_default());
            match (&p.url, f.preview) {
                (Some(u), false) => h.push_str(&format!(r#"<a class="card" href="{}" target="_top">{inner}</a>"#, esc(u))),
                _ => h.push_str(&format!(r#"<div class="card">{inner}</div>"#)),
            }
        }
        h.push_str("</div>");
    }
    if !stock.is_empty() {
        h.push_str(r#"<h3 style="margin:20px 0 8px">Waiting to come back in stock</h3>"#);
        for s in stock {
            let img = s.image_url.as_deref().map(|u| format!(r#"<img src="{}" alt="">"#, esc(u))).unwrap_or_default();
            let title = match &s.url {
                Some(u) => format!(r#"<a href="{}" target="_top">{}</a>"#, esc(u), esc(&s.title)),
                None => esc(&s.title),
            };
            let cancel = f.form(&format!("/stock/{}/cancel", s.id), &format!(r#"<button class="btn secondary small" type="submit" aria-label="Stop waiting for {}">Cancel</button>"#, esc(&s.title)), "");
            h.push_str(&format!(r#"<div class="item">{img}<div class="grow">{title}<div class="muted">Asked {}</div></div>{cancel}</div>"#, esc(&s.since)));
        }
    }
    h.push_str("</section>");
    h
}

fn prefs_html(f: &Frame<'_>, p: &Prefs) -> String {
    let mut h = String::from(r#"<section id="preferences" aria-labelledby="prefs-h"><h2 id="prefs-h">Email and texts</h2>"#);
    let email_line = match p.email_status {
        EmailStatus::Subscribed => format!("You get our emails at {}.", esc(&p.email)),
        EmailStatus::NotSubscribed => format!("You're not getting our marketing emails at {}.", esc(&p.email)),
        EmailStatus::Unsubscribed => format!("You've unsubscribed {} from our emails.", esc(&p.email)),
        EmailStatus::Suppressed => format!("We can't email {}: messages to it bounced or were reported, so it can't be changed here.", esc(&p.email)),
    };
    h.push_str(&format!("<p>{email_line}</p>"));
    if p.email_status == EmailStatus::Subscribed && !p.lists.is_empty() {
        let boxes: String = p
            .lists
            .iter()
            .map(|l| {
                format!(
                    r#"<label class="check"><input type="checkbox" name="list" value="{}"{}><span><strong>{}</strong>{}</span></label>"#,
                    l.id,
                    if l.member { " checked" } else { "" },
                    esc(&l.name),
                    if l.description.trim().is_empty() { String::new() } else { format!(r#"<br><span class="muted">{}</span>"#, esc(&l.description)) }
                )
            })
            .collect();
        h.push_str(&f.form("/prefs", &format!(r#"<fieldset style="border:0;padding:0;margin:0"><legend class="muted" style="margin-bottom:8px">Choose what you get:</legend>{boxes}</fieldset><input type="hidden" name="action" value="save_lists"><button class="btn" type="submit">Save choices</button>"#), ""));
    }
    match p.email_status {
        EmailStatus::Subscribed => h.push_str(&f.form("/prefs", r#"<input type="hidden" name="action" value="email_unsubscribe"><button class="btn secondary" type="submit">Unsubscribe from all email</button>"#, "actions")),
        EmailStatus::NotSubscribed | EmailStatus::Unsubscribed => h.push_str(&f.form("/prefs", r#"<input type="hidden" name="action" value="email_subscribe"><button class="btn" type="submit">Get our emails</button>"#, "actions")),
        EmailStatus::Suppressed => {}
    }
    if let Some(last) = &p.phone_hint {
        if p.sms_subscribed {
            h.push_str(&format!(r#"<p style="margin-top:20px">You get our texts at the number ending {}.</p>"#, esc(last)));
            h.push_str(&f.form("/prefs", r#"<input type="hidden" name="action" value="sms_unsubscribe"><button class="btn secondary" type="submit">Stop texts</button>"#, "actions"));
        } else {
            h.push_str(&format!(r#"<p style="margin-top:20px" class="muted">You're not getting our texts at the number ending {}. To get them, sign up for texts on our site.</p>"#, esc(last)));
        }
    }
    h.push_str("</section>");
    h
}

fn help_html(s: &Settings) -> String {
    if s.help.is_empty() && s.contact_url.is_none() {
        return String::new();
    }
    let mut h = String::from(r#"<section id="help" aria-labelledby="help-h"><h2 id="help-h">Help</h2>"#);
    for e in &s.help {
        h.push_str(&format!("<details><summary>{}</summary><p>{}</p></details>", esc(&e.question), esc(&e.answer).replace('\n', "<br>")));
    }
    if let Some(u) = &s.contact_url {
        let label = s.contact_label.as_deref().unwrap_or("Contact us");
        h.push_str(&format!(r#"<p class="actions"><a class="btn secondary" href="{}" target="_top">{}</a></p>"#, esc(u), esc(label)));
    }
    h.push_str("</section>");
    h
}

/// The signed-in hub.
pub fn hub_page(f: &Frame<'_>, v: &View) -> String {
    hub_page_with(f, v, false)
}

pub fn hub_page_with(f: &Frame<'_>, v: &View, all_orders: bool) -> String {
    let mut nav = Vec::new();
    let mut body = format!(
        r#"<h1>{}</h1><p class="muted" style="margin:0 0 16px">Signed in as {}</p>"#,
        v.first_name.as_deref().map(|n| format!("Hi, {}", esc(n))).unwrap_or_else(|| "Your account".into()),
        esc(&v.email)
    );
    let mut sections = String::new();
    if let Some(o) = &v.orders {
        nav.push(("orders", "Orders"));
        sections.push_str(&orders_html(f, o, v.more_orders, all_orders));
    }
    if let Some(r) = &v.records {
        nav.push(("bookings", "Bookings"));
        sections.push_str(&records_html(r));
    }
    if let Some(p) = &v.picks {
        nav.push(("picks", "Picked for you"));
        sections.push_str(&picks_html(f, p, v.stock.as_deref().unwrap_or(&[])));
    }
    if let Some(p) = &v.prefs {
        nav.push(("preferences", "Email and texts"));
        sections.push_str(&prefs_html(f, p));
    }
    if f.settings.sections.help {
        let h = help_html(f.settings);
        if !h.is_empty() {
            nav.push(("help", "Help"));
            sections.push_str(&h);
        }
    }
    if nav.len() > 1 {
        let links: String = nav.iter().map(|(id, label)| format!(r##"<li><a href="#{id}">{label}</a></li>"##)).collect();
        body.push_str(&format!(r#"<nav aria-label="Sections"><ul>{links}</ul></nav>"#));
    }
    body.push_str(&sections);
    document(f, &format!("Your account · {}", f.name), &body)
}

/// One order in full.
pub fn order_page(f: &Frame<'_>, o: &Order) -> String {
    let items: String = o
        .items
        .iter()
        .map(|i| {
            let img = i.image_url.as_deref().map(|u| format!(r#"<img src="{}" alt="">"#, esc(u))).unwrap_or_else(|| r#"<span style="width:48px;height:48px;border-radius:6px;background:#f3f4f6;display:inline-block"></span>"#.into());
            let name = match &i.url {
                Some(u) => format!(r#"<a href="{}" target="_top">{}</a>"#, esc(u), esc(&i.name)),
                None => esc(&i.name),
            };
            let qty = i.quantity.filter(|q| *q > 0.0).map(|q| format!("Quantity {}", if q.fract() == 0.0 { format!("{q:.0}") } else { q.to_string() })).unwrap_or_default();
            format!(r#"<div class="item">{img}<div class="grow">{name}<div class="muted">{qty}</div></div><div>{}</div></div>"#, i.price_text.as_deref().map(esc).unwrap_or_default())
        })
        .collect();
    let refunded = o.refunded_text.as_deref().map(|r| format!(r#"<p class="muted">{} refunded</p>"#, esc(r))).unwrap_or_default();
    let ships = if o.shipments.is_empty() { String::new() } else { format!(r#"<h2 style="margin-top:20px">Shipping</h2>{}"#, shipments_html(o)) };
    let body = format!(
        r#"<p><a href="{back}">← All orders</a></p><section><div class="row"><div><h1>Order {num}</h1><div class="muted">Placed {date}</div></div><div>{pill}</div></div>
<h2 style="margin-top:20px">Items</h2>{items}<div class="row" style="border-top:1px solid #e5e7eb;margin-top:8px;padding-top:10px"><strong>Total</strong><strong>{total}</strong></div>{refunded}{ships}</section>"#,
        back = f.href("#orders"),
        num = esc(&o.number),
        date = esc(&o.placed),
        pill = status_pill(o),
        items = if items.is_empty() { r#"<p class="empty">No item details for this order.</p>"#.to_string() } else { items },
        total = o.total_text.as_deref().map(esc).unwrap_or_default(),
    );
    document(f, &format!("Order {} · {}", o.number, f.name), &body)
}

/// Just the notices (e.g. "We couldn't find that order") and a way back.
pub fn document_only(f: &Frame<'_>, title: &str) -> String {
    document(f, &format!("{title} · {}", f.name), &format!(r#"<p><a href="{}">← Back to your account</a></p>"#, f.href("")))
}

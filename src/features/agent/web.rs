//! The agent's public pages, on the customer hub's host (the store's branded link domain, else
//! the shared link host), and the bubble script the website snippet loads.
//!
//! - `/hub/{key}/chat`: the chat panel inside the customer hub, for signed-in shoppers. The
//!   shopper is the hub session's ([`hub::session::current`]); messages carry the session's form
//!   token and must come from the hub's exact origin. Orders only for an address the store gave
//!   (the hub's own trusted-email rule), checked again whenever they're read.
//! - `/hub/{key}/ask`: the chat in the website's bubble, for anyone. Always anonymous, even when
//!   the browser has a hub session: it never shows anything personal and offers the hub sign-in
//!   for order questions. Framed only by the store's own pages.
//!
//! Answers stream back as server-sent events: `delta` (text), `reset` (throw the streamed text
//! away), then `done` with the final reply and the conversation token.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::chat::{self, Event, Orders, Start, Who};
use super::settings::{self as agent_settings, Settings};
use crate::features::hub::page::{self, esc, Frame};
use crate::features::hub::settings::{self as hub_settings, Settings as HubSettings};
use crate::features::hub::session;
use crate::models::Account;
use crate::state::AppState;

const BUBBLE_JS: &str = include_str!("agent.js");
/// The most bytes in one message request.
const MAX_BODY: usize = 16 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/hub/{key}/chat", get(chat_page))
        .route("/hub/{key}/chat/send", post(chat_send))
        .route("/hub/{key}/ask", get(ask_page))
        .route("/hub/{key}/ask/send", post(ask_send))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .route("/client/agent.js", get(bubble_js))
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    embed: Option<String>,
}

struct Store {
    account: Account,
    hub: HubSettings,
    agent: Settings,
}

/// The store a URL names, on the hub's own host only.
async fn store_for(state: &AppState, key: &str, headers: &HeaderMap) -> Option<Store> {
    if key.len() > 100 {
        return None;
    }
    let id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(key).fetch_optional(&state.db).await.ok()??;
    let account = crate::campaigns::load_account(&state.db, id).await.ok()?;
    let host = crate::site::request_host(headers);
    if !host.is_empty() && host != session::hub_host(state, &account) {
        return None;
    }
    let hub = hub_settings::load(&state.db, id).await.ok()?;
    let agent = agent_settings::load(&state.db, id).await.ok()?;
    Some(Store { account, hub, agent })
}

/// The origins whose pages may show the bubble's chat in a frame: the hub's approved list, else
/// the store's website. The bubble's chat is always anonymous, so a same-site cookie isn't needed.
pub fn site_origins(account: &Account, hub: &HubSettings) -> Vec<String> {
    if !hub.panel_origins.is_empty() {
        return hub.panel_origins.clone();
    }
    let site = account.website.as_deref().map(str::trim).filter(|w| !w.is_empty());
    site.and_then(|w| hub_settings::exact_origin(&if w.contains("://") { w.to_string() } else { format!("https://{w}") }).or_else(|| url::Url::parse(w).ok().filter(|u| u.scheme() == "https").map(|u| u.origin().ascii_serialization())))
        .into_iter()
        .collect()
}

fn not_found() -> Response {
    page_response(StatusCode::NOT_FOUND, "not found".into(), &[], None)
}

/// No caching, no referrer to other sites, scripts only with this page's nonce, requests only to
/// its own origin, framing only by `ancestors` (none: never).
fn page_response(status: StatusCode, html: String, ancestors: &[String], nonce: Option<&str>) -> Response {
    let ancestors = if ancestors.is_empty() { "'none'".to_string() } else { ancestors.join(" ") };
    let script = nonce.map(|n| format!("script-src 'nonce-{n}'; connect-src 'self'; ")).unwrap_or_default();
    let csp = format!("default-src 'none'; {script}img-src https: data:; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors {ancestors}");
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

fn redirect(to: &str) -> Response {
    let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

const CHAT_CSS: &str = ".chat{display:flex;flex-direction:column;gap:10px;min-height:240px;max-height:60vh;overflow-y:auto;padding:4px 0 12px}
.msg{max-width:85%;padding:10px 12px;border-radius:12px;white-space:pre-wrap;overflow-wrap:anywhere;font-size:15px}
.msg.me{align-self:flex-end;background:var(--accent);color:#fff;border-bottom-right-radius:4px}.msg.me a{color:#fff}
.msg.agent{align-self:flex-start;background:#f3f4f6;color:#111827;border-bottom-left-radius:4px}
.msg.agent.thinking{color:#6b7280}.ask{display:flex;gap:8px;align-items:flex-end}.ask textarea{flex:1;min-height:44px;max-height:140px;padding:10px 12px;border:1px solid #d1d5db;border-radius:8px;font:inherit;resize:vertical}
.fine{font-size:12px;color:#6b7280;margin:10px 0 0}.embed .chat{max-height:none;flex:1}";

/// The chat page (both places). `send` is where messages go; `token` is the hub session's form token.
fn chat_html(f: &Frame<'_>, name: &str, intro: &str, send: &str, token: Option<&str>, nonce: &str, place: &str, extra: &str) -> String {
    let cfg = json!({"send": send, "t": token, "key": format!("dm_agent_{place}_{}", f.base.trim_start_matches("/hub/")), "intro": intro}).to_string().replace('<', "\\u003c");
    let body = format!(
        r#"<style>{CHAT_CSS}</style><section aria-labelledby="chat-title"><h1 id="chat-title" style="font-size:20px">{name}</h1>
<div id="log" class="chat" role="log" aria-live="polite" aria-relevant="additions"><div class="msg agent">{intro}</div></div>
<form id="ask" class="ask"><label for="q" class="skip">Your question</label><textarea id="q" name="q" rows="2" maxlength="{max}" placeholder="Ask a question" required></textarea><button class="btn" id="go" type="submit">Send</button></form>
<p class="fine">Answers come from an AI assistant using {store}'s own information. It can make mistakes; it can pass you to a person.</p>{extra}</section>
<script nonce="{nonce}">window.__AGENT__={cfg};</script><script nonce="{nonce}">{js}</script>"#,
        name = esc(name),
        intro = esc(intro),
        max = chat::MAX_MESSAGE_CHARS,
        store = esc(&f.name),
        js = CHAT_JS,
    );
    page::document(f, name, &body)
}

const CHAT_JS: &str = r#"(function(){"use strict";
var C=window.__AGENT__,log=document.getElementById("log"),form=document.getElementById("ask"),q=document.getElementById("q"),go=document.getElementById("go");
var conv=null;try{conv=sessionStorage.getItem(C.key)}catch(e){}
function fill(el,text){el.textContent="";var re=/(https?:\/\/[^\s<>"']+[^\s<>"'.,;:!?)\]])/g,last=0,m;while((m=re.exec(text))){el.appendChild(document.createTextNode(text.slice(last,m.index)));var a=document.createElement("a");a.href=m[1];a.textContent=m[1];a.target="_blank";a.rel="noopener noreferrer";el.appendChild(a);last=m.index+m[1].length}el.appendChild(document.createTextNode(text.slice(last)))}
function add(cls,text){var d=document.createElement("div");d.className="msg "+cls;fill(d,text);log.appendChild(d);log.scrollTop=log.scrollHeight;return d}
function busy(b){go.disabled=b;q.disabled=b}
form.addEventListener("submit",function(e){e.preventDefault();var text=q.value.trim();if(!text)return;add("me",text);q.value="";busy(true);
var out=add("agent thinking","…"),shown="";
fetch(C.send,{method:"POST",credentials:"same-origin",headers:{"content-type":"application/json"},body:JSON.stringify({message:text,conversation:conv,t:C.t})}).then(function(r){
if(!r.ok||!r.body)throw new Error("status");var rd=r.body.getReader(),dec=new TextDecoder(),buf="";
function handle(ev){var line=ev.split("\n").filter(function(l){return l.indexOf("data:")===0}).map(function(l){return l.slice(5).trim()}).join("");if(!line)return;var d;try{d=JSON.parse(line)}catch(x){return}
if(d.t==="delta"){shown+=d.text;out.className="msg agent";fill(out,shown)}else if(d.t==="reset"){shown="";fill(out,"…")}else if(d.t==="done"){if(d.conversation){conv=d.conversation;try{sessionStorage.setItem(C.key,conv)}catch(x){}}out.className="msg agent";fill(out,d.reply||shown)}log.scrollTop=log.scrollHeight}
function pump(){return rd.read().then(function(x){if(x.done){if(buf)handle(buf);return}buf+=dec.decode(x.value,{stream:true});var i;while((i=buf.indexOf("\n\n"))>=0){handle(buf.slice(0,i));buf=buf.slice(i+2)}return pump()})}
return pump()}).catch(function(){out.className="msg agent";fill(out,"Sorry, something went wrong. Please try again.")}).then(function(){busy(false);q.focus()})});
q.addEventListener("keydown",function(e){if(e.key==="Enter"&&!e.shiftKey){e.preventDefault();form.requestSubmit?form.requestSubmit():go.click()}});
})();"#;

fn nonce() -> String {
    crate::auth::random_token(16)
}

fn intro(name: &str, verified: bool) -> String {
    if verified {
        format!("Hi! I'm {name}. Ask me about our products, shipping, returns or your orders.")
    } else {
        format!("Hi! I'm {name}. Ask me about our products, shipping or returns.")
    }
}

/// The chat panel inside the customer hub (signed-in shoppers only).
async fn chat_page(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    let Some(store) = store_for(&state, &key, &headers).await else { return not_found() };
    let embed = q.embed.as_deref() == Some("1");
    if !store.hub.enabled || !chat::available(&state, &store.account, &store.agent, "hub") {
        return not_found();
    }
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(Some(s)) => s,
        Ok(None) => return redirect(&format!("{}{}", session::cookie_path(&store.account), if embed { "?embed=1" } else { "" })),
        Err(_) => return page_response(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Please try again in a minute.".into(), &[], None),
    };
    let mut f = Frame::new(&store.account, &store.hub, embed);
    let token = session::form_token(&state, shopper.session_id);
    f.token = Some(token.clone());
    f.signed_in = true;
    let name = store.agent.display_name(&store.account);
    let n = nonce();
    let back = format!(r#"<p class="fine"><a href="{}">Back to your account</a></p>"#, esc(&format!("{}{}", session::cookie_path(&store.account), if embed { "?embed=1" } else { "" })));
    let html = chat_html(&f, &name, &intro(&name, true), &format!("{}/chat/send", session::cookie_path(&store.account)), Some(&token), &n, "hub", &back);
    let ancestors = if embed { hub_settings::panel_origins(&store.account, &store.hub) } else { Vec::new() };
    page_response(StatusCode::OK, html, &ancestors, Some(&n))
}

/// The bubble's chat (anyone; never personal).
async fn ask_page(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap) -> Response {
    let Some(store) = store_for(&state, &key, &headers).await else { return not_found() };
    if !chat::available(&state, &store.account, &store.agent, "site") {
        return not_found();
    }
    let f = Frame::new(&store.account, &store.hub, true);
    let name = store.agent.display_name(&store.account);
    let n = nonce();
    let html = chat_html(&f, &name, &intro(&name, false), &format!("{}/ask/send", session::cookie_path(&store.account)), None, &n, "site", "");
    page_response(StatusCode::OK, html, &site_origins(&store.account, &store.hub), Some(&n))
}

/// Messages must come from the hub's exact origin (scheme, host and port).
fn same_origin(state: &AppState, headers: &HeaderMap, account: &Account) -> bool {
    let from = headers.get(header::ORIGIN).or_else(|| headers.get(header::REFERER)).and_then(|v| v.to_str().ok());
    let Some(from) = from else { return false };
    let (Ok(u), Ok(hub)) = (url::Url::parse(from), url::Url::parse(&session::hub_url(state, account))) else { return false };
    u.origin() == hub.origin()
}

#[derive(Deserialize)]
struct SendBody {
    message: String,
    #[serde(default)]
    conversation: Option<String>,
    #[serde(default)]
    t: Option<String>,
}

fn plain(status: StatusCode, msg: &str) -> Response {
    let mut r = (status, [(header::CONTENT_TYPE, "application/json")], json!({"error": msg}).to_string()).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn chat_send(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(store) = store_for(&state, &key, &headers).await else { return plain(StatusCode::NOT_FOUND, "not found") };
    if !store.hub.enabled {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    if !same_origin(&state, &headers, &store.account) {
        return plain(StatusCode::FORBIDDEN, "This chat only works on the store's own page.");
    }
    let Ok(b) = serde_json::from_slice::<SendBody>(&body) else { return plain(StatusCode::BAD_REQUEST, "Send a message.") };
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(Some(s)) if session::form_token_ok(&state, s.session_id, b.t.as_deref().unwrap_or("")) => s,
        Ok(Some(_)) => return plain(StatusCode::FORBIDDEN, "This page has expired. Reload it and try again."),
        Ok(None) => return plain(StatusCode::UNAUTHORIZED, "Sign in again to keep chatting."),
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong."),
    };
    // Orders only where the hub shows them, for an address the store gave (checked again on use).
    let trusted = crate::features::hub::data::trusted_email(&state.db, &shopper).await.unwrap_or(false);
    let orders = if store.hub.sections.orders && trusted { Orders::Full } else { Orders::None };
    let who = Who { profile_id: Some(shopper.profile_id), orders };
    stream(state, store.account, "hub", who, b, crate::public::client_ip(&headers))
}

async fn ask_send(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(store) = store_for(&state, &key, &headers).await else { return plain(StatusCode::NOT_FOUND, "not found") };
    if !same_origin(&state, &headers, &store.account) {
        return plain(StatusCode::FORBIDDEN, "This chat only works on the store's own page.");
    }
    let Ok(b) = serde_json::from_slice::<SendBody>(&body) else { return plain(StatusCode::BAD_REQUEST, "Send a message.") };
    // Always anonymous here, whatever cookies the browser has.
    stream(state, store.account, "site", Who::anonymous(), b, crate::public::client_ip(&headers))
}

fn sse(v: Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

/// Run the message on its own task (it finishes and saves even if the browser goes away) and
/// stream what it says.
fn stream(state: AppState, account: Account, place: &'static str, who: Who, b: SendBody, ip: Option<String>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        let sink_tx = tx.clone();
        let sink = move |e: Event| {
            let _ = sink_tx.send(match e {
                Event::Text(t) => sse(json!({"t": "delta", "text": t})),
                Event::Reset => sse(json!({"t": "reset"})),
            });
        };
        let st = Start { account: &account, place, who, token: b.conversation.as_deref(), sms_conversation_id: None, ip: ip.as_deref() };
        let done = match chat::say(&state, st, &b.message, sink).await {
            Ok(o) => json!({"t": "done", "reply": o.reply, "conversation": o.token.or(b.conversation), "handed_off": o.handed_off, "refused": o.refused}),
            Err(e) => {
                tracing::error!(error = %e.public_message(), "agent chat failed");
                json!({"t": "done", "reply": "Sorry, something went wrong. Please try again.", "conversation": b.conversation, "refused": "error"})
            }
        };
        let _ = tx.send(sse(done));
    });
    let body = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<Bytes, std::convert::Infallible>(b), rx)) });
    let mut r = Response::new(Body::from_stream(body));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

/// The bubble launcher, with this store's settings (null when the bubble is off).
async fn bubble_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let config = async {
        if q.k.len() > 100 {
            return None;
        }
        let id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(&q.k).fetch_optional(&state.db).await.ok()??;
        let s = agent_settings::load(&state.db, id).await.ok()?;
        let a = crate::campaigns::load_account(&state.db, id).await.ok()?;
        if !chat::available(&state, &a, &s, "site") {
            return None;
        }
        let hub = hub_settings::load(&state.db, id).await.ok()?;
        Some(json!({
            "url": format!("{}/ask", session::hub_url(&state, &a)),
            "label": format!("Chat with {}", s.display_name(&a)),
            "color": crate::render::safe_color(hub.accent_color.as_deref().unwrap_or(&a.brand_color), "#1f6f5c"),
            "origins": site_origins(&a, &hub),
        }))
    }
    .await;
    let cfg = config.map(|c| c.to_string().replace('<', "\\u003c")).unwrap_or_else(|| "null".into());
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], BUBBLE_JS.replacen("__CONFIG__", &cfg, 1)).into_response()
}

#[derive(Deserialize)]
struct KeyQuery {
    #[serde(default)]
    k: String,
}

/// Whether the snippet should load the bubble for this account (its storefront config says so).
pub async fn bubble_on(state: &AppState, account: &Account) -> bool {
    let Ok(s) = agent_settings::load(&state.db, account.id).await else { return false };
    chat::available(state, account, &s, "site")
}

/// The hub's chat entry, when the hub chat is on: its name, for the hub page's link.
pub async fn hub_chat_name(state: &AppState, account: &Account) -> Option<String> {
    let s = agent_settings::load(&state.db, account.id).await.ok()?;
    chat::available(state, account, &s, "hub").then(|| s.display_name(account))
}

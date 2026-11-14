//! Unauthenticated endpoints: tracking, unsubscribe, browser events, sign-up forms and provider webhooks.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::audience::{self, NewEvent, ProfileInput};
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_phone, Account, FormRow};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/t/o/{token}", get(open_pixel))
        .route("/t/c/{token}", get(click))
        .route("/u/{token}", get(unsubscribe_page).post(unsubscribe_post))
        .route("/client/postlark.js", get(client_js))
        .route("/client/events", post(client_event).options(cors_preflight))
        .route("/client/subscribe", post(client_subscribe).options(cors_preflight))
        .route("/client/forms/{id}/view", post(form_view).options(cors_preflight))
        .route("/f/{id}", get(form_page))
        .route("/f/{id}/embed.js", get(form_embed))
        .route("/webhooks/ses/{secret}", post(ses_webhook))
        .route("/webhooks/telnyx/{secret}", post(telnyx_webhook))
        .route("/webhooks/twilio/{secret}", post(twilio_webhook))
        .route("/healthz", get(|| async { "ok" }))
}

const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

fn message_id_from(state: &AppState, token: &str, prefix: &str) -> Option<Uuid> {
    let payload = auth::verify(&state.cfg.secret_key, token)?;
    let hex = payload.strip_prefix(prefix)?;
    Uuid::parse_str(hex).ok()
}

#[derive(sqlx::FromRow)]
struct Msg {
    id: Uuid,
    account_id: Uuid,
    profile_id: Uuid,
    channel: String,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    is_test: bool,
}

async fn load_msg(state: &AppState, id: Uuid) -> AppResult<Option<Msg>> {
    Ok(sqlx::query_as("SELECT id, account_id, profile_id, channel, campaign_id, flow_id, is_test FROM messages WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?)
}

async fn msg_event(state: &AppState, m: &Msg, metric: &str, extra: Value, unique: Option<String>) -> AppResult<()> {
    if m.is_test {
        return Ok(());
    }
    let mut props = json!({"campaign_id": m.campaign_id, "flow_id": m.flow_id, "message_id": m.id});
    if let (Some(o), Value::Object(e)) = (props.as_object_mut(), extra) {
        o.extend(e);
    }
    audience::record_event(
        &state.db,
        NewEvent { account_id: m.account_id, profile_id: m.profile_id, metric, properties: props, value: None, occurred_at: None, unique_id: unique, message_id: Some(m.id) },
    )
    .await?;
    Ok(())
}

async fn open_pixel(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    if let Some(id) = message_id_from(&state, &token, "") {
        if let Ok(Some(m)) = load_msg(&state, id).await {
            let first: Result<Option<Uuid>, _> = sqlx::query_scalar("UPDATE messages SET opened_at = now() WHERE id = $1 AND opened_at IS NULL RETURNING id")
                .bind(id)
                .fetch_optional(&state.db)
                .await;
            if matches!(first, Ok(Some(_))) {
                let _ = msg_event(&state, &m, "Opened Email", json!({}), Some(format!("open:{id}"))).await;
            }
        }
    }
    (
        [(header::CONTENT_TYPE, "image/gif"), (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0")],
        GIF,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ClickQuery {
    u: String,
    s: String,
}

async fn click(State(state): State<AppState>, Path(token): Path<String>, Query(q): Query<ClickQuery>) -> Response {
    let Some(id) = message_id_from(&state, &token, "") else {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    };
    // Only redirect to URLs we signed, so this can't be used as an open redirect.
    if auth::link_sig(&state.cfg.secret_key, id, &q.u) != q.s || !(q.u.starts_with("https://") || q.u.starts_with("http://")) {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    }
    if let Ok(Some(m)) = load_msg(&state, id).await {
        let _ = sqlx::query("INSERT INTO message_clicks (message_id, url) VALUES ($1, $2)").bind(id).bind(&q.u).execute(&state.db).await;
        let first: Result<Option<Uuid>, _> = sqlx::query_scalar(
            "UPDATE messages SET clicked_at = COALESCE(clicked_at, now()), opened_at = CASE WHEN channel = 'email' THEN COALESCE(opened_at, now()) ELSE opened_at END
             WHERE id = $1 AND clicked_at IS NULL RETURNING id",
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await;
        let metric = if m.channel == "sms" { "Clicked SMS" } else { "Clicked Email" };
        let _ = msg_event(&state, &m, metric, json!({"url": q.u}), None).await;
        if matches!(first, Ok(Some(_))) && m.channel == "email" {
            let _ = msg_event(&state, &m, "Opened Email", json!({}), Some(format!("open:{id}"))).await;
        }
    }
    Redirect::to(&q.u).into_response()
}

fn page(title: &str, body: &str, color: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{t}</title>
<style>body{{margin:0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#f4f4f5;color:#111827;display:flex;min-height:100vh;align-items:center;justify-content:center}}
.c{{background:#fff;border-radius:12px;padding:32px;max-width:420px;width:calc(100% - 32px);box-shadow:0 1px 3px rgba(0,0,0,.08)}}h1{{font-size:22px;margin:0 0 12px}}p{{color:#4b5563;line-height:1.5}}
button,input[type=submit]{{background:{color};color:#fff;border:0;border-radius:8px;padding:12px 20px;font-size:16px;font-weight:600;cursor:pointer;width:100%}}
input[type=email],input[type=tel],input[type=text]{{width:100%;box-sizing:border-box;padding:12px;border:1px solid #d1d5db;border-radius:8px;font-size:16px;margin:0 0 10px}}label{{font-size:13px;color:#4b5563}}</style></head><body><div class="c">{body}</div></body></html>"#,
        t = esc(title)
    ))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn account_for_msg(state: &AppState, m: &Msg) -> Option<Account> {
    crate::campaigns::load_account(&state.db, m.account_id).await.ok()
}

async fn unsubscribe_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(id) = message_id_from(&state, &token, "u") else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let Ok(Some(m)) = load_msg(&state, id).await else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let a = account_for_msg(&state, &m).await;
    let (name, color) = a.map(|a| (a.name, a.brand_color)).unwrap_or(("this sender".into(), "#1f6f5c".into()));
    let what = if m.channel == "sms" { "text messages" } else { "marketing emails" };
    page(
        "Unsubscribe",
        &format!(r#"<h1>Unsubscribe</h1><p>Stop receiving {what} from {n}?</p><form method="post"><button type="submit">Unsubscribe</button></form>"#, n = esc(&name)),
        &color,
    )
    .into_response()
}

async fn unsubscribe_post(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(id) = message_id_from(&state, &token, "u") else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let Ok(Some(m)) = load_msg(&state, id).await else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let col = if m.channel == "sms" { "sms_consent" } else { "email_consent" };
    let changed: Result<Option<Uuid>, _> = sqlx::query_scalar(&format!(
        "UPDATE profiles SET {col} = 'unsubscribed', updated_at = now() WHERE id = $1 AND {col} <> 'unsubscribed' RETURNING id"
    ))
    .bind(m.profile_id)
    .fetch_optional(&state.db)
    .await;
    let _ = sqlx::query("UPDATE messages SET unsubscribed_at = COALESCE(unsubscribed_at, now()) WHERE id = $1").bind(id).execute(&state.db).await;
    if matches!(changed, Ok(Some(_))) {
        let metric = if m.channel == "sms" { "Unsubscribed from SMS" } else { "Unsubscribed" };
        let _ = msg_event(&state, &m, metric, json!({"method": "link"}), None).await;
    }
    let a = account_for_msg(&state, &m).await;
    let (name, color) = a.map(|a| (a.name, a.brand_color)).unwrap_or(("this sender".into(), "#1f6f5c".into()));
    page("Unsubscribed", &format!("<h1>You're unsubscribed</h1><p>You won't get marketing messages from {} anymore.</p>", esc(&name)), &color).into_response()
}

// ---------- Browser tracking ----------

#[derive(Deserialize)]
struct KeyQuery {
    k: String,
}

async fn account_by_public_key(state: &AppState, k: &str) -> AppResult<Account> {
    sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1")
        .bind(k)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Unauthorized)
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type".parse().expect("static header"));
    r
}

async fn cors_preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn public_profile(v: &Value) -> AppResult<ProfileInput> {
    let mut p = v.clone();
    if let Some(o) = p.as_object_mut() {
        // Browsers may not change consent or pick profiles by internal id.
        for k in ["email_consent", "sms_consent", "id"] {
            o.remove(k);
        }
    }
    Ok(serde_json::from_value(p)?)
}

async fn client_event(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    let res: AppResult<Value> = async {
        let a = account_by_public_key(&state, &q.k).await?;
        let b: Value = serde_json::from_slice(&body)?;
        let metric = b["metric"].as_str().ok_or_else(|| AppError::bad("metric is required"))?;
        let input = public_profile(&b["profile"])?;
        let p = audience::upsert_profile(&state.db, a.id, &input).await?.profile;
        let props = if b["properties"].is_object() { b["properties"].clone() } else { json!({}) };
        let value = b["value"].as_f64();
        audience::record_event(
            &state.db,
            NewEvent { account_id: a.id, profile_id: p.id, metric, properties: props, value, occurred_at: None, unique_id: b["unique_id"].as_str().map(String::from), message_id: None },
        )
        .await?;
        Ok(json!({"ok": true}))
    }
    .await;
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

async fn client_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let base = state.cfg.public_url();
    let k = serde_json::to_string(&q.k).unwrap_or_default();
    let js = format!(
        r#"(function(){{var K={k},B="{base}",S="pl_profile";
function g(){{try{{return JSON.parse(localStorage.getItem(S)||"{{}}")}}catch(e){{return{{}}}}}}
function send(p,b){{try{{fetch(B+p+"?k="+encodeURIComponent(K),{{method:"POST",headers:{{"content-type":"application/json"}},body:JSON.stringify(b),keepalive:true}})}}catch(e){{}}}}
var api={{identify:function(p){{var c=g();for(var x in p)c[x]=p[x];try{{localStorage.setItem(S,JSON.stringify(c))}}catch(e){{}}return api}},
track:function(m,props,value){{var p=g();if(!p.email&&!p.phone_number&&!p.external_id)return api;send("/client/events",{{metric:m,profile:p,properties:props||{{}},value:value}});return api}},
profile:g}};
var q=(window.postlark&&window.postlark.q)||[];window.postlark=api;q.forEach(function(c){{api[c[0]]&&api[c[0]].apply(api,c.slice(1))}});
api.track("Active on Site",{{url:location.href,title:document.title}});}})();"#
    );
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], js).into_response()
}

// ---------- Forms ----------

async fn load_form(state: &AppState, id: Uuid) -> AppResult<(FormRow, Account)> {
    let f: FormRow = sqlx::query_as("SELECT * FROM forms WHERE id = $1").bind(id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("form"))?;
    let a = crate::campaigns::load_account(&state.db, f.account_id).await?;
    Ok((f, a))
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    form_id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    first_name: Option<String>,
    #[serde(default)]
    sms_consent: bool,
}

pub async fn subscribe(state: &AppState, account: &Account, b: &SubscribeBody) -> AppResult<Value> {
    let (form, _) = load_form(state, b.form_id).await?;
    if form.account_id != account.id || form.status != "live" {
        return Err(AppError::not_found("form"));
    }
    let email = b.email.clone().filter(|e| !e.trim().is_empty());
    let phone = b.phone_number.clone().filter(|p| !p.trim().is_empty());
    if email.is_none() && phone.is_none() {
        return Err(AppError::bad("enter your email"));
    }
    if let Some(p) = &phone {
        normalize_phone(p).ok_or_else(|| AppError::bad("enter a valid mobile number"))?;
    }
    let input = ProfileInput {
        email: email.clone(),
        phone_number: phone.clone(),
        first_name: b.first_name.clone().filter(|n| !n.trim().is_empty()),
        email_consent: email.as_ref().map(|_| "subscribed".to_string()),
        sms_consent: if b.sms_consent && phone.is_some() { Some("subscribed".into()) } else { None },
        ..Default::default()
    };
    let p = audience::upsert_profile(&state.db, account.id, &input).await?.profile;
    sqlx::query("UPDATE forms SET submits = submits + 1 WHERE id = $1").bind(form.id).execute(&state.db).await?;
    audience::record_event(
        &state.db,
        NewEvent { account_id: account.id, profile_id: p.id, metric: "Subscribed to List", properties: json!({"form_id": form.id, "form": form.name, "list_id": form.list_id}), value: None, occurred_at: None, unique_id: None, message_id: None },
    )
    .await?;
    if let Some(l) = form.list_id {
        audience::add_to_list(&state.db, account.id, l, &[p.id]).await?;
    }
    Ok(json!({"ok": true, "message": form.config["success_message"].as_str().unwrap_or("Thanks for subscribing!")}))
}

async fn client_subscribe(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let is_form = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    let res: AppResult<Value> = async {
        let a = account_by_public_key(&state, &q.k).await?;
        let b: SubscribeBody = if is_form {
            let pairs: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
            SubscribeBody {
                form_id: pairs.get("form_id").and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("missing form"))?,
                email: pairs.get("email").cloned(),
                phone_number: pairs.get("phone_number").cloned(),
                first_name: pairs.get("first_name").cloned(),
                sms_consent: pairs.contains_key("sms_consent"),
            }
        } else {
            serde_json::from_slice(&body)?
        };
        subscribe(&state, &a, &b).await
    }
    .await;
    if is_form {
        // Hosted page fallback without JavaScript.
        return match res {
            Ok(v) => page("Thanks", &format!("<h1>{}</h1>", esc(v["message"].as_str().unwrap_or("Thanks!"))), "#1f6f5c").into_response(),
            Err(e) => page("Oops", &format!("<h1>Something's off</h1><p>{}</p>", esc(&e.public_message())), "#1f6f5c").into_response(),
        };
    }
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

async fn form_view(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let _ = sqlx::query("UPDATE forms SET views = views + 1 WHERE id = $1 AND status = 'live'").bind(id).execute(&state.db).await;
    cors(StatusCode::NO_CONTENT.into_response())
}

fn form_fields_html(f: &FormRow) -> String {
    let collect: Vec<String> = f.config["collect"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_else(|| vec!["email".into()]);
    let mut h = String::new();
    if collect.iter().any(|c| c == "first_name") {
        h.push_str(r#"<input type="text" name="first_name" placeholder="First name" autocomplete="given-name">"#);
    }
    if collect.iter().any(|c| c == "email") {
        h.push_str(r#"<input type="email" name="email" placeholder="Email address" autocomplete="email" required>"#);
    }
    if collect.iter().any(|c| c == "phone_number") {
        let consent = f.config["sms_consent_text"].as_str().unwrap_or("By checking this box you agree to receive recurring automated marketing texts at this number. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out.");
        h.push_str(&format!(r#"<input type="tel" name="phone_number" placeholder="Mobile number" autocomplete="tel"><label style="display:flex;gap:8px;align-items:flex-start;margin:0 0 12px"><input type="checkbox" name="sms_consent" value="1" style="margin-top:3px"><span>{}</span></label>"#, esc(consent)));
    }
    h
}

async fn form_page(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Ok((f, a)) = load_form(&state, id).await else { return (StatusCode::NOT_FOUND, "form not found").into_response() };
    if f.status != "live" {
        return (StatusCode::NOT_FOUND, "this form isn't published yet").into_response();
    }
    let _ = sqlx::query("UPDATE forms SET views = views + 1 WHERE id = $1").bind(id).execute(&state.db).await;
    let color = f.config["brand_color"].as_str().unwrap_or(&a.brand_color).to_string();
    let img = f.config["image_url"].as_str().filter(|s| !s.is_empty()).map(|u| format!(r#"<img src="{}" alt="" style="width:100%;border-radius:8px;margin-bottom:16px">"#, esc(u))).unwrap_or_default();
    let body = format!(
        r#"{img}<h1>{h}</h1><p>{b}</p><form method="post" action="{base}/client/subscribe?k={k}"><input type="hidden" name="form_id" value="{id}">{fields}<input type="submit" value="{btn}"></form>"#,
        h = esc(f.config["headline"].as_str().unwrap_or("")),
        b = esc(f.config["body"].as_str().unwrap_or("")),
        base = state.cfg.public_url(),
        k = esc(&a.public_key),
        fields = form_fields_html(&f),
        btn = esc(f.config["button_text"].as_str().unwrap_or("Sign up")),
    );
    page(&a.name, &body, &color).into_response()
}

async fn form_embed(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Ok((f, a)) = load_form(&state, id).await else { return (StatusCode::NOT_FOUND, "// form not found").into_response() };
    if f.status != "live" {
        return ([(header::CONTENT_TYPE, "application/javascript")], "/* form not published */").into_response();
    }
    let cfg = json!({
        "id": f.id, "kind": f.kind, "key": a.public_key, "base": state.cfg.public_url(),
        "headline": f.config["headline"], "body": f.config["body"], "button": f.config["button_text"],
        "success": f.config["success_message"], "delay": f.config["delay_seconds"].as_f64().unwrap_or(5.0),
        "color": f.config["brand_color"].as_str().unwrap_or(&a.brand_color), "image": f.config["image_url"],
        "fields": form_fields_html(&f),
    });
    let js = format!(
        r##"(function(){{var C={cfg};var SK="pl_form_"+C.id;try{{if(C.kind!=="embed"&&localStorage.getItem(SK))return}}catch(e){{}}
function e(t,a,h){{var x=document.createElement(t);for(var k in a)x.setAttribute(k,a[k]);if(h!=null)x.innerHTML=h;return x}}
function esc(s){{return String(s||"").replace(/[&<>"]/g,function(c){{return{{"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}}[c]}})}}
var css="font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#fff;color:#111827;border-radius:12px;padding:28px;box-sizing:border-box;";
var box=e("div",{{style:css+(C.kind==="embed"?"max-width:480px;":"width:min(420px,calc(100vw - 32px));box-shadow:0 20px 50px rgba(0,0,0,.25);position:relative;")}},
(C.image?'<img src="'+esc(C.image)+'" style="width:100%;border-radius:8px;margin-bottom:14px">':"")+
'<h2 style="margin:0 0 8px;font-size:22px">'+esc(C.headline)+'</h2><p style="margin:0 0 16px;color:#4b5563;line-height:1.5">'+esc(C.body)+'</p>'+
'<form>'+C.fields+'<button type="submit" style="width:100%;padding:12px;border:0;border-radius:8px;background:'+esc(C.color)+';color:#fff;font-size:16px;font-weight:600;cursor:pointer">'+esc(C.button)+'</button><p class="pl-err" style="color:#b91c1c;font-size:13px;margin:8px 0 0"></p></form>');
box.querySelectorAll("input[type=email],input[type=tel],input[type=text]").forEach(function(i){{i.style.cssText="width:100%;box-sizing:border-box;padding:12px;border:1px solid #d1d5db;border-radius:8px;font-size:16px;margin:0 0 10px"}});
box.querySelectorAll("label").forEach(function(l){{l.style.fontSize="12px";l.style.color="#6b7280"}});
var wrap=box;function close(){{try{{localStorage.setItem(SK,"1")}}catch(e){{}}wrap.remove()}}
if(C.kind!=="embed"){{var x=e("button",{{type:"button","aria-label":"Close",style:"position:absolute;top:10px;right:12px;border:0;background:none;font-size:22px;cursor:pointer;color:#6b7280"}},"&times;");x.onclick=close;box.appendChild(x);
wrap=e("div",{{style:C.kind==="flyout"?"position:fixed;right:16px;bottom:16px;z-index:2147483000":"position:fixed;inset:0;background:rgba(17,24,39,.55);display:flex;align-items:center;justify-content:center;z-index:2147483000"}});wrap.appendChild(box);
if(C.kind==="popup")wrap.addEventListener("click",function(ev){{if(ev.target===wrap)close()}})}}
box.querySelector("form").addEventListener("submit",function(ev){{ev.preventDefault();var f=ev.target,d={{form_id:C.id}};
["email","phone_number","first_name"].forEach(function(n){{var i=f.querySelector("[name="+n+"]");if(i&&i.value)d[n]=i.value}});var c=f.querySelector("[name=sms_consent]");d.sms_consent=!!(c&&c.checked);
fetch(C.base+"/client/subscribe?k="+encodeURIComponent(C.key),{{method:"POST",headers:{{"content-type":"application/json"}},body:JSON.stringify(d)}}).then(function(r){{return r.json()}}).then(function(j){{
if(j.error){{f.querySelector(".pl-err").textContent=j.error.message;return}}try{{localStorage.setItem(SK,"1")}}catch(e){{}}
if(window.postlark&&d.email)window.postlark.identify({{email:d.email}});box.querySelector("form").outerHTML='<p style="font-size:18px;font-weight:600">'+esc(j.message||C.success)+'</p>';if(C.kind!=="embed")setTimeout(function(){{wrap.remove()}},2500)}})}});
function show(){{var s=document.currentScript;if(C.kind==="embed"){{var t=document.querySelector('[data-postlark-form="'+C.id+'"]');(t||document.body).appendChild(wrap)}}else document.body.appendChild(wrap);
fetch(C.base+"/client/forms/"+C.id+"/view",{{method:"POST"}}).catch(function(){{}})}}
if(C.kind==="embed"){{document.readyState==="loading"?document.addEventListener("DOMContentLoaded",show):show()}}else setTimeout(show,C.delay*1000);}})();"##,
        cfg = cfg
    );
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=60")], js).into_response()
}

// ---------- Provider webhooks ----------

fn check_secret(state: &AppState, s: &str) -> AppResult<()> {
    let a = state.cfg.webhook_secret.as_bytes();
    let b = s.as_bytes();
    if a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 {
        Ok(())
    } else {
        Err(AppError::NotFound("not found".into()))
    }
}

async fn msg_by_provider_id(state: &AppState, pid: &str) -> AppResult<Option<Msg>> {
    Ok(sqlx::query_as("SELECT id, account_id, profile_id, channel, campaign_id, flow_id, is_test FROM messages WHERE provider_message_id = $1")
        .bind(pid)
        .fetch_optional(&state.db)
        .await?)
}

/// Amazon SES events delivered through an SNS HTTPS subscription.
async fn ses_webhook(State(state): State<AppState>, Path(secret): Path<String>, body: Bytes) -> AppResult<StatusCode> {
    check_secret(&state, &secret)?;
    let env: Value = serde_json::from_slice(&body)?;
    match env["Type"].as_str() {
        Some("SubscriptionConfirmation") => {
            let url = env["SubscribeURL"].as_str().unwrap_or_default();
            let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
            if url.starts_with("https://") && host.starts_with("sns.") && host.ends_with(".amazonaws.com") {
                state.http.get(url).send().await.map_err(|e| AppError::Internal(e.into()))?;
            }
            return Ok(StatusCode::OK);
        }
        Some("Notification") => {}
        _ => return Ok(StatusCode::OK),
    }
    let msg: Value = env["Message"].as_str().map(serde_json::from_str).transpose()?.unwrap_or(Value::Null);
    let kind = msg["eventType"].as_str().or(msg["notificationType"].as_str()).unwrap_or("");
    let Some(pid) = msg["mail"]["messageId"].as_str() else { return Ok(StatusCode::OK) };
    let Some(m) = msg_by_provider_id(&state, pid).await? else { return Ok(StatusCode::OK) };
    match kind {
        "Delivery" => {
            sqlx::query("UPDATE messages SET status = CASE WHEN status = 'sent' THEN 'delivered' ELSE status END, delivered_at = now() WHERE id = $1").bind(m.id).execute(&state.db).await?;
        }
        "Bounce" => {
            let permanent = msg["bounce"]["bounceType"].as_str() == Some("Permanent");
            sqlx::query("UPDATE messages SET status = 'bounced', error = $2 WHERE id = $1").bind(m.id).bind(msg["bounce"]["bounceSubType"].as_str()).execute(&state.db).await?;
            if permanent {
                sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce', updated_at = now() WHERE id = $1").bind(m.profile_id).execute(&state.db).await?;
            }
            msg_event(&state, &m, "Bounced Email", json!({"bounce_type": msg["bounce"]["bounceType"]}), Some(format!("bounce:{}", m.id))).await?;
        }
        "Complaint" => {
            sqlx::query("UPDATE messages SET status = 'complained' WHERE id = $1").bind(m.id).execute(&state.db).await?;
            sqlx::query("UPDATE profiles SET email_suppressed_reason = 'complaint', email_consent = 'unsubscribed', updated_at = now() WHERE id = $1").bind(m.profile_id).execute(&state.db).await?;
            msg_event(&state, &m, "Marked Email as Spam", json!({}), Some(format!("complaint:{}", m.id))).await?;
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

const STOP_WORDS: &[&str] = &["stop", "stopall", "unsubscribe", "cancel", "end", "quit", "optout", "revoke"];
const START_WORDS: &[&str] = &["start", "unstop", "yes", "subscribe"];

/// Inbound text: STOP/START keywords update SMS consent for the matching profile.
async fn inbound_sms(state: &AppState, from: &str, to: &str, text: &str) -> AppResult<()> {
    let (Some(from), Some(to)) = (normalize_phone(from), normalize_phone(to)) else { return Ok(()) };
    let accounts: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE sms_from = $1").bind(&to).fetch_all(&state.db).await?;
    let word: String = text.trim().to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    for account_id in accounts {
        let pid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&from).fetch_optional(&state.db).await?;
        let Some(pid) = pid else { continue };
        let (consent, metric) = if STOP_WORDS.contains(&word.as_str()) {
            ("unsubscribed", "Unsubscribed from SMS")
        } else if START_WORDS.contains(&word.as_str()) {
            ("subscribed", "Subscribed to SMS")
        } else {
            audience::record_event(&state.db, NewEvent { account_id, profile_id: pid, metric: "Received SMS Reply", properties: json!({"text": text}), value: None, occurred_at: None, unique_id: None, message_id: None }).await?;
            continue;
        };
        sqlx::query("UPDATE profiles SET sms_consent = $2, updated_at = now() WHERE id = $1").bind(pid).bind(consent).execute(&state.db).await?;
        audience::record_event(&state.db, NewEvent { account_id, profile_id: pid, metric, properties: json!({"keyword": word}), value: None, occurred_at: None, unique_id: None, message_id: None }).await?;
    }
    Ok(())
}

async fn sms_status(state: &AppState, provider_id: &str, delivered: bool) -> AppResult<()> {
    let status = if delivered { "delivered" } else { "failed" };
    sqlx::query("UPDATE messages SET status = $2, delivered_at = CASE WHEN $2 = 'delivered' THEN now() ELSE delivered_at END WHERE provider_message_id = $1 AND channel = 'sms'")
        .bind(provider_id)
        .bind(status)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn telnyx_webhook(State(state): State<AppState>, Path(secret): Path<String>, Json(v): Json<Value>) -> AppResult<StatusCode> {
    check_secret(&state, &secret)?;
    let p = &v["data"]["payload"];
    match v["data"]["event_type"].as_str().unwrap_or("") {
        "message.received" => {
            let to = p["to"][0]["phone_number"].as_str().unwrap_or_default();
            inbound_sms(&state, p["from"]["phone_number"].as_str().unwrap_or_default(), to, p["text"].as_str().unwrap_or_default()).await?;
        }
        "message.finalized" => {
            let st = p["to"][0]["status"].as_str().unwrap_or_default();
            if let Some(id) = p["id"].as_str() {
                sms_status(&state, id, st == "delivered").await?;
            }
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

async fn twilio_webhook(State(state): State<AppState>, Path(secret): Path<String>, body: Bytes) -> AppResult<Response> {
    check_secret(&state, &secret)?;
    let f: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    if let (Some(from), Some(to), Some(text)) = (f.get("From"), f.get("To"), f.get("Body")) {
        inbound_sms(&state, from, to, text).await?;
    } else if let (Some(sid), Some(st)) = (f.get("MessageSid"), f.get("MessageStatus")) {
        match st.as_str() {
            "delivered" => sms_status(&state, sid, true).await?,
            "failed" | "undelivered" => sms_status(&state, sid, false).await?,
            _ => {}
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/xml")], "<Response></Response>").into_response())
}

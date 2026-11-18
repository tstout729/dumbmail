//! The hosted review page (`/e/review/{token}`, on the account's link domain), its photo uploads,
//! edits within a day, the author's own delete, and the public "leave a review" entry with its
//! email confirmation.
//!
//! A review link is a bearer link: the page shows the item's name and image and the author's own
//! review, nothing else about the person. Every write is rate-limited per link and per address.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::photos;
use super::requests::{self, Request, SUBMITTED_METRIC};
use super::store::{self, Review};
use crate::audience::{record_event, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile, PROFILE_COLS};
use crate::state::AppState;

/// How long after submitting the author may change their review.
pub const EDIT_HOURS: i64 = 24;
/// Public (no-link) reviews waiting for their email confirmation are dropped after this.
pub const CONFIRM_DAYS: i64 = 7;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// JSON that's safe inside a `<script>` element.
fn script_json(v: &Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

fn json_err(e: AppError) -> Response {
    e.into_response()
}

async fn profile(state: &AppState, id: Uuid) -> AppResult<Option<Profile>> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1");
    Ok(sqlx::query_as(&q).bind(id).fetch_optional(&state.db).await?)
}

/// The request a link names, its account, and the review made with it (if any).
async fn load(state: &AppState, token: &str) -> AppResult<(Request, Account, Option<Review>)> {
    let id = requests::id_from_token(state, token).ok_or_else(|| AppError::not_found("review link"))?;
    let r = requests::get(&state.db, id).await?.ok_or_else(|| AppError::not_found("review link"))?;
    let account = crate::campaigns::load_account(&state.db, r.account_id).await?;
    if account.closed_at.is_some() {
        return Err(AppError::not_found("review link"));
    }
    let review = match r.review_id {
        Some(rid) => Some(store::get(&state.db, r.account_id, rid).await?),
        None => None,
    };
    Ok((r, account, review))
}

fn editable(review: &Review) -> bool {
    review.status != "removed" && review.created_at > Utc::now() - Duration::hours(EDIT_HOURS)
}

// ---------- The page ----------

const PAGE_CSS: &str = "*{box-sizing:border-box}body{margin:0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;background:#f4f4f5;color:#111827}
main{max-width:560px;margin:0 auto;padding:16px}.card{background:#fff;border-radius:12px;padding:20px;box-shadow:0 1px 3px rgba(0,0,0,.08);margin:0 0 16px}
.brand{display:flex;align-items:center;gap:10px;font-weight:600;margin:4px 0 16px}.brand img{max-height:36px;max-width:160px}
.item{display:flex;gap:14px;align-items:center}.item img{width:72px;height:72px;object-fit:cover;border-radius:8px;flex:none}.item h1{font-size:20px;margin:0}
label,.label{display:block;font-size:14px;font-weight:600;margin:16px 0 6px}.hint{font-size:13px;color:#6b7280;font-weight:400}
input[type=text],input[type=email],textarea,select{width:100%;padding:12px;border:1px solid #d1d5db;border-radius:8px;font:inherit;font-size:16px;background:#fff;color:#111827}
textarea{min-height:120px;resize:vertical}.stars{display:flex;flex-wrap:wrap;gap:4px;border:0;padding:0;margin:0}.stars legend{padding:0;margin:20px 0 6px;width:100%}.stars input{position:absolute;opacity:0;width:1px;height:1px}
.stars label{margin:0;font-size:40px;line-height:1;color:#d1d5db;cursor:pointer;padding:2px}.stars label.on{color:#f59e0b}.stars input:focus-visible+label{outline:2px solid #2563eb;border-radius:4px}
.choices{display:flex;flex-wrap:wrap;gap:8px}.choices label{margin:0;font-weight:400;border:1px solid #d1d5db;border-radius:999px;padding:8px 14px;cursor:pointer}
.choices input{margin-right:6px}button{font:inherit;font-size:16px;font-weight:600;border:0;border-radius:8px;padding:14px 20px;cursor:pointer}
.primary{color:#fff;width:100%;margin-top:20px}.link{background:none;color:#b91c1c;padding:8px 0;font-weight:500}.photos{display:flex;flex-wrap:wrap;gap:8px;margin-top:8px}
.photos div{position:relative}.photos img{width:72px;height:72px;object-fit:cover;border-radius:6px;display:block}.photos button{position:absolute;top:-6px;right:-6px;width:24px;height:24px;padding:0;border-radius:50%;background:#111827;color:#fff;font-size:14px;line-height:24px}
.err{color:#b91c1c;font-size:14px;margin-top:10px}.note{font-size:13px;color:#4b5563;background:#f9fafb;border-radius:8px;padding:10px 12px;margin-top:16px}
.check{display:flex;gap:8px;align-items:flex-start;font-weight:400;font-size:14px}.others a{display:block;padding:10px 0;color:inherit}.sr{position:absolute;left:-9999px}";

fn page_html(account: &Account, title: &str, body: &str) -> Html<String> {
    let color = crate::render::safe_color(&account.brand_color, "#1f6f5c");
    let logo = account.logo_url.as_deref().filter(|u| u.starts_with("https://")).map(|u| format!(r#"<img src="{}" alt="">"#, esc(u))).unwrap_or_default();
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><title>{t}</title><style>{PAGE_CSS}.primary{{background:{color}}}.choices input,input[type=checkbox]{{accent-color:{color}}}</style></head><body><main><div class="brand">{logo}<span>{name}</span></div>{body}</main></body></html>"#,
        t = esc(title),
        name = esc(&account.name),
    ))
}

fn message(account: &Account, heading: &str, text: &str) -> Response {
    page_html(account, heading, &format!(r#"<div class="card"><h1 style="font-size:22px;margin:0 0 8px">{}</h1><p style="color:#4b5563;line-height:1.5;margin:0">{}</p></div>"#, esc(heading), esc(text))).into_response()
}

fn not_found() -> Response {
    private((StatusCode::NOT_FOUND, Html("<!doctype html><title>Link not found</title><p style=\"font-family:sans-serif;padding:24px\">This review link isn't valid.</p>")).into_response())
}

/// Every answer about a review link: never cached anywhere, and never sent on as a referrer.
pub fn private(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

/// The cookie that lets the browser that wrote a review see and change it during the edit window.
fn edit_cookie_name(request_id: Uuid) -> String {
    format!("rv_{}", request_id.simple())
}

fn edit_cookie_value(state: &AppState, review_id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("rve{}", review_id.simple()))
}

/// Whether this browser wrote the review (it holds the cookie set when it was submitted).
fn wrote_it(state: &AppState, headers: &HeaderMap, request_id: Uuid, review_id: Uuid) -> bool {
    let name = edit_cookie_name(request_id);
    let want = format!("rve{}", review_id.simple());
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).filter_map(|c| c.trim().split_once('=')).any(|(k, v)| {
        k == name && crate::auth::verify(&state.cfg.secret_key, v).as_deref() == Some(want.as_str())
    })
}

fn edit_cookie(state: &AppState, account: &Account, request_id: Uuid, review_id: Uuid) -> String {
    let secure = if crate::domains::link_base(state, account).starts_with("https://") { "; Secure" } else { "" };
    format!("{}={}; Path=/e/review/; Max-Age={}; HttpOnly; SameSite=Lax{secure}", edit_cookie_name(request_id), edit_cookie_value(state, review_id), EDIT_HOURS * 3600)
}

#[derive(Deserialize)]
pub struct PageQuery {
    rating: Option<i16>,
}

pub async fn page(State(state): State<AppState>, Path(token): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    if token == "preview" {
        return preview_page(&state, &headers).await;
    }
    let (r, account, review) = match load(&state, &token).await {
        Ok(x) => x,
        Err(AppError::NotFound(_)) => return not_found(),
        Err(e) => return json_err(e),
    };
    let mine = review.as_ref().is_some_and(|rv| wrote_it(&state, &headers, r.id, rv.id));
    private(match render_form(&state, &token, &r, &account, review.as_ref(), mine, q.rating).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e.public_message(), "review page failed");
            message(&account, "Something went wrong", "Please try the link again in a minute.")
        }
    })
}

async fn preview_page(state: &AppState, headers: &HeaderMap) -> Response {
    // A sample for previews and test sends: the account is the one whose link domain this is, if any.
    let host = crate::site::request_host(headers);
    let account: Option<Account> = sqlx::query_as("SELECT * FROM accounts WHERE link_domain = $1").bind(&host).fetch_optional(&state.db).await.ok().flatten();
    let body = r#"<div class="card"><h1 style="font-size:22px;margin:0 0 8px">This is a preview</h1><p style="color:#4b5563;line-height:1.5;margin:0">In a real email this link opens the review form for the person's own purchase.</p></div>"#;
    private(match account {
        Some(a) => page_html(&a, "Review preview", body).into_response(),
        None => Html(format!("<!doctype html><meta name=viewport content=\"width=device-width,initial-scale=1\"><style>{PAGE_CSS}</style><main>{body}</main>")).into_response(),
    })
}

/// The form. It shows the item's name and (trusted) image, nothing about the person. During the
/// edit window the review's own answers show only to the browser that submitted it (`mine`);
/// anyone else holding the link sees that it was reviewed, and may delete it.
#[allow(clippy::too_many_arguments)]
async fn render_form(state: &AppState, token: &str, r: &Request, account: &Account, review: Option<&Review>, mine: bool, rating: Option<i16>) -> AppResult<Response> {
    let s = store::settings(&state.db, account.id).await?;
    let delete_form = format!(
        r#"<form method="post" action="{}/e/review/{}/delete" onsubmit="return confirm('Delete your review? This can\'t be undone.')"><button class="link" type="submit">Delete my review</button></form>"#,
        esc(&crate::domains::link_base(state, account)),
        esc(token)
    );
    if let Some(rv) = review {
        if rv.status == "removed" {
            return Ok(message(account, "Review deleted", "This review was deleted. Thanks for letting us know what you thought."));
        }
        if !editable(rv) || !mine {
            let body = format!(r#"<div class="card"><h1 style="font-size:22px;margin:0 0 8px">Thanks for your review</h1><p style="color:#4b5563;line-height:1.5">{} has been reviewed with this link.</p>{delete_form}</div>"#, esc(r.name()));
            return Ok(page_html(account, "Your review", &body).into_response());
        }
    } else if r.submitted_at.is_some() {
        // Used up for good: a deleted review doesn't free its link.
        return Ok(message(account, "This link has been used", "A review was already written with this link."));
    } else if r.expires_at <= Utc::now() {
        return Ok(message(account, "This link has expired", "Thanks for thinking of us. This review link is no longer open."));
    }
    let base = crate::domains::link_base(state, account);
    let existing_photos: Vec<(Uuid,)> = match review {
        Some(rv) => sqlx::query_as("SELECT id FROM review_photos WHERE review_id = $1 ORDER BY position, created_at").bind(rv.id).fetch_all(&state.db).await?,
        None => sqlx::query_as("SELECT id FROM review_photos WHERE request_id = $1 AND review_id IS NULL ORDER BY position, created_at").bind(r.id).fetch_all(&state.db).await?,
    };
    // Nothing here depends on who the person is or what they agreed to.
    let offer_signup = s.subscribe_form_id.is_some() && review.is_none();
    let incentive = s.thank_you_coupon.is_some() && review.is_none();
    let data = json!({
        "base": format!("{base}/e/review/{token}"),
        "rating": review.map(|x| x.rating).or(rating.filter(|n| (1..=5).contains(n))),
        "title": review.map(|x| x.title.clone()),
        "body": review.map(|x| x.body.clone()),
        "name": review.map(|x| x.display_name.clone()),
        "recommend": review.and_then(|x| x.recommend),
        "answers": review.map(|x| x.answers.clone()).unwrap_or(json!([])),
        "questions": s.questions,
        "photos": existing_photos.iter().map(|(id,)| json!({"id": id, "url": photos::signed_url(state, *id)})).collect::<Vec<_>>(),
        "editing": review.is_some(),
        "signup": offer_signup,
        "max_photos": photos::MAX_PER_REVIEW,
        "max_body": store::MAX_BODY,
    });
    // The image only when a trusted source supplied it (requests store no other).
    let img = r.item["image_url"].as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://")).map(|u| format!(r#"<img src="{}" alt="">"#, esc(u))).unwrap_or_default();
    let incentive_note = if incentive {
        r#"<p class="note">If you get our emails, we'll email you a discount code as a thank-you for your review, whatever you rate us. Reviews that got one say so.</p>"#
    } else {
        ""
    };
    let body = format!(
        r#"<div class="card"><div class="item">{img}<div><div class="hint">{lead}</div><h1>{name}</h1></div></div>
<form id="f" novalidate>
<fieldset class="stars" id="stars"><legend class="label">Your rating <span class="hint">(required)</span></legend></fieldset>
<label for="title">Title <span class="hint">(optional)</span></label><input type="text" id="title" maxlength="{mt}" autocomplete="off">
<label for="body">Your review</label><textarea id="body" maxlength="{mb}" placeholder="What did you like or not like?"></textarea>
<div id="q"></div>
<div class="label" id="rec-l">Would you recommend it?</div><div class="choices" role="radiogroup" aria-labelledby="rec-l"><label><input type="radio" name="rec" value="yes">Yes</label><label><input type="radio" name="rec" value="no">No</label></div>
<div class="label">Photos <span class="hint">(up to {mp}, optional)</span></div><input type="file" id="ph" accept="image/png,image/jpeg,image/gif,image/webp" multiple aria-label="Add photos"><div class="photos" id="phl"></div>
<label for="name">Name shown with your review <span class="hint">(leave empty for your first name and last initial)</span></label><input type="text" id="name" maxlength="{mn}" autocomplete="off">
<div id="su"></div>{incentive_note}
<p class="note">Reviews are shown as written. We don't publish your email address.</p>
<button class="primary" type="submit" id="go">{button}</button><div class="err" id="err" role="alert"></div></form>
{delete}</div><div id="done"></div>
<script>window.__rv={data};</script><script>{js}</script>"#,
        lead = if review.is_some() { "Edit your review of" } else { "Review" },
        name = esc(r.name()),
        mt = store::MAX_TITLE,
        mb = store::MAX_BODY,
        mn = store::MAX_NAME,
        mp = photos::MAX_PER_REVIEW,
        button = if review.is_some() { "Save changes" } else { "Submit review" },
        delete = if review.is_some() { delete_form.clone() } else { String::new() },
        data = script_json(&data),
        js = PAGE_JS,
    );
    Ok(page_html(account, &format!("Review {}", r.name()), &body).into_response())
}

const PAGE_JS: &str = r#"(function(){var D=window.__rv,$=function(i){return document.getElementById(i)},rating=D.rating||0,photos=D.photos.slice(),busy=0;
function esc(s){return String(s==null?"":s).replace(/[&<>"']/g,function(c){return{"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]})}
var st=$("stars");for(var i=1;i<=5;i++){st.insertAdjacentHTML("beforeend",'<input type="radio" name="r" id="r'+i+'" value="'+i+'"><label for="r'+i+'" aria-label="'+i+' star'+(i>1?'s':'')+'">&#9733;</label>')}
function paint(){for(var i=1;i<=5;i++){$("r"+i).nextSibling.className=i<=rating?"on":"";$("r"+i).checked=i===rating}}
st.addEventListener("change",function(e){rating=Number(e.target.value);paint()});paint();
$("title").value=D.title||"";$("body").value=D.body||"";$("name").value=D.name||"";
if(D.recommend!=null){var rr=document.querySelector('input[name=rec][value='+(D.recommend?"yes":"no")+']');if(rr)rr.checked=true}
var ans={};(D.answers||[]).forEach(function(a){ans[a.id]=a.answer});
D.questions.forEach(function(q){var h='<div class="label" id="ql-'+esc(q.id)+'">'+esc(q.label)+' <span class="hint">(optional)</span></div>';
if(q.kind==="choice"){h+='<div class="choices" role="radiogroup" aria-labelledby="ql-'+esc(q.id)+'">';q.options.forEach(function(o){h+='<label><input type="radio" name="q-'+esc(q.id)+'" value="'+esc(o)+'"'+(ans[q.id]===o?" checked":"")+'>'+esc(o)+'</label>'});h+='</div>'}
else h+='<input type="text" maxlength="200" id="q-'+esc(q.id)+'" aria-labelledby="ql-'+esc(q.id)+'" value="'+esc(ans[q.id]||"")+'">';$("q").insertAdjacentHTML("beforeend",h)});
if(D.signup)$("su").innerHTML='<label class="check"><input type="checkbox" id="sub"> Also send me emails with news and offers (you can unsubscribe at any time)</label>';
function drawPhotos(){$("phl").innerHTML=photos.map(function(p,i){return '<div><img src="'+esc(p.url)+'" alt="Your photo '+(i+1)+'"><button type="button" data-i="'+i+'" aria-label="Remove photo '+(i+1)+'">&times;</button></div>'}).join("");$("ph").disabled=photos.length>=D.max_photos}
drawPhotos();
$("phl").addEventListener("click",function(e){var i=e.target.getAttribute&&e.target.getAttribute("data-i");if(i==null)return;var p=photos[Number(i)];
fetch(D.base+"/photos/"+p.id,{method:"DELETE"}).then(function(){photos.splice(Number(i),1);drawPhotos()})});
$("ph").addEventListener("change",function(){var files=[].slice.call($("ph").files,0,D.max_photos-photos.length);$("ph").value="";
files.forEach(function(f){busy++;$("err").textContent="Uploading…";fetch(D.base+"/photos",{method:"POST",headers:{"content-type":f.type||"application/octet-stream"},body:f}).then(function(r){return r.json().then(function(j){return{ok:r.ok,j:j}})}).then(function(x){busy--;
if(!x.ok){$("err").textContent=(x.j.error&&x.j.error.message)||"That photo couldn't be added.";return}photos.push(x.j);drawPhotos();if(!busy)$("err").textContent=""}).catch(function(){busy--;$("err").textContent="That photo couldn't be added."})})});
$("f").addEventListener("submit",function(e){e.preventDefault();if(!rating){$("err").textContent="Choose a star rating.";$("r1").focus();return}if(busy){$("err").textContent="Wait for your photos to finish uploading.";return}
var a={};D.questions.forEach(function(q){var v=q.kind==="choice"?(document.querySelector('input[name="q-'+q.id+'"]:checked')||{}).value:($("q-"+q.id)||{}).value;if(v)a[q.id]=v});
var rec=document.querySelector("input[name=rec]:checked");$("go").disabled=true;$("err").textContent="";
fetch(D.base,{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({rating:rating,title:$("title").value,body:$("body").value,name:$("name").value,recommend:rec?rec.value==="yes":null,answers:a,photos:photos.map(function(p){return p.id}),subscribe:!!($("sub")&&$("sub").checked)})})
.then(function(r){return r.json().then(function(j){return{ok:r.ok,j:j}})}).then(function(x){$("go").disabled=false;if(!x.ok){$("err").textContent=(x.j.error&&x.j.error.message)||"Your review couldn't be saved. Please try again.";return}
var h='<div class="card"><h1 style="font-size:22px;margin:0 0 8px">Thank you!</h1><p style="color:#4b5563;line-height:1.5;margin:0">'+esc(x.j.message)+'</p></div>';
if(x.j.others&&x.j.others.length){h+='<div class="card others"><div class="label" style="margin-top:0">Review your other items</div>'+x.j.others.map(function(o){return '<a href="'+esc(o.url)+'">'+esc(o.name)+' &rarr;</a>'}).join("")+'</div>'}
$("f").parentNode.style.display="none";$("done").innerHTML=h;window.scrollTo(0,0)}).catch(function(){$("go").disabled=false;$("err").textContent="Your review couldn't be saved. Check your connection and try again."})});})();"#;

// ---------- Submitting ----------

#[derive(Deserialize)]
pub struct SubmitBody {
    rating: i16,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    recommend: Option<bool>,
    #[serde(default)]
    answers: Option<Value>,
    #[serde(default)]
    photos: Option<Vec<Uuid>>,
    #[serde(default)]
    subscribe: bool,
}

async fn limit_link(state: &AppState, id: Uuid, ip: Option<&str>) -> AppResult<()> {
    crate::features::team::ratelimit::hit(state, &format!("review-link:{id}"), 30, 3600).await?;
    crate::features::team::ratelimit::hit_ip(state, "review-link", ip, 120, 3600).await
}

pub async fn submit(State(state): State<AppState>, Path(token): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let ip = crate::public::client_ip(&headers);
    let res: AppResult<(Value, Option<String>)> = async {
        let (r, account, review) = load(&state, &token).await?;
        limit_link(&state, r.id, ip.as_deref()).await?;
        let b: SubmitBody = serde_json::from_slice(&body).map_err(|_| AppError::bad("send the review as JSON with a rating from 1 to 5"))?;
        // Only the browser that wrote a review may change it.
        let editing = match &review {
            Some(rv) if wrote_it(&state, &headers, r.id, rv.id) => Some(rv),
            Some(_) => return Err(AppError::Conflict("You've already reviewed this. Thank you!".into())),
            None => None,
        };
        let v = save(&state, &account, &r, editing, &b).await?;
        let cookie = match (editing, v["review_id"].as_str().and_then(|x| x.parse::<Uuid>().ok())) {
            (None, Some(id)) => Some(edit_cookie(&state, &account, r.id, id)),
            _ => None,
        };
        Ok((v, cookie))
    }
    .await;
    private(match res {
        Ok((v, cookie)) => {
            let mut resp = Json(v).into_response();
            if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
                resp.headers_mut().insert(header::SET_COOKIE, c);
            }
            resp
        }
        Err(e) => e.into_response(),
    })
}

struct Written {
    rating: i16,
    title: String,
    body: String,
    name: String,
    answers: Value,
    flags: Vec<String>,
}

fn written(s: &store::Settings, b: &SubmitBody, default_name: &str) -> AppResult<Written> {
    if !(1..=5).contains(&b.rating) {
        return Err(AppError::bad("choose a rating from 1 to 5 stars"));
    }
    let title = store::plain(b.title.as_deref().unwrap_or(""), store::MAX_TITLE).replace('\n', " ");
    let body = store::plain(b.body.as_deref().unwrap_or(""), store::MAX_BODY);
    let name = store::plain(b.name.as_deref().unwrap_or(""), store::MAX_NAME).replace('\n', " ");
    let name = if name.is_empty() { default_name.to_string() } else { name };
    let answers = store::check_answers(&s.questions, b.answers.as_ref().unwrap_or(&json!({})))?;
    // Everything that will be shown is scanned: the title, the text, the name and the answers.
    let answer_text: Vec<&str> = answers.as_array().into_iter().flatten().filter_map(|a| a["answer"].as_str()).collect();
    let flags = store::flags(&format!("{title}\n{body}\n{name}\n{}", answer_text.join("\n")), &s.flag_words);
    Ok(Written { rating: b.rating, title, body, name, answers, flags })
}

/// Save a review from its link: the first submission, or an edit within [`EDIT_HOURS`].
async fn save(state: &AppState, account: &Account, r: &Request, existing: Option<&Review>, b: &SubmitBody) -> AppResult<Value> {
    let s = store::settings(&state.db, account.id).await?;
    let person = profile(state, r.profile_id).await?.ok_or_else(|| AppError::not_found("review link"))?;
    let w = written(&s, b, &store::default_name(person.first_name.as_deref(), person.last_name.as_deref()))?;
    let photo_ids: Vec<Uuid> = b.photos.clone().unwrap_or_default().into_iter().take(photos::MAX_PER_REVIEW as usize).collect();
    let publish = store::auto_publishes(&s, r.verified, w.rating, &w.flags);
    let mut tx = state.db.begin().await?;
    // The request row is the lock: two submissions with one link can't both write a review.
    let (current, used): (Option<Uuid>, bool) = sqlx::query_as("SELECT review_id, submitted_at IS NOT NULL FROM review_requests WHERE id = $1 FOR UPDATE").bind(r.id).fetch_one(&mut *tx).await?;
    let review_id = match (current, existing) {
        // A link writes one review, ever: deleting it doesn't free the link.
        (None, _) if used => return Err(AppError::Conflict("A review was already written with this link.".into())),
        (None, _) => {
            if r.expires_at <= Utc::now() {
                return Err(AppError::Conflict("This review link has expired.".into()));
            }
            let id = Uuid::now_v7();
            // Labelled when the thank-you code can actually go to them (they get our emails).
            let incentivized = s.thank_you_coupon.is_some() && requests::may_ask(&state.db, person.id, "email").await?;
            sqlx::query(
                "INSERT INTO reviews (id, account_id, subject_id, profile_id, request_id, status, rating, title, body, display_name, recommend, answers, verified,
                    source, incentivized, flags, wants_marketing, confirmed_at)
                 VALUES ($1, $2, $3, $4, $5, 'pending', $6, $7, $8, $9, $10, $11, $12, 'request', $13, $14, $15, now())",
            )
            .bind(id)
            .bind(account.id)
            .bind(r.subject_id)
            .bind(r.profile_id)
            .bind(r.id)
            .bind(w.rating)
            .bind(&w.title)
            .bind(&w.body)
            .bind(&w.name)
            .bind(b.recommend)
            .bind(&w.answers)
            .bind(r.verified)
            .bind(incentivized)
            .bind(&w.flags)
            .bind(b.subscribe && s.subscribe_form_id.is_some())
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE review_requests SET review_id = $2, submitted_at = now() WHERE id = $1").bind(r.id).bind(id).execute(&mut *tx).await?;
            if publish {
                store::set_status(&mut tx, account.id, &[id], "published", None).await?;
            }
            // The event, the thank-you and any sign-up happen in a job queued with the review.
            crate::jobs::enqueue(&mut *tx, "reviews_submitted", json!({"review_id": id}), None).await?;
            id
        }
        (Some(rid), Some(rv)) if rid == rv.id && editable(rv) => {
            let (status, rating): (String, i16) = sqlx::query_as("SELECT status, rating FROM reviews WHERE id = $1 FOR UPDATE").bind(rid).fetch_one(&mut *tx).await?;
            if status == "removed" || status == "rejected" {
                return Err(AppError::Conflict("This review can't be changed any more.".into()));
            }
            let new_status = if publish { "published" } else { "pending" };
            store::replace_rating(&mut tx, (&status, rating, r.subject_id), new_status, w.rating).await?;
            sqlx::query(
                "UPDATE reviews SET rating = $2, title = $3, body = $4, display_name = $5, recommend = $6, answers = $7, flags = $8, status = $9, edited_at = now(), updated_at = now(),
                    published_at = CASE WHEN $9 = 'published' THEN COALESCE(published_at, now()) ELSE published_at END WHERE id = $1",
            )
            .bind(rid)
            .bind(w.rating)
            .bind(&w.title)
            .bind(&w.body)
            .bind(&w.name)
            .bind(b.recommend)
            .bind(&w.answers)
            .bind(&w.flags)
            .bind(new_status)
            .execute(&mut *tx)
            .await?;
            rid
        }
        _ => return Err(AppError::Conflict("You've already reviewed this. Thank you!".into())),
    };
    // Photos uploaded with this link (and still here) belong to the review, in the order given.
    sqlx::query("DELETE FROM review_photos WHERE request_id = $1 AND (review_id IS NULL OR review_id = $2) AND NOT (id = ANY($3))").bind(r.id).bind(review_id).bind(&photo_ids).execute(&mut *tx).await?;
    for (i, pid) in photo_ids.iter().enumerate() {
        sqlx::query("UPDATE review_photos SET review_id = $2, position = $3 WHERE id = $1 AND request_id = $4").bind(pid).bind(review_id).bind(i as i16).bind(r.id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let others = other_items(state, account, r).await?;
    let msg = if existing.is_some() {
        "Your changes are saved."
    } else if publish {
        "Your review is live. Thanks for helping other people choose."
    } else {
        "We've got your review. It will appear once we've had a look."
    };
    Ok(json!({"ok": true, "review_id": review_id, "status": if publish { "published" } else { "pending" }, "message": msg,
        "edit_until": Utc::now() + Duration::hours(EDIT_HOURS), "others": others}))
}

/// The person's other open requests from the same ask, with their links.
async fn other_items(state: &AppState, account: &Account, r: &Request) -> AppResult<Vec<Value>> {
    let q = format!("SELECT {} FROM review_requests q WHERE q.batch_id = $1 AND q.profile_id = $2 AND q.id <> $3 AND q.submitted_at IS NULL AND q.expires_at > now() ORDER BY q.created_at LIMIT 6", requests::REQUEST_COLS);
    let rows: Vec<Request> = sqlx::query_as(&q).bind(r.batch_id).bind(r.profile_id).bind(r.id).fetch_all(&state.db).await?;
    Ok(rows.iter().map(|o| json!({"name": o.name(), "url": requests::link(state, account, o.id)})).collect())
}

pub async fn upload_photo(State(state): State<AppState>, Path(token): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let ip = crate::public::client_ip(&headers);
    let res: AppResult<Value> = async {
        let (r, _account, review) = load(&state, &token).await?;
        limit_link(&state, r.id, ip.as_deref()).await?;
        match &review {
            Some(rv) if !editable(rv) || !wrote_it(&state, &headers, r.id, rv.id) => return Err(AppError::Conflict("This review can't be changed any more.".into())),
            None if r.submitted_at.is_some() || r.expires_at <= Utc::now() => return Err(AppError::Conflict("This review link can't take photos any more.".into())),
            _ => {}
        }
        // The cap is checked before anything is decoded (and again when the photo is saved).
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM review_photos WHERE request_id = $1").bind(r.id).fetch_one(&state.db).await?;
        if n >= photos::MAX_PER_REVIEW {
            return Err(AppError::Conflict(format!("You can add up to {} photos.", photos::MAX_PER_REVIEW)));
        }
        let enc = photos::reencode_for(r.account_id, body.to_vec()).await?;
        let id = Uuid::new_v4();
        // Counted and saved under the request's lock, so parallel uploads can't pass the limit.
        let mut tx = state.db.begin().await?;
        sqlx::query("SELECT 1 FROM review_requests WHERE id = $1 FOR UPDATE").bind(r.id).execute(&mut *tx).await?;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM review_photos WHERE request_id = $1").bind(r.id).fetch_one(&mut *tx).await?;
        if n >= photos::MAX_PER_REVIEW {
            return Err(AppError::Conflict(format!("You can add up to {} photos.", photos::MAX_PER_REVIEW)));
        }
        sqlx::query("INSERT INTO review_photos (id, account_id, request_id, position, width, height, size, bytes) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(id)
            .bind(r.account_id)
            .bind(r.id)
            .bind(n as i16)
            .bind(enc.width as i32)
            .bind(enc.height as i32)
            .bind(enc.bytes.len() as i32)
            .bind(&enc.bytes)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(json!({"id": id, "url": photos::signed_url(&state, id)}))
    }
    .await;
    private(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

pub async fn delete_photo(State(state): State<AppState>, Path((token, photo)): Path<(String, Uuid)>, headers: HeaderMap) -> Response {
    let ip = crate::public::client_ip(&headers);
    let res: AppResult<()> = async {
        let (r, _a, review) = load(&state, &token).await?;
        limit_link(&state, r.id, ip.as_deref()).await?;
        if review.as_ref().is_some_and(|rv| !editable(rv) || !wrote_it(&state, &headers, r.id, rv.id)) {
            return Err(AppError::Conflict("This review can't be changed any more.".into()));
        }
        sqlx::query("DELETE FROM review_photos WHERE id = $1 AND request_id = $2").bind(photo).bind(r.id).execute(&state.db).await?;
        Ok(())
    }
    .await;
    private(match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    })
}

/// The author deletes their own review from their link: it's taken down, its words and photos
/// are erased, and the summary is corrected. The business sees that it was removed by its author.
pub async fn delete_own(State(state): State<AppState>, Path(token): Path<String>, headers: HeaderMap) -> Response {
    let ip = crate::public::client_ip(&headers);
    let res: AppResult<Account> = async {
        let (r, account, review) = load(&state, &token).await?;
        limit_link(&state, r.id, ip.as_deref()).await?;
        let Some(rv) = review else { return Err(AppError::not_found("review")) };
        let mut tx = state.db.begin().await?;
        store::set_status(&mut tx, account.id, &[rv.id], "removed", None).await?;
        sqlx::query("UPDATE reviews SET title = '', body = '', answers = '[]', reply = NULL, featured = false, updated_at = now() WHERE id = $1").bind(rv.id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM review_photos WHERE review_id = $1 OR request_id = $2").bind(rv.id).bind(r.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(account)
    }
    .await;
    private(match res {
        Ok(account) => message(&account, "Review deleted", "Your review has been deleted."),
        Err(AppError::NotFound(_)) => not_found(),
        Err(e) => e.into_response(),
    })
}

// ---------- After a review is in ----------

/// Job: a review was submitted (or a public one confirmed). Records "Submitted Review", signs the
/// author up if they ticked the box (through the form's own consent path), and emails the
/// thank-you code. Each part runs once however often the job does.
pub async fn after_submit(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let err = |e: AppError| anyhow::anyhow!(e.public_message());
    let Some(id) = payload["review_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT account_id FROM reviews WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
    let Some((account_id,)) = row else { return Ok(()) };
    let rv = store::get(&state.db, account_id, id).await.map_err(err)?;
    let (Some(profile_id), Some(_)) = (rv.profile_id, rv.confirmed_at) else { return Ok(()) };
    let subject = store::subject_by_id(&state.db, rv.subject_id).await.map_err(err)?;
    let has_photos: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM review_photos WHERE review_id = $1)").bind(id).fetch_one(&state.db).await?;
    record_event(
        &state.db,
        NewEvent {
            account_id,
            profile_id,
            metric: SUBMITTED_METRIC,
            properties: json!({"rating": rv.rating, "product_id": subject.external_id, "product_name": subject.trusted_name(), "has_photos": has_photos,
                "verified": rv.verified, "review_id": rv.id, "source": rv.source}),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("review:{}", rv.id.simple())),
            message_id: None,
        },
    )
    .await
    .map_err(err)?;
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(err)?;
    let s = store::settings(&state.db, account_id).await.map_err(err)?;
    let wants: bool = sqlx::query_scalar("UPDATE reviews SET wants_marketing = false WHERE id = $1 AND wants_marketing RETURNING true").bind(id).fetch_optional(&state.db).await?.unwrap_or(false);
    let person = profile(state, profile_id).await.map_err(err)?;
    if let (true, Some(form_id), Some(p)) = (wants, s.subscribe_form_id, person.as_ref()) {
        if let Some(email) = &p.email {
            let b: crate::public::SubscribeBody = serde_json::from_value(json!({"form_id": form_id, "email": email}))?;
            if let Err(e) = crate::public::subscribe(state, &account, &b, None).await {
                tracing::warn!(account = %account_id, error = %e.public_message(), "reviewer sign-up failed");
            }
        }
    }
    if rv.incentivized {
        if let (Some(coupon), Some(p)) = (s.thank_you_coupon.as_deref(), person.as_ref()) {
            // Claimed before sending: the thank-you goes at most once.
            let claimed: Option<bool> = sqlx::query_scalar("UPDATE reviews SET thanked_at = now() WHERE id = $1 AND thanked_at IS NULL RETURNING true").bind(id).fetch_optional(&state.db).await?;
            if claimed.is_some() && requests::may_ask(&state.db, p.id, "email").await.map_err(err)? {
                let content = json!({"subject": format!("Thanks for your review, {}", "{{ first_name|default('friend') }}"), "preheader": "A little thank-you from us", "blocks": [
                    {"type": "heading", "text": "Thank you for your review"},
                    {"type": "text", "html": "<p>We read every review. Here's a thank-you code for your next order.</p>"},
                    {"type": "coupon", "coupon": coupon, "title": "Your code", "button_text": "Shop now", "href": account.website.clone().map(|w| if w.starts_with("http") { w } else { format!("https://{w}") }).unwrap_or_default()}
                ]});
                let out = crate::sending::send(
                    state,
                    crate::sending::SendSpec { account: &account, profile: p, channel: "email", content: &content, campaign_id: None, flow_id: None, flow_node_id: None, event: None, is_test: false, message_id: None, to_override: None },
                )
                .await;
                if let Err(e) = out {
                    tracing::warn!(account = %account_id, error = %e.public_message(), "review thank-you email failed");
                }
            }
        }
    }
    Ok(())
}

// ---------- Public "leave a review" (no link) ----------

#[derive(Deserialize)]
pub struct WriteBody {
    product_id: String,
    email: String,
    rating: i16,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    recommend: Option<bool>,
    #[serde(default)]
    answers: Option<Value>,
}

pub fn confirm_token(state: &AppState, review_id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("rvc{}", review_id.simple()))
}

fn confirm_id(state: &AppState, token: &str) -> Option<Uuid> {
    crate::auth::verify(&state.cfg.secret_key, token)?.strip_prefix("rvc")?.parse().ok()
}

/// Confirmation emails one address gets a day for public reviews, across every account.
const ADDRESS_CONFIRMS_PER_DAY: i32 = 3;

/// A review from the website without a link. Off unless the account turns it on. It is unverified,
/// counts only after the writer confirms their address by email, and is always held for a person.
///
/// Nothing here says anything about the address: whether it belongs to a customer, is suppressed,
/// already has a review waiting, or the account can send email right now, the answer is the same.
/// At most one review per (normalised) address waits for confirmation per account, and an address
/// gets at most [`ADDRESS_CONFIRMS_PER_DAY`] confirmation emails a day across all accounts; both
/// are enforced in the insert. No profile is created or changed until the link is used; the email
/// goes from a job.
pub async fn write_public(state: &AppState, account: &Account, b: &WriteBody, ip: Option<&str>) -> AppResult<Value> {
    let s = store::settings(&state.db, account.id).await?;
    if !s.public_reviews {
        return Err(AppError::Forbidden("Reviews can only be written from the link we email after a purchase.".into()));
    }
    let email = crate::models::normalize_email(&b.email).ok_or_else(|| AppError::bad("enter a valid email"))?;
    let key = store::valid_key(&b.product_id).ok_or_else(|| AppError::bad("product_id is missing"))?;
    // Only something the store already has: a review subject or a published catalog product.
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM review_subjects WHERE account_id = $1 AND external_id = $2) OR EXISTS (SELECT 1 FROM catalog_products WHERE account_id = $1 AND external_id = $2 AND published)",
    )
    .bind(account.id)
    .bind(&key)
    .fetch_one(&state.db)
    .await?;
    if !known {
        return Err(AppError::not_found("product"));
    }
    let sb = SubmitBody { rating: b.rating, title: b.title.clone(), body: b.body.clone(), name: b.name.clone(), recommend: b.recommend, answers: b.answers.clone(), photos: None, subscribe: false };
    let w = written(&s, &sb, "A customer")?;
    // Limits by visitor and product (nothing about the address).
    crate::features::team::ratelimit::hit_ip(state, "review-write", ip, 5, 3600).await?;
    crate::features::team::ratelimit::hit(state, &format!("review-write-product:{}:{key}", account.id), 30, 3600).await?;
    let subject = store::subject_for(&state.db, account.id, &key, &store::Hint::default()).await?;
    let email_key = crate::optin::email_key(&email);
    let done = json!({"ok": true, "confirm": true, "message": "Almost done! Check your inbox and tap the link to confirm your review."});
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO reviews (id, account_id, subject_id, status, rating, title, body, display_name, recommend, answers, verified, source, flags, pending_email, email_key, ip_hash)
         VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, false, 'public', $10, $11, $12, $13) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(account.id)
    .bind(subject.id)
    .bind(w.rating)
    .bind(&w.title)
    .bind(&w.body)
    .bind(&w.name)
    .bind(b.recommend)
    .bind(&w.answers)
    .bind(&w.flags)
    .bind(&email)
    .bind(&email_key)
    .bind(ip.map(crate::auth::sha256_hex))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !inserted {
        // A review from this address is already waiting for its confirmation here.
        return Ok(done);
    }
    // The address's daily allowance across all accounts, taken in the same transaction.
    let allowed: Option<i32> = sqlx::query_scalar(
        "INSERT INTO review_address_limits (email_key, day, sent) VALUES ($1, (now() AT TIME ZONE 'America/Los_Angeles')::date, 1)
         ON CONFLICT (email_key, day) DO UPDATE SET sent = review_address_limits.sent + 1 WHERE review_address_limits.sent < $2 RETURNING sent",
    )
    .bind(crate::auth::sha256_hex(&format!("rv-addr:{email_key}")))
    .bind(ADDRESS_CONFIRMS_PER_DAY)
    .fetch_optional(&mut *tx)
    .await?;
    if allowed.is_none() {
        return Ok(done);
    }
    crate::jobs::enqueue(&mut *tx, "reviews_confirm", json!({"review_id": id}), None).await?;
    tx.commit().await?;
    Ok(done)
}

/// Job: email a public review's confirmation link, once. Nothing goes to a suppressed address, from
/// a paused account, or past the account's hourly cap; the writer is never told which. No profile
/// is made for the address: the send is kept for bounce and complaint counts only.
pub async fn send_confirmation(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let err = |e: AppError| anyhow::anyhow!(e.public_message());
    let Some(id) = payload["review_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let claimed: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "UPDATE reviews SET confirm_sent_at = now() WHERE id = $1 AND source = 'public' AND confirmed_at IS NULL AND confirm_sent_at IS NULL RETURNING account_id, pending_email",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, Some(email))) = claimed else { return Ok(()) };
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(err)?;
    let suppressed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM profiles WHERE account_id = $1 AND email = $2 AND email_suppressed_reason IS NOT NULL)")
        .bind(account_id)
        .bind(&email)
        .fetch_one(&state.db)
        .await?;
    if suppressed || account.sending_paused_at.is_some() {
        return Ok(());
    }
    if crate::features::team::ratelimit::hit(state, &format!("review-confirm-account:{account_id}"), 200, 3600).await.is_err() {
        tracing::warn!(account = %account_id, "public review confirmations for the hour used up; one not sent");
        return Ok(());
    }
    let Some(from_email) = account.from_email.clone() else { return Ok(()) };
    if state.email.name() == "ses" && crate::sending::check_sender_domain_name(&account, &from_email).is_err() {
        return Ok(());
    }
    let url = format!("{}/e/review-confirm/{}", crate::domains::link_base(state, &account), confirm_token(state, id));
    let copy = crate::optin::ConfirmCopy {
        subject: &format!("Confirm your review for {}", account.name),
        heading: "Confirm your review",
        line: "Tap the button so we know the review came from you. This doesn't sign you up for anything.",
        text_line: "Open this link to confirm your review",
        button: "Confirm my review",
        didnt: "write a review",
    };
    let (subject, html, text) = crate::optin::confirm_email(&account, &url, &copy);
    let message_id = Uuid::now_v7();
    let out = crate::providers::email::OutgoingEmail {
        message_id,
        to: email,
        from_name: account.from_name.clone().unwrap_or_else(|| account.name.clone()),
        from_email,
        reply_to: account.reply_to.clone(),
        subject,
        html,
        text,
        unsubscribe_url: None,
    };
    crate::sending::throttle_email(state).await;
    match state.email.send(&out).await {
        Ok(provider_id) => crate::deliverability::record_confirmation_send(&state.db, message_id, &provider_id, account_id).await?,
        Err(e) => tracing::warn!(account = %account_id, error = %format!("{e:#}"), "public review confirmation not sent"),
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct KeyQuery {
    k: String,
}

pub async fn write_public_h(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let res: AppResult<Value> = async {
        let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(&q.k).fetch_optional(&state.db).await?.ok_or(AppError::Unauthorized)?;
        let b: WriteBody = serde_json::from_slice(&body).map_err(|_| AppError::bad("send JSON with product_id, email and rating"))?;
        let ip = crate::public::client_ip(&headers);
        write_public(&state, &account, &b, ip.as_deref()).await
    }
    .await;
    super::widget::cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

pub async fn confirm_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    private(confirm_response(&state, &token, false).await)
}

pub async fn confirm_post(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    private(confirm_response(&state, &token, true).await)
}

/// Like double opt-in, confirming takes a button press (a POST), so link scanners can't confirm.
async fn confirm_response(state: &AppState, token: &str, confirm: bool) -> Response {
    let Some(id) = confirm_id(state, token) else { return not_found() };
    let row: Option<(Uuid, Option<String>, bool, bool)> = match sqlx::query_as(
        "SELECT account_id, pending_email, confirmed_at IS NOT NULL, created_at < now() - make_interval(days => $2) FROM reviews WHERE id = $1 AND source = 'public'",
    )
    .bind(id)
    .bind(CONFIRM_DAYS as i32)
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => return json_err(e.into()),
    };
    let Some((account_id, email, confirmed, expired)) = row else { return not_found() };
    let Ok(account) = crate::campaigns::load_account(&state.db, account_id).await else { return not_found() };
    if account.closed_at.is_some() {
        return not_found();
    }
    if confirmed {
        return message(&account, "Review confirmed", "Thanks! Your review will appear once we've had a look.");
    }
    if expired {
        return message(&account, "This link has expired", "Unconfirmed reviews are removed after a week. You're welcome to write it again.");
    }
    if !confirm {
        let body = r#"<div class="card"><h1 style="font-size:22px;margin:0 0 8px">Confirm your review?</h1><p style="color:#4b5563;line-height:1.5">Tap the button so we know the review came from you.</p><form method="post"><button class="primary" type="submit">Confirm my review</button></form></div>"#;
        return page_html(&account, "Confirm your review", body).into_response();
    }
    let res: AppResult<()> = async {
        let Some(email) = email else { return Ok(()) };
        let p = crate::audience::upsert_profile_public(&state.db, account_id, &ProfileInput { email: Some(email), ..Default::default() }).await?.profile;
        let mut tx = state.db.begin().await?;
        let done = sqlx::query("UPDATE reviews SET confirmed_at = now(), profile_id = $2, pending_email = NULL, updated_at = now() WHERE id = $1 AND confirmed_at IS NULL")
            .bind(id)
            .bind(p.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if done == 1 {
            crate::jobs::enqueue(&mut *tx, "reviews_submitted", json!({"review_id": id}), None).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    match res {
        Ok(()) => message(&account, "Review confirmed", "Thanks! Your review will appear once we've had a look."),
        Err(e) => {
            tracing::error!(error = %e.public_message(), "review confirmation failed");
            message(&account, "Something went wrong", "Please try the link again in a minute.")
        }
    }
}

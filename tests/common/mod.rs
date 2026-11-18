//! Shared test harness: a throwaway database, the app router and a signed-in session.
//! Use it from a test file with `#[macro_use] mod common;` and `use common::*;`.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use dumbmail::config::Config;
use dumbmail::{app, jobs, AppState, MIGRATOR};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

pub struct Harness {
    pub state: AppState,
    pub router: axum::Router,
    pub cookie: Option<String>,
    pub admin: PgPool,
    pub dbname: String,
}

impl Harness {
    pub async fn new() -> Option<Self> {
        Self::with(|_| {}).await
    }

    /// A harness whose config `tweak` adjusts first (e.g. pointing Stripe at a stand-in).
    pub async fn with(tweak: impl FnOnce(&mut Config)) -> Option<Self> {
        let admin_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let admin = PgPoolOptions::new().max_connections(2).connect(&admin_url).await.expect("admin connect");
        let dbname = format!("dumbmail_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {dbname}")).execute(&admin).await.unwrap();
        let url = match admin_url.rsplit_once('/') {
            Some((base, _)) => format!("{base}/{dbname}"),
            None => panic!("bad TEST_DATABASE_URL"),
        };
        let db = PgPoolOptions::new().max_connections(5).connect(&url).await.unwrap();
        MIGRATOR.run(&db).await.unwrap();
        let mut cfg = Config {
            database_url: url,
            bind: "127.0.0.1:0".into(),
            public_url: "https://pl.test".into(),
            secret_key: "test-secret".into(),
            email_provider: "log".into(),
            ses_configuration_set: None,
            sms_provider: "log".into(),
            telnyx_api_key: None,
            telnyx_messaging_profile_id: None,
            telnyx_public_key: None,
            twilio_account_sid: None,
            twilio_auth_token: None,
            webhook_secret: "hooksecret".into(),
            hosted: true,
            allow_signup: true,
            send_rate_per_sec: 1000,
            // Most tests send more than a new account's first-day limit; the warm-up test sets its own.
            warmup_start_daily: 1_000_000,
            workers: 1,
            background_workers: 1,
            secure_cookies: false,
            stripe_secret_key: None,
            stripe_webhook_secret: Some("whsec_test".into()),
            stripe_api_base: "https://api.stripe.com/v1".into(),
            price_email_per_1000: 0.20,
            price_sms_segment: 0.0099,
            price_mms: 0.025,
            price_number_monthly: 2.20,
            price_push: 0.0,
            push_api_base: None,
            free_emails_per_month: 25,
            aws_region: "us-east-1".into(),
            railway_api_token: None,
            railway_project_id: None,
            railway_environment_id: None,
            railway_service_id: None,
            admin_token: Some("test-admin-token-0123456789abcdef0123".into()),
            google_client_id: Some("test-client.apps.googleusercontent.com".into()),
            google_client_secret: Some("test-google-secret".into()),
            google_site_verification: Some("google0123abcd.html".into()),
            support_email: Some("help@pl.test".into()),
            operator_alert_email: Some("ops@pl.test".into()),
            redirect_hosts: vec!["old.pl.test".into()],
            link_url: None,
            site_url: None,
            cookie_domain: None,
            google_redirect_base: None,
            anthropic_api_key: None,
            anthropic_api_base: None,
            // Sender screening holds a new account's sends until it's approved; tests/screening.rs turns it on.
            screen_senders: false,
            translation_daily_chars: 200_000,
            price_translation_per_1000_chars: 0.0,
            meta_app_id: None,
            meta_app_secret: None,
            google_ads_client_id: None,
            google_ads_client_secret: None,
            tiktok_app_id: None,
            tiktok_app_secret: None,
            ad_platform_api_base: None,
            shopify: Default::default(),
        };
        tweak(&mut cfg);
        let state = AppState::build(cfg, db).await.unwrap();
        let router = app(state.clone());
        Some(Self { state, router, cookie: None, admin, dbname })
    }

    /// What the dashboard adds to a write made with the session cookie: the app's own `Origin`
    /// and the account the page shows (here: the one the session has open, as after a reload).
    pub async fn page_headers(&self, method: &str, mut b: axum::http::request::Builder) -> axum::http::request::Builder {
        let Some(c) = &self.cookie else { return b };
        b = b.header(header::COOKIE, c);
        if method == "GET" || method == "HEAD" {
            return b;
        }
        b = b.header(header::ORIGIN, self.state.cfg.public_url());
        let token = c.strip_prefix("pl_session=").unwrap_or("");
        let open: Option<uuid::Uuid> = sqlx::query_scalar("SELECT account_id FROM sessions WHERE token_hash = $1")
            .bind(dumbmail::auth::sha256_hex(token))
            .fetch_optional(&self.state.db)
            .await
            .unwrap();
        match open {
            Some(a) => b.header("x-dumbmail-account", a.to_string()),
            None => b,
        }
    }

    pub async fn req(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let b = self.page_headers(method, Request::builder().method(method).uri(path)).await;
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = self.router.clone().oneshot(req).await.unwrap();
        if let Some(set) = res.headers().get(header::SET_COOKIE) {
            let v = set.to_str().unwrap().split(';').next().unwrap().to_string();
            self.cookie = Some(v);
        }
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)}));
        (status, v)
    }

    pub async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body).await;
        assert!(s.is_success(), "{method} {path} -> {s}: {v}");
        v
    }

    pub async fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = self.router.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let h = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, h, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A request with a binary body, sent with this harness's session cookie.
    pub async fn bytes(&self, method: &str, path: &str, content_type: &str, body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let b = self.page_headers(method, Request::builder().method(method).uri(path).header(header::CONTENT_TYPE, content_type)).await;
        let res = self.router.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
        let status = res.status();
        let h = res.headers().clone();
        (status, h, res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    pub async fn drain(&self) {
        jobs::drain(&self.state).await.unwrap();
    }

    /// Make every scheduled job due now (fast-forward delays).
    pub async fn fast_forward(&self) {
        sqlx::query("UPDATE jobs SET run_at = now() WHERE run_at <> 'infinity'").execute(&self.state.db).await.unwrap();
        self.drain().await;
    }

    pub async fn signup(&mut self) -> Value {
        let v = self.ok("POST", "/api/auth/signup", Some(json!({"email": "owner@acme.test", "password": "correct horse", "company": "Acme Goods"}))).await;
        // Sign-up emails a link to confirm the address in the background; tests that count the
        // emails they send start from none.
        for _ in 0..200 {
            if self.state.email.sent().iter().any(|e| e.to == "owner@acme.test" && e.text.contains("/confirm-email?token=")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.state.email.clear_sent();
        self.ok("PATCH", "/api/account", Some(json!({"address": "1 Main St, Portland, OR 97201", "from_email": "hello@acme.test", "website": "acme.test"}))).await;
        v
    }

    /// Give the account prepaid credit (as a Stripe top-up would).
    pub async fn credit(&self, usd: f64) {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&self.state.db).await.unwrap();
        dumbmail::billing::post_entry(&self.state.db, id, "topup", dumbmail::billing::to_micros(usd), "test credit", &uuid::Uuid::new_v4().to_string()).await.unwrap();
    }

    /// Claim a texting number from the dev inventory and return it.
    pub async fn claim_number(&mut self) -> String {
        let avail = self.ok("GET", "/api/phone-numbers/available?type=toll_free", None).await;
        let num = avail["data"][0]["phone_number"].as_str().unwrap().to_string();
        let row = self.ok("POST", "/api/phone-numbers", Some(json!({"phone_number": num}))).await;
        assert_eq!(row["status"], "active", "{row}");
        num
    }

    pub async fn cleanup(self) {
        let Harness { state, admin, dbname, .. } = self;
        state.db.close().await;
        drop(state);
        sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)")).execute(&admin).await.unwrap();
    }
}

macro_rules! harness {
    () => {
        match $crate::common::Harness::new().await {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

pub fn find_link(html: &str, needle: &str) -> String {
    let i = html.find(needle).unwrap_or_else(|| panic!("{needle} not in html"));
    let rest = &html[i..];
    let end = rest.find('"').unwrap();
    rest[..end].replace("&amp;", "&")
}

//! Outbound HTTP to addresses customers type in (webhook endpoints, export buckets).
//!
//! Only HTTPS to public internet addresses, with the DNS answer checked and pinned for the
//! connection, no proxies (an ambient `HTTPS_PROXY` would connect somewhere else than the address
//! we checked), no redirects and a timeout. Callers put one deadline around the check and the
//! request together (see [`within`]). HTTP to localhost is allowed only when the server runs with
//! `WEBHOOKS_ALLOW_LOCALHOST=true` (development) or a test turns it on.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use crate::error::{AppError, AppResult};

static TEST_LOCAL: AtomicBool = AtomicBool::new(false);
static ENV_LOCAL: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WEBHOOKS_ALLOW_LOCALHOST").is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes")));

/// DNS answers for a customer's host must come back this fast.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Let tests send to servers on 127.0.0.1. Never called by the server itself.
pub fn allow_localhost_for_tests() {
    TEST_LOCAL.store(true, Ordering::Relaxed);
}

pub fn localhost_allowed() -> bool {
    TEST_LOCAL.load(Ordering::Relaxed) || *ENV_LOCAL
}

/// A checked target: the parsed URL, its host, and the vetted address to connect to.
pub struct Target {
    pub url: url::Url,
    pub host: String,
    pub addr: SocketAddr,
}

/// Check a URL a customer gave us. `what` names it in error messages ("webhook url").
pub async fn check(raw: &str, what: &str) -> AppResult<Target> {
    let raw = raw.trim();
    if raw.len() > 2000 {
        return Err(AppError::bad(format!("{what} is too long (2,000 characters at most)")));
    }
    let url = url::Url::parse(raw).map_err(|_| AppError::bad(format!("{what} is not a valid URL")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::bad(format!("{what} can't include a user name or password")));
    }
    let host = url.host_str().ok_or_else(|| AppError::bad(format!("{what} needs a host")))?.trim_matches(['[', ']']).to_string();
    if localhost_allowed() && matches!(url.scheme(), "http" | "https") {
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = match tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), port))).await {
            Ok(Ok(a)) => a.collect(),
            _ => Vec::new(),
        };
        if !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback()) {
            return Ok(Target { addr: addrs[0], url, host });
        }
    }
    if url.scheme() != "https" {
        return Err(AppError::bad(format!("{what} must start with https://")));
    }
    let (host, addr) = tokio::time::timeout(DNS_TIMEOUT, crate::flows::check_webhook_url(url.as_str()))
        .await
        .map_err(|_| AppError::bad(format!("{what}'s host didn't resolve in time")))?
        .map_err(|_| AppError::bad(format!("{what} must point to a public internet address")))?;
    Ok(Target { url, host, addr })
}

/// A client that connects only to the checked address, through no proxy, and never follows redirects.
pub fn client(t: &Target, timeout: Duration) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .resolve(&t.host, t.addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .user_agent("DumbMail-Webhooks/1.0 (+https://dumbmail.ai)")
        .build()?)
}

/// Run `work` (checking the address, connecting, sending, reading the answer) under one deadline.
/// None when it ran out of time.
pub async fn within<T>(deadline: Duration, work: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(deadline, work).await.ok()
}

/// A response body read up to a size limit.
pub struct Body {
    pub bytes: Vec<u8>,
    /// The body was longer than the limit (the rest wasn't read).
    pub truncated: bool,
    /// Reading stopped on an error (a reset, a timeout) before the body ended.
    pub failed: bool,
}

impl Body {
    /// The whole body arrived.
    pub fn complete(&self) -> bool {
        !self.truncated && !self.failed
    }
}

/// Read at most `max` bytes of a response body, then stop. Says whether it got the whole body.
pub async fn read_capped(mut res: reqwest::Response, max: usize) -> Body {
    let mut out = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => {
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if chunk.len() > room {
                    return Body { bytes: out, truncated: true, failed: false };
                }
            }
            Ok(None) => return Body { bytes: out, truncated: false, failed: false },
            Err(_) => return Body { bytes: out, truncated: false, failed: true },
        }
    }
}

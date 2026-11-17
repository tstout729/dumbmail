//! Requests to addresses found in a customer's email (link checks, video thumbnails).
//!
//! Public internet addresses only (checked after DNS, and the connection pinned to the checked
//! address), no proxy, no redirects followed, a timeout on the whole request, a cap on bytes read,
//! and no headers from the customer. Errors are reduced to a short class; nothing a remote server
//! sends back is stored or shown except its status code.

use std::time::Duration;

use reqwest::Method;

#[derive(Debug, Clone, PartialEq)]
pub enum NetError {
    /// Not an http(s) address.
    Invalid,
    /// Resolves to a private, local or reserved address.
    NotPublic,
    /// The domain doesn't exist.
    NoSuchDomain,
    Timeout,
    /// Couldn't connect or the connection failed (a short class).
    Failed(&'static str),
}

impl NetError {
    pub fn describe(&self) -> &'static str {
        match self {
            NetError::Invalid => "isn't a web address",
            NetError::NotPublic => "doesn't point to a public website",
            NetError::NoSuchDomain => "points to a domain that doesn't exist",
            NetError::Timeout => "took too long to answer",
            NetError::Failed(c) => c,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub content_length: Option<u64>,
    /// Where a redirect points (not followed).
    pub location: Option<String>,
    pub body: Vec<u8>,
}

fn classify(e: &reqwest::Error) -> NetError {
    if e.is_timeout() {
        NetError::Timeout
    } else if e.is_connect() {
        NetError::Failed("couldn't connect")
    } else if e.is_body() || e.is_decode() {
        NetError::Failed("broke off while answering")
    } else {
        NetError::Failed("couldn't be reached")
    }
}

/// One request to a public web address (see the module docs). Reads at most `max_bytes` of body.
pub async fn request(method: Method, url: &str, max_bytes: usize, timeout: Duration) -> Result<Response, NetError> {
    let parsed = url::Url::parse(url).map_err(|_| NetError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(NetError::Invalid);
    }
    let checked = tokio::time::timeout(timeout, crate::flows::check_webhook_url(url)).await.map_err(|_| NetError::Timeout)?;
    let (host, addr) = checked.map_err(|e| if e.public_message().contains("doesn't resolve") { NetError::NoSuchDomain } else { NetError::NotPublic })?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(&host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .user_agent("Mozilla/5.0 (compatible; DumbMail link check; +https://dumbmail.ai)")
        .build()
        .map_err(|_| NetError::Failed("couldn't be reached"))?;
    let work = async {
        let mut res = client.request(method, parsed).header("accept", "text/html,image/*;q=0.9,*/*;q=0.8").send().await.map_err(|e| classify(&e))?;
        let status = res.status().as_u16();
        let header = |k: reqwest::header::HeaderName| res.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default().to_ascii_lowercase();
        let content_length = header(reqwest::header::CONTENT_LENGTH).and_then(|v| v.trim().parse().ok());
        let location = header(reqwest::header::LOCATION);
        let mut body = Vec::new();
        while body.len() < max_bytes {
            match res.chunk().await.map_err(|e| classify(&e))? {
                Some(c) => body.extend_from_slice(&c[..c.len().min(max_bytes - body.len())]),
                None => break,
            }
        }
        Ok(Response { status, content_type, content_length, location, body })
    };
    tokio::time::timeout(timeout, work).await.map_err(|_| NetError::Timeout)?
}

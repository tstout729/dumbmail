//! A small S3 client (AWS Signature Version 4) for S3-compatible object storage: AWS S3,
//! Cloudflare R2, Google Cloud Storage (XML API with HMAC keys), Backblaze B2, MinIO.
//!
//! Requests go through the same checks as webhooks (`webhooks::net`): HTTPS to public addresses
//! only, pinned DNS, no redirects, timeouts, capped response bodies.

use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::features::webhooks::net;

const TIMEOUT: Duration = Duration::from_secs(120);

pub struct Bucket {
    /// e.g. `https://<account>.r2.cloudflarestorage.com`; None means AWS S3 in `region`.
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub path_style: bool,
    pub access_key: String,
    pub secret_key: String,
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 encoding as SigV4 wants it: everything but unreserved characters (and `/` in paths).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') || (keep_slash && c == '/') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Sign a request. `headers` must include `host` and `x-amz-date`; returns the Authorization value.
pub fn authorization(
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}", uri_encode(path, true));
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let k = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let sig = hex::encode(hmac(&k, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={sig}")
}

pub struct Response {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
    /// The whole answer arrived (not cut off by the size limit or an error).
    pub complete: bool,
}

/// S3 error codes worth showing as they are. Anything else is reported as `other`, so nothing a
/// storage service puts in its answer (which can echo request details) is ever stored or emailed.
const KNOWN_CODES: &[&str] = &[
    "AccessDenied", "AccountProblem", "AuthorizationHeaderMalformed", "BadDigest", "EntityTooLarge", "EntityTooSmall", "ExpiredToken", "InternalError",
    "InvalidAccessKeyId", "InvalidArgument", "InvalidBucketName", "InvalidPart", "InvalidPartOrder", "InvalidRequest", "InvalidToken", "KeyTooLongError",
    "MalformedXML", "MethodNotAllowed", "NoSuchBucket", "NoSuchKey", "NoSuchUpload", "NotImplemented", "PermanentRedirect", "PreconditionFailed",
    "QuotaExceeded", "RequestTimeTooSkewed", "RequestTimeout", "ServiceUnavailable", "SignatureDoesNotMatch", "SlowDown", "TemporaryRedirect",
    "TokenRefreshRequired", "XAmzContentSHA256Mismatch",
];

/// "<what> failed: HTTP 403 AccessDenied". `what` is one of this module's fixed phrases.
fn failure(what: &str, r: &Response) -> anyhow::Error {
    let code = xml_tag(&r.body, "Code").filter(|c| KNOWN_CODES.contains(&c.as_str())).unwrap_or_else(|| "other".into());
    let detail = if !r.complete { "incomplete_response".to_string() } else { code };
    anyhow::anyhow!("{what} failed: HTTP {} {detail}", r.status)
}

/// Keys DumbMail writes never have `.` or `..` segments (a URL parser would rewrite them, and the
/// request would no longer match its signature).
pub fn has_dot_segment(path: &str) -> bool {
    path.split('/').any(|seg| seg == "." || seg == "..")
}

/// How long one request may take, from checking the address to reading the answer.
const DEADLINE: Duration = Duration::from_secs(150);

impl Bucket {
    /// Scheme+host(+port) and the path for an object key.
    pub(super) fn locate(&self, key: &str) -> anyhow::Result<(String, String)> {
        let key = key.trim_start_matches('/');
        let (scheme, host, base_path) = match &self.endpoint {
            None => ("https".to_string(), format!("s3.{}.amazonaws.com", self.region), String::new()),
            Some(e) => {
                let u = url::Url::parse(e).map_err(|_| anyhow::anyhow!("endpoint is not a valid URL"))?;
                let host = u.host_str().ok_or_else(|| anyhow::anyhow!("endpoint needs a host"))?;
                let host = match u.port() {
                    Some(p) => format!("{host}:{p}"),
                    None => host.to_string(),
                };
                (u.scheme().to_string(), host, u.path().trim_end_matches('/').to_string())
            }
        };
        if self.path_style {
            Ok((format!("{scheme}://{host}"), format!("{base_path}/{}/{key}", self.bucket)))
        } else {
            Ok((format!("{scheme}://{}.{host}", self.bucket), format!("{base_path}/{key}")))
        }
    }

    /// Where requests go (scheme, host and port): the keys are only ever sent there.
    pub fn origin(&self) -> String {
        self.locate("").map(|(o, _)| o).unwrap_or_default()
    }

    pub async fn send(&self, method: &str, key: &str, query: &[(&str, &str)], body: Vec<u8>, content_type: Option<&str>) -> anyhow::Result<Response> {
        let (origin, path) = self.locate(key)?;
        if has_dot_segment(&path) {
            anyhow::bail!("object paths can't have . or .. segments");
        }
        let query: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let qs = query.iter().map(|(k, v)| if v.is_empty() { uri_encode(k, false) } else { format!("{}={}", uri_encode(k, false), uri_encode(v, false)) }).collect::<Vec<_>>().join("&");
        let encoded = uri_encode(&path, true);
        let url = if qs.is_empty() { format!("{origin}{encoded}") } else { format!("{origin}{encoded}?{qs}") };
        let work = async {
            let target = net::check(&url, "storage endpoint").await.map_err(|_| anyhow::anyhow!("storage request failed: address_rejected"))?;
            // Sign exactly what goes out: if parsing changed the path, the signature wouldn't match it.
            if target.url.path() != encoded {
                anyhow::bail!("storage request failed: path_changed");
            }
            let host = match target.url.port() {
                Some(p) => format!("{}:{p}", target.host),
                None => target.host.clone(),
            };
            let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let payload_hash = sha256_hex(&body);
            let mut headers = vec![("host".to_string(), host), ("x-amz-content-sha256".to_string(), payload_hash.clone()), ("x-amz-date".to_string(), amz_date.clone())];
            if let Some(ct) = content_type {
                headers.push(("content-type".to_string(), ct.to_string()));
            }
            let auth = authorization(method, &path, &query, &headers, &payload_hash, &self.access_key, &self.secret_key, &self.region, "s3", &amz_date);
            let client = net::client(&target, TIMEOUT)?;
            let m = reqwest::Method::from_bytes(method.as_bytes())?;
            let mut req = client.request(m, target.url.clone()).header("authorization", auth).body(body);
            for (k, v) in headers.iter().filter(|(k, _)| k != "host") {
                req = req.header(k.as_str(), v.as_str());
            }
            let res = req.send().await.map_err(|e| {
                let class = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect_failed"
                } else {
                    "request_failed"
                };
                anyhow::anyhow!("storage request failed: {class}")
            })?;
            let status = res.status().as_u16();
            let etag = res.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
            let b = net::read_capped(res, 64 * 1024).await;
            let complete = b.complete();
            Ok(Response { status, etag, body: String::from_utf8_lossy(&b.bytes).into_owned(), complete })
        };
        net::within(DEADLINE, work).await.unwrap_or_else(|| Err(anyhow::anyhow!("storage request failed: timeout")))
    }

    fn ok(r: &Response, what: &str) -> anyhow::Result<()> {
        // S3 can answer 200 and then an <Error> document (CompleteMultipartUpload, CopyObject).
        if (200..300).contains(&r.status) && r.complete && !r.body.contains("<Error>") {
            return Ok(());
        }
        Err(failure(what, r))
    }

    /// A single upload. It counts only with a 2xx, the whole answer and an ETag.
    pub async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> anyhow::Result<()> {
        let r = self.send("PUT", key, &[], body, Some(content_type)).await?;
        Self::ok(&r, "upload")?;
        if r.etag.is_none() {
            anyhow::bail!("upload failed: HTTP {} no_etag", r.status);
        }
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let r = self.send("DELETE", key, &[], Vec::new(), None).await?;
        Self::ok(&r, "delete")
    }

    pub async fn create_multipart(&self, key: &str, content_type: &str) -> anyhow::Result<String> {
        let r = self.send("POST", key, &[("uploads", "")], Vec::new(), Some(content_type)).await?;
        Self::ok(&r, "starting a multipart upload")?;
        xml_tag(&r.body, "UploadId").filter(|u| !u.is_empty()).ok_or_else(|| anyhow::anyhow!("starting a multipart upload failed: HTTP {} no_upload_id", r.status))
    }

    pub async fn upload_part(&self, key: &str, upload_id: &str, n: usize, body: Vec<u8>) -> anyhow::Result<String> {
        let n = n.to_string();
        let r = self.send("PUT", key, &[("partNumber", &n), ("uploadId", upload_id)], body, None).await?;
        Self::ok(&r, "uploading a part")?;
        r.etag.ok_or_else(|| anyhow::anyhow!("uploading a part failed: HTTP {} no_etag", r.status))
    }

    /// Finish a multipart upload. It counts only when the whole answer arrived and is a
    /// CompleteMultipartUploadResult with an ETag: S3 answers 200 at once and can still fail
    /// (an <Error> document after whitespace), and a cut-off answer proves nothing.
    pub async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[String]) -> anyhow::Result<()> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, etag) in parts.iter().enumerate() {
            let etag = etag.replace('&', "&amp;").replace('<', "&lt;");
            xml.push_str(&format!("<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>", i + 1));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let r = self.send("POST", key, &[("uploadId", upload_id)], xml.into_bytes(), Some("application/xml")).await?;
        Self::ok(&r, "finishing a multipart upload")?;
        if !completed(&r.body) {
            anyhow::bail!("finishing a multipart upload failed: HTTP {} bad_response", r.status);
        }
        Ok(())
    }

    pub async fn abort_multipart(&self, key: &str, upload_id: &str) {
        if let Err(e) = self.send("DELETE", key, &[("uploadId", upload_id)], Vec::new(), None).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't abort multipart upload");
        }
    }
}

/// A CompleteMultipartUpload answer that says it worked: a whole, well-formed XML document (S3
/// may send whitespace first) whose root is `CompleteMultipartUploadResult` with a non-empty ETag.
/// A cut-off document doesn't parse, so it never counts.
fn completed(body: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(body.trim_start()) else { return false };
    let root = doc.root_element();
    root.tag_name().name() == "CompleteMultipartUploadResult"
        && root.children().any(|c| c.is_element() && c.tag_name().name() == "ETag" && c.text().is_some_and(|t| !t.trim().is_empty()))
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's worked example ("Create a signed AWS API request", IAM ListUsers).
    #[test]
    fn matches_aws_example() {
        let headers = vec![
            ("Content-Type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string()),
            ("Host".to_string(), "iam.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ];
        let query = vec![("Action".to_string(), "ListUsers".to_string()), ("Version".to_string(), "2010-05-08".to_string())];
        let auth = authorization(
            "GET",
            "/",
            &query,
            &headers,
            &sha256_hex(b""),
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "iam",
            "20150830T123600Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// AWS's S3 example ("GET Object" in the Signature Version 4 header examples).
    #[test]
    fn matches_s3_get_object_example() {
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization("GET", "/test.txt", &[], &headers, &sha256_hex(b""), "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "us-east-1", "s3", "20130524T000000Z");
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
    }

    #[test]
    fn locates_objects() {
        let mut b = Bucket { endpoint: None, region: "us-west-2".into(), bucket: "acme".into(), path_style: false, access_key: "a".into(), secret_key: "s".into() };
        assert_eq!(b.locate("x/y.gz").unwrap(), ("https://acme.s3.us-west-2.amazonaws.com".into(), "/x/y.gz".into()));
        b.endpoint = Some("https://abc.r2.cloudflarestorage.com".into());
        b.path_style = true;
        assert_eq!(b.locate("x/y.gz").unwrap(), ("https://abc.r2.cloudflarestorage.com".into(), "/acme/x/y.gz".into()));
        assert_eq!(xml_tag("<a><UploadId> u1 </UploadId></a>", "UploadId").as_deref(), Some("u1"));
        assert!(has_dot_segment("/acme/./x") && has_dot_segment("/a/../b") && !has_dot_segment("/a/.hidden/b.gz"));
    }

    #[test]
    fn completion_must_say_it_worked() {
        assert!(completed("<?xml version=\"1.0\"?>\n<CompleteMultipartUploadResult><Bucket>b</Bucket><ETag>\"abc-2\"</ETag></CompleteMultipartUploadResult>"));
        assert!(!completed("  \n  \n<Error><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>"));
        assert!(!completed(""));
        assert!(!completed("<CompleteMultipartUploadResult><Bucket>b</Bucket>"));
        // Cut off after the ETag: everything a success has, except its end.
        assert!(!completed("<?xml version=\"1.0\"?>\n<CompleteMultipartUploadResult><Bucket>b</Bucket><ETag>\"abc-2\"</ETag>"));
        assert!(!completed("<CompleteMultipartUploadResult><Bucket>b</Bucket><ETag>\"abc-2\"</ETag></CompleteMultipartUploadResu"));
        assert!(!completed("<CompleteMultipartUploadResult><ETag> </ETag></CompleteMultipartUploadResult>"));
        assert!(completed("   \n<CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><ETag>\"e\"</ETag></CompleteMultipartUploadResult>"));
        let r = Response { status: 403, etag: None, body: "<Error><Code>AccessDenied</Code><Message>signature abc123 for AKIA...</Message></Error>".into(), complete: true };
        assert_eq!(failure("upload", &r).to_string(), "upload failed: HTTP 403 AccessDenied");
        let r = Response { status: 400, etag: None, body: "<Error><Code>Reflected AKIA123</Code></Error>".into(), complete: true };
        assert_eq!(failure("upload", &r).to_string(), "upload failed: HTTP 400 other");
    }
}

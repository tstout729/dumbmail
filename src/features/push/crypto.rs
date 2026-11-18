//! The cryptography push needs: VAPID keys and their JWTs (RFC 8292, ES256), Web Push payload
//! encryption (RFC 8291, aes128gcm from RFC 8188), APNs provider tokens (ES256) and Google service
//! account assertions (RS256). Built on the RustCrypto crates; the encryption is checked against
//! the RFC 8291 example in the tests below.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use p256::{PublicKey, SecretKey};
use rand::RngCore;
use serde_json::Value;
use sha2::Sha256;

pub fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode base64url with or without padding (browsers send either), or standard base64.
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let t = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(t).ok().or_else(|| STANDARD.decode(s.trim()).ok())
}

fn random_secret() -> SecretKey {
    loop {
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        if let Ok(k) = SecretKey::from_slice(&b) {
            return k;
        }
    }
}

/// A new VAPID key pair: (public key, uncompressed and base64url; private key, base64url).
pub fn new_vapid_keys() -> (String, String) {
    let sk = random_secret();
    (b64u(&sk.public_key().to_sec1_bytes()), b64u(&sk.to_bytes()))
}

fn secret_from_b64(private_b64u: &str) -> anyhow::Result<SecretKey> {
    let raw = unb64(private_b64u).ok_or_else(|| anyhow::anyhow!("bad private key encoding"))?;
    SecretKey::from_slice(&raw).map_err(|_| anyhow::anyhow!("bad private key"))
}

/// A compact JWS signed with ES256 (signature is r || s, as JWS requires).
fn es256_jwt(key: &SecretKey, header: &Value, claims: &Value) -> String {
    let signing = format!("{}.{}", b64u(header.to_string().as_bytes()), b64u(claims.to_string().as_bytes()));
    let sk = p256::ecdsa::SigningKey::from(key.clone());
    let sig: p256::ecdsa::Signature = sk.sign(signing.as_bytes());
    format!("{signing}.{}", b64u(&sig.to_bytes()))
}

/// The `Authorization` header for a push service (RFC 8292): `vapid t=<jwt>, k=<public key>`.
/// `audience` is the push service's origin (scheme://host[:port]); `subject` a mailto: or https: URL.
pub fn vapid_authorization(private_b64u: &str, public_b64u: &str, audience: &str, subject: &str, now: i64) -> anyhow::Result<String> {
    let key = secret_from_b64(private_b64u)?;
    // At most 24 hours (RFC 8292 section 2); 12 keeps clock skew from mattering.
    let claims = serde_json::json!({"aud": audience, "exp": now + 12 * 3600, "sub": subject});
    let jwt = es256_jwt(&key, &serde_json::json!({"typ": "JWT", "alg": "ES256"}), &claims);
    Ok(format!("vapid t={jwt}, k={public_b64u}"))
}

/// APNs provider token: ES256 with the .p8 key, `kid` the key id, `iss` the team id.
pub fn apns_token(p8_pem: &str, key_id: &str, team_id: &str, now: i64) -> anyhow::Result<String> {
    let key = SecretKey::from_pem(p8_pem.trim()).map_err(|_| anyhow::anyhow!("the .p8 key isn't a valid P-256 private key"))?;
    Ok(es256_jwt(&key, &serde_json::json!({"alg": "ES256", "kid": key_id}), &serde_json::json!({"iss": team_id, "iat": now})))
}

/// Check a .p8 key parses (at save time).
pub fn check_p8(p8_pem: &str) -> bool {
    SecretKey::from_pem(p8_pem.trim()).is_ok()
}

/// A Google service account's signed assertion (RS256) for the OAuth token endpoint.
pub fn google_assertion(private_key_pem: &str, key_id: &str, client_email: &str, scope: &str, token_uri: &str, now: i64) -> anyhow::Result<String> {
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer as _};
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem.trim()).map_err(|_| anyhow::anyhow!("the service account's private_key isn't a valid RSA key"))?;
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": key_id});
    let claims = serde_json::json!({"iss": client_email, "scope": scope, "aud": token_uri, "iat": now, "exp": now + 3600});
    let signing = format!("{}.{}", b64u(header.to_string().as_bytes()), b64u(claims.to_string().as_bytes()));
    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key);
    let sig = signer.sign(signing.as_bytes());
    Ok(format!("{signing}.{}", b64u(&sig.to_vec())))
}

/// Check a service account private key parses (at save time).
pub fn check_rsa_pem(pem: &str) -> bool {
    use rsa::pkcs8::DecodePrivateKey;
    rsa::RsaPrivateKey::from_pkcs8_pem(pem.trim()).is_ok()
}

/// Check a browser's subscription keys: p256dh a P-256 point, auth 16 bytes.
pub fn check_subscription_keys(p256dh: &str, auth: &str) -> bool {
    let ok_point = unb64(p256dh).is_some_and(|b| b.len() == 65 && PublicKey::from_sec1_bytes(&b).is_ok());
    ok_point && unb64(auth).is_some_and(|b| b.len() == 16)
}

/// The record size we declare. One record carries the whole message (payloads stay far below it).
const RECORD_SIZE: u32 = 4096;

/// Encrypt `plaintext` for a subscription (RFC 8291 with aes128gcm content coding). Returns the
/// request body: salt, record size, the server's public key and the ciphertext.
pub fn encrypt(p256dh_b64u: &str, auth_b64u: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    encrypt_with(p256dh_b64u, auth_b64u, plaintext, &random_secret(), &salt)
}

fn encrypt_with(p256dh_b64u: &str, auth_b64u: &str, plaintext: &[u8], as_secret: &SecretKey, salt: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let ua_public_raw = unb64(p256dh_b64u).ok_or_else(|| anyhow::anyhow!("bad p256dh"))?;
    let ua_public = PublicKey::from_sec1_bytes(&ua_public_raw).map_err(|_| anyhow::anyhow!("bad p256dh"))?;
    let auth = unb64(auth_b64u).filter(|a| a.len() == 16).ok_or_else(|| anyhow::anyhow!("bad auth secret"))?;
    // A single record holds plaintext + the 0x02 delimiter + the 16-byte tag.
    if plaintext.len() + 1 + 16 > RECORD_SIZE as usize {
        anyhow::bail!("push payload too large");
    }
    let as_public = as_secret.public_key().to_sec1_bytes();
    let shared = as_secret.diffie_hellman(&ua_public);
    let (cek, nonce) = derive(shared.raw_secret_bytes().as_ref(), &auth, &ua_public_raw, &as_public, salt)?;
    let mut record = plaintext.to_vec();
    record.push(2);
    let ct = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow::anyhow!("key"))?
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    let mut out = Vec::with_capacity(16 + 4 + 1 + as_public.len() + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// The content encryption key and nonce (RFC 8291 section 3.4).
fn derive(ecdh_secret: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8]) -> anyhow::Result<([u8; 16], [u8; 12])> {
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), ecdh_secret).expand(&key_info, &mut ikm).map_err(|_| anyhow::anyhow!("hkdf"))?;
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow::anyhow!("hkdf"))?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow::anyhow!("hkdf"))?;
    Ok((cek, nonce))
}

/// Decrypt a Web Push body as a browser would (tests and the local push-service stand-in use it).
pub fn decrypt(ua_private_b64u: &str, auth_b64u: &str, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    if body.len() < 21 {
        anyhow::bail!("too short");
    }
    let salt = &body[..16];
    let idlen = body[20] as usize;
    let as_public_raw = body.get(21..21 + idlen).ok_or_else(|| anyhow::anyhow!("bad header"))?;
    let ct = &body[21 + idlen..];
    let ua_secret = secret_from_b64(ua_private_b64u)?;
    let ua_public = ua_secret.public_key().to_sec1_bytes();
    let as_public = PublicKey::from_sec1_bytes(as_public_raw).map_err(|_| anyhow::anyhow!("bad key"))?;
    let auth = unb64(auth_b64u).ok_or_else(|| anyhow::anyhow!("bad auth"))?;
    let shared = ua_secret.diffie_hellman(&as_public);
    let (cek, nonce) = derive(shared.raw_secret_bytes().as_ref(), &auth, &ua_public, as_public_raw, salt)?;
    let mut pt = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow::anyhow!("key"))?
        .decrypt(Nonce::from_slice(&nonce), ct)
        .map_err(|_| anyhow::anyhow!("decrypt failed"))?;
    // Strip padding and the last-record delimiter.
    while pt.last() == Some(&0) {
        pt.pop();
    }
    if pt.pop() != Some(2) {
        anyhow::bail!("bad delimiter");
    }
    Ok(pt)
}

/// A browser-side key pair for tests: (p256dh public, private, auth), all base64url.
pub fn test_subscription_keys() -> (String, String, String) {
    let sk = random_secret();
    let mut auth = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut auth);
    (b64u(&sk.public_key().to_sec1_bytes()), b64u(&sk.to_bytes()), b64u(&auth))
}

/// HMAC-SHA256 of `message` as lowercase hex.
pub fn hmac_hex(secret: &str, message: &str) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("any key length");
    m.update(message.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// Compare a claimed MAC (hex) with the right one in constant time.
pub fn hmac_hex_matches(secret: &str, message: &str, claimed: &str) -> bool {
    let Ok(raw) = hex::decode(claimed.trim()) else { return false };
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("any key length");
    m.update(message.as_bytes());
    m.verify_slice(&raw).is_ok()
}

/// Longest an enrollment grant may be valid.
pub const GRANT_MAX_SECONDS: i64 = 86_400;

/// What an enrollment grant signs: version, purpose, the account, the identifier's type and value,
/// and when it expires. Field by field, so no value can be read as another field.
pub fn enroll_grant_message(account_id: &str, id_type: &str, value: &str, expires: i64) -> String {
    format!("v1|push-enroll|{account_id}|{id_type}|{value}|{expires}")
}

/// A grant as the customer's server makes it: `v1.<expires>.<hex HMAC-SHA256>`.
pub fn make_enroll_grant(secret: &str, account_id: &str, id_type: &str, value: &str, expires: i64) -> String {
    format!("v1.{expires}.{}", hmac_hex(secret, &enroll_grant_message(account_id, id_type, value, expires)))
}

/// Check a grant for this account and identifier: the right format, not expired, expiring within
/// [`GRANT_MAX_SECONDS`] (plus five minutes of clock skew), and the MAC (compared in constant time).
pub fn verify_enroll_grant(secret: &str, account_id: &str, id_type: &str, value: &str, grant: &str, now: i64) -> Result<(), &'static str> {
    let mut parts = grant.trim().splitn(3, '.');
    let (Some("v1"), Some(exp), Some(mac)) = (parts.next(), parts.next(), parts.next()) else { return Err("the grant isn't in the v1.<expires>.<mac> format") };
    let expires: i64 = exp.parse().map_err(|_| "the grant's expiry isn't a number")?;
    if expires < now {
        return Err("the grant has expired");
    }
    if expires > now + GRANT_MAX_SECONDS + 300 {
        return Err("a grant can be valid for at most 24 hours");
    }
    if hmac_hex_matches(secret, &enroll_grant_message(account_id, id_type, value, expires), mac) { Ok(()) } else { Err("the grant doesn't match") }
}

/// Compare two strings in constant time (for MACs computed elsewhere).
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A new per-install secret for an app device: 256 random bits, base64url. Only its
/// [`install_secret_hash`] is stored.
pub fn new_install_secret() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// The stored form of an install secret (SHA-256, hex).
pub fn install_secret_hash(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(secret.trim().as_bytes()))
}

/// Whether `claimed` is the secret whose hash is `stored` (compared in constant time).
pub fn install_secret_matches(stored: Option<&str>, claimed: Option<&str>) -> bool {
    match (stored, claimed) {
        (Some(h), Some(c)) if !c.trim().is_empty() && c.len() <= 200 => ct_eq(h, &install_secret_hash(c)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_secrets_are_long_random_and_checked_by_hash() {
        let (a, b) = (new_install_secret(), new_install_secret());
        assert!(a.len() >= 22 && a != b, "at least 128 bits, never repeated");
        let h = install_secret_hash(&a);
        assert!(install_secret_matches(Some(&h), Some(&a)));
        assert!(!install_secret_matches(Some(&h), Some(&b)) && !install_secret_matches(Some(&h), None) && !install_secret_matches(None, Some(&a)));
    }

    fn b(s: &str) -> Vec<u8> {
        unb64(&s.replace([' ', '\n'], "")).unwrap()
    }

    /// RFC 8291 section 5 and appendix A.
    #[test]
    fn rfc8291_example() {
        let as_secret = SecretKey::from_slice(&b("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        assert_eq!(b64u(&as_secret.public_key().to_sec1_bytes()), "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let ua_public = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        let salt: [u8; 16] = b("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt_with(ua_public, "BTBZMqHH6r4Tts7J_aSIgg", b"When I grow up, I want to be a watermelon", &as_secret, &salt).unwrap();
        let expected = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";
        assert_eq!(b64u(&body), expected);
        // And the browser's side decrypts it.
        let pt = decrypt("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94", "BTBZMqHH6r4Tts7J_aSIgg", &body).unwrap();
        assert_eq!(pt, b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn round_trip_and_limits() {
        let (public, private, auth) = test_subscription_keys();
        assert!(check_subscription_keys(&public, &auth));
        assert!(!check_subscription_keys("AAAA", &auth));
        let body = encrypt(&public, &auth, b"{\"title\":\"Hi\"}").unwrap();
        assert_eq!(decrypt(&private, &auth, &body).unwrap(), b"{\"title\":\"Hi\"}");
        assert!(encrypt(&public, &auth, &vec![b'x'; 4096]).is_err());
    }

    #[test]
    fn vapid_jwt_verifies() {
        use p256::ecdsa::signature::Verifier;
        let (public, private) = new_vapid_keys();
        let h = vapid_authorization(&private, &public, "https://fcm.googleapis.com", "mailto:ops@example.com", 1_700_000_000).unwrap();
        let (t, k) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, public);
        let (signing, sig) = t.rsplit_once('.').unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&unb64(&public).unwrap()).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&unb64(sig).unwrap()).unwrap();
        vk.verify(signing.as_bytes(), &sig).unwrap();
        let claims: Value = serde_json::from_slice(&unb64(signing.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_700_000_000 + 12 * 3600);
    }

    #[test]
    fn enroll_grants() {
        let now = 1_800_000_000;
        let g = make_enroll_grant("s", "acct", "external_id", "42", now + 600);
        assert!(verify_enroll_grant("s", "acct", "external_id", "42", &g, now).is_ok());
        // Bound to the identifier's type, the value, the account and the secret.
        assert!(verify_enroll_grant("s", "acct", "email", "42", &g, now).is_err());
        assert!(verify_enroll_grant("s", "acct", "external_id", "43", &g, now).is_err());
        assert!(verify_enroll_grant("s", "other", "external_id", "42", &g, now).is_err());
        assert!(verify_enroll_grant("t", "acct", "external_id", "42", &g, now).is_err());
        // Expired, or valid for too long.
        assert_eq!(verify_enroll_grant("s", "acct", "external_id", "42", &g, now + 601), Err("the grant has expired"));
        let long = make_enroll_grant("s", "acct", "external_id", "42", now + 3 * 86_400);
        assert!(verify_enroll_grant("s", "acct", "external_id", "42", &long, now).is_err());
        assert!(verify_enroll_grant("s", "acct", "external_id", "42", "v2.1.aa", now).is_err());
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
    }

    #[test]
    fn user_hash() {
        let h = hmac_hex("secret", "customer-42");
        assert!(hmac_hex_matches("secret", "customer-42", &h));
        assert!(!hmac_hex_matches("secret", "customer-43", &h));
        assert!(!hmac_hex_matches("secret", "customer-42", "zz"));
    }
}

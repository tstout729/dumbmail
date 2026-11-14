//! Encryption at rest for customer provider credentials (AES-256-GCM, key derived from SECRET_KEY).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

fn cipher(secret: &str) -> Aes256Gcm {
    let key = Sha256::digest(format!("postlark-credentials:{secret}").as_bytes());
    Aes256Gcm::new_from_slice(&key).expect("32-byte key")
}

pub fn encrypt(secret: &str, plaintext: &str) -> anyhow::Result<String> {
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher(secret).encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes()).map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

pub fn decrypt(secret: &str, encoded: &str) -> anyhow::Result<String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    if raw.len() < 13 {
        anyhow::bail!("ciphertext too short");
    }
    let (nonce, ct) = raw.split_at(12);
    let pt = cipher(secret).decrypt(Nonce::from_slice(nonce), ct).map_err(|_| anyhow::anyhow!("decrypt failed (SECRET_KEY changed?)"))?;
    Ok(String::from_utf8(pt)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let c = super::encrypt("k", "{\"api_key\":\"x\"}").unwrap();
        assert_eq!(super::decrypt("k", &c).unwrap(), "{\"api_key\":\"x\"}");
        assert!(super::decrypt("other", &c).is_err());
    }
}

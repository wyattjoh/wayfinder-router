use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "wf";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedKey {
    pub plaintext: String,
    pub hash: String,
}

pub fn hash_key(presented: &str) -> String {
    let digest = Sha256::digest(presented.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn verify(presented: &str, expected_hash: &str) -> bool {
    constant_time_eq(
        hash_key(presented).as_bytes(),
        expected_hash.trim().to_lowercase().as_bytes(),
    )
}

pub fn match_key<I, K, V>(presented: Option<&str>, hashes: I) -> Option<String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let presented = presented.filter(|value| !value.is_empty())?;
    let digest = hash_key(presented);
    let mut found = None;
    for (key_id, expected) in hashes {
        if constant_time_eq(
            digest.as_bytes(),
            expected.as_ref().trim().to_lowercase().as_bytes(),
        ) {
            found = Some(key_id.as_ref().to_string());
        }
    }
    found
}

pub fn extract_bearer(authorization: Option<&str>) -> Option<String> {
    let value = authorization?.trim();
    if value.is_empty() {
        return None;
    }
    let lower = value.to_lowercase();
    if lower == "bearer" || lower.starts_with("bearer ") {
        let token = value[6..].trim();
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    } else {
        Some(value.to_string())
    }
}

pub fn generate(prefix: &str) -> GeneratedKey {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let plaintext = format!("{prefix}-{}", URL_SAFE_NO_PAD.encode(bytes));
    GeneratedKey {
        hash: hash_key(&plaintext),
        plaintext,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let max = a.len().max(b.len());
    for index in 0..max {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

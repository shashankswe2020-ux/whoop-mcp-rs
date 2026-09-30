//! Cryptographic helpers backed by `ring`.

use ring::rand::{SecureRandom, SystemRandom};
use ring::{digest, hkdf, hmac};

/// Cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system random number generator unavailable");
    buf
}

/// SHA-256 digest.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

/// HMAC-SHA256 tag.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data).as_ref().to_vec()
}

/// Verify an HMAC-SHA256 tag in constant time.
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], tag: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&key, data, tag).is_ok()
}

struct KeyLen(usize);

impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

/// HKDF-SHA256 (RFC 5869) output keying material.
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let info = [info];
    let okm = prk
        .expand(&info, KeyLen(len))
        .expect("HKDF output length within limits");
    let mut out = vec![0u8; len];
    okm.fill(&mut out)
        .expect("HKDF output length within limits");
    out
}

/// Constant-time comparison of two tokens after SHA-256 hashing (no length oracle).
/// Empty inputs never match.
pub fn safe_token_compare(provided: &str, expected: &str) -> bool {
    if provided.is_empty() || expected.is_empty() {
        return false;
    }
    let a = sha256(provided.as_bytes());
    let b = sha256(expected.as_bytes());
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url encoding.
pub fn base64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(B64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Decode unpadded base64url; rejects padding and non-alphabet characters.
pub fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in input.bytes() {
        let value = B64URL.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if input.len() % 4 == 1 || buffer != 0 {
        return None;
    }
    Some(out)
}

/// Lowercase hex encoding.
pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random RFC 4122 version 4 UUID.
pub fn uuid_v4() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        for len in 0..40 {
            let data = random_bytes(len);
            assert_eq!(base64url_decode(&base64url_encode(&data)).unwrap(), data);
        }
        assert!(base64url_decode("Zm9v=").is_none());
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url_encode(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn hkdf_matches_rfc_5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        assert_eq!(
            hex(&hkdf_sha256(&ikm, &salt, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn token_compare_and_uuid() {
        assert!(safe_token_compare("abc", "abc"));
        assert!(!safe_token_compare("abc", "abd"));
        assert!(!safe_token_compare("", ""));
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}

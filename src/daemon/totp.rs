//! RFC 6238 time-based one-time passwords (the 6-digit codes from authenticator apps), used as an
//! optional second factor for the remote dashboard.
use hmac::{Hmac, Mac};

const STEP_SECS: u64 = 30;
const DIGITS: u32 = 6;
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// A new random 160-bit secret, base32 encoded.
pub fn new_secret() -> String {
    let mut buf = [0u8; 20];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    base32_encode(&buf)
}

pub fn code_at(secret: &[u8], step: u64) -> u32 {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[h.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    bin % 10u32.pow(DIGITS)
}

pub fn current_step() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) / STEP_SECS
}

/// Checks `code` against the steps around `now_step` (±1, to allow for clock drift) and returns the
/// matching step so callers can reject reuse of the same code.
pub fn verify(secret_b32: &str, code: &str, now_step: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let want: u32 = code.parse().ok()?;
    let secret = base32_decode(secret_b32)?;
    let mut hit = None;
    for step in [now_step.saturating_sub(1), now_step, now_step + 1] {
        if code_at(&secret, step) == want {
            hit = Some(step);
        }
    }
    hit
}

/// The `otpauth://` URI authenticator apps import (usually via a QR code).
pub fn uri(secret_b32: &str, account: &str, issuer: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!(
        "otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        enc(issuer),
        enc(account),
        enc(issuer)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, SHA-1 secret "12345678901234567890", 8-digit codes truncated to 6.
        let s = b"12345678901234567890";
        assert_eq!(code_at(s, 59 / 30), 94287082 % 1_000_000);
        assert_eq!(code_at(s, 1111111109 / 30), 7081804 % 1_000_000);
        assert_eq!(code_at(s, 2000000000 / 30), 69279037 % 1_000_000);
    }

    #[test]
    fn base32_roundtrip_and_verify() {
        let s = new_secret();
        assert_eq!(s.len(), 32);
        assert_eq!(base32_encode(&base32_decode(&s).unwrap()), s);
        assert_eq!(base32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let key = base32_decode(&s).unwrap();
        let code = format!("{:06}", code_at(&key, 1000));
        assert_eq!(verify(&s, &code, 1000), Some(1000));
        assert_eq!(verify(&s, &code, 1001), Some(1000));
        assert_eq!(verify(&s, &code, 1003), None);
        assert_eq!(verify(&s, "12345", 1000), None);
        assert!(uri(&s, "admin@mac", "LStack").starts_with("otpauth://totp/LStack:admin%40mac?secret="));
    }
}

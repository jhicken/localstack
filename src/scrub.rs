//! Redacts secret values from any text before it is stored or shown (logs, errors, API output).
//! The patterns are registered by the secret store; every encoding a value commonly leaks in
//! (raw, base64, base64url, percent-encoded, hex) is matched.
//!
//! The patterns are kept encrypted in memory (one blob, ChaCha20-Poly1305 under a random key in locked
//! memory) and opened only for the length of a scrub, then wiped.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use std::borrow::Cow;
use std::sync::RwLock;
use zeroize::{Zeroize, Zeroizing};

/// Values shorter than this are not scrubbed (too likely to match ordinary text).
pub const MIN_LEN: usize = 4;

struct SealedPatterns {
    key: Box<[u8; 32]>,
    nonce: [u8; 12],
    ct: Vec<u8>,
    /// (offset, length, mask) per pattern in the opened blob, longest first. Lengths and names aren't secret.
    index: Vec<(usize, usize, String)>,
}

impl Drop for SealedPatterns {
    fn drop(&mut self) {
        self.key.zeroize();
        unsafe { libc::munlock(self.key.as_ptr() as *const libc::c_void, 32) };
    }
}

impl SealedPatterns {
    fn seal(pats: Vec<(Zeroizing<String>, String)>) -> Option<SealedPatterns> {
        if pats.is_empty() {
            return None;
        }
        let mut key = Box::new([0u8; 32]);
        unsafe { libc::mlock(key.as_ptr() as *const libc::c_void, 32) };
        fill_random(&mut key[..]);
        let mut nonce = [0u8; 12];
        fill_random(&mut nonce);
        let mut blob = Zeroizing::new(Vec::with_capacity(pats.iter().map(|(p, _)| p.len()).sum()));
        let mut index = Vec::with_capacity(pats.len());
        for (p, m) in pats {
            index.push((blob.len(), p.len(), m));
            blob.extend_from_slice(p.as_bytes());
        }
        let ct = ChaCha20Poly1305::new_from_slice(&key[..]).ok()?.encrypt(Nonce::from_slice(&nonce), Payload { msg: &blob, aad: b"scrub" }).ok()?;
        Some(SealedPatterns { key, nonce, ct, index })
    }

    fn open(&self) -> Opened<'_> {
        let blob = ChaCha20Poly1305::new_from_slice(&self.key[..])
            .ok()
            .and_then(|c| c.decrypt(Nonce::from_slice(&self.nonce), Payload { msg: &self.ct, aad: b"scrub" }).ok())
            .unwrap_or_default();
        Opened { blob: Zeroizing::new(blob), index: &self.index }
    }

    fn max_len(&self) -> usize {
        self.index.iter().map(|(_, l, _)| *l).max().unwrap_or(0)
    }
}

/// The patterns in plaintext, for one scrub; wiped when dropped.
struct Opened<'a> {
    blob: Zeroizing<Vec<u8>>,
    index: &'a [(usize, usize, String)],
}

impl Opened<'_> {
    fn iter(&self) -> impl Iterator<Item = (&[u8], &str)> {
        self.index.iter().filter_map(|(o, l, m)| self.blob.get(*o..o + l).map(|p| (p, m.as_str())))
    }
}

fn fill_random(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(buf);
    }
}

static PATTERNS: RwLock<Option<SealedPatterns>> = RwLock::new(None);

pub fn mask(name: &str) -> String {
    format!("••••[{name}]")
}

fn b64(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64(data: &[u8]) -> String {
    b64(data, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/", true)
}

fn base64url(data: &[u8]) -> String {
    b64(data, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_", false)
}

pub fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Every form of `value` we redact.
pub fn variants(value: &str) -> Vec<String> {
    let mut v = vec![value.to_string()];
    let mut b = base64(value.as_bytes());
    v.push(b.trim_end_matches('=').to_string());
    zeroize::Zeroize::zeroize(&mut b);
    v.push(base64url(value.as_bytes()));
    v.push(percent(value));
    v.push(value.bytes().map(|b| format!("{b:02x}")).collect());
    v.push(value.bytes().map(|b| format!("{b:02X}")).collect());
    v.sort();
    let mut dropped = vec![];
    let mut out: Vec<String> = Vec::with_capacity(v.len());
    for s in v {
        if s.len() < MIN_LEN || out.last() == Some(&s) {
            dropped.push(s);
        } else {
            out.push(s);
        }
    }
    for mut s in dropped {
        zeroize::Zeroize::zeroize(&mut s);
    }
    out
}

/// Replace the registered set of secrets: (name, value).
pub fn set_secrets<'a, S: AsRef<str> + 'a>(secrets: impl IntoIterator<Item = (&'a str, S)>) {
    let mut pats: Vec<(Zeroizing<String>, String)> = secrets
        .into_iter()
        .filter(|(_, v)| v.as_ref().len() >= MIN_LEN)
        .flat_map(|(n, v)| variants(v.as_ref()).into_iter().map(move |p| (Zeroizing::new(p), mask(n))))
        .collect();
    // Longest first, so a value that contains another secret is masked as a whole.
    pats.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let sealed = SealedPatterns::seal(pats);
    *PATTERNS.write().unwrap_or_else(|e| e.into_inner()) = sealed;
}

pub fn active() -> bool {
    PATTERNS.read().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub fn scrub(text: &str) -> Cow<'_, str> {
    let g = PATTERNS.read().unwrap_or_else(|e| e.into_inner());
    let Some(sealed) = g.as_ref().filter(|_| text.len() >= MIN_LEN) else {
        return Cow::Borrowed(text);
    };
    let pats = sealed.open();
    let mut out: Cow<str> = Cow::Borrowed(text);
    for (p, m) in pats.iter() {
        let Ok(p) = std::str::from_utf8(p) else { continue };
        if out.contains(p) {
            let mut old = out.into_owned();
            out = Cow::Owned(old.replace(p, m));
            old.zeroize();
        }
    }
    out
}

pub fn scrub_string(s: String) -> String {
    match scrub(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => o,
    }
}

fn replace_bytes(hay: &[u8], pat: &[u8], with: &[u8]) -> Option<Vec<u8>> {
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    let mut out = Vec::with_capacity(hay.len());
    let (mut i, mut last, mut hit) = (0, 0, false);
    while i + pat.len() <= hay.len() {
        if &hay[i..i + pat.len()] == pat {
            out.extend_from_slice(&hay[last..i]);
            out.extend_from_slice(with);
            i += pat.len();
            last = i;
            hit = true;
        } else {
            i += 1;
        }
    }
    hit.then(|| {
        out.extend_from_slice(&hay[last..]);
        out
    })
}

/// Byte-level `scrub` (terminal output isn't guaranteed to be UTF-8 at chunk boundaries).
fn scrub_bytes_with(pats: &Opened, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (p, m) in pats.iter() {
        if let Some(r) = replace_bytes(&out, p, m.as_bytes()) {
            out.zeroize();
            out = r;
        }
    }
    out
}

/// Length of the longest suffix of `data` that is the start of a secret (in any encoding), i.e.
/// bytes that must not be shown yet because the rest of the secret may still be on its way.
fn pending_prefix(pats: &Opened, data: &[u8]) -> usize {
    let mut best = 0;
    for (p, _) in pats.iter() {
        for n in (best + 1..p.len().min(data.len() + 1)).rev() {
            if data[data.len() - n..] == p[..n] {
                best = n;
                break;
            }
        }
    }
    best
}

/// A secret whose first bytes were already shown (`shown` ends with them) and whose rest starts
/// `held`: mask the rest, so at most those first bytes leak.
fn mask_straddling(pats: &Opened, shown: &[u8], held: &[u8]) -> Option<Vec<u8>> {
    for (p, m) in pats.iter() {
        for k in (1..p.len()).rev() {
            if shown.ends_with(&p[..k]) && held.starts_with(&p[k..]) {
                let mut out = m.as_bytes().to_vec();
                out.extend_from_slice(&held[p.len() - k..]);
                return Some(out);
            }
        }
    }
    None
}

/// Scrubs a byte stream (e.g. an interactive terminal) where a secret can be split across reads.
/// Output is passed through immediately except for a trailing partial match, which is held until
/// more data shows whether it's a secret, or until the stream goes idle (`flush`).
#[derive(Default)]
pub struct StreamScrubber {
    held: Vec<u8>,
    /// Recently shown bytes, to catch a secret that's printed slowly, a character at a time.
    shown: Vec<u8>,
    /// Length of the partial match `held` ends (it may begin in `shown`).
    partial: usize,
}

impl StreamScrubber {
    /// Partial matches shorter than this are released after a brief pause (`flush(false)`), so
    /// typing isn't delayed every time a character happens to start some secret.
    pub const SHORT: usize = 3;

    /// Feed bytes; returns what is safe to show now.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        if !active() {
            let mut out = std::mem::take(&mut self.held);
            out.extend_from_slice(data);
            self.shown.clear();
            return out;
        }
        self.held.extend_from_slice(data);
        let g = PATTERNS.read().unwrap_or_else(|e| e.into_inner());
        let Some(sealed) = g.as_ref() else {
            self.shown.clear();
            return std::mem::take(&mut self.held);
        };
        let pats = sealed.open();
        let mut clean = scrub_bytes_with(&pats, &self.held);
        if let Some(c) = mask_straddling(&pats, &self.shown, &clean) {
            clean = c;
        }
        let mut window = self.shown.clone();
        window.extend_from_slice(&clean);
        self.partial = pending_prefix(&pats, &window);
        let keep = self.partial.min(clean.len());
        self.held = clean.split_off(clean.len() - keep);
        let max = sealed.max_len();
        drop(pats);
        drop(g);
        self.remember(&clean, max);
        clean
    }

    fn remember(&mut self, out: &[u8], max: usize) {
        self.shown.extend_from_slice(out);
        let cut = self.shown.len().saturating_sub(max);
        self.shown.drain(..cut);
    }

    /// The stream paused. `all = false` releases only short partial matches; `all = true` releases
    /// everything (a long pause: the rest of a secret isn't coming).
    pub fn flush(&mut self, all: bool) -> Vec<u8> {
        if !(all || self.partial < Self::SHORT) {
            return Vec::new();
        }
        let out = std::mem::take(&mut self.held);
        self.partial = 0;
        let max = PATTERNS.read().map(|p| p.as_ref().map_or(0, SealedPatterns::max_len)).unwrap_or(0);
        self.remember(&out, max);
        out
    }

    pub fn has_held(&self) -> bool {
        !self.held.is_empty()
    }
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(base64(b"hunter2"), "aHVudGVyMg==");
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        assert_eq!(percent("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn scrubs_all_forms() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_secrets([("TOKEN", "s3cr3t/value"), ("SHORT", "ab")]);
        let line = "raw=s3cr3t/value b64=czNjcjN0L3ZhbHVl url=s3cr3t%2Fvalue hex=7333637233742f76616c7565 ab";
        let out = scrub(line);
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(!out.contains("czNjcjN0L3ZhbHVl"));
        assert_eq!(out.matches("••••[TOKEN]").count(), 4);
        assert!(out.ends_with(" ab"), "short values are not scrubbed");
        assert!(matches!(scrub("nothing here"), Cow::Borrowed(_)));
        set_secrets::<&str>([]);
        assert!(!active());
    }

    #[test]
    fn stream_split_across_reads() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_secrets([("TOKEN", "s3cr3t-value")]);
        let mut sc = StreamScrubber::default();
        let mut out = Vec::new();
        for chunk in [&b"TOKEN=s3c"[..], b"r3t-va", b"lue\r\n$ "] {
            out.extend(sc.push(chunk));
        }
        out.extend(sc.flush(true));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "TOKEN=••••[TOKEN]\r\n$ ");
        // Ordinary output isn't held back; a partial secret is, until the stream goes idle.
        assert_eq!(sc.push(b"hello "), b"hello ");
        assert_eq!(sc.push(b"s3cr3"), b"");
        assert!(sc.has_held());
        assert_eq!(sc.flush(false), b"");
        assert_eq!(sc.flush(true), b"s3cr3");
        // A 1-2 byte partial match is released on a short pause (keeps typing responsive).
        assert_eq!(sc.push(b"ls s"), b"ls ");
        assert_eq!(sc.flush(false), b"s");
        // Printed a character at a time with pauses: at most the first SHORT-1 bytes show.
        let mut sc = StreamScrubber::default();
        let mut out = Vec::new();
        for c in b"s3cr3t-value\n" {
            out.extend(sc.push(&[*c]));
            out.extend(sc.flush(false));
        }
        out.extend(sc.flush(true));
        assert_eq!(String::from_utf8(out).unwrap(), "s3••••[TOKEN]\n");
        set_secrets::<&str>([]);
    }
}

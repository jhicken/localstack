//! Redacts secret values from any text before it is stored or shown (logs, errors, API output).
//! The patterns are registered by the secret store; every encoding a value commonly leaks in
//! (raw, base64, base64url, percent-encoded, hex) is matched.
use std::borrow::Cow;
use std::sync::RwLock;

/// Values shorter than this are not scrubbed (too likely to match ordinary text).
pub const MIN_LEN: usize = 4;

static PATTERNS: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

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
pub fn set_secrets<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) {
    let mut pats: Vec<(String, String)> = secrets
        .into_iter()
        .filter(|(_, v)| v.len() >= MIN_LEN)
        .flat_map(|(n, v)| variants(v).into_iter().map(move |p| (p, mask(n))))
        .collect();
    // Longest first, so a value that contains another secret is masked as a whole.
    pats.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut old = std::mem::replace(&mut *PATTERNS.write().unwrap_or_else(|e| e.into_inner()), pats);
    for (p, _) in old.iter_mut() {
        zeroize::Zeroize::zeroize(p);
    }
}

pub fn active() -> bool {
    !PATTERNS.read().unwrap_or_else(|e| e.into_inner()).is_empty()
}

pub fn scrub(text: &str) -> Cow<'_, str> {
    let pats = PATTERNS.read().unwrap_or_else(|e| e.into_inner());
    if pats.is_empty() || text.len() < MIN_LEN {
        return Cow::Borrowed(text);
    }
    let mut out: Cow<str> = Cow::Borrowed(text);
    for (p, m) in pats.iter() {
        if out.contains(p.as_str()) {
            out = Cow::Owned(out.replace(p.as_str(), m));
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
fn scrub_bytes_with(pats: &[(String, String)], data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (p, m) in pats {
        if let Some(r) = replace_bytes(&out, p.as_bytes(), m.as_bytes()) {
            out = r;
        }
    }
    out
}

/// Length of the longest suffix of `data` that is the start of a secret (in any encoding), i.e.
/// bytes that must not be shown yet because the rest of the secret may still be on its way.
fn pending_prefix(pats: &[(String, String)], data: &[u8]) -> usize {
    let mut best = 0;
    for (p, _) in pats {
        let p = p.as_bytes();
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
fn mask_straddling(pats: &[(String, String)], shown: &[u8], held: &[u8]) -> Option<Vec<u8>> {
    for (p, m) in pats {
        let p = p.as_bytes();
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
        let pats = PATTERNS.read().unwrap_or_else(|e| e.into_inner());
        let mut clean = scrub_bytes_with(&pats, &self.held);
        if let Some(c) = mask_straddling(&pats, &self.shown, &clean) {
            clean = c;
        }
        let mut window = self.shown.clone();
        window.extend_from_slice(&clean);
        self.partial = pending_prefix(&pats, &window);
        let keep = self.partial.min(clean.len());
        self.held = clean.split_off(clean.len() - keep);
        let max = pats.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
        drop(pats);
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
        let max = PATTERNS.read().map(|p| p.iter().map(|(p, _)| p.len()).max().unwrap_or(0)).unwrap_or(0);
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
        set_secrets([]);
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
        set_secrets([]);
    }
}

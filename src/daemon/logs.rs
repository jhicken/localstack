use crate::paths;
use crate::types::LogLine;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::broadcast;

const MAX_MEMORY_LINES: usize = 5_000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Most lines one page of older logs can return.
pub const MAX_PAGE: usize = 2_000;
const READ_CHUNK: u64 = 64 * 1024;

/// In-memory ring buffer + rotating file for one service's logs. Every line carries a cursor
/// (`pos`) into the file so clients can page backwards through history without loading it all.
pub struct LogStore {
    pub file: PathBuf,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<LogLine>,
}

struct Inner {
    lines: VecDeque<LogLine>,
    fd: Option<File>,
    size: u64,
    /// Bumped whenever the log file starts over (rotation, clear), so older cursors can't point into the wrong file.
    gen: u64,
    /// The generation the rotated `.1` file holds, while it directly precedes the current file.
    rotated: Option<u64>,
}

fn tag(stream: &str) -> &'static str {
    match stream {
        "stderr" => "err",
        "system" => "sys",
        _ => "out",
    }
}

fn rotated_path(file: &Path) -> PathBuf {
    let mut p = file.to_path_buf().into_os_string();
    p.push(".1");
    PathBuf::from(p)
}

fn encode_pos(gen: u64, off: u64) -> String {
    format!("{gen}:{off}")
}

fn parse_pos(s: &str) -> Option<(u64, u64)> {
    let (g, o) = s.split_once(':')?;
    Some((g.parse().ok()?, o.parse().ok()?))
}

impl LogStore {
    pub fn new(service: &str) -> Self {
        Self::at(paths::logs_dir().join(format!("{service}.log")))
    }

    fn at(file: PathBuf) -> Self {
        let lines = read_before(&file, u64::MAX, 500, 1).0.into();
        let rotated = rotated_path(&file).exists().then_some(0);
        let (tx, _) = broadcast::channel(1024);
        LogStore { file, inner: Mutex::new(Inner { lines, fd: None, size: 0, gen: 1, rotated }), tx }
    }

    pub fn push(&self, stream: &str, line: String) {
        // Secrets never reach the ring buffer, the log file or any stream.
        let line = crate::scrub::scrub_string(line);
        let mut l = LogLine { ts: paths::now_ms(), stream: stream.to_string(), line, pos: None };
        let mut g = self.inner.lock().unwrap();
        l.pos = Self::write_file(&self.file, &mut g, &l).ok().map(|(gen, off)| encode_pos(gen, off));
        g.lines.push_back(l.clone());
        while g.lines.len() > MAX_MEMORY_LINES {
            g.lines.pop_front();
        }
        drop(g);
        let _ = self.tx.send(l);
    }

    /// Appends the line; returns the (generation, offset) it was written at.
    fn write_file(file: &PathBuf, g: &mut Inner, l: &LogLine) -> std::io::Result<(u64, u64)> {
        if g.fd.is_none() {
            let f = OpenOptions::new().create(true).append(true).open(file)?;
            g.size = f.metadata()?.len();
            g.fd = Some(f);
        }
        let at = (g.gen, g.size);
        let rec = format!("{} {} {}\n", paths::iso(l.ts), tag(&l.stream), l.line);
        g.fd.as_mut().unwrap().write_all(rec.as_bytes())?;
        g.size += rec.len() as u64;
        if g.size > MAX_FILE_BYTES {
            g.fd = None;
            std::fs::rename(file, rotated_path(file))?;
            g.rotated = Some(g.gen);
            g.gen += 1;
        }
        Ok(at)
    }

    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let g = self.inner.lock().unwrap();
        let skip = g.lines.len().saturating_sub(n);
        g.lines.iter().skip(skip).cloned().collect()
    }

    /// Up to `n` lines written just before cursor `before` (oldest first), read from the log
    /// file (and the rotated one before it). Empty once there's nothing older.
    pub fn older(&self, before: &str, n: usize) -> Vec<LogLine> {
        let Some((mut gen, mut off)) = parse_pos(before) else { return vec![] };
        let (cur, rotated) = {
            let g = self.inner.lock().unwrap();
            (g.gen, g.rotated)
        };
        let n = n.clamp(1, MAX_PAGE);
        let mut out: Vec<LogLine> = vec![];
        loop {
            let path = if gen == cur {
                self.file.clone()
            } else if Some(gen) == rotated && gen + 1 == cur {
                rotated_path(&self.file)
            } else {
                break;
            };
            let (mut page, at_start) = read_before(&path, off, n - out.len(), gen);
            page.append(&mut out);
            out = page;
            if out.len() >= n || !at_start || gen != cur || rotated != Some(cur.wrapping_sub(1)) {
                break;
            }
            gen -= 1;
            off = u64::MAX;
        }
        out
    }

    pub fn last_ts(&self) -> Option<i64> {
        self.inner.lock().unwrap().lines.back().map(|l| l.ts).filter(|t| *t > 0)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.lines.clear();
        g.fd = None;
        g.gen += 1;
        g.rotated = None;
        let _ = OpenOptions::new().write(true).truncate(true).open(&self.file);
    }

    /// Re-scrub what's already stored (after a secret was added or changed).
    pub fn rescrub(&self) {
        if !crate::scrub::active() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        for l in g.lines.iter_mut() {
            if let std::borrow::Cow::Owned(s) = crate::scrub::scrub(&l.line) {
                l.line = s;
            }
        }
        g.fd = None;
        for f in [self.file.clone(), rotated_path(&self.file)] {
            rescrub_file(&f);
        }
    }

    pub fn close(&self) {
        self.inner.lock().unwrap().fd = None;
    }
}

/// Rewrite a log file with secrets masked (only if something changed).
pub fn rescrub_file(f: &PathBuf) {
    let Ok(bytes) = std::fs::read(f) else { return };
    let text = String::from_utf8_lossy(&bytes);
    if let std::borrow::Cow::Owned(clean) = crate::scrub::scrub(&text) {
        let tmp = f.with_extension("scrub.tmp");
        if std::fs::write(&tmp, clean).is_ok() {
            let _ = std::fs::rename(tmp, f);
        }
    }
}

/// The last `n` complete lines that end at or before byte `end` of `file`, reading backwards in
/// chunks (never the whole file). Returns the lines and whether the start of the file was reached.
fn read_before(file: &PathBuf, end: u64, n: usize, gen: u64) -> (Vec<LogLine>, bool) {
    let Ok(mut f) = File::open(file) else { return (vec![], true) };
    let end = end.min(f.metadata().map(|m| m.len()).unwrap_or(0));
    let mut start = end;
    let mut buf: Vec<u8> = vec![];
    while start > 0 {
        let from = start.saturating_sub(READ_CHUNK);
        let mut chunk = vec![0u8; (start - from) as usize];
        if f.seek(SeekFrom::Start(from)).is_err() || f.read_exact(&mut chunk).is_err() {
            break;
        }
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = from;
        if buf.iter().filter(|b| **b == b'\n').count() > n {
            break;
        }
    }
    // (offset, text) of each newline-terminated row; a trailing fragment (cursor mid-line) is skipped.
    let mut rows: Vec<(usize, &[u8])> = vec![];
    let mut s = 0;
    for (i, b) in buf.iter().enumerate() {
        if *b == b'\n' {
            rows.push((s, &buf[s..i]));
            s = i + 1;
        }
    }
    let at_start = start == 0;
    if !at_start && !rows.is_empty() {
        rows.remove(0); // may start mid-line
    }
    rows.retain(|(_, r)| !r.is_empty());
    let skip = rows.len().saturating_sub(n);
    let lines = rows[skip..]
        .iter()
        .map(|(o, r)| {
            let mut l = parse_line(&String::from_utf8_lossy(r));
            l.pos = Some(encode_pos(gen, start + *o as u64));
            l
        })
        .collect();
    (lines, at_start && skip == 0)
}

pub fn parse_line(r: &str) -> LogLine {
    let mut it = r.splitn(3, ' ');
    if let (Some(ts), Some(t), rest) = (it.next(), it.next(), it.next()) {
        let stream = match t {
            "out" => Some("stdout"),
            "err" => Some("stderr"),
            "sys" => Some("system"),
            _ => None,
        };
        if let (Some(ts), Some(stream)) = (paths::parse_iso(ts), stream) {
            return LogLine { ts, stream: stream.into(), line: rest.unwrap_or("").into(), pos: None };
        }
    }
    LogLine { ts: 0, stream: "stdout".into(), line: r.into(), pos: None }
}

/// Splits a byte stream into lines (handles \r\n, caps runaway partial lines at 16KB).
#[derive(Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    pub fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(String)) {
        for &b in chunk {
            if b == b'\n' {
                let mut l = std::mem::take(&mut self.partial);
                if l.last() == Some(&b'\r') {
                    l.pop();
                }
                emit(String::from_utf8_lossy(&l).into_owned());
            } else {
                self.partial.push(b);
                if self.partial.len() > 16_384 {
                    emit(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned());
                }
            }
        }
    }
    pub fn flush(&mut self, mut emit: impl FnMut(String)) {
        if !self.partial.is_empty() {
            emit(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splitter() {
        let mut s = LineSplitter::default();
        let mut got = vec![];
        s.feed(b"a\r\nb", |l| got.push(l));
        s.feed(b"c\n\nd", |l| got.push(l));
        s.flush(|l| got.push(l));
        assert_eq!(got, vec!["a", "bc", "", "d"]);
    }
    #[test]
    fn parse() {
        let l = parse_line("2024-01-02T03:04:05.678Z err boom bang");
        assert_eq!(l.stream, "stderr");
        assert_eq!(l.line, "boom bang");
        assert_eq!(paths::iso(l.ts), "2024-01-02T03:04:05.678Z");
        assert_eq!(parse_line("garbage").ts, 0);
    }

    fn page_all(s: &LogStore, first: &[LogLine], n: usize) -> Vec<String> {
        let mut all: Vec<String> = first.iter().map(|l| l.line.clone()).collect();
        let mut before = first.first().and_then(|l| l.pos.clone());
        while let Some(b) = before {
            let page = s.older(&b, n);
            assert!(page.len() <= n);
            before = page.first().and_then(|l| l.pos.clone());
            all.splice(0..0, page.into_iter().map(|l| l.line));
        }
        all
    }

    #[test]
    fn pages_backwards_through_file() {
        let dir = std::env::temp_dir().join(format!("lstack-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("svc.log");
        let s = LogStore::at(file.clone());
        for i in 0..1000 {
            s.push("stdout", format!("line {i} {}", "x".repeat(i % 200)));
        }
        let tail = s.tail(10);
        assert_eq!(tail.last().unwrap().line, format!("line 999 {}", "x".repeat(999 % 200)));
        let all = page_all(&s, &tail, 77);
        assert_eq!(all.len(), 1000);
        assert!(all.iter().enumerate().all(|(i, l)| l.starts_with(&format!("line {i} "))));

        // A restarted store resumes cursors from the file.
        let s2 = LogStore::at(file.clone());
        let t2 = s2.tail(5);
        assert_eq!(t2.len(), 5);
        assert_eq!(page_all(&s2, &t2, 300).len(), 1000);

        // Bad / stale cursors return nothing; clearing invalidates old cursors.
        assert!(s.older("nope", 10).is_empty());
        let old = tail[0].pos.clone().unwrap();
        s.clear();
        assert!(s.older(&old, 10).is_empty());
        s.push("stderr", "fresh".into());
        let t = s.tail(10);
        assert_eq!(t.len(), 1);
        assert!(s.older(t[0].pos.as_ref().unwrap(), 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pages_into_rotated_file() {
        let dir = std::env::temp_dir().join(format!("lstack-logrot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("svc.log");
        let s = LogStore::at(file.clone());
        let big = "y".repeat(4000);
        let total = (MAX_FILE_BYTES / 4000) as usize + 300;
        for i in 0..total {
            s.push("stdout", format!("{i} {big}"));
        }
        assert!(rotated_path(&file).exists());
        let tail = s.tail(3);
        let all = page_all(&s, &tail, 500);
        assert_eq!(all.len(), total);
        assert!(all.iter().enumerate().all(|(i, l)| l.starts_with(&format!("{i} "))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

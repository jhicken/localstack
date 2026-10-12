use crate::paths;
use crate::types::LogLine;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::broadcast;

const MAX_MEMORY_LINES: usize = 5_000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// In-memory ring buffer + rotating file for one service's logs.
pub struct LogStore {
    pub file: PathBuf,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<LogLine>,
}

struct Inner {
    lines: VecDeque<LogLine>,
    fd: Option<File>,
    size: u64,
}

fn tag(stream: &str) -> &'static str {
    match stream {
        "stderr" => "err",
        "system" => "sys",
        _ => "out",
    }
}

impl LogStore {
    pub fn new(service: &str) -> Self {
        let file = paths::logs_dir().join(format!("{service}.log"));
        let lines = read_tail(&file, 500);
        let (tx, _) = broadcast::channel(1024);
        LogStore { file, inner: Mutex::new(Inner { lines, fd: None, size: 0 }), tx }
    }

    pub fn push(&self, stream: &str, line: String) {
        let l = LogLine { ts: paths::now_ms(), stream: stream.to_string(), line };
        let mut g = self.inner.lock().unwrap();
        g.lines.push_back(l.clone());
        while g.lines.len() > MAX_MEMORY_LINES {
            g.lines.pop_front();
        }
        let _ = Self::write_file(&self.file, &mut g, &l);
        drop(g);
        let _ = self.tx.send(l);
    }

    fn write_file(file: &PathBuf, g: &mut Inner, l: &LogLine) -> std::io::Result<()> {
        if g.fd.is_none() {
            let f = OpenOptions::new().create(true).append(true).open(file)?;
            g.size = f.metadata()?.len();
            g.fd = Some(f);
        }
        let rec = format!("{} {} {}\n", paths::iso(l.ts), tag(&l.stream), l.line);
        g.fd.as_mut().unwrap().write_all(rec.as_bytes())?;
        g.size += rec.len() as u64;
        if g.size > MAX_FILE_BYTES {
            g.fd = None;
            let mut rotated = file.clone().into_os_string();
            rotated.push(".1");
            std::fs::rename(file, rotated)?;
        }
        Ok(())
    }

    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let g = self.inner.lock().unwrap();
        let skip = g.lines.len().saturating_sub(n);
        g.lines.iter().skip(skip).cloned().collect()
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
        let _ = OpenOptions::new().write(true).truncate(true).open(&self.file);
    }

    pub fn close(&self) {
        self.inner.lock().unwrap().fd = None;
    }
}

fn read_tail(file: &PathBuf, n: usize) -> VecDeque<LogLine> {
    let mut out = VecDeque::new();
    let Ok(mut f) = File::open(file) else { return out };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let len = size.min(256 * 1024);
    if f.seek(SeekFrom::Start(size - len)).is_err() {
        return out;
    }
    let mut buf = Vec::with_capacity(len as usize);
    let _ = f.take(len).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut rows: Vec<&str> = text.split('\n').filter(|s| !s.is_empty()).collect();
    if len < size && !rows.is_empty() {
        rows.remove(0);
    }
    let start = rows.len().saturating_sub(n);
    for r in &rows[start..] {
        out.push_back(parse_line(r));
    }
    out
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
            return LogLine { ts, stream: stream.into(), line: rest.unwrap_or("").into() };
        }
    }
    LogLine { ts: 0, stream: "stdout".into(), line: r.into() }
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
}

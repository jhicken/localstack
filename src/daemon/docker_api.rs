//! Minimal Docker Engine API client over the unix socket. Replaces `docker inspect/stats/logs -f`
//! subprocesses (each docker CLI process costs ~20-40MB of RAM).
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn docker_bin() -> String {
    std::env::var("LSTACK_DOCKER").unwrap_or_else(|_| "docker".into())
}

fn context_socket() -> Option<PathBuf> {
    static CTX: OnceLock<Option<PathBuf>> = OnceLock::new();
    CTX.get_or_init(|| {
        let out = std::process::Command::new(docker_bin())
            .args(["context", "inspect", "--format", "{{.Endpoints.docker.Host}}"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        s.strip_prefix("unix://").map(PathBuf::from)
    })
    .clone()
}

pub fn socket_path() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        return h.strip_prefix("unix://").map(PathBuf::from);
    }
    let home = crate::paths::user_home();
    let mut candidates = vec![home.join(".docker/run/docker.sock"), PathBuf::from("/var/run/docker.sock")];
    if let Some(c) = context_socket() {
        candidates.insert(0, c);
    }
    candidates.into_iter().find(|p| p.exists())
}

pub struct Body {
    r: BufReader<UnixStream>,
    chunked: bool,
    remaining: Option<usize>, // content-length or current chunk
    done: bool,
}

impl Body {
    /// Next piece of the body, or None at the end.
    pub async fn next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        if self.chunked {
            let mut line = String::new();
            if self.r.read_line(&mut line).await? == 0 {
                self.done = true;
                return Ok(None);
            }
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if size == 0 {
                self.done = true;
                return Ok(None);
            }
            let mut buf = vec![0u8; size];
            self.r.read_exact(&mut buf).await?;
            let mut crlf = [0u8; 2];
            self.r.read_exact(&mut crlf).await?;
            return Ok(Some(buf));
        }
        let want = self.remaining.unwrap_or(16_384).min(16_384);
        if want == 0 {
            self.done = true;
            return Ok(None);
        }
        let mut buf = vec![0u8; want];
        let n = self.r.read(&mut buf).await?;
        if n == 0 {
            self.done = true;
            return Ok(None);
        }
        buf.truncate(n);
        if let Some(r) = self.remaining.as_mut() {
            *r -= n;
        }
        Ok(Some(buf))
    }

    pub async fn all(mut self) -> std::io::Result<Vec<u8>> {
        let mut out = vec![];
        while let Some(c) = self.next().await? {
            out.extend(c);
        }
        Ok(out)
    }
}

/// Send a request; returns (status, body reader).
pub async fn request(method: &str, path: &str, body: Option<&Value>) -> std::io::Result<(u16, Body)> {
    let sock = socket_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "docker socket not found"))?;
    let mut s = UnixStream::connect(sock).await?;
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: lstack\r\nConnection: close\r\n");
    if body.is_some() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", payload.len()));
    } else if method == "POST" {
        req.push_str("Content-Length: 0\r\n");
    }
    req.push_str("\r\n");
    req.push_str(&payload);
    s.write_all(req.as_bytes()).await?;
    let mut r = BufReader::new(s);
    let mut status_line = String::new();
    r.read_line(&mut status_line).await?;
    let status = status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let (mut chunked, mut len) = (false, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        } else if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().ok();
        }
    }
    Ok((status, Body { r, chunked, remaining: if chunked { None } else { len }, done: false }))
}

/// Simple JSON call with a timeout. Err holds a readable message.
pub async fn call(method: &str, path: &str, body: Option<&Value>, secs: u64) -> Result<(u16, Value), String> {
    let fut = async {
        let (status, b) = request(method, path, body).await.map_err(|e| e.to_string())?;
        let bytes = b.all().await.map_err(|e| e.to_string())?;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        Ok((status, v))
    };
    tokio::time::timeout(Duration::from_secs(secs), fut).await.map_err(|_| "docker API timed out".to_string())?
}

pub fn api_error(status: u16, v: &Value) -> String {
    v.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| format!("docker API returned {status}"))
}

pub async fn ping() -> bool {
    matches!(call("GET", "/_ping", None, 5).await, Ok((200, _)))
}

/// Wait for the Docker engine (Docker Desktop can take a while after login).
pub async fn wait_for_docker(log: impl Fn(&str), cancelled: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let mut delay = 2.0f64;
    let mut announced = false;
    while std::time::Instant::now() < deadline && !cancelled() {
        if ping().await {
            return true;
        }
        if !announced {
            log("waiting for Docker engine to become available…");
            announced = true;
        }
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        delay = (delay * 1.5).min(15.0);
    }
    false
}

pub fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub async fn inspect(name: &str) -> Result<Option<Value>, String> {
    match call("GET", &format!("/containers/{}/json", enc(name)), None, 10).await? {
        (200, v) => Ok(Some(v)),
        (404, _) => Ok(None),
        (s, v) => Err(api_error(s, &v)),
    }
}

/// All containers that belong to some compose project.
pub async fn compose_containers() -> Result<Vec<Value>, String> {
    let filters = enc(r#"{"label":["com.docker.compose.project"]}"#);
    match call("GET", &format!("/containers/json?all=1&filters={filters}"), None, 15).await? {
        (200, Value::Array(a)) => Ok(a),
        (s, v) => Err(api_error(s, &v)),
    }
}

pub async fn stats(id: &str) -> Option<Value> {
    match call("GET", &format!("/containers/{}/stats?stream=false&one-shot=true", enc(id)), None, 20).await {
        Ok((200, v)) => Some(v),
        _ => None,
    }
}

/// Demultiplexes docker's 8-byte-header log frames.
#[derive(Default)]
pub struct Demux {
    buf: Vec<u8>,
}

impl Demux {
    /// Feed raw bytes; calls `emit(is_stderr, payload)` per complete frame.
    pub fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(bool, &[u8])) {
        self.buf.extend_from_slice(chunk);
        let mut off = 0;
        while self.buf.len() - off >= 8 {
            let h = &self.buf[off..off + 8];
            let size = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
            if self.buf.len() - off < 8 + size {
                break;
            }
            let is_err = h[0] == 2;
            emit(is_err, &self.buf[off + 8..off + 8 + size]);
            off += 8 + size;
        }
        self.buf.drain(..off);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn demux() {
        let mut d = Demux::default();
        let mut frame = vec![1, 0, 0, 0, 0, 0, 0, 3];
        frame.extend(b"hi\n");
        frame.extend([2, 0, 0, 0, 0, 0, 0, 4]);
        frame.extend(b"err\n");
        let mut got = vec![];
        d.feed(&frame[..5], |e, p| got.push((e, p.to_vec())));
        assert!(got.is_empty());
        d.feed(&frame[5..], |e, p| got.push((e, p.to_vec())));
        assert_eq!(got, vec![(false, b"hi\n".to_vec()), (true, b"err\n".to_vec())]);
    }
    #[test]
    fn encoding() {
        assert_eq!(enc("a b/{}"), "a%20b%2F%7B%7D");
    }
}

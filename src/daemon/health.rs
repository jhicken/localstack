use crate::types::HealthCheck;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub async fn check(h: &HealthCheck) -> bool {
    let t = Duration::from_secs_f64(h.timeout.unwrap_or(5.0).max(0.1));
    if h.kind == "http" {
        let url = h.url.clone().unwrap_or_default();
        return match url.strip_prefix("http://") {
            Some(rest) => timeout(t, http_get(rest)).await.unwrap_or(false),
            None => curl(&url, t).await,
        };
    }
    let host = h.host.clone().unwrap_or_else(|| "127.0.0.1".into());
    let port = h.port.unwrap_or(0);
    matches!(timeout(t, TcpStream::connect((host.as_str(), port))).await, Ok(Ok(_)))
}

async fn http_get(rest: &str) -> bool {
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
    let Ok(mut s) = TcpStream::connect(addr).await else { return false };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {hostport}\r\nUser-Agent: lstack-health\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let mut n = 0;
    while n < 12 {
        match s.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    // "HTTP/1.1 200 OK"
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).is_some_and(|c| c < 400)
}

async fn curl(url: &str, t: Duration) -> bool {
    let out = tokio::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-k", "--max-time"])
        .arg(format!("{:.1}", t.as_secs_f64()))
        .arg(url)
        .output()
        .await;
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse::<u16>().is_ok_and(|c| c > 0 && c < 400),
        Err(_) => false,
    }
}

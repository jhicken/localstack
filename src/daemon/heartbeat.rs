//! healthchecks.io-compatible pings: per-service (success / `/start` / `/fail`) and a machine heartbeat.
//!
//! Ping URLs are either explicit (`https://hc-ping.com/<uuid>`) or built from the project ping key
//! (kept in the secret store as `LSTACK_HC_PING_KEY`) and a slug: `<base>/<key>/<slug>?create=1`.
//! Pings go through /usr/bin/curl with the URL on stdin, so keys never appear in argv.
use super::supervisor::{Event, Sup};
use super::terminal::{self, Healthchecks};
use crate::paths::now_ms;
use crate::types::{Heartbeat, HeartbeatStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PING_KEY_SECRET: &str = "LSTACK_HC_PING_KEY";
const MAX_BODY: usize = 10_000;

static MACHINE: Mutex<Option<HeartbeatStatus>> = Mutex::new(None);

/// (url without query, query) or None when the heartbeat can't be addressed (no url and no ping key).
pub fn resolve(hb: &Heartbeat, default_slug: &str, hc: &Healthchecks, ping_key: Option<&str>) -> Option<(String, String)> {
    if let Some(u) = hb.url.as_deref().filter(|u| !u.is_empty()) {
        return Some(match u.split_once('?') {
            Some((a, q)) => (a.trim_end_matches('/').to_string(), format!("?{q}")),
            None => (u.trim_end_matches('/').to_string(), String::new()),
        });
    }
    let key = ping_key?;
    let slug = hb.slug.as_deref().filter(|s| !s.is_empty()).unwrap_or(default_slug);
    let base = if hc.base_url.is_empty() { "https://hc-ping.com" } else { hc.base_url.trim_end_matches('/') };
    Some((format!("{base}/{key}/{slug}"), "?create=1".into()))
}

fn curl_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Blocking: send one ping. `kind` is "success", "start" or "fail".
pub fn ping(target: &(String, String), kind: &str, body: Option<&str>) -> Result<(), String> {
    let suffix = match kind {
        "start" => "/start",
        "fail" => "/fail",
        _ => "",
    };
    let mut cfg = format!("url = {}\n", curl_quote(&format!("{}{suffix}{}", target.0, target.1)));
    if let Some(b) = body {
        let b = crate::scrub::scrub(b);
        let b = if b.len() > MAX_BODY { &b[b.len() - MAX_BODY..] } else { &b[..] };
        cfg.push_str(&format!("data-binary = {}\n", curl_quote(b)));
    }
    let mut child = Command::new("/usr/bin/curl")
        .args(["-fsS", "-m", "10", "--retry", "2", "-o", "/dev/null", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child.stdin.take().map(|mut s| s.write_all(cfg.as_bytes()));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        // curl may echo the URL (with the ping key) in its error; scrubbing hides registered secrets.
        Err(crate::scrub::scrub_string(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

fn enabled(hb: &Heartbeat) -> bool {
    hb.enabled != Some(false)
}

fn interval_ms(hb: &Heartbeat) -> i64 {
    (hb.interval.unwrap_or(60.0).clamp(5.0, 86_400.0) * 1000.0) as i64
}

#[derive(Default, Clone)]
struct Seen {
    status: String,
    health: String,
    last_success: i64,
}

/// Subscribe now (so boot-time starts aren't missed) and run the ping loop in the background.
pub fn start(sup: &Arc<Sup>) -> tokio::task::JoinHandle<()> {
    let rx = sup.events.subscribe();
    tokio::spawn(run(sup.clone(), rx))
}

/// React to service transitions and send periodic success pings.
async fn run(sup: Arc<Sup>, mut rx: tokio::sync::broadcast::Receiver<Event>) {
    let mut seen: HashMap<String, Seen> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut machine_last = 0i64;
    for s in sup.list() {
        seen.insert(s.name.clone(), Seen { status: s.status.clone(), health: s.health.clone(), last_success: 0 });
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Event::Service(s)) => {
                    let prev = seen.entry(s.name.clone()).or_default().clone();
                    let Some(hb) = s.config.heartbeat.clone().filter(enabled) else {
                        seen.insert(s.name.clone(), Seen { status: s.status.clone(), health: s.health.clone(), ..prev });
                        continue;
                    };
                    let mut kind = None;
                    if s.status != prev.status {
                        if s.status == "starting" || (s.status == "running" && !matches!(prev.status.as_str(), "starting" | "running")) {
                            if hb.start != Some(false) { kind = Some("start"); }
                        } else if matches!(s.status.as_str(), "crashed" | "errored") && hb.fail != Some(false) {
                            kind = Some("fail");
                        }
                    } else if s.status == "running" && s.health == "unhealthy" && prev.health != "unhealthy" && hb.fail != Some(false) {
                        kind = Some("fail");
                    }
                    let mut next = Seen { status: s.status.clone(), health: s.health.clone(), last_success: prev.last_success };
                    if s.status == "running" && s.health == "healthy" && prev.health == "unhealthy" {
                        next.last_success = 0; // recovered → report success right away
                    }
                    seen.insert(s.name.clone(), next);
                    if let Some(k) = kind {
                        send(&sup, &s.name, &hb, k);
                    }
                }
                Ok(Event::Removed(n)) => { seen.remove(&n); }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
            _ = tick.tick() => {
                let now = now_ms();
                for s in sup.list() {
                    let Some(hb) = s.config.heartbeat.clone().filter(enabled) else { continue };
                    let e = seen.entry(s.name.clone()).or_default();
                    if s.status == "running" && matches!(s.health.as_str(), "healthy" | "none") && now - e.last_success >= interval_ms(&hb) {
                        e.last_success = now;
                        send(&sup, &s.name, &hb, "success");
                    }
                }
                let hc = terminal::load().healthchecks;
                if let Some(m) = hc.machine.clone().filter(enabled) {
                    if now - machine_last >= interval_ms(&m) {
                        machine_last = now;
                        let key = super::secrets::internal_get(PING_KEY_SECRET);
                        let slug = format!("{}-lstack", super::remote::hostname().to_lowercase().replace(['.', ' '], "-"));
                        if let Some(t) = resolve(&m, &slug, &hc, key.as_deref()) {
                            tokio::task::spawn_blocking(move || {
                                let r = ping(&t, "success", None);
                                *MACHINE.lock().unwrap() = Some(status_of("success", r));
                            });
                        }
                    }
                }
            }
        }
    }
}

fn status_of(kind: &str, r: Result<(), String>) -> HeartbeatStatus {
    HeartbeatStatus { last_ping: now_ms(), kind: kind.into(), ok: r.is_ok(), error: r.err() }
}

fn send(sup: &Arc<Sup>, name: &str, hb: &Heartbeat, kind: &'static str) {
    let hc = terminal::load().healthchecks;
    let key = super::secrets::internal_get(PING_KEY_SECRET);
    let Some(target) = resolve(hb, name, &hc, key.as_deref()) else {
        let msg = format!("heartbeat has no url and no ping key ({PING_KEY_SECRET}) is set");
        sup.with(name, None, |m| {
            let st = status_of(kind, Err(msg));
            if m.heartbeat.as_ref().map(|h| &h.error) != Some(&st.error) {
                m.heartbeat = Some(st);
                sup.changed(m);
            }
        });
        return;
    };
    let body = (kind == "fail" && hb.logs != Some(false))
        .then(|| sup.logs(name).ok())
        .flatten()
        .map(|l| l.tail(hc.log_lines.max(1)).into_iter().map(|x| x.line).collect::<Vec<_>>().join("\n"));
    let (sup, name) = (sup.clone(), name.to_string());
    tokio::task::spawn_blocking(move || {
        let r = ping(&target, kind, body.as_deref());
        sup.with(&name, None, |m| {
            if let Err(e) = &r {
                m.system(format!("[lstack] heartbeat {kind} ping failed: {e}"));
            }
            m.heartbeat = Some(status_of(kind, r));
            sup.changed(m);
        });
    });
}

/// Settings view (never includes the ping key itself).
pub fn settings_status(hc: &Healthchecks) -> Value {
    json!({
        "baseUrl": hc.base_url,
        "logLines": hc.log_lines,
        "machine": hc.machine,
        "hasPingKey": super::secrets::internal_get(PING_KEY_SECRET).is_some(),
        "machineStatus": *MACHINE.lock().unwrap(),
    })
}

/// Fire a test ping for a service (or the machine when `name` is None). Blocking.
pub fn test(sup: &Arc<Sup>, name: Option<&str>) -> Result<Value, String> {
    let hc = terminal::load().healthchecks;
    let key = super::secrets::internal_get(PING_KEY_SECRET);
    let (hb, slug) = match name {
        Some(n) => {
            let s = sup.get_state(n).map_err(|e| e.msg)?;
            (s.config.heartbeat.clone().ok_or(format!("{n} has no heartbeat configured"))?, n.to_string())
        }
        None => (
            hc.machine.clone().ok_or("no machine heartbeat configured")?,
            format!("{}-lstack", super::remote::hostname().to_lowercase().replace(['.', ' '], "-")),
        ),
    };
    let t = resolve(&hb, &slug, &hc, key.as_deref()).ok_or(format!("no url and no ping key ({PING_KEY_SECRET}) set"))?;
    let r = ping(&t, "success", None);
    let st = status_of("success", r.clone());
    match name {
        Some(n) => {
            sup.with(n, None, |m| {
                m.heartbeat = Some(st.clone());
                sup.changed(m);
            });
        }
        None => *MACHINE.lock().unwrap() = Some(st.clone()),
    }
    r.map(|_| json!(st))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let hc = Healthchecks::default();
        let hb = Heartbeat::default();
        assert_eq!(resolve(&hb, "api", &hc, None), None);
        assert_eq!(resolve(&hb, "api", &hc, Some("KEY")), Some(("https://hc-ping.com/KEY/api".into(), "?create=1".into())));
        let hb = Heartbeat { slug: Some("my-api".into()), ..Default::default() };
        let hc2 = Healthchecks { base_url: "https://hc.example/ping/".into(), ..Default::default() };
        assert_eq!(resolve(&hb, "api", &hc2, Some("K")).unwrap().0, "https://hc.example/ping/K/my-api");
        let hb = Heartbeat { url: Some("https://hc-ping.com/uuid-1?rid=x".into()), ..Default::default() };
        assert_eq!(resolve(&hb, "api", &hc, Some("K")), Some(("https://hc-ping.com/uuid-1".into(), "?rid=x".into())));
        assert_eq!(curl_quote("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn pings_local_server() {
        use std::io::{BufRead, BufReader, Read};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut len = 0;
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            (&s).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK").unwrap();
            (first, String::from_utf8(body).unwrap())
        });
        let t = (format!("http://127.0.0.1:{port}/abc"), "?create=1".to_string());
        ping(&t, "fail", Some("line 1\nline \"2\"")).unwrap();
        let (first, body) = h.join().unwrap();
        assert!(first.starts_with("POST /abc/fail?create=1 "), "{first}");
        assert_eq!(body, "line 1\nline \"2\"");
    }
}

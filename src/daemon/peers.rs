//! Other machines this daemon watches ("peers"). Each paired machine runs LStack with remote access
//! on; we hold a bearer token for its `/peer/api`, keep an event stream open, cache its services,
//! system load and busy processes, and re-broadcast everything as `Event::Peer` tagged with the
//! machine name. Logs, metrics and start/stop are proxied on demand.
use super::server::ApiErr;
use super::supervisor::{Event, Sup};
use crate::paths::{self, now_ms};
use crate::types::{ProcInfo, ServiceState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PeerCfg {
    pub name: String,
    /// Base URL of the machine's remote listener, e.g. http://studio.tailnet:7380
    pub url: String,
    pub token: String,
    pub added_at: i64,
}

/// What the rest of the daemon publishes for one machine.
#[derive(Debug, Clone)]
pub struct PeerMsg {
    pub machine: String,
    pub event: String,
    pub data: Value,
}

struct PeerState {
    cfg: PeerCfg,
    status: &'static str,
    error: Option<String>,
    info: Value,
    services: BTreeMap<String, ServiceState>,
    config_errors: Vec<String>,
    system: Value,
    processes: Vec<ProcInfo>,
    last_seen: Option<i64>,
    stop: Arc<AtomicBool>,
}

pub struct Peers {
    inner: Mutex<BTreeMap<String, PeerState>>,
    sup: Arc<Sup>,
}

fn file() -> std::path::PathBuf {
    paths::home().join("peers.json")
}

fn load() -> Vec<PeerCfg> {
    std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(list: &[PeerCfg]) -> std::io::Result<()> {
    let path = file();
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(list).unwrap_or_default().as_bytes())?;
    std::fs::rename(tmp, path)
}

/// Short name for this machine (hostname without `.local`).
pub fn local_name() -> String {
    let h = super::remote::hostname();
    h.strip_suffix(".local").unwrap_or(&h).to_string()
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn slug(n: &str) -> String {
    let s: String = n.strip_suffix(".local").unwrap_or(n).chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    if s.is_empty() { "machine".into() } else { s.chars().take(64).collect() }
}

/// `host`, `host:port`, `http://host[:port]` → `http://host:port` (default port 7380).
pub fn normalize_url(u: &str) -> Result<String, String> {
    let u = u.trim().trim_end_matches('/');
    if u.is_empty() {
        return Err("machine address required (e.g. studio.local or http://100.64.1.2:7380)".into());
    }
    let (scheme, rest) = match u.split_once("://") {
        Some((s, r)) if s == "http" || s == "https" => (s, r),
        Some((s, _)) => return Err(format!("unsupported scheme {s}://")),
        None => ("http", u),
    };
    if rest.contains('/') {
        return Err("give just the machine address, without a path".into());
    }
    let has_port = rest.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok() && !rest.ends_with(']'));
    Ok(if has_port { format!("{scheme}://{rest}") } else { format!("{scheme}://{rest}:{}", super::remote::DEFAULT_PORT) })
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let hp = rest.split('/').next().unwrap_or(rest);
    if hp.starts_with('[') {
        return hp.split(']').next().unwrap_or(hp).to_string() + "]";
    }
    hp.rsplit_once(':').map(|x| x.0).unwrap_or(hp).to_string()
}

/// Point a peer's localhost URLs at the peer's address.
pub fn rewrite_urls(s: &mut ServiceState, host: &str) {
    for u in &mut s.urls {
        for lh in ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"] {
            let from = format!("://{lh}");
            if u.url.contains(&from) {
                u.url = u.url.replacen(&from, &format!("://{host}"), 1);
                break;
            }
        }
    }
}

fn agent(read_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout_read(Duration::from_secs(read_secs)).build()
}

fn http_err(e: ureq::Error) -> (u16, String) {
    match e {
        ureq::Error::Status(c, r) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            (c, v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {c}")))
        }
        ureq::Error::Transport(t) => (0, t.to_string()),
    }
}

impl Peers {
    pub fn new(sup: Arc<Sup>) -> Arc<Self> {
        let p = Arc::new(Peers { inner: Mutex::new(BTreeMap::new()), sup });
        for cfg in load() {
            p.attach(cfg);
        }
        p
    }

    fn emit(&self, machine: &str, event: &str, data: Value) {
        let _ = self.sup.events.send(Event::Peer(Arc::new(PeerMsg { machine: machine.into(), event: event.into(), data })));
    }

    fn attach(self: &Arc<Self>, cfg: PeerCfg) {
        let stop = Arc::new(AtomicBool::new(false));
        let name = cfg.name.clone();
        self.inner.lock().unwrap().insert(
            name.clone(),
            PeerState {
                cfg,
                status: "connecting",
                error: None,
                info: Value::Null,
                services: BTreeMap::new(),
                config_errors: vec![],
                system: Value::Null,
                processes: vec![],
                last_seen: None,
                stop: stop.clone(),
            },
        );
        let me = self.clone();
        std::thread::Builder::new().name(format!("peer-{name}")).spawn(move || me.run(&name, &stop)).ok();
    }

    fn set_status(&self, name: &str, status: &'static str, error: Option<String>) {
        let summary = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return };
            if p.status == status && p.error == error {
                return;
            }
            p.status = status;
            p.error = error.map(crate::scrub::scrub_string);
            if status != "online" {
                p.services.values_mut().for_each(|s| s.metrics = None);
            }
            summary(p)
        };
        self.emit(name, "machine", summary);
    }

    fn cfg(&self, name: &str) -> Option<PeerCfg> {
        self.inner.lock().unwrap().get(name).map(|p| p.cfg.clone())
    }

    /// Connection loop for one peer (own thread; blocking I/O).
    fn run(self: Arc<Self>, name: &str, stop: &AtomicBool) {
        let mut backoff = 2u64;
        while !stop.load(Ordering::Relaxed) {
            let Some(cfg) = self.cfg(name) else { return };
            match self.session(name, &cfg, stop) {
                Ok(()) => backoff = 2,
                Err((401, e)) => {
                    self.set_status(name, "unauthorized", Some(format!("{e} — pair again with: lstack machine add {} --code …", cfg.url)));
                    backoff = 60;
                }
                Err((_, e)) => {
                    self.set_status(name, "offline", Some(e));
                    backoff = (backoff * 2).min(60);
                }
            }
            for _ in 0..backoff * 10 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn get_json(cfg: &PeerCfg, path: &str) -> Result<Value, (u16, String)> {
        agent(15)
            .get(&format!("{}{path}", cfg.url))
            .set("Authorization", &format!("Bearer {}", cfg.token))
            .call()
            .map_err(http_err)?
            .into_json()
            .map_err(|e| (0, e.to_string()))
    }

    fn session(&self, name: &str, cfg: &PeerCfg, stop: &AtomicBool) -> Result<(), (u16, String)> {
        let info = Self::get_json(cfg, "/peer/api/info")?;
        let system = Self::get_json(cfg, "/peer/api/system").unwrap_or(Value::Null);
        let procs: Vec<ProcInfo> = Self::get_json(cfg, "/peer/api/processes")
            .ok()
            .and_then(|v| serde_json::from_value(v["processes"].clone()).ok())
            .unwrap_or_default();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return Ok(()) };
            p.info = info;
            p.system = system;
            p.processes = procs;
        }
        let resp = agent(45)
            .get(&format!("{}/peer/api/events", cfg.url))
            .set("Authorization", &format!("Bearer {}", cfg.token))
            .call()
            .map_err(http_err)?;
        let host = host_of(&cfg.url);
        let mut rd = std::io::BufReader::new(resp.into_reader());
        let mut event = String::from("message");
        let mut line = String::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            line.clear();
            match rd.read_line(&mut line) {
                Ok(0) => return Err((0, "connection closed".into())),
                Ok(_) => {}
                Err(e) => return Err((0, e.to_string())),
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if let Some(e) = l.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = l.strip_prefix("data: ") {
                let Ok(data) = serde_json::from_str::<Value>(d) else { continue };
                self.ingest(name, &host, &event, data);
                event = "message".into();
            } else if l.starts_with(':') {
                if let Some(p) = self.inner.lock().unwrap().get_mut(name) {
                    p.last_seen = Some(now_ms());
                }
            }
        }
    }

    fn ingest(&self, name: &str, host: &str, event: &str, data: Value) {
        let tag = |mut s: ServiceState| {
            s.machine = Some(name.to_string());
            rewrite_urls(&mut s, host);
            s
        };
        let out = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return };
            p.last_seen = Some(now_ms());
            let became_online = p.status != "online";
            p.status = "online";
            p.error = None;
            let out = match event {
                "snapshot" => {
                    let services: Vec<ServiceState> = serde_json::from_value(data["services"].clone()).unwrap_or_default();
                    p.services = services.into_iter().map(|s| (s.name.clone(), tag(s))).collect();
                    p.config_errors = serde_json::from_value(data["configErrors"].clone()).unwrap_or_default();
                    Some(("snapshot", json!({ "services": p.services.values().collect::<Vec<_>>(), "configErrors": p.config_errors })))
                }
                "service" => serde_json::from_value::<ServiceState>(data).ok().map(|s| {
                    let mut s = tag(s);
                    if s.metrics.is_none() && s.status == "running" {
                        s.metrics = p.services.get(&s.name).and_then(|o| o.metrics);
                    }
                    let v = json!(s);
                    p.services.insert(s.name.clone(), s);
                    ("service", v)
                }),
                "removed" => {
                    if let Some(n) = data["name"].as_str() {
                        p.services.remove(n);
                    }
                    Some(("removed", data))
                }
                "metrics" => {
                    if let (Some(n), Ok(m)) = (data["name"].as_str(), serde_json::from_value(data["metrics"].clone())) {
                        if let Some(s) = p.services.get_mut(n) {
                            s.metrics = Some(m);
                        }
                    }
                    Some(("metrics", data))
                }
                "system" => {
                    if let Some(arr) = p.system.get_mut("samples").and_then(Value::as_array_mut) {
                        arr.push(data.clone());
                        if arr.len() > 300 {
                            arr.drain(..arr.len() - 300);
                        }
                    }
                    Some(("system", data))
                }
                "processes" => {
                    p.processes = serde_json::from_value(data["processes"].clone()).unwrap_or_default();
                    Some(("processes", data))
                }
                _ => None,
            };
            (out, became_online.then(|| summary(p)))
        };
        if let Some(s) = out.1 {
            self.emit(name, "machine", s);
        }
        if let Some((ev, d)) = out.0 {
            self.emit(name, ev, d);
        }
    }

    // ------------------------------------------------------------ API

    /// Every peer with its cached services, processes and system info.
    pub fn list(&self) -> Vec<Value> {
        self.inner.lock().unwrap().values().map(full).collect()
    }

    pub fn get(&self, name: &str) -> Result<Value, ApiErr> {
        self.inner.lock().unwrap().get(name).map(full).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))
    }

    /// Pair with another machine (blocking): by one-time code or by its dashboard login (+2FA).
    pub fn pair(self: &Arc<Self>, url: &str, auth: Value, name: Option<&str>) -> Result<Value, ApiErr> {
        let url = normalize_url(url).map_err(|e| ApiErr::new(400, e))?;
        if self.inner.lock().unwrap().values().any(|p| p.cfg.url == url) {
            return Err(ApiErr::new(409, format!("{url} is already paired")));
        }
        let mut body = auth;
        body["hostName"] = json!(local_name());
        let resp: Value = agent(20)
            .post(&format!("{url}/peer/pair"))
            .set("X-LStack", "1")
            .send_json(body)
            .map_err(|e| {
                let (c, m) = http_err(e);
                ApiErr::new(if c == 0 { 502 } else { c }, if c == 0 { format!("can't reach {url}: {m} (is remote access on there?)") } else { m })
            })?
            .into_json()
            .map_err(|e| ApiErr::new(502, e.to_string()))?;
        let token = resp["token"].as_str().ok_or_else(|| ApiErr::new(502, "machine didn't return a token"))?.to_string();
        let mut name = slug(name.unwrap_or_else(|| resp["hostname"].as_str().unwrap_or("machine")));
        if !valid_name(&name) {
            return Err(ApiErr::new(400, "machine names may use letters, digits, - _ and ."));
        }
        let base = name.clone();
        let mut i = 2;
        while self.inner.lock().unwrap().contains_key(&name) || name == local_name() {
            name = format!("{base}-{i}");
            i += 1;
        }
        let cfg = PeerCfg { name: name.clone(), url, token, added_at: now_ms() };
        let mut all = load();
        all.push(cfg.clone());
        save(&all).map_err(|e| ApiErr::new(500, format!("cannot save peers.json: {e}")))?;
        self.attach(cfg);
        self.get(&name)
    }

    pub fn remove(&self, name: &str) -> Result<(), ApiErr> {
        let p = self.inner.lock().unwrap().remove(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        p.stop.store(true, Ordering::Relaxed);
        let all: Vec<PeerCfg> = load().into_iter().filter(|c| c.name != name).collect();
        save(&all).map_err(|e| ApiErr::new(500, format!("cannot save peers.json: {e}")))?;
        self.emit(name, "removed-machine", json!({ "name": name }));
        Ok(())
    }

    pub fn rename(self: &Arc<Self>, name: &str, to: &str) -> Result<Value, ApiErr> {
        if !valid_name(to) {
            return Err(ApiErr::new(400, "machine names may use letters, digits, - _ and ."));
        }
        if self.inner.lock().unwrap().contains_key(to) || to == local_name() {
            return Err(ApiErr::new(409, format!("{to:?} is already used")));
        }
        let mut cfg = self.cfg(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        self.remove(name)?;
        cfg.name = to.to_string();
        let mut all = load();
        all.push(cfg.clone());
        save(&all).map_err(|e| ApiErr::new(500, format!("cannot save peers.json: {e}")))?;
        self.attach(cfg);
        self.get(to)
    }

    /// Proxy a JSON request to a peer (blocking).
    pub fn call(&self, name: &str, method: &str, path: &str) -> Result<Value, ApiErr> {
        let cfg = self.cfg(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        let r = agent(60).request(method, &format!("{}/peer/api{path}", cfg.url)).set("Authorization", &format!("Bearer {}", cfg.token)).set("X-LStack", "1");
        let resp = if method == "GET" { r.call() } else { r.send_json(json!({})) };
        let mut v: Value = resp
            .map_err(|e| {
                let (c, m) = http_err(e);
                ApiErr::new(if c == 0 { 502 } else { c }, if c == 0 { format!("{name} is unreachable: {m}") } else { format!("{name}: {m}") })
            })?
            .into_json()
            .map_err(|e| ApiErr::new(502, e.to_string()))?;
        if let Some(lines) = v.get_mut("lines").and_then(Value::as_array_mut) {
            for l in lines {
                if let Some(s) = l["line"].as_str() {
                    l["line"] = json!(crate::scrub::scrub(s));
                }
            }
        }
        if v.get("status").is_some() && v.get("config").is_some() {
            if let Ok(mut s) = serde_json::from_value::<ServiceState>(v.clone()) {
                s.machine = Some(name.to_string());
                rewrite_urls(&mut s, &host_of(&cfg.url));
                v = json!(s);
            }
        }
        Ok(v)
    }

    /// Open a proxied SSE stream (blocking); the reader yields raw SSE bytes.
    pub fn stream(&self, name: &str, path: &str) -> Result<Box<dyn Read + Send>, ApiErr> {
        let cfg = self.cfg(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        let resp = agent(45)
            .get(&format!("{}/peer/api{path}", cfg.url))
            .set("Authorization", &format!("Bearer {}", cfg.token))
            .call()
            .map_err(|e| {
                let (c, m) = http_err(e);
                ApiErr::new(if c == 0 { 502 } else { c }, format!("{name}: {m}"))
            })?;
        Ok(resp.into_reader())
    }

    pub fn shutdown(&self) {
        for p in self.inner.lock().unwrap().values() {
            p.stop.store(true, Ordering::Relaxed);
        }
    }
}

fn summary(p: &PeerState) -> Value {
    json!({
        "name": p.cfg.name,
        "local": false,
        "url": p.cfg.url,
        "status": p.status,
        "error": p.error,
        "hostname": p.info["hostname"],
        "version": p.info["version"],
        "allowControl": p.info["allowControl"].as_bool().unwrap_or(false),
        "lastSeen": p.last_seen,
        "addedAt": p.cfg.added_at,
    })
}

fn full(p: &PeerState) -> Value {
    let mut v = summary(p);
    v["services"] = json!(p.services.values().collect::<Vec<_>>());
    v["configErrors"] = json!(p.config_errors);
    v["processes"] = json!(p.processes);
    v["system"] = json!({
        "cores": p.system["cores"],
        "memTotal": p.system["memTotal"],
        "samples": p.system["samples"].as_array().map(|a| a.iter().rev().take(120).rev().cloned().collect::<Vec<_>>()).unwrap_or_default(),
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(normalize_url("studio.local").unwrap(), "http://studio.local:7380");
        assert_eq!(normalize_url("http://10.0.0.2:9000/").unwrap(), "http://10.0.0.2:9000");
        assert_eq!(normalize_url("100.64.1.2:7400").unwrap(), "http://100.64.1.2:7400");
        assert!(normalize_url("ftp://x").is_err());
        assert_eq!(host_of("http://studio.local:7380"), "studio.local");
        assert_eq!(host_of("http://[fd00::1]:7380"), "[fd00::1]");
        let mut s = ServiceState::default();
        s.urls = vec![crate::types::ServiceUrl { url: "http://localhost:3000/x".into(), source: "detected".into(), ..Default::default() }];
        rewrite_urls(&mut s, "studio.local");
        assert_eq!(s.urls[0].url, "http://studio.local:3000/x");
        assert_eq!(slug("Jeffs MacBook.local"), "Jeffs-MacBook");
    }
}

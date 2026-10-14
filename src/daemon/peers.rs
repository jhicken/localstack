//! Other machines this daemon watches ("peers"). Each paired Mac shares itself over the encrypted link
//! (`link.rs`); we keep one link per Mac, read its event stream over it, cache its services, system load
//! and busy processes, and re-broadcast everything as `Event::Peer` tagged with the machine name.
//! Logs, metrics and start/stop go over the same link on demand, and so do automatic forwards
//! (`forwards.rs`) to the services it lets us tunnel to.
use super::forwards::{self, Forwards};
use super::link::{self, Mux};
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
use tokio::runtime::Handle;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PeerCfg {
    pub name: String,
    /// Link address, e.g. studio.local:7381
    pub addr: String,
    /// The other Mac's install key (hex), pinned at pairing.
    pub public_key: String,
    pub added_at: i64,
    /// Old (pre-link) pairing: dashboard URL. Kept only to offer a re-pair.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Old bearer token: read so the entry loads, never written back.
    #[serde(skip_serializing)]
    pub token: String,
}

impl PeerCfg {
    /// Paired before the encrypted link existed: must pair again.
    pub fn legacy(&self) -> bool {
        self.public_key.is_empty()
    }
    pub fn address(&self) -> String {
        if self.addr.is_empty() {
            format!("{}:{}", host_of(&self.url), link::DEFAULT_PORT)
        } else {
            self.addr.clone()
        }
    }
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
    mux: Option<Arc<Mux>>,
}

pub struct Peers {
    inner: Mutex<BTreeMap<String, PeerState>>,
    sup: Arc<Sup>,
    rt: Handle,
    pub forwards: Arc<Forwards>,
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

/// `host`, `host:port`, `[v6]`, (`http://`)`host:port` → `host:port` (default link port 7381).
pub fn normalize_addr(u: &str) -> Result<String, String> {
    let u = u.trim().trim_end_matches('/');
    if u.is_empty() {
        return Err("machine address required (e.g. studio.local or 100.64.1.2)".into());
    }
    let rest = match u.split_once("://") {
        Some((s, r)) if s == "http" || s == "https" || s == "lstack" => r,
        Some((s, _)) => return Err(format!("unsupported scheme {s}://")),
        None => u,
    };
    if rest.contains('/') {
        return Err("give just the machine address, without a path".into());
    }
    let has_port = rest.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok() && !rest.ends_with(']') && (!h.contains(':') || h.ends_with(']')));
    Ok(if has_port { rest.to_string() } else { format!("{rest}:{}", link::DEFAULT_PORT) })
}

fn host_of(addr: &str) -> String {
    let rest = addr.split_once("://").map(|x| x.1).unwrap_or(addr);
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

/// Whether the grants a Mac gave us (`{tunnel: "all" | "none" | [..]}`) cover `service`.
fn tunnel_granted(grants: &Value, service: &str) -> bool {
    match &grants["tunnel"] {
        Value::String(s) => s == "all",
        Value::Array(a) => a.iter().any(|v| v.as_str() == Some(service)),
        _ => false,
    }
}

type Failure = (u16, String);

impl Peers {
    pub fn new(sup: Arc<Sup>, rt: Handle) -> Arc<Self> {
        let forwards = Forwards::new(rt.clone());
        let p = Arc::new(Peers { inner: Mutex::new(BTreeMap::new()), sup, rt, forwards });
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
                mux: None,
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

    fn sleep(stop: &AtomicBool, secs: u64) -> bool {
        for _ in 0..secs * 10 {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !stop.load(Ordering::Relaxed)
    }

    /// Connection loop for one peer (own thread; blocking on link I/O).
    fn run(self: Arc<Self>, name: &str, stop: &AtomicBool) {
        let mut backoff = 2u64;
        while !stop.load(Ordering::Relaxed) {
            let Some(cfg) = self.cfg(name) else { return };
            if cfg.legacy() {
                self.set_status(name, "repair", Some(format!("paired with an older LStack — pair again: lstack machine pair {} --code …", host_of(&cfg.address()))));
                while Self::sleep(stop, 60) {}
                return;
            }
            let r = self.session(name, &cfg, stop);
            let mux = self.inner.lock().unwrap().get_mut(name).and_then(|p| p.mux.take());
            if let Some(m) = mux {
                m.shutdown();
            }
            self.forwards.set_link(name, None);
            match r {
                Ok(()) => backoff = 2,
                Err((401, e)) => {
                    self.set_status(name, "unauthorized", Some(format!("{e} — pair again: lstack machine pair {} --code …", cfg.address())));
                    backoff = 60;
                }
                Err((_, e)) => {
                    self.set_status(name, "offline", Some(e));
                    backoff = (backoff * 2).min(60);
                }
            }
            if !Self::sleep(stop, backoff) {
                return;
            }
        }
    }

    /// One HTTP request to the peer API over the link; JSON in and out.
    fn request(&self, mux: &Arc<Mux>, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let (code, mut rd) = link::http(mux, &self.rt, method, &format!("/peer/api{path}"), body).map_err(|e| (0, e))?;
        let mut s = String::new();
        rd.read_to_string(&mut s).map_err(|e| (0, e.to_string()))?;
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        if code >= 400 {
            return Err((code, v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {code}"))));
        }
        Ok(v)
    }

    fn session(&self, name: &str, cfg: &PeerCfg, stop: &AtomicBool) -> Result<(), Failure> {
        let id = link::identity().map_err(|e| (0, e))?;
        let their = link::unhex32(&cfg.public_key).ok_or((401, "the saved key for this machine is invalid".to_string()))?;
        let addr = cfg.address();
        let hello = json!({ "name": local_name(), "version": paths::VERSION });
        let (mux, _) = self.rt.block_on(link::connect(&addr, &id, &their, hello)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                (401, e.to_string())
            } else {
                (0, format!("can't reach {addr}: {e}"))
            }
        })?;
        if let Some(p) = self.inner.lock().unwrap().get_mut(name) {
            p.mux = Some(mux.clone());
        } else {
            mux.shutdown();
            return Ok(());
        }
        let info = self.request(&mux, "GET", "/info", None)?;
        let system = self.request(&mux, "GET", "/system", None).unwrap_or(Value::Null);
        let procs: Vec<ProcInfo> = self.request(&mux, "GET", "/processes", None).ok().and_then(|v| serde_json::from_value(v["processes"].clone()).ok()).unwrap_or_default();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return Ok(()) };
            p.info = info;
            p.system = system;
            p.processes = procs;
        }
        self.forwards.set_link(name, Some(mux.clone()));
        let (code, mut rd) = link::http(&mux, &self.rt, "GET", "/peer/api/events", None).map_err(|e| (0, e))?;
        if code >= 400 {
            return Err((code, format!("HTTP {code}")));
        }
        let host = host_of(&addr);
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

    /// Ports of `p`'s running services that the machine lets us tunnel to.
    fn wanted(p: &PeerState) -> Vec<(String, u16)> {
        let mut want = vec![];
        for s in p.services.values().filter(|s| s.status == "running" && tunnel_granted(&p.info["grants"], &s.name)) {
            for u in s.urls.iter().filter(|u| u.listening) {
                if let Some(port) = u.port {
                    if !want.contains(&(s.name.clone(), port)) {
                        want.push((s.name.clone(), port));
                    }
                }
            }
        }
        want
    }

    /// Point URLs at local forwards where there is one, else at the machine's address.
    fn place_urls(&self, name: &str, host: &str, s: &mut ServiceState) {
        for u in &mut s.urls {
            let local = u.port.filter(|_| u.listening).and_then(|port| self.forwards.local_port(name, &s.name, port));
            match (local, u.port) {
                (Some(l), _) => {
                    u.url = forwards::to_local(&u.url, l);
                    u.via = Some(name.to_string());
                }
                (None, Some(port)) if u.via.is_some() => {
                    let (scheme, rest) = u.url.split_once("://").unwrap_or(("http", &u.url));
                    let path = rest.find(['/', '?', '#']).map(|i| &rest[i..]).unwrap_or("");
                    u.url = format!("{scheme}://{host}:{port}{path}");
                    u.via = None;
                }
                _ => {}
            }
        }
    }

    fn ingest(&self, name: &str, host: &str, event: &str, data: Value) {
        let tag = |mut s: ServiceState| {
            s.machine = Some(name.to_string());
            s.urls.iter_mut().for_each(|u| u.via = None);
            rewrite_urls(&mut s, host);
            s
        };
        let (out, online, want) = {
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
                    Some(("snapshot", Value::Null))
                }
                "service" => serde_json::from_value::<ServiceState>(data).ok().map(|s| {
                    let mut s = tag(s);
                    if s.metrics.is_none() && s.status == "running" {
                        s.metrics = p.services.get(&s.name).and_then(|o| o.metrics);
                    }
                    let n = s.name.clone();
                    p.services.insert(n.clone(), s);
                    ("service", json!(n))
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
            let want = matches!(event, "snapshot" | "service" | "removed").then(|| Self::wanted(p));
            (out, became_online, want)
        };
        // Bind/unbind local forwards (may block briefly) outside the peer lock, then point URLs at them.
        let changed = want.is_some_and(|w| self.forwards.sync(name, w));
        let (msgs, online) = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return };
            let mut services = std::mem::take(&mut p.services);
            for s in services.values_mut() {
                self.place_urls(name, host, s);
            }
            p.services = services;
            let snapshot = || json!({ "services": p.services.values().collect::<Vec<_>>(), "configErrors": p.config_errors });
            let mut msgs: Vec<(&str, Value)> = vec![];
            match out {
                Some(("snapshot", _)) => msgs.push(("snapshot", snapshot())),
                Some(("service", _)) if changed => msgs.push(("snapshot", snapshot())),
                Some(("service", n)) => {
                    if let Some(s) = n.as_str().and_then(|n| p.services.get(n)) {
                        msgs.push(("service", json!(s)));
                    }
                }
                Some((ev, d)) => {
                    msgs.push((ev, d));
                    if changed {
                        msgs.push(("snapshot", snapshot()));
                    }
                }
                None => {}
            }
            (msgs, online.then(|| summary(p)))
        };
        if let Some(s) = online {
            self.emit(name, "machine", s);
        }
        for (ev, d) in msgs {
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

    /// Pair with the Mac at `addr`: its one-time code plus its dashboard login (+2FA), all inside the
    /// code-authenticated (SPAKE2) exchange. Pins its install key; replaces an older pairing with it.
    pub async fn pair(self: &Arc<Self>, addr: &str, code: &str, login: Value, name: Option<&str>) -> Result<Value, ApiErr> {
        let addr = normalize_addr(addr).map_err(|e| ApiErr::new(400, e))?;
        if code.trim().is_empty() {
            return Err(ApiErr::new(400, "pairing code required (Share this Mac → Show pairing code on the other Mac)"));
        }
        let paired = link::pair_client(&addr, code, login, &local_name()).await.map_err(|e| ApiErr::new(403, e))?;
        let key = link::key_hex(&paired.public);
        let host = host_of(&addr);
        let old: Vec<PeerCfg> = load().into_iter().filter(|c| c.public_key == key || c.address() == addr || (c.legacy() && host_of(&c.address()) == host)).collect();
        for c in &old {
            let _ = self.remove(&c.name);
        }
        let wanted = name.map(String::from).or_else(|| old.first().map(|c| c.name.clone()));
        let mut name = slug(wanted.as_deref().unwrap_or(if paired.hostname.is_empty() { "machine" } else { &paired.hostname }));
        if !valid_name(&name) {
            return Err(ApiErr::new(400, "machine names may use letters, digits, - _ and ."));
        }
        let base = name.clone();
        let mut i = 2;
        while self.inner.lock().unwrap().contains_key(&name) || name == local_name() {
            name = format!("{base}-{i}");
            i += 1;
        }
        let cfg = PeerCfg { name: name.clone(), addr, public_key: key, added_at: now_ms(), ..Default::default() };
        let mut all = load();
        all.push(cfg.clone());
        save(&all).map_err(|e| ApiErr::new(500, format!("cannot save peers.json: {e}")))?;
        self.attach(cfg);
        let mut v = self.get(&name)?;
        v["phrase"] = json!(paired.phrase);
        v["grants"] = paired.grants;
        v["remoteVersion"] = json!(paired.version);
        Ok(v)
    }

    pub fn remove(&self, name: &str) -> Result<(), ApiErr> {
        let p = self.inner.lock().unwrap().remove(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        p.stop.store(true, Ordering::Relaxed);
        if let Some(m) = &p.mux {
            m.shutdown();
        }
        self.forwards.remove_machine(name);
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

    /// Pause or resume local forwards to `machine`'s `service`, and re-point its URLs right away.
    pub fn set_forward_paused(&self, machine: &str, service: &str, paused: bool) -> Result<(), ApiErr> {
        let host = host_of(&self.cfg(machine).ok_or_else(|| ApiErr::new(404, format!("no machine named {machine:?}")))?.address());
        let fw = self.forwards.clone();
        if !fw.set_paused(machine, service, paused) {
            return Ok(());
        }
        let snap = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(machine) else { return Ok(()) };
            let mut services = std::mem::take(&mut p.services);
            for s in services.values_mut() {
                self.place_urls(machine, &host, s);
            }
            p.services = services;
            json!({ "services": p.services.values().collect::<Vec<_>>(), "configErrors": p.config_errors })
        };
        self.emit(machine, "snapshot", snap);
        Ok(())
    }

    /// Reconnect every machine (e.g. after this install's key changed).
    pub fn reconnect_all(&self) {
        for p in self.inner.lock().unwrap().values() {
            if let Some(m) = &p.mux {
                m.shutdown();
            }
        }
    }

    fn live(&self, name: &str) -> Result<(Arc<Mux>, String), ApiErr> {
        let g = self.inner.lock().unwrap();
        let p = g.get(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
        let mux = p.mux.clone().filter(|m| !m.is_closed()).ok_or_else(|| ApiErr::new(502, format!("{name} is {}", if p.status == "online" { "reconnecting" } else { p.status })))?;
        Ok((mux, host_of(&p.cfg.address())))
    }

    /// Proxy a JSON request to a peer (blocking).
    pub fn call(&self, name: &str, method: &str, path: &str) -> Result<Value, ApiErr> {
        let (mux, host) = self.live(name)?;
        let body = (method != "GET").then(|| json!({}));
        let mut v = self.request(&mux, method, path, body.as_ref()).map_err(|(c, m)| ApiErr::new(if c == 0 { 502 } else { c }, format!("{name}: {m}")))?;
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
                rewrite_urls(&mut s, &host);
                self.place_urls(name, &host, &mut s);
                v = json!(s);
            }
        }
        Ok(v)
    }

    /// Open a proxied SSE stream (blocking); the reader yields raw SSE bytes.
    pub fn stream(&self, name: &str, path: &str) -> Result<Box<dyn Read + Send>, ApiErr> {
        let (mux, _) = self.live(name)?;
        let (code, rd) = link::http(&mux, &self.rt, "GET", &format!("/peer/api{path}"), None).map_err(|e| ApiErr::new(502, format!("{name}: {e}")))?;
        if code >= 400 {
            return Err(ApiErr::new(code, format!("{name}: HTTP {code}")));
        }
        Ok(Box::new(rd))
    }

    pub fn shutdown(&self) {
        for p in self.inner.lock().unwrap().values() {
            p.stop.store(true, Ordering::Relaxed);
            if let Some(m) = &p.mux {
                m.shutdown();
            }
        }
        self.forwards.shutdown();
    }
}

fn summary(p: &PeerState) -> Value {
    json!({
        "name": p.cfg.name,
        "local": false,
        "url": p.cfg.address(),
        "address": p.cfg.address(),
        "status": p.status,
        "error": p.error,
        "hostname": p.info["hostname"],
        "version": p.info["version"],
        "fingerprint": super::link::unhex32(&p.cfg.public_key).map(|k| super::link::fingerprint(&k)),
        "grants": p.info["grants"],
        "allowControl": p.info["grants"]["control"].as_bool().unwrap_or(false),
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
        assert_eq!(normalize_addr("studio.local").unwrap(), "studio.local:7381");
        assert_eq!(normalize_addr("http://10.0.0.2:9000/").unwrap(), "10.0.0.2:9000");
        assert_eq!(normalize_addr("100.64.1.2:7400").unwrap(), "100.64.1.2:7400");
        assert_eq!(normalize_addr("[fd00::1]").unwrap(), "[fd00::1]:7381");
        assert!(normalize_addr("ftp://x").is_err());
        assert!(normalize_addr("a/b").is_err());
        assert_eq!(host_of("studio.local:7381"), "studio.local");
        assert_eq!(host_of("http://studio.local:7380"), "studio.local");
        assert_eq!(host_of("[fd00::1]:7381"), "[fd00::1]");
        let mut s = ServiceState { urls: vec![crate::types::ServiceUrl { url: "http://localhost:3000/x".into(), source: "detected".into(), ..Default::default() }], ..Default::default() };
        rewrite_urls(&mut s, "studio.local");
        assert_eq!(s.urls[0].url, "http://studio.local:3000/x");
        assert_eq!(slug("Jeffs MacBook.local"), "Jeffs-MacBook");
        // Old entries (bearer token, no key) load as needing a re-pair, and the token is never written back.
        let old: PeerCfg = serde_json::from_str(r#"{"name":"studio","url":"http://studio.local:7380","token":"lsp_x","addedAt":1}"#).unwrap();
        assert!(old.legacy());
        assert!(!serde_json::to_string(&old).unwrap().contains("lsp_x"));
        let g = json!({ "tunnel": ["web"] });
        assert!(tunnel_granted(&g, "web") && !tunnel_granted(&g, "db"));
        assert!(tunnel_granted(&json!({ "tunnel": "all" }), "db"));
        assert!(!tunnel_granted(&json!({ "tunnel": "none" }), "db"));
    }
}

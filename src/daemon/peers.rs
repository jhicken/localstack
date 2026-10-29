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
    /// Link address, e.g. studio.local:7381 (as entered when pairing).
    pub addr: String,
    /// More LAN addresses it reported (`/peer/api/info`), tried in order after `addr`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    /// The other Mac's install key (hex), pinned at pairing.
    pub public_key: String,
    pub added_at: i64,
    /// Old (pre-link) pairing: dashboard URL. Kept only to offer a re-pair.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Old bearer token: read so the entry loads, never written back.
    #[serde(skip_serializing)]
    pub token: String,
    /// 1Password references whose cached values this Mac may deliver to it (it can't call here, so we
    /// send them over our link when it asks or accepts).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Of `secrets`: pushed to it and not accepted or declined there yet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub push: Vec<String>,
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
    /// LAN addresses to try, in order.
    pub fn lan_addrs(&self) -> Vec<String> {
        let mut v = vec![self.address()];
        for a in &self.addrs {
            if !v.contains(a) {
                v.push(a.clone());
            }
        }
        v
    }
}

pub const QUICK_LINK_SCHEME: &str = "lstack-pair://";

/// A Quick Connect link: the shared Mac's address and its one-time pairing code (never a 2FA code).
pub fn quick_link(addr: &str, code: &str) -> String {
    format!("{QUICK_LINK_SCHEME}{addr}?code={code}")
}

/// `lstack-pair://host:port?code=XXXX-XXXX` → (address, code).
pub fn parse_quick_link(s: &str) -> Option<(String, String)> {
    let rest = s.trim().strip_prefix(QUICK_LINK_SCHEME)?;
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let code = query.split('&').find_map(|kv| kv.strip_prefix("code=")).unwrap_or("").to_string();
    Some((addr.trim_end_matches('/').to_string(), code))
}

/// Open the link to a paired Mac: a direct, end-to-end encrypted connection from this daemon to its
/// daemon, trying each LAN address in turn. Returns the link and how/where it connected.
pub async fn dial_link(cfg: &PeerCfg, id: &link::Identity, their: &[u8; 32], hello: Value) -> std::io::Result<(Arc<Mux>, Value, &'static str, String)> {
    use std::io::{Error, ErrorKind};
    let mut last = Error::new(ErrorKind::NotFound, "no address");
    let mut refused = None;
    for a in cfg.lan_addrs() {
        match link::connect(&a, id, their, hello.clone()).await {
            Ok((m, v)) => return Ok((m, v, "lan", a)),
            // It answered and refused (e.g. it only accepts another of its addresses now): try the rest, and
            // report the refusal if none works.
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                refused.get_or_insert(e);
            }
            Err(e) => last = Error::new(e.kind(), format!("{a}: {e}")),
        }
    }
    Err(refused.unwrap_or(last))
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
    /// How the current link connected (always "lan") and to which address.
    via: Option<(&'static str, String)>,
    /// Set to end the current wait or link and connect again right away.
    kick: Arc<AtomicBool>,
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

/// An unpair notice still to be delivered to a Mac that was offline when it was removed here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Unpair {
    addr: String,
    public_key: String,
    at: i64,
}

fn unpairs_file() -> std::path::PathBuf {
    paths::home().join("unpair.json")
}

fn load_unpairs() -> Vec<Unpair> {
    std::fs::read(unpairs_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_unpairs(list: &[Unpair]) -> std::io::Result<()> {
    if list.is_empty() {
        return match std::fs::remove_file(unpairs_file()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(unpairs_file())?;
    f.write_all(serde_json::to_string_pretty(list).unwrap_or_default().as_bytes())
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

/// The host part of a URL ("http://studio.lan:3000/x" → "studio.lan").
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let auth = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if auth.starts_with('[') {
        return auth.find(']').map_or(auth, |i| &auth[..=i]);
    }
    auth.split(':').next().unwrap_or(auth)
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
        let me = p.clone();
        std::thread::Builder::new().name("peer-unpairs".into()).spawn(move || me.deliver_unpairs()).ok();
        p
    }

    fn emit(&self, machine: &str, event: &str, data: Value) {
        let _ = self.sup.events.send(Event::Peer(Arc::new(PeerMsg { machine: machine.into(), event: event.into(), data })));
    }

    fn attach(self: &Arc<Self>, cfg: PeerCfg) {
        let stop = Arc::new(AtomicBool::new(false));
        let kick = Arc::new(AtomicBool::new(false));
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
                via: None,
                kick: kick.clone(),
            },
        );
        let me = self.clone();
        std::thread::Builder::new().name(format!("peer-{name}")).spawn(move || me.run(&name, &stop, &kick)).ok();
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
        Self::wait(stop, None, secs)
    }

    /// Sleep up to `secs`; a kick ends it early. False when stopping.
    fn wait(stop: &AtomicBool, kick: Option<&AtomicBool>, secs: u64) -> bool {
        for _ in 0..secs * 10 {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            if kick.is_some_and(|k| k.swap(false, Ordering::SeqCst)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !stop.load(Ordering::Relaxed)
    }

    /// Redial machines whose link is down now (after wake or a network change), skipping their backoff.
    pub fn reconnect_down(&self) -> usize {
        let g = self.inner.lock().unwrap();
        let mut n = 0;
        for p in g.values().filter(|p| !p.cfg.legacy() && p.mux.as_ref().is_none_or(|m| m.is_closed())) {
            p.kick.store(true, Ordering::SeqCst);
            n += 1;
        }
        n
    }

    /// Connection loop for one peer (own thread; blocking on link I/O).
    fn run(self: Arc<Self>, name: &str, stop: &AtomicBool, kick: &AtomicBool) {
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
            let farewell = mux.as_ref().and_then(|m| m.farewell());
            if let Some(m) = mux {
                m.shutdown();
            }
            self.forwards.set_link(name, None);
            if kick.swap(false, Ordering::SeqCst) {
                backoff = 2;
                self.set_status(name, "connecting", None);
                if !Self::sleep(stop, 0) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let revoked = farewell.as_deref() == Some(link::BYE_REVOKED) || matches!(&r, Err((401, e)) if e.contains(link::REFUSED_REVOKED));
            if revoked {
                // The other Mac unpaired this one: forget it here too (its tunnels close with it).
                println!("{} peers: {name} revoked this Mac's access — forgetting it", paths::iso(now_ms()));
                let _ = self.remove(name);
                return;
            }
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
            if !Self::wait(stop, Some(kick), backoff) {
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
        let (mux, _, via, dialed) = self.rt.block_on(dial_link(cfg, &id, &their, hello)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                (401, e.to_string())
            } else {
                (0, format!("can't reach {addr}: {e}"))
            }
        })?;
        if let Some(p) = self.inner.lock().unwrap().get_mut(name) {
            p.mux = Some(mux.clone());
            p.via = Some((via, dialed));
        } else {
            mux.shutdown();
            return Ok(());
        }
        let info = self.request(&mux, "GET", "/info", None)?;
        self.learn(name, &info);
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

    /// Change a saved machine (memory and peers.json). Returns whether anything changed.
    fn update_cfg(&self, name: &str, f: impl FnOnce(&mut PeerCfg)) -> bool {
        let new = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.get_mut(name) else { return false };
            let mut c = p.cfg.clone();
            f(&mut c);
            if serde_json::to_value(&c).ok() == serde_json::to_value(&p.cfg).ok() {
                return false;
            }
            p.cfg = c.clone();
            c
        };
        let mut all = load();
        if let Some(e) = all.iter_mut().find(|c| c.name == name) {
            *e = new;
            let _ = save(&all);
        }
        true
    }

    /// Remember the other LAN addresses the machine reported for itself.
    fn learn(&self, name: &str, info: &Value) {
        // `addrs` (current), or `transports.local.addrs` from LStack versions that had a Tailscale option.
        let list = if info["addrs"].is_array() { &info["addrs"] } else { &info["transports"]["local"]["addrs"] };
        let Some(list) = list.as_array() else { return };
        let lan: Vec<String> = list.iter().filter_map(|v| v.as_str()).filter(|a| normalize_addr(a).is_ok()).take(8).map(str::to_string).collect();
        let changed = self.update_cfg(name, |c| {
            let own = c.address();
            c.addrs = lan.into_iter().filter(|a| *a != own).collect();
        });
        if changed {
            println!("{} peers: {name}: updated its addresses", paths::iso(now_ms()));
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

    /// Point URLs at local forwards where there is one. Anything still pointing at the machine itself is
    /// flagged rather than offered: services on paired Macs are only reached through the encrypted link,
    /// never by plain connections over the network.
    fn place_urls(&self, name: &str, host: &str, s: &mut ServiceState) {
        for u in &mut s.urls {
            u.blocked = None;
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
            if u.via.is_none() {
                if u.port.is_some_and(|port| u.listening && self.forwards.off(name, &s.name, port)) {
                    u.blocked = Some("off".into());
                } else if url_host(&u.url) == host {
                    u.blocked = Some("untunneled".into());
                }
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

    /// Pair with the Mac at `addr`: its one-time code, plus its 2FA code (in `auth`) if it uses two-factor
    /// authentication, inside the code-authenticated (SPAKE2) exchange. Pins its install key; replaces an
    /// older pairing with it.
    pub async fn pair(self: &Arc<Self>, addr: &str, code: &str, auth: Value, name: Option<&str>) -> Result<Value, ApiErr> {
        let addr = normalize_addr(addr).map_err(|e| ApiErr::new(400, e))?;
        if code.trim().is_empty() {
            return Err(ApiErr::new(400, "pairing code required (Share this Mac → Show pairing code on the other Mac)"));
        }
        let paired = link::pair_client(&addr, code, auth, &local_name()).await.map_err(|e| ApiErr::new(403, e))?;
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

    /// Unpair `name` on purpose: tell it so it forgets this Mac too — now over the link, or (when it's
    /// offline) the next time it can be reached.
    pub fn unpair(&self, name: &str) -> Result<(), ApiErr> {
        let (cfg, mux) = {
            let mut g = self.inner.lock().unwrap();
            let p = g.get_mut(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
            // Taken out so `remove` doesn't cut the link before the goodbye is flushed (`bye` closes it).
            (p.cfg.clone(), p.mux.take().filter(|m| !m.is_closed()))
        };
        match mux {
            Some(m) => m.bye(link::BYE_REVOKED),
            None if !cfg.legacy() => {
                let mut list = load_unpairs();
                list.retain(|u| u.public_key != cfg.public_key);
                list.push(Unpair { addr: cfg.address(), public_key: cfg.public_key.clone(), at: now_ms() });
                let _ = save_unpairs(&list);
            }
            None => {}
        }
        self.remove(name)
    }

    /// Deliver unpair notices queued while those Macs were offline (own thread; retries every minute
    /// for up to 30 days).
    fn deliver_unpairs(self: Arc<Self>) {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let list = load_unpairs();
            if list.is_empty() {
                continue;
            }
            let Ok(id) = link::identity() else { continue };
            let mut keep = vec![];
            for u in list {
                if now_ms() - u.at > 30 * 86_400_000 {
                    continue;
                }
                let Some(their) = link::unhex32(&u.public_key) else { continue };
                // Paired again since: nothing to deliver.
                if load().iter().any(|c| c.public_key == u.public_key) {
                    continue;
                }
                let hello = json!({ "name": local_name(), "version": paths::VERSION });
                let res = self.rt.block_on(link::connect(&u.addr, &id, &their, hello));
                match res {
                    Ok((m, _)) => {
                        m.bye(link::BYE_REVOKED);
                        println!("{} peers: told {} this Mac unpaired it", paths::iso(now_ms()), u.addr);
                    }
                    // It already forgot this Mac.
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                    Err(_) => keep.push(u),
                }
            }
            let _ = save_unpairs(&keep);
        }
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

    /// Turn local forwards to `machine`'s `service` on or off, and re-point its URLs right away.
    pub fn set_forward_enabled(&self, machine: &str, service: &str, on: bool) -> Result<(), ApiErr> {
        let host = host_of(&self.cfg(machine).ok_or_else(|| ApiErr::new(404, format!("no machine named {machine:?}")))?.address());
        let fw = self.forwards.clone();
        if !fw.set_enabled(machine, service, on) {
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

    /// JSON request with a body to a peer's API (blocking). Nothing is scrubbed or rewritten.
    pub fn call_json(&self, name: &str, method: &str, path: &str, body: &Value) -> Result<Value, ApiErr> {
        let (mux, _) = self.live(name)?;
        self.request(&mux, method, path, Some(body)).map_err(|(c, m)| ApiErr::new(if c == 0 { 502 } else { c }, format!("{name}: {m}")))
    }

    /// A watched machine's saved settings.
    pub fn machine(&self, name: &str) -> Option<PeerCfg> {
        self.cfg(name)
    }

    /// Every watched machine's saved settings.
    pub fn machines(&self) -> Vec<PeerCfg> {
        self.inner.lock().unwrap().values().map(|p| p.cfg.clone()).collect()
    }

    /// Change which 1Password references are shared with (`secrets`) or pushed to (`push`) a watched
    /// machine. Saved; the link isn't touched.
    pub fn set_secret_lists(&self, name: &str, f: impl FnOnce(&mut PeerCfg)) -> Result<PeerCfg, ApiErr> {
        let cfg = {
            let mut g = self.inner.lock().unwrap();
            let p = g.get_mut(name).ok_or_else(|| ApiErr::new(404, format!("no machine named {name:?}")))?;
            f(&mut p.cfg);
            p.cfg.secrets.sort();
            p.cfg.secrets.dedup();
            p.cfg.push.retain(|r| p.cfg.secrets.contains(r));
            p.cfg.push.sort();
            p.cfg.push.dedup();
            p.cfg.clone()
        };
        let mut all = load();
        for c in all.iter_mut().filter(|c| c.name == name) {
            c.secrets = cfg.secrets.clone();
            c.push = cfg.push.clone();
        }
        save(&all).map_err(|e| ApiErr::new(500, format!("cannot save peers.json: {e}")))?;
        Ok(cfg)
    }

    /// The pinned link key (hex) of a paired Mac.
    pub fn public_key(&self, name: &str) -> Option<String> {
        self.cfg(name).map(|c| c.public_key).filter(|k| !k.is_empty())
    }

    /// Machines whose link is up now.
    pub fn online(&self) -> Vec<String> {
        self.inner.lock().unwrap().iter().filter(|(_, p)| p.status == "online" && p.mux.as_ref().is_some_and(|m| !m.is_closed())).map(|(n, _)| n.clone()).collect()
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
        "addrs": p.cfg.lan_addrs(),
        "via": p.via.as_ref().filter(|_| p.mux.as_ref().is_some_and(|m| !m.is_closed())).map(|(v, a)| json!({ "kind": v, "address": a })),
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
    fn old_entries_load() {
        let c: PeerCfg = serde_json::from_value(json!({ "name": "studio", "addr": "studio.lan:7381", "publicKey": "ab", "addedAt": 1 })).unwrap();
        assert_eq!(c.lan_addrs(), vec!["studio.lan:7381"]);
        // Saved by a version with the Tailscale option: those fields are dropped.
        let c: PeerCfg = serde_json::from_value(json!({ "name": "studio", "addr": "studio.lan:7381", "addrs": ["192.168.1.5:7381"], "publicKey": "ab", "addedAt": 1,
            "transport": "tailscale", "tailnetName": "lstack-studio.t.ts.net", "pendingTransport": "local" })).unwrap();
        assert_eq!(c.lan_addrs(), vec!["studio.lan:7381", "192.168.1.5:7381"]);
        let l = quick_link("automain.tail1.ts.net:7381", "K7QP-3M9X");
        assert_eq!(parse_quick_link(&l), Some(("automain.tail1.ts.net:7381".into(), "K7QP-3M9X".into())));
        assert_eq!(parse_quick_link(" lstack-pair://[fd00::1]:7381/?code=AB-CD&x=1 "), Some(("[fd00::1]:7381".into(), "AB-CD".into())));
        assert_eq!(parse_quick_link("studio.lan:7381"), None);
        let saved = serde_json::to_string(&c).unwrap();
        assert!(!saved.contains("transport") && !saved.contains("tailnet") && !saved.contains("ts.net"));
    }

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
        assert_eq!(url_host("http://studio.lan:3000/x?y"), "studio.lan");
        assert_eq!(url_host("https://[fd00::1]:8443/"), "[fd00::1]");
        assert_eq!(url_host("http://studio.lan"), "studio.lan");
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

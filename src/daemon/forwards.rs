//! Automatic local forwards to paired Macs' services ("tunnels").
//!
//! For every running service a paired Mac lets us tunnel to, and every port that service is listening on,
//! we listen on this Mac's loopback (127.0.0.1 and ::1 only) and relay each accepted connection as a
//! `tunnel` stream over that Mac's encrypted link. There is one link per Mac; forwards are just streams.
//!
//! - Local port: the one used last time (remembered in `forwards.json`), else the same number as on the
//!   other Mac if free, else any free port.
//! - Only processes of the macOS user running LStack may use a forward (checked per connection).
//! - Forwarding is opt-in per service (`enabled`); services default to off.
use crate::paths::{self, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream as StdTcp};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

use super::link::Mux;

/// Concurrent connections through all forwards to one Mac.
const MAX_CONNS: usize = 128;

type Key = (String, String, u16);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Saved {
    /// "machine/service/port" → local port.
    ports: BTreeMap<String, u16>,
    /// "machine/service" with forwarding turned on.
    enabled: BTreeSet<String>,
    /// Pre-opt-in files only: services whose forwarding was paused. Read for the migration, never written.
    #[serde(skip_serializing)]
    paused: BTreeSet<String>,
}

impl Saved {
    /// Files from before forwarding became opt-in have no `enabled` list: keep every service that was
    /// forwarded (it has a remembered port) and wasn't paused, so existing setups carry on unchanged.
    fn load() -> Saved {
        let Some(raw) = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Saved::default() };
        let migrate = raw.get("enabled").is_none();
        let saved = Saved::from_json(raw);
        if migrate {
            Forwards::save(&saved);
        }
        saved
    }

    fn from_json(raw: Value) -> Saved {
        let has_enabled = raw.get("enabled").is_some();
        let mut saved: Saved = serde_json::from_value(raw).unwrap_or_default();
        if !has_enabled {
            let paused = std::mem::take(&mut saved.paused);
            saved.enabled = saved.ports.keys().filter_map(|k| k.rsplit_once('/').map(|(svc, _)| svc.to_string())).filter(|svc| !paused.contains(svc)).collect();
        }
        saved
    }
}

struct Fwd {
    local: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    conns: Arc<AtomicUsize>,
}

#[derive(Default)]
struct Inner {
    links: HashMap<String, Arc<Mux>>,
    active: BTreeMap<Key, Fwd>,
    /// Last wanted (service, port) list per machine, to re-sync after pause/resume.
    wanted: HashMap<String, Vec<(String, u16)>>,
    errors: BTreeMap<Key, String>,
    saved: Saved,
}

pub struct Forwards {
    rt: Handle,
    inner: Mutex<Inner>,
    /// Recently verified client pids (checked first by the same-user lookup).
    recent: Arc<Mutex<Vec<i32>>>,
    conns: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
}

fn file() -> std::path::PathBuf {
    paths::home().join("forwards.json")
}

fn skey(k: &Key) -> String {
    format!("{}/{}/{}", k.0, k.1, k.2)
}

/// Something already answers on this loopback port (v4 or v6)?
fn in_use(port: u16) -> bool {
    [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        .iter()
        .any(|ip| StdTcp::connect_timeout(&SocketAddr::new(*ip, port), Duration::from_millis(150)).is_ok())
}

/// Bind 127.0.0.1:port (and ::1 when possible). Port 0 = any.
fn bind(port: u16) -> Option<(std::net::TcpListener, Option<std::net::TcpListener>)> {
    if port != 0 && in_use(port) {
        return None;
    }
    let v4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok()?;
    let p = v4.local_addr().ok()?.port();
    let v6 = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, p)).ok();
    Some((v4, v6))
}

impl Forwards {
    pub fn new(rt: Handle) -> Arc<Self> {
        let saved = Saved::load();
        Arc::new(Forwards {
            rt,
            inner: Mutex::new(Inner { saved, ..Default::default() }),
            recent: Arc::new(Mutex::new(vec![])),
            conns: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn save(saved: &Saved) {
        let path = file();
        let tmp = path.with_extension("json.tmp");
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp) {
            let _ = f.write_all(serde_json::to_string_pretty(saved).unwrap_or_default().as_bytes());
            let _ = std::fs::rename(tmp, path);
        }
    }

    /// The link to `machine` came up (Some) or went down (None). Listeners stay either way so ports
    /// are stable; while it's down they refuse connections.
    pub fn set_link(&self, machine: &str, mux: Option<Arc<Mux>>) {
        let mut g = self.inner.lock().unwrap();
        match mux {
            Some(m) => g.links.insert(machine.to_string(), m),
            None => g.links.remove(machine),
        };
    }

    /// Make `machine`'s forwards match `want` (service, port). Returns true if anything changed.
    pub fn sync(self: &Arc<Self>, machine: &str, want: Vec<(String, u16)>) -> bool {
        let mut g = self.inner.lock().unwrap();
        g.wanted.insert(machine.to_string(), want.clone());
        let enabled = g.saved.enabled.clone();
        let want: BTreeSet<Key> = want.into_iter().filter(|(s, _)| enabled.contains(&format!("{machine}/{s}"))).map(|(s, p)| (machine.to_string(), s, p)).collect();
        let stale: Vec<Key> = g.active.keys().filter(|k| k.0 == machine && !want.contains(*k)).cloned().collect();
        let mut changed = !stale.is_empty();
        for k in stale {
            if let Some(f) = g.active.remove(&k) {
                f.tasks.iter().for_each(|t| t.abort());
            }
        }
        g.errors.retain(|k, _| k.0 != machine || want.contains(k));
        for k in want {
            if g.active.contains_key(&k) {
                continue;
            }
            let remembered = g.saved.ports.get(&skey(&k)).copied();
            let bound = remembered.and_then(bind).or_else(|| bind(k.2)).or_else(|| bind(0));
            let Some((v4, v6)) = bound else {
                g.errors.insert(k.clone(), "no free local port".into());
                continue;
            };
            let local = v4.local_addr().map(|a| a.port()).unwrap_or(0);
            if remembered != Some(local) {
                g.saved.ports.insert(skey(&k), local);
                Self::save(&g.saved);
            }
            let conns = self.conns.lock().unwrap().entry(machine.to_string()).or_insert_with(|| Arc::new(AtomicUsize::new(0))).clone();
            let mut tasks = vec![];
            for l in std::iter::once(v4).chain(v6) {
                let _ = l.set_nonblocking(true);
                let _guard = self.rt.enter();
                let Ok(l) = tokio::net::TcpListener::from_std(l) else { continue };
                tasks.push(self.rt.spawn(self.clone().accept_loop(l, k.clone(), local, conns.clone())));
            }
            println!("{} forwards: {}/{}:{} → localhost:{local}", paths::iso(now_ms()), k.0, k.1, k.2);
            g.active.insert(k, Fwd { local, tasks, conns: Arc::new(AtomicUsize::new(0)) });
            changed = true;
        }
        changed
    }

    async fn accept_loop(self: Arc<Self>, l: tokio::net::TcpListener, key: Key, local: u16, machine_conns: Arc<AtomicUsize>) {
        loop {
            let Ok((sock, from)) = l.accept().await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            let me = self.clone();
            let key = key.clone();
            let mc = machine_conns.clone();
            tokio::spawn(async move { me.relay(sock, from, key, local, mc).await });
        }
    }

    async fn relay(self: Arc<Self>, mut sock: tokio::net::TcpStream, from: SocketAddr, key: Key, local: u16, machine_conns: Arc<AtomicUsize>) {
        // Same-user check: the connecting socket must belong to one of this user's processes.
        let recent = self.recent.lock().unwrap().clone();
        let owner = tokio::task::spawn_blocking(move || super::procinfo::tcp_client_owner(from.port(), local, &recent)).await.ok().flatten();
        let Some(pid) = owner else {
            println!("{} forwards: refused a connection to localhost:{local} from another user's process", paths::iso(now_ms()));
            return;
        };
        {
            let mut r = self.recent.lock().unwrap();
            r.retain(|p| *p != pid);
            r.insert(0, pid);
            r.truncate(8);
        }
        let mux = self.inner.lock().unwrap().links.get(&key.0).cloned();
        let Some(mux) = mux.filter(|m| !m.is_closed()) else { return };
        if machine_conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            machine_conns.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let fwd_conns = self.inner.lock().unwrap().active.get(&key).map(|f| f.conns.clone());
        if let Some(c) = &fwd_conns {
            c.fetch_add(1, Ordering::SeqCst);
        }
        if let Ok(mut stream) = mux.open(json!({ "kind": "tunnel", "service": key.1, "port": key.2 })).await {
            let _ = sock.set_nodelay(true);
            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        }
        if let Some(c) = &fwd_conns {
            c.fetch_sub(1, Ordering::SeqCst);
        }
        machine_conns.fetch_sub(1, Ordering::SeqCst);
    }

    /// Local port forwarding to `machine`'s `service:port`, if active.
    pub fn local_port(&self, machine: &str, service: &str, port: u16) -> Option<u16> {
        self.inner.lock().unwrap().active.get(&(machine.to_string(), service.to_string(), port)).map(|f| f.local)
    }

    /// Whether `machine`'s `service:port` could be forwarded but forwarding is off for the service.
    pub fn off(&self, machine: &str, service: &str, port: u16) -> bool {
        let g = self.inner.lock().unwrap();
        !g.saved.enabled.contains(&format!("{machine}/{service}")) && g.wanted.get(machine).is_some_and(|w| w.iter().any(|(s, p)| s == service && *p == port))
    }

    pub fn remove_machine(&self, machine: &str) {
        let mut g = self.inner.lock().unwrap();
        g.links.remove(machine);
        g.wanted.remove(machine);
        let keys: Vec<Key> = g.active.keys().filter(|k| k.0 == machine).cloned().collect();
        for k in keys {
            if let Some(f) = g.active.remove(&k) {
                f.tasks.iter().for_each(|t| t.abort());
            }
        }
        g.errors.retain(|k, _| k.0 != machine);
    }

    /// Turn forwarding of `machine`'s `service` on or off. Returns true if it changed.
    pub fn set_enabled(self: &Arc<Self>, machine: &str, service: &str, on: bool) -> bool {
        let want = {
            let mut g = self.inner.lock().unwrap();
            let k = format!("{machine}/{service}");
            let changed = if on { g.saved.enabled.insert(k) } else { g.saved.enabled.remove(&k) };
            if !changed {
                return false;
            }
            Self::save(&g.saved);
            g.wanted.get(machine).cloned().unwrap_or_default()
        };
        self.sync(machine, want);
        true
    }

    pub fn list(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<Value> = vec![];
        for (machine, want) in &g.wanted {
            let online = g.links.get(machine).is_some_and(|m| !m.is_closed());
            for (service, port) in want {
                let k = (machine.clone(), service.clone(), *port);
                let enabled = g.saved.enabled.contains(&format!("{machine}/{service}"));
                let f = g.active.get(&k);
                out.push(json!({
                    "machine": machine,
                    "service": service,
                    "port": port,
                    "localPort": f.map(|f| f.local),
                    "url": f.map(|f| format!("http://localhost:{}", f.local)),
                    "enabled": enabled,
                    "online": online,
                    "connections": f.map_or(0, |f| f.conns.load(Ordering::Relaxed)),
                    "error": g.errors.get(&k),
                }));
            }
        }
        out.sort_by(|a, b| (a["machine"].as_str(), a["service"].as_str(), a["port"].as_u64()).cmp(&(b["machine"].as_str(), b["service"].as_str(), b["port"].as_u64())));
        json!(out)
    }

    pub fn shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        for (_, f) in std::mem::take(&mut g.active) {
            f.tasks.iter().for_each(|t| t.abort());
        }
    }
}

/// `scheme://host:port/rest` → `scheme://localhost:<local>/rest`.
pub fn to_local(url: &str, local: u16) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let path = rest.find(['/', '?', '#']).map(|i| &rest[i..]).unwrap_or("");
    format!("{scheme}://localhost:{local}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls() {
        assert_eq!(to_local("http://localhost:3000/x?y=1", 4100), "http://localhost:4100/x?y=1");
        assert_eq!(to_local("https://studio.local:8443", 9443), "https://localhost:9443");
        assert_eq!(to_local("http://[::1]:5173#a", 5174), "http://localhost:5174#a");
    }

    #[test]
    fn old_files_keep_forwarded_services_unless_paused() {
        let old = json!({ "ports": { "studio/web/3000": 3000, "studio/web/3001": 3001, "studio/db/5432": 5432, "mini/api/80": 8080 }, "paused": ["studio/db"] });
        let saved = Saved::from_json(old);
        assert_eq!(saved.enabled.iter().map(String::as_str).collect::<Vec<_>>(), ["mini/api", "studio/web"]);
        assert!(saved.paused.is_empty());
        // Already-migrated files are taken as they are.
        let new = Saved::from_json(json!({ "ports": { "studio/web/3000": 3000 }, "enabled": [] }));
        assert!(new.enabled.is_empty());
    }

    #[test]
    fn busy_ports_are_skipped() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(bind(p).is_none());
        drop(l);
        let (v4, _) = bind(0).unwrap();
        assert_ne!(v4.local_addr().unwrap().port(), 0);
    }
}

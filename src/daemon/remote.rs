//! Two independent features:
//! - the **web dashboard** (`enabled`, default port 7380): a small web UI behind a username/password login.
//!   Available on this Mac only, or to other devices too when `webRemote` is set. Start/stop/restart from
//!   the dashboard need `allowControl`.
//! - **sharing** (`sharing`, link port 7381): paired Macs connect over the encrypted link (see `link.rs`,
//!   `share.rs`). Each paired Mac has its own grants (control, tunnels). Independent of the dashboard:
//!   pairing needs a one-time code (+ a 2FA code when two-factor authentication is set up), no login.
use super::server::{self, ApiErr, AppState};
use super::totp;
use crate::paths::{self, now_ms, VERSION};
use crate::types::ServiceState;
use std::time::Instant;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

pub const DEFAULT_PORT: u16 = 7380;
const SESSION_TTL_MS: i64 = 12 * 3_600_000;
const COOKIE: &str = "lstack_session";
const PBKDF2_ROUNDS: u32 = 120_000;
const MAX_FAILURES: u32 = 5;
const LOCKOUT_MS: i64 = 5 * 60_000;

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const ICON_PNG: &[u8] = include_bytes!("web/icon.png");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteConfig {
    /// Web dashboard on.
    pub enabled: bool,
    /// The web dashboard is reachable from other devices (not just this Mac). Missing in old configs = on.
    pub web_remote: Option<bool>,
    /// Share this machine's services with paired Macs. Missing in old configs = same as `enabled`.
    pub sharing: Option<bool>,
    pub port: u16,
    pub bind: String,
    pub username: String,
    pub password_hash: String,
    pub allow_control: bool,
    /// Base32 TOTP secret (Settings → Security); empty = 2FA off. Asked for when a new Mac pairs and at
    /// web dashboard sign-in.
    pub totp_secret: String,
    /// Port of the encrypted machine link (sharing).
    pub link_port: u16,
    /// Macs paired with this one: their pinned public keys and what each may do.
    pub peers: Vec<SharedPeer>,
    /// Keys revoked here while their Mac was offline (hex, newest last): when one connects again it's told,
    /// so it forgets this Mac too. Cleared when that key pairs again.
    pub revoked: Vec<String>,
    /// Address shown first for reaching this Mac (pairing code, Settings) — e.g. a router DNS name that
    /// survives DHCP changes. None = automatic (LAN IPs, then the host name).
    pub share_address: Option<String>,
}

/// A Mac allowed to connect to this one over the link.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SharedPeer {
    pub id: String,
    /// Name the other Mac gave when pairing.
    pub name: String,
    /// Its install identity (X25519, hex), pinned at pairing.
    pub public_key: String,
    pub grants: Grants,
    /// The confirmation words both Macs showed at pairing.
    pub phrase: String,
    pub created_at: i64,
    pub last_seen: Option<i64>,
}

/// What one paired Mac may do here (viewing status is always allowed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Grants {
    /// Start / stop / restart services.
    pub control: bool,
    /// Tunnels to every service's listening ports…
    pub tunnel_all: bool,
    /// …or only these services'.
    pub tunnel: Vec<String>,
    /// 1Password references (exact `op://…`) whose cached values it may receive (Secrets › Share).
    pub secrets: Vec<String>,
    /// Of `secrets`: values pushed to it (Machines › Push Secrets) that it hasn't accepted or declined yet.
    pub push: Vec<String>,
}

impl Grants {
    pub fn new_pairing() -> Grants {
        Grants { control: false, tunnel_all: true, tunnel: vec![], secrets: vec![], push: vec![] }
    }
    pub fn tunnel_allows(&self, service: &str) -> bool {
        self.tunnel_all || self.tunnel.iter().any(|s| s == service)
    }
    pub fn json(&self) -> Value {
        let tunnel = if self.tunnel_all { json!("all") } else if self.tunnel.is_empty() { json!("none") } else { json!(self.tunnel) };
        json!({ "control": self.control, "tunnel": tunnel })
    }
}

const PAIR_CODE_TTL_MS: i64 = 10 * 60_000;
/// Wrong attempts (code, then 2FA) before a pairing code is burned.
const PAIR_CODE_TRIES: u32 = 3;

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            enabled: false,
            web_remote: None,
            sharing: None,
            port: DEFAULT_PORT,
            bind: "0.0.0.0".into(),
            username: "admin".into(),
            password_hash: String::new(),
            allow_control: false,
            totp_secret: String::new(),
            link_port: super::link::DEFAULT_PORT,
            peers: vec![],
            revoked: vec![],
            share_address: None,
        }
    }
}

pub fn load_config() -> RemoteConfig {
    match std::fs::read(paths::remote_config()).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(cfg) => migrate(cfg),
        // Fresh install: everything off, dashboard local-only once turned on.
        None => RemoteConfig { web_remote: Some(false), sharing: Some(false), ..Default::default() },
    }
}

/// Before the split, `enabled` meant "dashboard on the network + sharing": keep that behavior.
fn migrate(mut cfg: RemoteConfig) -> RemoteConfig {
    cfg.web_remote.get_or_insert(true);
    cfg.sharing.get_or_insert(cfg.enabled);
    cfg
}

impl RemoteConfig {
    pub fn web_remote(&self) -> bool {
        self.web_remote.unwrap_or(false)
    }
    pub fn sharing(&self) -> bool {
        self.sharing.unwrap_or(false)
    }
    /// Whether the web dashboard listener runs at all.
    pub fn listening(&self) -> bool {
        self.enabled
    }
    /// The dashboard's bind address: the configured one when other devices may use it, else loopback only.
    pub fn bind_addr(&self) -> String {
        if self.enabled && self.web_remote() {
            self.bind.clone()
        } else {
            "127.0.0.1".into()
        }
    }
    pub fn peer_by_key(&self, key: &[u8; 32]) -> Option<&SharedPeer> {
        let hex = super::link::key_hex(key);
        self.peers.iter().find(|p| ct_eq(p.public_key.as_bytes(), hex.as_bytes()))
    }
    /// Whether a dashboard request from `ip` may be served (None) or why not (status, message).
    pub fn admit(&self, _path: &str, ip: IpAddr) -> Option<(u16, &'static str)> {
        if !self.enabled {
            return Some((404, "the web dashboard is off"));
        }
        if !self.web_remote() && !is_loopback(ip) {
            return Some((403, "the web dashboard is only available on this Mac"));
        }
        None
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback(),
        IpAddr::V6(v) => v.is_loopback() || v.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
    }
}

fn save_config(cfg: &RemoteConfig) -> std::io::Result<()> {
    let path = paths::remote_config();
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(cfg).unwrap_or_default().as_bytes())?;
    std::fs::rename(tmp, path)
}

// ------------------------------------------------------------------ passwords

pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    hex(&buf)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(password: &str, salt: &str, rounds: u32) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt.as_bytes(), rounds, &mut out);
    hex(&out)
}

/// `pbkdf2-sha256$<rounds>$<salt>$<hash>`
pub fn hash_password(password: &str) -> String {
    let salt = random_hex(16);
    format!("pbkdf2-sha256${PBKDF2_ROUNDS}${salt}${}", derive(password, &salt, PBKDF2_ROUNDS))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [scheme, rounds, salt, hash] = parts[..] else { return false };
    let Ok(rounds) = rounds.parse::<u32>() else { return false };
    scheme == "pbkdf2-sha256" && ct_eq(derive(password, salt, rounds).as_bytes(), hash.as_bytes())
}

// ------------------------------------------------------------------ manager

struct Running {
    port: u16,
    bind: String,
    task: tokio::task::JoinHandle<()>,
}

/// A connected paired Mac.
pub(super) struct LiveLink {
    pub peer_id: String,
    pub mux: Arc<super::link::Mux>,
    pub addr: SocketAddr,
}

type TunnelKey = (String, String, u16);

/// The address paired Macs must connect through ("Show this Mac as"): the chosen name, the IPs of this Mac it
/// leads to, and whether they're all on a physical (local network) interface.
#[derive(Clone, Debug, Default)]
pub struct Via {
    pub address: String,
    pub ips: Vec<IpAddr>,
    pub lan: bool,
}

/// Owns the remote listener's lifecycle, its sessions and login throttling.
pub struct Remote {
    cfg: Mutex<RemoteConfig>,
    running: Mutex<Option<Running>>,
    error: Mutex<Option<String>>,
    sessions: Mutex<HashMap<String, (String, i64)>>,
    failures: Mutex<HashMap<IpAddr, (u32, i64)>>,
    /// Secret being enrolled, until confirmed with a valid code.
    totp_pending: Mutex<Option<String>>,
    /// Highest TOTP time step accepted so far; a code can't be used twice.
    totp_last_step: Mutex<u64>,
    /// Bumped whenever the listener or credentials change; open streams and stale routers stop serving.
    gen: watch::Sender<u64>,
    /// One-time pairing code shown to the user (code, expires at ms, wrong attempts).
    pair_code: Mutex<Option<(String, i64, u32)>>,
    /// The link listener (sharing).
    link: Mutex<Option<Running>>,
    pub(super) links: Mutex<Vec<LiveLink>>,
    /// Recent link / tunnel events, newest last.
    activity: Mutex<std::collections::VecDeque<Value>>,
    /// Tunnel use per (peer, service, port): connections, bytes, last at.
    /// (peer, service, port) → (connections, bytes, last used).
    tunnel_stats: Mutex<HashMap<TunnelKey, (u64, u64, i64)>>,
    /// `via()` result, re-resolved after a minute or when the chosen address changes.
    via: Mutex<Option<(Via, Instant)>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemotePatch {
    pub enabled: Option<bool>,
    pub web_remote: Option<bool>,
    pub sharing: Option<bool>,
    pub port: Option<u16>,
    pub link_port: Option<u16>,
    pub bind: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub allow_control: Option<bool>,
    /// Preferred address to show for this Mac; "" = automatic.
    pub share_address: Option<String>,
}

impl Remote {
    pub fn new() -> Arc<Self> {
        Arc::new(Remote {
            cfg: Mutex::new(load_config()),
            running: Mutex::new(None),
            error: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            totp_pending: Mutex::new(None),
            totp_last_step: Mutex::new(0),
            gen: watch::channel(0).0,
            pair_code: Mutex::new(None),
            link: Mutex::new(None),
            links: Mutex::new(vec![]),
            activity: Mutex::new(Default::default()),
            tunnel_stats: Mutex::new(HashMap::new()),
            via: Mutex::new(None),
        })
    }

    /// New one-time code another machine can use to pair with this one (valid 10 minutes).
    pub fn new_pair_code(&self) -> Result<Value, ApiErr> {
        let cfg = self.config();
        if !cfg.sharing() {
            return Err(ApiErr::new(400, "turn on sharing first (lstack remote share on) so other Macs can reach this one"));
        }
        super::link::identity().map_err(|e| ApiErr::new(500, e))?;
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let raw = random_hex(8);
        let code: String = raw.as_bytes().chunks(2).map(|c| ALPHABET[u8::from_str_radix(std::str::from_utf8(c).unwrap_or("0"), 16).unwrap_or(0) as usize % ALPHABET.len()] as char).collect();
        let exp = now_ms() + PAIR_CODE_TTL_MS;
        *self.pair_code.lock().unwrap() = Some((code.clone(), exp, 0));
        let code = format!("{}-{}", &code[..4], &code[4..]);
        let urls = self.status()["shareUrls"].clone();
        // Address + code in one string to paste on the other Mac, once an address is chosen to connect through.
        let link = cfg.share_address.as_ref().and(urls[0].as_str()).map(|u| super::peers::quick_link(u, &code));
        Ok(json!({ "code": code, "expiresAt": exp, "urls": urls, "link": link, "totp": !cfg.totp_secret.is_empty() }))
    }

    pub(super) fn current_pair_code(&self) -> Option<String> {
        let mut pc = self.pair_code.lock().unwrap();
        match &*pc {
            Some((c, exp, _)) if *exp > now_ms() => Some(c.clone()),
            _ => {
                *pc = None;
                None
            }
        }
    }

    /// A pairing attempt with the current code failed; burn the code after a few.
    pub(super) fn pair_code_failed(&self) {
        let mut pc = self.pair_code.lock().unwrap();
        if let Some((_, _, n)) = pc.as_mut() {
            *n += 1;
            if *n >= PAIR_CODE_TRIES {
                *pc = None;
                println!("{} remote: pairing code burned after {PAIR_CODE_TRIES} failed attempts", paths::iso(now_ms()));
            }
        }
    }

    pub(super) fn pair_code_used(&self) {
        *self.pair_code.lock().unwrap() = None;
    }

    pub(super) fn locked_out(&self, ip: IpAddr) -> bool {
        let now = now_ms();
        let mut f = self.failures.lock().unwrap();
        f.retain(|_, (_, last)| now - *last < LOCKOUT_MS);
        f.get(&ip).is_some_and(|(n, _)| *n >= MAX_FAILURES)
    }

    pub(super) fn failed(&self, ip: IpAddr) -> u32 {
        let now = now_ms();
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert((0, now));
        *e = (e.0 + 1, now);
        e.0
    }

    pub(super) fn succeeded(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }

    /// Whether two-factor authentication (Settings → Security) is set up.
    pub fn totp_enabled(&self) -> bool {
        !self.config().totp_secret.is_empty()
    }

    /// Check the 2FA code a pairing Mac sent inside the code-authenticated exchange (always ok when 2FA is off).
    pub(super) fn check_pair_totp(&self, code: Option<&str>) -> bool {
        let cfg = self.config();
        cfg.totp_secret.is_empty() || self.totp_check(&cfg.totp_secret, code)
    }

    /// Seconds until a locked-out address may try again.
    pub(super) fn lockout_left_secs(&self, ip: IpAddr) -> i64 {
        let f = self.failures.lock().unwrap();
        f.get(&ip).map_or(0, |(_, last)| ((LOCKOUT_MS - (now_ms() - last)) / 1000).max(1))
    }

    /// The chosen "Show this Mac as" address and where it leads on this Mac; None when it's automatic
    /// (local network only) or the address doesn't lead here (then the automatic rule applies too).
    pub async fn via(&self) -> Option<Via> {
        let address = self.config().share_address?;
        if let Some((v, at)) = &*self.via.lock().unwrap() {
            if v.address == address && at.elapsed() < Duration::from_secs(60) {
                return (!v.ips.is_empty()).then(|| v.clone());
            }
        }
        let a = address.clone();
        let v = tokio::time::timeout(Duration::from_secs(3), tokio::task::spawn_blocking(move || resolve_via(&a)))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(Via { address, ips: vec![], lan: false });
        *self.via.lock().unwrap() = Some((v.clone(), Instant::now()));
        (!v.ips.is_empty()).then_some(v)
    }

    /// Addresses a paired Mac can reach this one at: the chosen one, or every local-network one.
    pub fn lan_addrs(&self) -> Vec<String> {
        self.status()["shareUrls"].as_array().map(|a| a.iter().filter_map(|u| u.as_str()).map(str::to_string).collect()).unwrap_or_default()
    }

    fn save(&self, cfg: RemoteConfig) -> Result<(), ApiErr> {
        save_config(&cfg).map_err(|e| ApiErr::new(500, format!("cannot save remote.json: {e}")))?;
        *self.cfg.lock().unwrap() = cfg;
        Ok(())
    }

    /// Pin a newly paired Mac. A Mac pairing again under the same name or key replaces its old entry
    /// (keeping its grants if the key is the same).
    pub(super) fn add_peer(&self, name: &str, public: &[u8; 32], phrase: &str) -> Result<SharedPeer, ApiErr> {
        let mut cfg = self.config();
        let key = super::link::key_hex(public);
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        let old: Vec<String> = cfg.peers.iter().filter(|p| p.public_key == key || p.name == name).map(|p| p.id.clone()).collect();
        let grants = cfg.peers.iter().find(|p| p.public_key == key).map(|p| p.grants.clone()).unwrap_or_else(Grants::new_pairing);
        cfg.peers.retain(|p| !old.contains(&p.id));
        cfg.revoked.retain(|k| *k != key);
        let peer = SharedPeer { id: random_hex(4), name, public_key: key, grants, phrase: phrase.to_string(), created_at: now_ms(), last_seen: None };
        cfg.peers.push(peer.clone());
        self.save(cfg)?;
        for id in old {
            self.drop_links(&id);
        }
        Ok(peer)
    }

    pub(super) fn touch_peer(&self, id: &str) {
        let mut cfg = self.cfg.lock().unwrap();
        if let Some(p) = cfg.peers.iter_mut().find(|p| p.id == id) {
            p.last_seen = Some(now_ms());
            let snapshot = cfg.clone();
            drop(cfg);
            let _ = save_config(&snapshot);
        }
    }

    /// Close every link (and so every stream and tunnel) of one paired Mac.
    fn drop_links(&self, id: &str) {
        for l in self.links.lock().unwrap().iter().filter(|l| l.peer_id == id) {
            l.mux.shutdown();
        }
    }

    /// Close a paired Mac's links telling it the pairing is gone, so it forgets this Mac too.
    fn bye_links(&self, id: &str) -> bool {
        let links = self.links.lock().unwrap();
        let mut told = false;
        for l in links.iter().filter(|l| l.peer_id == id) {
            told |= !l.mux.is_closed();
            l.mux.bye(super::link::BYE_REVOKED);
        }
        told
    }

    pub(super) fn is_revoked(&self, key: &[u8; 32]) -> bool {
        let key = super::link::key_hex(key);
        self.cfg.lock().unwrap().revoked.contains(&key)
    }

    /// A revoked Mac connected again and was told; no need to remember its key any longer.
    pub(super) fn revocation_delivered(&self, key: &[u8; 32]) {
        let key = super::link::key_hex(key);
        let mut cfg = self.config();
        if cfg.revoked.contains(&key) {
            cfg.revoked.retain(|k| *k != key);
            let _ = self.save(cfg);
        }
    }

    /// The paired Mac removed this one on its side (it said so over the link): forget it here too.
    pub(super) fn unpaired_by_peer(&self, id: &str) {
        let mut cfg = self.config();
        let Ok(i) = Self::find_peer(&cfg, id) else { return };
        let p = cfg.peers.remove(i);
        if self.save(cfg).is_ok() {
            self.drop_links(&p.id);
            self.audit(json!({ "event": "unpaired", "peer": p.name, "reason": "removed on that Mac" }));
        }
    }

    fn find_peer(cfg: &RemoteConfig, id: &str) -> Result<usize, ApiErr> {
        cfg.peers.iter().position(|p| p.id == id || p.name == id).ok_or_else(|| ApiErr::new(404, format!("no paired machine {id:?}")))
    }

    /// Paired Macs with their grants and whether they're connected now.
    pub fn shared_peers(&self) -> Value {
        let cfg = self.config();
        let links = self.links.lock().unwrap();
        json!(cfg
            .peers
            .iter()
            .map(|p| {
                let live: Vec<&LiveLink> = links.iter().filter(|l| l.peer_id == p.id && !l.mux.is_closed()).collect();
                json!({
                    "id": p.id,
                    "name": p.name,
                    "fingerprint": super::link::unhex32(&p.public_key).map(|k| super::link::fingerprint(&k)),
                    "phrase": p.phrase,
                    "grants": p.grants.json(),
                    "createdAt": p.created_at,
                    "lastSeen": p.last_seen,
                    "online": !live.is_empty(),
                    "address": live.first().map(|l| l.addr.ip().to_string()),
                    "streams": live.iter().map(|l| l.mux.stream_count()).sum::<usize>(),
                    "connectedSince": live.iter().map(|l| l.mux.opened_at).min(),
                })
            })
            .collect::<Vec<_>>())
    }

    /// Change what a paired Mac may do: `{control: bool, tunnel: "all" | "none" | [services]}`.
    /// Its link is reset so open streams that lost access close.
    pub fn set_grants(&self, id: &str, patch: &Value) -> Result<Value, ApiErr> {
        let mut cfg = self.config();
        let i = Self::find_peer(&cfg, id)?;
        let g = &mut cfg.peers[i].grants;
        if let Some(c) = patch.get("control") {
            g.control = c.as_bool().ok_or_else(|| ApiErr::new(400, "control must be true or false"))?;
        }
        match patch.get("tunnel") {
            None => {}
            Some(Value::String(s)) if s == "all" => (g.tunnel_all, g.tunnel) = (true, vec![]),
            Some(Value::String(s)) if s == "none" => (g.tunnel_all, g.tunnel) = (false, vec![]),
            Some(Value::Array(a)) => {
                let list: Option<Vec<String>> = a.iter().map(|v| v.as_str().map(String::from)).collect();
                (g.tunnel_all, g.tunnel) = (false, list.ok_or_else(|| ApiErr::new(400, "tunnel must list service names"))?);
                g.tunnel.sort();
                g.tunnel.dedup();
            }
            Some(_) => return Err(ApiErr::new(400, "tunnel must be \"all\", \"none\" or a list of services")),
        }
        let (pid, name, grants) = (cfg.peers[i].id.clone(), cfg.peers[i].name.clone(), cfg.peers[i].grants.json());
        self.save(cfg)?;
        self.drop_links(&pid);
        self.audit(json!({ "event": "grants", "peer": name, "grants": grants }));
        Ok(self.shared_peers())
    }

    /// Allow (or stop) sending a paired Mac the cached values of these exact references. Doesn't reset its link.
    pub fn set_secret_grants(&self, id: &str, refs: &[String], allow: bool) -> Result<(String, Vec<String>), ApiErr> {
        let mut cfg = self.config();
        let i = Self::find_peer(&cfg, id)?;
        let g = &mut cfg.peers[i].grants;
        for r in refs {
            g.secrets.retain(|x| x != r);
            if allow {
                g.secrets.push(r.clone());
            } else {
                g.push.retain(|x| x != r);
            }
        }
        g.secrets.sort();
        let list = g.secrets.clone();
        let name = cfg.peers[i].name.clone();
        self.save(cfg)?;
        self.audit(json!({ "event": if allow { "secrets-shared" } else { "secrets-unshared" }, "peer": name, "refs": refs }));
        Ok((name, list))
    }

    /// Offer (`add`) these shared references to a paired Mac for it to accept, or forget the offer (it
    /// accepted, declined, or already holds them). Unchanged lists aren't saved.
    pub fn set_secret_push(&self, id: &str, refs: &[String], add: bool) -> Result<Vec<String>, ApiErr> {
        let mut cfg = self.config();
        let i = Self::find_peer(&cfg, id)?;
        let g = &mut cfg.peers[i].grants;
        let before = g.push.clone();
        for r in refs {
            g.push.retain(|x| x != r);
            if add && g.secrets.contains(r) {
                g.push.push(r.clone());
            }
        }
        g.push.sort();
        let list = g.push.clone();
        if list != before {
            let name = cfg.peers[i].name.clone();
            self.save(cfg)?;
            if add {
                self.audit(json!({ "event": "secrets-pushed", "peer": name, "refs": refs }));
            }
        }
        Ok(list)
    }

    /// Paired Mac by id or name.
    pub fn peer(&self, id: &str) -> Option<SharedPeer> {
        let cfg = self.config();
        Self::find_peer(&cfg, id).ok().map(|i| cfg.peers[i].clone())
    }

    pub fn revoke_peer(&self, id: &str) -> Result<(), ApiErr> {
        let mut cfg = self.config();
        let i = Self::find_peer(&cfg, id)?;
        let p = cfg.peers.remove(i);
        let pending = !self.links.lock().unwrap().iter().any(|l| l.peer_id == p.id && !l.mux.is_closed());
        if pending {
            // Offline: tell it the next time it tries to connect.
            cfg.revoked.retain(|k| *k != p.public_key);
            cfg.revoked.push(p.public_key.clone());
            let n = cfg.revoked.len();
            cfg.revoked.drain(..n.saturating_sub(64));
        }
        self.save(cfg)?;
        self.bye_links(&p.id);
        self.audit(json!({ "event": "revoked", "peer": p.name, "told": !pending }));
        Ok(())
    }

    /// Forget every pairing (after this Mac's key changed, they can't connect anyway).
    pub fn forget_peers(&self) {
        let mut cfg = self.config();
        let ids: Vec<String> = cfg.peers.drain(..).map(|p| p.id).collect();
        let _ = self.save(cfg);
        for id in ids {
            self.bye_links(&id);
        }
    }

    /// Record a link / tunnel event (never payload data).
    pub(super) fn audit(&self, mut ev: Value) {
        let line = ev
            .as_object()
            .map(|o| o.iter().map(|(k, v)| format!("{k}={}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        println!("{} share: {line}", paths::iso(now_ms()));
        ev["at"] = json!(now_ms());
        let mut a = self.activity.lock().unwrap();
        a.push_back(ev);
        while a.len() > 300 {
            a.pop_front();
        }
    }

    pub(super) fn count_tunnel(&self, peer: &str, service: &str, port: u16, bytes: u64) {
        let mut t = self.tunnel_stats.lock().unwrap();
        let e = t.entry((peer.to_string(), service.to_string(), port)).or_insert((0, 0, 0));
        *e = (e.0 + 1, e.1 + bytes, now_ms());
    }

    /// Recent link / tunnel events (newest first) and tunnel totals since the daemon started.
    pub fn activity(&self) -> Value {
        let mut tunnels: Vec<Value> = self
            .tunnel_stats
            .lock()
            .unwrap()
            .iter()
            .map(|((peer, service, port), (n, bytes, at))| json!({ "peer": peer, "service": service, "port": port, "connections": n, "bytes": bytes, "lastAt": at }))
            .collect();
        tunnels.sort_by_key(|t| -t["lastAt"].as_i64().unwrap_or(0));
        json!({ "events": self.activity.lock().unwrap().iter().rev().cloned().collect::<Vec<_>>(), "tunnels": tunnels })
    }

    fn generation(&self) -> u64 {
        *self.gen.borrow()
    }

    fn bump(&self) {
        self.gen.send_modify(|g| *g += 1);
    }

    pub fn config(&self) -> RemoteConfig {
        self.cfg.lock().unwrap().clone()
    }

    pub fn status(&self) -> Value {
        let cfg = self.config();
        let running = self.running.lock().unwrap();
        let network: Vec<String> = match &*running {
            Some(r) if r.bind == "0.0.0.0" || r.bind == "::" => {
                local_addresses().into_iter().map(|ip| format!("http://{ip}:{}", r.port)).collect()
            }
            Some(r) => vec![format!("http://{}:{}", r.bind, r.port)],
            None => vec![],
        };
        // Dashboard links: network addresses when other devices may use it, else this Mac's.
        let urls: Vec<String> = match &*running {
            Some(r) if cfg.enabled && !cfg.web_remote() => vec![format!("http://127.0.0.1:{}", r.port)],
            Some(_) if cfg.enabled => network.clone(),
            _ => vec![],
        };
        let link = self.link.lock().unwrap();
        let share_urls: Vec<String> = match &*link {
            Some(l) if l.bind == "0.0.0.0" || l.bind == "::" => {
                // Only local-network addresses: the link refuses connections from anywhere else.
                if let Some(a) = &cfg.share_address {
                    // The chosen address is the only way in.
                    vec![format!("{}:{}", host_for_url(a), l.port)]
                } else {
                    let mut v: Vec<String> = local_addresses().into_iter().filter(|ip| !is_tailscale_host(ip)).map(|ip| format!("{ip}:{}", l.port)).collect();
                    // Names work too (router DNS like `studio.lan`, or Bonjour `studio.local`) and survive DHCP changes.
                    let h = hostname();
                    if !v.is_empty() && h != "localhost" {
                        v.push(format!("{}:{}", if h.contains('.') { h } else { format!("{h}.local") }, l.port));
                    }
                    v
                }
            }
            Some(l) => vec![format!("{}:{}", l.bind, l.port)],
            None => vec![],
        };
        json!({
            "enabled": cfg.enabled,
            "webRemote": cfg.web_remote(),
            "sharing": cfg.sharing(),
            "shareUrls": share_urls,
            "shareAddress": cfg.share_address,
            "linkPort": cfg.link_port,
            "pairedMachines": cfg.peers.len(),
            "running": running.is_some(),
            "port": cfg.port,
            "bind": running.as_ref().map_or(cfg.bind.clone(), |r| r.bind.clone()),
            "username": cfg.username,
            "hasPassword": !cfg.password_hash.is_empty(),
            "allowControl": cfg.allow_control,
            "totp": !cfg.totp_secret.is_empty(),
            "hostname": hostname(),
            "urls": urls,
            "error": *self.error.lock().unwrap(),
        })
    }

    /// Apply a settings change from the local API (or CLI), persist it and (re)start/stop the listener.
    pub async fn update(self: &Arc<Self>, app: &AppState, patch: RemotePatch) -> Result<Value, ApiErr> {
        let mut cfg = self.config();
        let old = cfg.clone();
        if let Some(u) = patch.username {
            let u = u.trim().to_string();
            if u.is_empty() || u.len() > 64 {
                return Err(ApiErr::new(400, "username must be 1–64 characters"));
            }
            cfg.username = u;
        }
        if let Some(p) = patch.password {
            if p.chars().count() < 8 {
                return Err(ApiErr::new(400, "password must be at least 8 characters"));
            }
            cfg.password_hash = tokio::task::spawn_blocking(move || hash_password(&p)).await.unwrap_or_default();
        }
        if let Some(p) = patch.port {
            if p < 1024 {
                return Err(ApiErr::new(400, "port must be 1024 or higher"));
            }
            cfg.port = p;
        }
        if let Some(p) = patch.link_port {
            if p < 1024 {
                return Err(ApiErr::new(400, "link port must be 1024 or higher"));
            }
            cfg.link_port = p;
        }
        if let Some(b) = patch.bind {
            if b.parse::<IpAddr>().is_err() {
                return Err(ApiErr::new(400, format!("invalid bind address {b:?}")));
            }
            cfg.bind = b;
        }
        if let Some(c) = patch.allow_control {
            cfg.allow_control = c;
        }
        if let Some(a) = patch.share_address {
            let a = a.trim().trim_end_matches('.').to_ascii_lowercase();
            cfg.share_address = if a.is_empty() || a == "auto" {
                None
            } else if valid_address(&a) {
                Some(a)
            } else {
                return Err(ApiErr::new(400, format!("{a:?} is not a host name or IP address (no port)")));
            };
        }
        if let Some(e) = patch.enabled {
            cfg.enabled = e;
        }
        if let Some(w) = patch.web_remote {
            cfg.web_remote = Some(w);
        }
        if let Some(sh) = patch.sharing {
            cfg.sharing = Some(sh);
        }
        if cfg.enabled && cfg.password_hash.is_empty() {
            return Err(ApiErr::new(400, "set a password before turning on the web dashboard"));
        }
        if cfg.enabled && cfg.port == cfg.link_port {
            return Err(ApiErr::new(400, "the web dashboard and the machine link need different ports"));
        }
        save_config(&cfg).map_err(|e| ApiErr::new(500, format!("cannot save remote.json: {e}")))?;
        let creds_changed = cfg.username != old.username || cfg.password_hash != old.password_hash;
        let via_changed = cfg.share_address != old.share_address;
        *self.cfg.lock().unwrap() = cfg;
        if via_changed {
            // Links that came in another way reconnect through the new address (or are refused).
            *self.via.lock().unwrap() = None;
            for l in self.links.lock().unwrap().drain(..) {
                l.mux.shutdown();
            }
        }
        if creds_changed {
            self.sessions.lock().unwrap().clear();
        }
        self.apply(app).await;
        Ok(self.status())
    }

    /// Make the listeners match the config (called at boot and after every change).
    pub async fn apply(self: &Arc<Self>, app: &AppState) {
        self.apply_link(app).await;
        let cfg = self.config();
        let old = self.running.lock().unwrap().take();
        self.bump();
        if let Some(r) = old {
            // Wait for the old listener to be dropped so its port can be bound again.
            r.task.abort();
            let _ = r.task.await;
        }
        *self.error.lock().unwrap() = None;
        if !cfg.enabled {
            self.sessions.lock().unwrap().clear();
        }
        if !cfg.listening() {
            return;
        }
        let bind = cfg.bind_addr();
        let addr = format!("{bind}:{}", cfg.port);
        let mut bound = tokio::net::TcpListener::bind(&addr).await;
        for _ in 0..10 {
            match &bound {
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    bound = tokio::net::TcpListener::bind(&addr).await;
                }
                _ => break,
            }
        }
        let listener = match bound {
            Ok(l) => l,
            Err(e) => {
                let msg = format!("cannot listen on {addr}: {e}");
                println!("{} remote: {msg}", paths::iso(now_ms()));
                *self.error.lock().unwrap() = Some(msg);
                return;
            }
        };
        let gen = self.generation();
        let app_router = router(app.clone(), self.clone(), gen);
        let task = tokio::spawn(async move {
            let svc = app_router.into_make_service_with_connect_info::<SocketAddr>();
            let _ = axum::serve(listener, svc).await;
        });
        println!("{} remote: web dashboard listening on http://{addr}", paths::iso(now_ms()));
        *self.running.lock().unwrap() = Some(Running { port: cfg.port, bind, task });
    }

    /// Start, keep or stop the link listener; stopping it drops every connected Mac.
    async fn apply_link(self: &Arc<Self>, app: &AppState) {
        let cfg = self.config();
        let want = cfg.sharing().then(|| (cfg.bind.clone(), cfg.link_port));
        {
            let cur = self.link.lock().unwrap();
            let same = want.as_ref() == cur.as_ref().map(|r| (r.bind.clone(), r.port)).as_ref();
            if same && (want.is_none() || cur.as_ref().is_some_and(|r| !r.task.is_finished())) {
                return;
            }
        }
        let old = self.link.lock().unwrap().take();
        if let Some(r) = old {
            r.task.abort();
            let _ = r.task.await;
        }
        for l in self.links.lock().unwrap().drain(..) {
            l.mux.shutdown();
        }
        let Some((bind, port)) = want else { return };
        if let Err(e) = super::link::identity() {
            *self.error.lock().unwrap() = Some(e);
            return;
        }
        let addr = format!("{bind}:{port}");
        let mut bound = tokio::net::TcpListener::bind(&addr).await;
        for _ in 0..10 {
            match &bound {
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    bound = tokio::net::TcpListener::bind(&addr).await;
                }
                _ => break,
            }
        }
        match bound {
            Ok(l) => {
                println!("{} remote: sharing — machine link listening on {addr}", paths::iso(now_ms()));
                let task = tokio::spawn(super::share::serve(l, app.clone(), self.clone()));
                *self.link.lock().unwrap() = Some(Running { port, bind, task });
            }
            Err(e) => {
                let msg = format!("cannot listen on {addr}: {e}");
                println!("{} remote: {msg}", paths::iso(now_ms()));
                *self.error.lock().unwrap() = Some(msg);
            }
        }
    }


    /// Start two-factor enrollment: returns a fresh secret and its otpauth:// URI. Login is unaffected
    /// until `totp_confirm` sees a valid code for it.
    pub fn totp_begin(&self) -> Value {
        let secret = totp::new_secret();
        let uri = totp::uri(&secret, &hostname(), "LStack");
        *self.totp_pending.lock().unwrap() = Some(secret.clone());
        json!({ "secret": secret, "uri": uri })
    }

    pub fn totp_confirm(&self, code: &str) -> Result<Value, ApiErr> {
        let secret = self.totp_pending.lock().unwrap().clone().ok_or_else(|| ApiErr::new(400, "start two-factor setup first"))?;
        let step = totp::verify(&secret, code, totp::current_step()).ok_or_else(|| ApiErr::new(400, "that code isn't valid — check the time on your phone and try the next one"))?;
        let mut cfg = self.config();
        cfg.totp_secret = secret;
        save_config(&cfg).map_err(|e| ApiErr::new(500, format!("cannot save remote.json: {e}")))?;
        *self.cfg.lock().unwrap() = cfg;
        *self.totp_pending.lock().unwrap() = None;
        *self.totp_last_step.lock().unwrap() = step;
        self.sessions.lock().unwrap().clear();
        println!("{} remote: two-factor authentication enabled", paths::iso(now_ms()));
        Ok(self.status())
    }

    pub fn totp_disable(&self) -> Result<Value, ApiErr> {
        let mut cfg = self.config();
        *self.totp_pending.lock().unwrap() = None;
        if cfg.totp_secret.is_empty() {
            return Ok(self.status());
        }
        cfg.totp_secret.clear();
        save_config(&cfg).map_err(|e| ApiErr::new(500, format!("cannot save remote.json: {e}")))?;
        *self.cfg.lock().unwrap() = cfg;
        println!("{} remote: two-factor authentication disabled", paths::iso(now_ms()));
        Ok(self.status())
    }

    /// Checks a login code and consumes its time step.
    fn totp_check(&self, secret: &str, code: Option<&str>) -> bool {
        let Some(step) = code.and_then(|c| totp::verify(secret, c, totp::current_step())) else { return false };
        let mut last = self.totp_last_step.lock().unwrap();
        if step <= *last {
            return false;
        }
        *last = step;
        true
    }

    pub fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            r.task.abort();
        }
        if let Some(r) = self.link.lock().unwrap().take() {
            r.task.abort();
        }
        for l in self.links.lock().unwrap().drain(..) {
            l.mux.shutdown();
        }
        self.bump();
    }

    fn session_user(&self, cookie_header: Option<&str>) -> Option<String> {
        let token = cookie_header?
            .split(';')
            .find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))?;
        let mut sessions = self.sessions.lock().unwrap();
        let now = now_ms();
        sessions.retain(|_, (_, exp)| *exp > now);
        sessions.get(&token).map(|(u, _)| u.clone())
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if !ok {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// A host name or IP address (without a port).
fn valid_address(a: &str) -> bool {
    if a.parse::<IpAddr>().is_ok() {
        return true;
    }
    a.len() <= 253
        && a.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

fn host_for_url(a: &str) -> String {
    if a.contains(':') { format!("[{a}]") } else { a.to_string() }
}

/// (address, netmask) of every interface, with its name.
fn interface_nets() -> Vec<(String, IpAddr, IpAddr)> {
    let mut out = vec![];
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && ifa.ifa_flags & (libc::IFF_UP as u32) != 0 {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                match (*ifa.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                        out.push((name, IpAddr::V4(u32::from_be(a.sin_addr.s_addr).into()), IpAddr::V4(u32::from_be(m.sin_addr.s_addr).into())));
                    }
                    libc::AF_INET6 => {
                        let a = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in6);
                        out.push((name, IpAddr::V6(a.sin6_addr.s6_addr.into()), IpAddr::V6(m.sin6_addr.s6_addr.into())));
                    }
                    _ => {}
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

fn same_subnet(a: IpAddr, b: IpAddr, mask: IpAddr) -> bool {
    match (a, b, mask) {
        (IpAddr::V4(a), IpAddr::V4(b), IpAddr::V4(m)) => u32::from(a) & u32::from(m) == u32::from(b) & u32::from(m),
        (IpAddr::V6(a), IpAddr::V6(b), IpAddr::V6(m)) => u128::from(a) & u128::from(m) == u128::from(b) & u128::from(m),
        _ => false,
    }
}

/// Where `address` (a name or IP) leads on this Mac.
fn resolve_via(address: &str) -> Via {
    use std::net::ToSocketAddrs;
    let found: Vec<IpAddr> = match address.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => (address, 0).to_socket_addrs().map(|a| a.map(|a| a.ip()).collect()).unwrap_or_default(),
    };
    let nets = interface_nets();
    let mut ips: Vec<IpAddr> = found.into_iter().map(unmap).filter(|ip| nets.iter().any(|(_, a, _)| a == ip)).collect();
    ips.sort();
    ips.dedup();
    let lan = !ips.is_empty() && ips.iter().all(|ip| !is_tailscale_ip(*ip) && nets.iter().any(|(n, a, _)| a == ip && n.starts_with("en")));
    Via { address: address.to_string(), ips, lan }
}

pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v => v,
    }
}

/// CGNAT 100.64/10 and fd7a:115c:a1e0::/48 (used by Tailscale and other overlay VPNs): never the local network.
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.octets()[0] == 100 && (64..128).contains(&v.octets()[1]),
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// `host[:port]` on an overlay VPN (100.64/10, Tailscale's IPv6 range, or a MagicDNS name).
pub fn is_tailscale_host(hp: &str) -> bool {
    let host = hp.rsplit_once(':').filter(|(h, p)| p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']'))).map_or(hp, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.ends_with(".ts.net") || host.parse::<IpAddr>().is_ok_and(is_tailscale_ip)
}

/// May a TCP link connection from `ip` count as "this local network"? Loopback, IPv6 link-local, or an
/// address inside the subnet of a physical interface (`en*`). Being a private (RFC 1918) address isn't enough:
/// a VPN hands those out too, and its whole network mustn't count as the LAN. Tailscale ranges never count.
pub fn is_local_source(ip: IpAddr) -> bool {
    local_source_in(ip, &interface_nets())
}

fn local_source_in(ip: IpAddr, nets: &[(String, IpAddr, IpAddr)]) -> bool {
    let ip = match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v => v,
    };
    if ip.is_loopback() {
        return true;
    }
    if is_tailscale_ip(ip) {
        return false;
    }
    if let IpAddr::V6(v) = ip {
        if v.segments()[0] & 0xffc0 == 0xfe80 {
            return true;
        }
    }
    nets.iter().any(|(name, addr, mask)| name.starts_with("en") && !is_tailscale_ip(*addr) && same_subnet(ip, *addr, *mask))
}

fn is_tunnel_if(name: &str) -> bool {
    ["utun", "ipsec", "ppp"].iter().any(|p| name.starts_with(p))
}

/// The interface carrying the default route, from `route -n get default`.
fn default_route_interface(route_out: &str) -> Option<String> {
    route_out.lines().find_map(|l| l.trim().strip_prefix("interface:")).map(|i| i.trim().to_string()).filter(|i| !i.is_empty())
}

/// A VPN that's on: a tunnel interface with an IPv4 address (macOS's own utuns only carry IPv6 link-local).
/// Full tunnel when the default route goes through it. The Tailscale app's tunnel (100.64/10) is reported
/// separately: it doesn't block the local network. LStack's own helper adds no interface at all.
fn vpn_from(default_if: Option<&str>, nets: &[(String, IpAddr, IpAddr)]) -> Value {
    let tunnels: Vec<(&str, IpAddr)> = nets.iter().filter(|(n, a, _)| is_tunnel_if(n) && a.is_ipv4()).map(|(n, a, _)| (n.as_str(), *a)).collect();
    let tailscale_app = tunnels.iter().any(|(_, a)| is_tailscale_ip(*a));
    let vpns: Vec<&str> = tunnels.iter().filter(|(_, a)| !is_tailscale_ip(*a)).map(|(n, _)| *n).collect();
    let full = default_if.filter(|d| vpns.contains(d) || (is_tunnel_if(d) && !tunnels.iter().any(|(n, a)| n == d && is_tailscale_ip(*a))));
    let iface = full.or_else(|| vpns.first().copied());
    json!({ "active": iface.is_some(), "interface": iface, "fullTunnel": full.is_some(), "tailscaleApp": tailscale_app })
}

/// Whether a VPN is on (`{active, interface, fullTunnel, tailscaleApp}`).
pub fn vpn_status() -> Value {
    let out = std::process::Command::new("/sbin/route").args(["-n", "get", "default"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    vpn_from(default_route_interface(&out).as_deref(), &interface_nets())
}

/// Every IP on this Mac's interfaces (any family, loopback included) — to tell whether a name points here.
fn interface_ips() -> std::collections::HashSet<IpAddr> {
    let mut out = std::collections::HashSet::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() {
                match (*ifa.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        out.insert(IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        out.insert(IpAddr::V6(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// Reverse DNS (PTR) name of an IPv4 address, if the network's resolver has one.
fn reverse_dns(ip: std::net::Ipv4Addr) -> Option<String> {
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_addr.s_addr = u32::from(ip).to_be();
    let mut host = [0 as libc::c_char; 1025];
    let rc = unsafe {
        libc::getnameinfo(
            &sin as *const _ as *const libc::sockaddr,
            sin.sin_len as libc::socklen_t,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }.to_string_lossy().trim_end_matches('.').to_ascii_lowercase();
    (!name.is_empty() && !name.ends_with(".arpa") && name.parse::<IpAddr>().is_err()).then_some(name)
}

pub(super) fn scutil(args: &[&str]) -> String {
    std::process::Command::new("/usr/sbin/scutil").args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// Ways other Macs might reach this one, each checked like `dig`/`nslookup` would: LAN and VPN IPs,
/// the DNS host name, the Bonjour `.local` name, reverse-DNS names of the LAN IPs, and this Mac's
/// names under the network's search domains (router DNS such as `studio.lan`). Names are resolved
/// and marked `pointsHere` when they lead back to one of this Mac's addresses. Blocking (a few seconds).
pub fn discover_addresses(selected: Option<&str>) -> Vec<Value> {
    // Local network addresses, plus Tailscale's (a paired Mac may connect through either once it's chosen).
    let ips: Vec<String> = local_addresses();
    let here = interface_ips();
    // (name, how it was found)
    let mut names: Vec<(String, &str)> = vec![];
    let mut add = |n: String, how: &'static str| {
        let n = n.trim().trim_end_matches('.').to_ascii_lowercase();
        if valid_address(&n) && n != "localhost" && n.parse::<IpAddr>().is_err() && !names.iter().any(|(m, _)| *m == n) {
            names.push((n, how));
        }
    };
    let host = hostname();
    let local_host = scutil(&["--get", "LocalHostName"]).trim().to_string();
    if host.contains('.') {
        add(host.clone(), if host.ends_with(".local") { "bonjour" } else { "hostname" });
    }
    if !local_host.is_empty() {
        add(format!("{local_host}.local"), "bonjour");
    }
    let rev: Vec<Option<String>> = std::thread::scope(|s| {
        let hs: Vec<_> = ips.iter().filter_map(|ip| ip.parse::<std::net::Ipv4Addr>().ok()).map(|ip| s.spawn(move || reverse_dns(ip))).collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    for n in rev.into_iter().flatten() {
        add(n, "reverse dns");
    }
    let short: Vec<String> = [host.split('.').next().unwrap_or(""), &local_host].iter().filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase()).collect();
    let mut domains: Vec<String> = vec![];
    for line in scutil(&["--dns"]).lines() {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim().trim_end_matches('.').to_ascii_lowercase());
            if (k.starts_with("search domain") || k == "domain") && !v.is_empty() && !v.ends_with(".arpa") && v != "local" && !domains.contains(&v) {
                domains.push(v);
            }
        }
    }
    for d in domains.iter().take(6) {
        for s in &short {
            add(format!("{s}.{d}"), "search domain");
        }
    }
    if let Some(sel) = selected {
        add(sel.to_string(), "chosen");
    }
    // Forward lookups in parallel, each capped so an unresolvable name can't stall the list.
    let resolved: Vec<Vec<IpAddr>> = {
        let (tx, rx) = std::sync::mpsc::channel();
        for (i, (n, _)) in names.iter().enumerate() {
            let (tx, n) = (tx.clone(), n.clone());
            std::thread::spawn(move || {
                use std::net::ToSocketAddrs;
                let v: Vec<IpAddr> = (n.as_str(), 0).to_socket_addrs().map(|a| a.map(|a| a.ip()).collect()).unwrap_or_default();
                let _ = tx.send((i, v));
            });
        }
        drop(tx);
        let mut out = vec![vec![]; names.len()];
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while let Ok((i, mut v)) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            v.sort();
            v.dedup();
            out[i] = v;
        }
        out
    };
    let mut out: Vec<(u8, Value)> = vec![];
    for ((n, how), ips) in names.iter().zip(resolved) {
        let points_here = ips.iter().any(|ip| here.contains(ip));
        if !points_here && matches!(*how, "search domain" | "reverse dns") {
            continue; // only a guess, and it doesn't lead here
        }
        let kind = if is_tailscale_host(n) { "tailscale" } else if n.ends_with(".local") { "bonjour" } else { "dns" };
        let rank = if !points_here { 4 } else { [("dns", 0), ("bonjour", 1), ("tailscale", 3)].iter().find(|k| k.0 == kind).map_or(3, |k| k.1) };
        let ips: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
        out.push((rank, json!({ "address": n, "kind": kind, "source": how, "resolves": ips, "pointsHere": points_here })));
    }
    for ip in &ips {
        let ts = is_tailscale_host(ip);
        out.push((if ts { 3 } else { 2 }, json!({ "address": ip, "kind": if ts { "tailscale" } else { "ip" }, "source": "interface", "resolves": [ip], "pointsHere": true })));
    }
    if let Some(sel) = selected {
        if !out.iter().any(|(_, v)| v["address"] == sel) {
            let here = sel.parse::<IpAddr>().is_ok_and(|ip| here.contains(&ip));
            out.push((5, json!({ "address": sel, "kind": "ip", "source": "chosen", "resolves": [sel], "pointsHere": here })));
        }
    }
    out.sort_by_key(|(r, _)| *r);
    out.into_iter()
        .map(|(_, mut v)| {
            v["selected"] = json!(selected.is_some_and(|s| v["address"] == s));
            v
        })
        .collect()
}

/// IPv4 addresses other machines can plausibly reach (for showing URLs): physical interfaces
/// first, plus Tailscale-style VPN addresses (100.64/10). VM/container bridges are skipped.
pub fn local_addresses() -> Vec<String> {
    let mut out: Vec<(u8, String)> = vec![];
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return vec![];
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0 && ifa.ifa_flags & (libc::IFF_LOOPBACK as u32) == 0;
            if up && !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let [a, b, ..] = ip.octets();
                let cgnat = a == 100 && (64..128).contains(&b);
                let rank = if name.starts_with("en") {
                    Some(0)
                } else if cgnat {
                    Some(1)
                } else if ["bridge", "vmnet", "docker", "veth", "utun", "awdl", "llw", "anpi", "ap"]
                    .iter()
                    .any(|p| name.starts_with(p))
                {
                    None
                } else {
                    Some(2)
                };
                if let (Some(r), false) = (rank, ip.is_link_local()) {
                    out.push((r, ip.to_string()));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.sort();
    out.dedup();
    out.into_iter().map(|(_, ip)| ip).collect()
}

// ------------------------------------------------------------------ HTTP

/// Which paired Mac a link-served request comes from (set from its Noise key, never from the request).
#[derive(Clone)]
struct LinkPeer {
    id: String,
    mux: Arc<super::link::Mux>,
}

#[derive(Clone)]
struct RState {
    app: AppState,
    remote: Arc<Remote>,
    gen: u64,
    peer: Option<LinkPeer>,
}

/// The API a paired Mac uses, served over `http` streams of its link.
pub(super) fn peer_router(app: AppState, remote: Arc<Remote>, peer_id: String, mux: Arc<super::link::Mux>) -> Router {
    let rs = RState { app, gen: remote.generation(), remote, peer: Some(LinkPeer { id: peer_id, mux }) };
    Router::new()
        .route("/peer/api/info", get(peer_info))
        .route("/peer/api/services", get(list))
        .route("/peer/api/services/:name", get(get_one))
        .route("/peer/api/services/:name/logs", get(logs))
        .route("/peer/api/services/:name/logs/stream", get(log_stream))
        .route("/peer/api/services/:name/metrics", get(metrics))
        .route("/peer/api/services/:name/:action", post(peer_action))
        .route("/peer/api/system", get(|State(rs): State<RState>| async move { Json(rs.app.sup.system()) }))
        .route("/peer/api/processes", get(|State(rs): State<RState>| async move { Json(json!({ "processes": *rs.app.sup.processes() })) }))
        .route("/peer/api/events", get(peer_events))
        .route("/peer/api/secrets/offer", post(peer_secrets_offer))
        .route("/peer/api/secrets/pull", post(peer_secrets_pull))
        .route("/peer/api/secrets/needs", post(peer_secrets_needs))
        .route("/peer/api/secrets/deliver", post(peer_secrets_deliver))
        .fallback(|| async { ApiErr::new(404, "not found") })
        .layer(middleware::from_fn_with_state(rs.clone(), require_link))
        .with_state(rs)
}

fn router(app: AppState, remote: Arc<Remote>, gen: u64) -> Router {
    let rs = RState { app, remote, gen, peer: None };
    let api = Router::new()
        .route("/api/session", get(session))
        .route("/api/services", get(list))
        .route("/api/services/:name", get(get_one))
        .route("/api/services/:name/logs", get(logs))
        .route("/api/services/:name/logs/stream", get(log_stream))
        .route("/api/services/:name/metrics", get(metrics))
        .route("/api/services/:name/:action", post(action))
        .route("/api/events", get(events))
        .route("/api/logout", post(logout))
        .route("/api/system", get(|State(rs): State<RState>| async move { Json(rs.app.sup.system()) }))
        .route("/api/processes", get(|State(rs): State<RState>| async move { server::processes(State(rs.app)).await.into_response() }))
        .route("/api/machines", get(|State(rs): State<RState>| async move { server::machines_list(State(rs.app)).await.into_response() }))
        .route("/api/machines/:m", get(|State(rs): State<RState>, p: Path<String>| async move { server::machine_get(State(rs.app), p).await.into_response() }))
        .route("/api/machines/:m/services/:name", get(m_service))
        .route("/api/machines/:m/services/:name/logs", get(|State(rs): State<RState>, p: Path<(String, String)>, q: Q| async move { server::m_logs(State(rs.app), p, q).await.into_response() }))
        .route("/api/machines/:m/services/:name/logs/stream", get(|State(rs): State<RState>, p: Path<(String, String)>, q: Q| async move { server::m_log_stream(State(rs.app), p, q).await.into_response() }))
        .route("/api/machines/:m/services/:name/metrics", get(|State(rs): State<RState>, p: Path<(String, String)>, q: Q| async move { server::m_metrics(State(rs.app), p, q).await.into_response() }))
        .route(
            "/api/machines/:m/services/:name/:action",
            post(m_action),
        )
        .layer(middleware::from_fn_with_state(rs.clone(), require_session));
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", INDEX_HTML.as_bytes()) }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS.as_bytes()) }))
        .route("/app.css", get(|| async { asset("text/css; charset=utf-8", APP_CSS.as_bytes()) }))
        .route("/icon.png", get(|| async { asset("image/png", ICON_PNG) }))
        .route("/api/login", get(login_info).post(login))
        .merge(api)
        .fallback(|| async { ApiErr::new(404, "not found") })
        .layer(middleware::from_fn_with_state(rs.clone(), guard))
        .with_state(rs)
}

fn asset(ctype: &'static str, body: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

/// Outer layer: refuse to serve after the listener was replaced, require a CSRF header on writes,
/// add security headers, and cut long-lived streams when remote access is turned off.
async fn guard(State(rs): State<RState>, req: Request, next: Next) -> Response {
    if rs.remote.generation() != rs.gen {
        return ApiErr::new(503, "remote access disabled").into_response();
    }
    let ip = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    if let Some((code, msg)) = rs.remote.config().admit(req.uri().path(), ip) {
        return ApiErr::new(code, msg).into_response();
    }
    if req.method() != Method::GET && req.headers().get("x-lstack").is_none() {
        return ApiErr::new(403, "missing X-LStack header").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'"),
    );
    let is_stream = resp.headers().get(header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    if !is_stream {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let mut data = body.into_data_stream();
    let mut gen = rs.remote.gen.subscribe();
    let (tx, rx) = mpsc::channel::<Result<Bytes, axum::Error>>(64);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                chunk = data.next() => match chunk {
                    Some(c) => if tx.send(c).await.is_err() { break },
                    None => break,
                },
                _ = gen.changed() => break,
            }
        }
    });
    Response::from_parts(parts, Body::from_stream(ReceiverStream::new(rx)))
}

async fn require_session(State(rs): State<RState>, req: Request, next: Next) -> Response {
    let cookie = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok());
    if rs.remote.session_user(cookie).is_none() {
        return ApiErr::new(401, "login required").into_response();
    }
    next.run(req).await
}

/// Link-served requests: the Mac must still be paired and sharing still on.
async fn require_link(State(rs): State<RState>, req: Request, next: Next) -> Response {
    let cfg = rs.remote.config();
    let ok = rs.peer.as_ref().is_some_and(|p| cfg.sharing() && !p.mux.is_closed() && cfg.peers.iter().any(|x| x.id == p.id));
    if !ok {
        return ApiErr::new(403, "this Mac is no longer shared with you").into_response();
    }
    next.run(req).await
}

fn link_grants(rs: &RState) -> Option<(String, Grants)> {
    let id = &rs.peer.as_ref()?.id;
    rs.remote.config().peers.iter().find(|p| &p.id == id).map(|p| (p.name.clone(), p.grants.clone()))
}

async fn peer_info(State(rs): State<RState>) -> Json<Value> {
    let (name, grants) = link_grants(&rs).unwrap_or_default();
    let addrs = rs.remote.lan_addrs();
    Json(json!({
        "hostname": hostname(),
        "version": VERSION,
        "you": name,
        "grants": grants.json(),
        "allowControl": grants.control,
        // Every local-network address it can be reached at (the link only runs over the LAN).
        "addrs": addrs,
        // For older LStack versions that had other transports: makes them switch back to the local network.
        "transport": "local",
        "transports": { "local": { "addrs": addrs } },
        "cores": super::procinfo::host_cores(),
        "memTotal": super::procinfo::host_mem_total(),
    }))
}

/// The calling Mac says which 1Password references its services wait for (`want`) and which values it holds
/// from this Mac (`have`: ref → fetchedAt). Returns those it may receive now. References only, never values.
async fn peer_secrets_offer(State(rs): State<RState>, body: Bytes) -> Response {
    let Some(lp) = rs.peer.as_ref() else { return ApiErr::new(403, "link only").into_response() };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(p) = rs.remote.peer(&lp.id) else { return ApiErr::new(403, "link only").into_response() };
    let (sup, remote, peers) = (rs.app.sup.clone(), rs.remote.clone(), rs.app.peers.clone());
    let r = tokio::task::spawn_blocking(move || super::secretshare::offer(&sup, &remote, &peers, &p, &v)).await;
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => ApiErr::new(500, e.to_string()).into_response(),
    }
}

/// Send the calling Mac the cached values of granted references: `{refs: [..]}`. Served from this Mac's
/// encrypted store only — never reads 1Password.
async fn peer_secrets_pull(State(rs): State<RState>, body: Bytes) -> Response {
    let Some(lp) = rs.peer.as_ref() else { return ApiErr::new(403, "link only").into_response() };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(p) = rs.remote.peer(&lp.id) else { return ApiErr::new(403, "link only").into_response() };
    let (remote, peers) = (rs.remote.clone(), rs.app.peers.clone());
    let r = tokio::task::spawn_blocking(move || super::secretshare::serve(&remote, &peers, &p, &v)).await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((c, m))) => ApiErr::new(c, m).into_response(),
        Err(e) => ApiErr::new(500, e.to_string()).into_response(),
    }
}

/// The calling Mac watches this one, so it can't be asked from here: it tells us what it pushes and
/// learns what we ask for and accepted (`{push}` → `{want, wantFor, have, declined, accepted}`).
async fn peer_secrets_needs(State(rs): State<RState>, body: Bytes) -> Response {
    let Some(lp) = rs.peer.as_ref() else { return ApiErr::new(403, "link only").into_response() };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(p) = rs.remote.peer(&lp.id) else { return ApiErr::new(403, "link only").into_response() };
    let sup = rs.app.sup.clone();
    match tokio::task::spawn_blocking(move || super::secretshare::needs(&sup, &p, &v)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => ApiErr::new(500, e.to_string()).into_response(),
    }
}

/// Values from a Mac that watches this one: `{values: [{ref, value, fetchedAt}]}`. Only what we asked for,
/// accepted, or hold from it is stored.
async fn peer_secrets_deliver(State(rs): State<RState>, body: Bytes) -> Response {
    let Some(lp) = rs.peer.as_ref() else { return ApiErr::new(403, "link only").into_response() };
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return ApiErr::new(400, "invalid JSON body").into_response() };
    let Some(p) = rs.remote.peer(&lp.id) else { return ApiErr::new(403, "link only").into_response() };
    let sup = rs.app.sup.clone();
    match tokio::task::spawn_blocking(move || super::secretshare::deliver(&sup, &p, v)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((c, m))) => ApiErr::new(c, m).into_response(),
        Err(e) => ApiErr::new(500, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
    #[serde(default)]
    code: Option<String>,
}

async fn login(State(rs): State<RState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    let ip = peer.ip();
    let now = now_ms();
    if rs.remote.locked_out(ip) {
        return ApiErr::new(429, "too many failed attempts, try again in a few minutes").into_response();
    }
    let Ok(l) = serde_json::from_slice::<Login>(&body) else {
        return ApiErr::new(400, "invalid JSON body").into_response();
    };
    let cfg = rs.remote.config();
    let user_ok = ct_eq(l.username.as_bytes(), cfg.username.as_bytes());
    let stored = cfg.password_hash.clone();
    let password = l.password;
    let pass_ok = tokio::task::spawn_blocking(move || verify_password(&password, &stored)).await.unwrap_or(false);
    let code_ok = cfg.totp_secret.is_empty() || (user_ok && pass_ok && rs.remote.totp_check(&cfg.totp_secret, l.code.as_deref()));
    if !(user_ok && pass_ok && code_ok) {
        let n = rs.remote.failed(ip);
        println!("{} remote: failed login from {ip} ({n}/{MAX_FAILURES})", paths::iso(now));
        tokio::time::sleep(Duration::from_millis(400)).await;
        let msg = if cfg.totp_secret.is_empty() { "invalid username or password" } else { "invalid username, password or code" };
        return ApiErr::new(401, msg).into_response();
    }
    rs.remote.succeeded(ip);
    let token = random_hex(32);
    rs.remote.sessions.lock().unwrap().insert(token.clone(), (cfg.username.clone(), now + SESSION_TTL_MS));
    println!("{} remote: {} logged in from {ip}", paths::iso(now), cfg.username);
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}", SESSION_TTL_MS / 1000);
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true, "user": cfg.username }))).into_response()
}

/// Public: what the sign-in form needs to show.
async fn login_info(State(rs): State<RState>) -> Json<Value> {
    Json(json!({ "totp": !rs.remote.config().totp_secret.is_empty() }))
}

async fn logout(State(rs): State<RState>, req: Request) -> Response {
    if let Some(c) = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        let mut s = rs.remote.sessions.lock().unwrap();
        for part in c.split(';') {
            if let Some(t) = part.trim().strip_prefix(&format!("{COOKIE}=")) {
                s.remove(t);
            }
        }
    }
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn session(State(rs): State<RState>, req: Request) -> Json<Value> {
    let cookie = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok());
    let cfg = rs.remote.config();
    Json(json!({
        "user": rs.remote.session_user(cookie),
        "hostname": hostname(),
        "version": VERSION,
        "allowControl": cfg.allow_control,
        "totp": !cfg.totp_secret.is_empty(),
        "daemonStartedAt": crate::client::read_info().map(|i| i.started_at),
    }))
}

/// Environment values often hold secrets, and heartbeat ping URLs embed a check UUID or ping key;
/// never send them to remote browsers or peers.
pub(super) fn redact(mut s: ServiceState) -> ServiceState {
    if let Some(env) = s.config.env.as_mut() {
        for v in env.values_mut() {
            *v = "••••".into();
        }
    }
    if let Some(hb) = s.config.heartbeat.as_mut() {
        if hb.url.is_some() {
            hb.url = Some("••••".into());
        }
    }
    // Approvals are signed only on this Mac; the summary can quote env values.
    if s.approval.is_some() {
        s.approval = Some(serde_json::json!({ "pending": true }));
    }
    s
}

async fn list(State(rs): State<RState>) -> Json<Value> {
    let services: Vec<ServiceState> = rs.app.sup.list().into_iter().map(redact).collect();
    Json(json!({ "services": services, "configErrors": rs.app.sup.config_errors() }))
}

async fn get_one(State(rs): State<RState>, Path(name): Path<String>) -> Result<Json<ServiceState>, ApiErr> {
    Ok(Json(redact(rs.app.sup.get_state(&name)?)))
}

// This Mac's own services by machine name must go through the redacting handlers above, like `/api/services`.
async fn m_service(State(rs): State<RState>, Path((m, name)): Path<(String, String)>) -> Response {
    if server::is_local(&m) {
        return get_one(State(rs), Path(name)).await.into_response();
    }
    server::m_service(State(rs.app), Path((m, name))).await.into_response()
}

async fn m_action(State(rs): State<RState>, Path((m, name, act)): Path<(String, String, String)>) -> Response {
    if server::is_local(&m) {
        return action(State(rs), Path((name, act))).await.into_response();
    }
    let allowed = rs.remote.config().allow_control;
    server::m_action(State(rs.app), Path((m, name, act)), allowed).await.into_response()
}

type Q = Query<HashMap<String, String>>;

async fn logs(State(rs): State<RState>, p: Path<String>, q: Q) -> Response {
    server::logs(State(rs.app), p, q).await.into_response()
}
async fn log_stream(State(rs): State<RState>, p: Path<String>, q: Q) -> Response {
    server::log_stream(State(rs.app), p, q).await.into_response()
}
async fn metrics(State(rs): State<RState>, p: Path<String>, q: Q) -> Response {
    server::metrics(State(rs.app), p, q).await.into_response()
}
async fn events(State(rs): State<RState>) -> Response {
    let remote = rs.remote.clone();
    let gen = rs.gen;
    server::events_stream(rs.app, true, true, Some(Box::new(move || remote.generation() == gen))).await
}
async fn peer_events(State(rs): State<RState>) -> Response {
    let Some(mux) = rs.peer.as_ref().map(|p| p.mux.clone()) else { return ApiErr::new(403, "not a link").into_response() };
    server::events_stream(rs.app, true, false, Some(Box::new(move || !mux.is_closed()))).await
}

/// Start/stop/restart from a paired Mac: needs its `control` grant.
async fn peer_action(State(rs): State<RState>, Path((name, action)): Path<(String, String)>) -> Result<Json<ServiceState>, ApiErr> {
    let (peer, grants) = link_grants(&rs).ok_or_else(|| ApiErr::new(403, "not paired"))?;
    if !grants.control {
        return Err(ApiErr::new(403, format!("{} doesn't let this Mac start or stop services (give it the control permission there)", hostname())));
    }
    let st = run_action(&rs, &name, &action).await?;
    rs.remote.audit(json!({ "event": "control", "peer": peer, "service": name, "action": action }));
    Ok(Json(redact(st)))
}

async fn run_action(rs: &RState, name: &str, action: &str) -> Result<ServiceState, ApiErr> {
    let sup = &rs.app.sup;
    match action {
        "start" => Ok(sup.start(name).await?),
        "stop" => Ok(sup.stop(name).await?),
        "restart" => Ok(sup.restart(name).await?),
        _ => Err(ApiErr::new(404, "unknown action")),
    }
}

async fn action(State(rs): State<RState>, Path((name, action)): Path<(String, String)>) -> Result<Json<ServiceState>, ApiErr> {
    if !rs.remote.config().allow_control {
        return Err(ApiErr::new(403, "remote control is disabled on this machine"));
    }
    let st = run_action(&rs, &name, &action).await?;
    println!("{} remote: {action} {name}", paths::iso(now_ms()));
    Ok(Json(redact(st)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_sources_are_physical_subnets_only() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let nets = vec![
            ("en0".to_string(), ip("192.168.12.173"), ip("255.255.255.0")),
            ("utun3".to_string(), ip("10.8.0.5"), ip("255.0.0.0")),
            ("lo0".to_string(), ip("127.0.0.1"), ip("255.0.0.0")),
        ];
        for ok in ["192.168.12.40", "127.0.0.1", "::1", "fe80::1", "::ffff:192.168.12.9"] {
            assert!(local_source_in(ip(ok), &nets), "{ok} should be local");
        }
        for bad in ["10.8.0.9", "10.1.2.3", "100.101.102.103", "fd7a:115c:a1e0::1", "192.168.13.4", "8.8.8.8"] {
            assert!(!local_source_in(ip(bad), &nets), "{bad} should not be local");
        }
    }

    #[test]
    fn link_is_local_network_only() {
        use super::super::share::link_refusal;
        let any: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(link_refusal("127.0.0.1".parse().unwrap(), any, None).is_none());
        assert!(link_refusal("fe80::1".parse().unwrap(), any, None).is_none());
        // Automatic: overlay VPNs (Tailscale's ranges) and public addresses are never the local network.
        for far in ["100.100.1.1", "fd7a:115c:a1e0::1", "8.8.8.8"] {
            assert!(link_refusal(far.parse().unwrap(), any, None).is_some(), "{far}");
        }
        // Configs written by versions with a Tailscale transport still load.
        let old: SharedPeer = serde_json::from_str(r#"{"id":"a","name":"x","publicKey":"00","phrase":"","createdAt":1,"transport":"tailscale","tailnetId":{"tailnet":"t"}}"#).unwrap();
        assert_eq!(old.name, "x");
        let cfg: RemoteConfig = serde_json::from_str(r#"{"enabled":false,"tailnet":{"enabled":true},"autoApproveTransport":false}"#).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn link_only_through_the_chosen_address() {
        use super::super::share::link_refusal;
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // Tailscale chosen: only connections to this Mac's Tailscale address, from anywhere.
        let ts = Via { address: "automain.tail1.ts.net".into(), ips: vec![ip("100.117.1.2")], lan: false };
        assert!(link_refusal(ip("100.117.232.223"), ip("100.117.1.2"), Some(&ts)).is_none());
        assert!(link_refusal(ip("::ffff:100.117.232.223"), ip("::ffff:100.117.1.2"), Some(&ts)).is_none());
        let why = link_refusal(ip("192.168.1.30"), ip("192.168.1.20"), Some(&ts)).unwrap();
        assert!(why.contains("automain.tail1.ts.net"), "{why}");
        // A LAN name chosen: only to that address, and still only from the local network (not a VPN into it).
        let lan = Via { address: "automain.lan".into(), ips: vec![ip("192.168.1.20")], lan: true };
        assert!(link_refusal(ip("100.117.232.223"), ip("100.117.1.2"), Some(&lan)).is_some());
        assert!(link_refusal(ip("10.8.0.9"), ip("192.168.1.20"), Some(&lan)).is_some());
        assert!(link_refusal(ip("127.0.0.1"), ip("127.0.0.1"), Some(&lan)).is_none());
    }

    #[test]
    fn share_addresses() {
        for ok in ["studio.lan", "studio", "192.168.1.20", "fd00::1", "my-mac.local"] {
            assert!(valid_address(ok), "{ok}");
        }
        for bad in ["", "studio.lan:7381", "a b", "-x.lan", "x..lan", "http://x"] {
            assert!(!valid_address(bad), "{bad}");
        }
        assert_eq!(host_for_url("fd00::1"), "[fd00::1]");
        let found = discover_addresses(Some("chosen.invalid"));
        println!("{}", serde_json::to_string_pretty(&found).unwrap());
        for ip in local_addresses() {
            assert!(found.iter().any(|c| c["address"] == ip.as_str() && c["pointsHere"] == true), "{ip}");
        }
        let chosen = found.iter().find(|c| c["address"] == "chosen.invalid").unwrap();
        assert_eq!((chosen["selected"].clone(), chosen["pointsHere"].clone()), (json!(true), json!(false)));
        assert_eq!(found.iter().filter(|c| c["selected"] == true).count(), 1);
    }

    #[test]
    fn web_and_sharing_are_separate() {
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let mut c = RemoteConfig { enabled: true, web_remote: Some(false), sharing: Some(false), ..Default::default() };
        assert_eq!(c.bind_addr(), "127.0.0.1");
        assert!(c.admit("/", lo).is_none());
        assert!(c.admit("/api/services", mapped).is_none());
        assert_eq!(c.admit("/", lan).unwrap().0, 403);

        // Sharing uses the separate link port; the dashboard stays on this Mac.
        c.sharing = Some(true);
        assert_eq!(c.bind_addr(), "127.0.0.1");
        assert_eq!(c.admit("/api/login", lan).unwrap().0, 403);

        c.enabled = false;
        assert!(!c.listening());
        assert_eq!(c.admit("/", lo).unwrap().0, 404);

        c = RemoteConfig { enabled: true, web_remote: Some(true), sharing: Some(false), ..Default::default() };
        assert_eq!(c.bind_addr(), "0.0.0.0");
        assert!(c.admit("/", lan).is_none());
        assert_eq!(c.link_port, 7381);
    }

    #[test]
    fn old_config_keeps_behavior() {
        let old: RemoteConfig = serde_json::from_str(r#"{"enabled":true,"port":7380}"#).unwrap();
        assert_eq!((old.web_remote, old.sharing), (None, None));
        let c = migrate(old);
        assert!(c.web_remote() && c.sharing());
        let off = migrate(serde_json::from_str(r#"{"enabled":false}"#).unwrap());
        assert!(off.web_remote() && !off.sharing());
    }

    #[test]
    fn grants() {
        let mut g = Grants::new_pairing();
        assert!(g.tunnel_allows("web") && !g.control);
        assert_eq!(g.json()["tunnel"], "all");
        g.tunnel_all = false;
        g.tunnel = vec!["web".into()];
        assert!(g.tunnel_allows("web") && !g.tunnel_allows("db"));
        g.tunnel.clear();
        assert_eq!(g.json()["tunnel"], "none");
        // Old configs with bearer tokens load (and drop them) cleanly.
        let c: RemoteConfig = serde_json::from_str(r#"{"enabled":true,"peerTokens":[{"id":"a","hash":"x"}]}"#).unwrap();
        assert!(c.peers.is_empty());
        let k = [9u8; 32];
        let c = RemoteConfig { peers: vec![SharedPeer { id: "p".into(), public_key: super::super::link::key_hex(&k), ..Default::default() }], ..Default::default() };
        assert_eq!(c.peer_by_key(&k).unwrap().id, "p");
        assert!(c.peer_by_key(&[1u8; 32]).is_none());
    }

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse");
        assert!(h.starts_with("pbkdf2-sha256$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
        assert!(!verify_password("x", "garbage"));
        assert_ne!(hash_password("same"), hash_password("same"));
    }

    #[test]
    fn redacts_env() {
        let mut cfg = crate::types::ServiceConfig { name: "db".into(), ..Default::default() };
        cfg.env = Some([("PASSWORD".to_string(), "hunter2".to_string())].into());
        let s = ServiceState { name: "db".into(), kind: "docker".into(), config: cfg, ..Default::default() };
        let mut s = redact(s);
        assert_eq!(s.config.env.take().unwrap()["PASSWORD"], "••••");
        let mut cfg = crate::types::ServiceConfig { name: "api".into(), ..Default::default() };
        cfg.heartbeat = Some(crate::types::Heartbeat { url: Some("https://hc-ping.com/abc-uuid".into()), ..Default::default() });
        let s = ServiceState { name: "api".into(), kind: "process".into(), config: cfg, ..Default::default() };
        assert_eq!(redact(s).config.heartbeat.unwrap().url.as_deref(), Some("••••"));
    }

    #[test]
    fn vpn_detection() {
        let route = "   route to: default\ndestination: default\n       mask: default\n    gateway: 10.8.0.1\n  interface: utun4\n      flags: <UP,GATEWAY,DONE>\n";
        assert_eq!(default_route_interface(route).as_deref(), Some("utun4"));
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let en0 = ("en0".to_string(), ip("192.168.1.5"), ip("255.255.255.0"));
        let sys = ("utun0".to_string(), ip("fe80::1"), ip("ffff:ffff:ffff:ffff::"));
        let vpn = ("utun4".to_string(), ip("10.8.0.2"), ip("255.255.255.255"));
        let ts = ("utun6".to_string(), ip("100.101.1.2"), ip("255.255.255.255"));
        // No VPN: only the system's IPv6 utuns.
        let v = vpn_from(Some("en0"), &[en0.clone(), sys.clone()]);
        assert_eq!(v["active"], false);
        // Full tunnel.
        let v = vpn_from(Some("utun4"), &[en0.clone(), sys.clone(), vpn.clone()]);
        assert_eq!((v["active"].clone(), v["fullTunnel"].clone(), v["interface"].clone()), (json!(true), json!(true), json!("utun4")));
        // Split tunnel.
        let v = vpn_from(Some("en0"), &[en0.clone(), vpn.clone()]);
        assert_eq!((v["active"].clone(), v["fullTunnel"].clone()), (json!(true), json!(false)));
        // The Tailscale app (even as an exit node) isn't a LAN-blocking VPN.
        let v = vpn_from(Some("utun6"), &[en0.clone(), ts.clone()]);
        assert_eq!((v["active"].clone(), v["tailscaleApp"].clone()), (json!(false), json!(true)));
    }
}

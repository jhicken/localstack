//! Optional remote web dashboard. When enabled, a second HTTP listener (default 0.0.0.0:7380) serves a
//! small web UI plus a read-only API behind a username/password login, so you can check on services and
//! logs from another machine's browser. Start/stop/restart are only exposed when `allowControl` is set.
use super::server::{self, ApiErr, AppState};
use super::totp;
use crate::paths::{self, now_ms, VERSION};
use crate::types::ServiceState;
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
const SESSION_TTL_MS: i64 = 12 * 3600_000;
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
    pub enabled: bool,
    pub port: u16,
    pub bind: String,
    pub username: String,
    pub password_hash: String,
    pub allow_control: bool,
    /// Base32 TOTP secret; empty = two-factor login off.
    pub totp_secret: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            enabled: false,
            port: DEFAULT_PORT,
            bind: "0.0.0.0".into(),
            username: "admin".into(),
            password_hash: String::new(),
            allow_control: false,
            totp_secret: String::new(),
        }
    }
}

pub fn load_config() -> RemoteConfig {
    std::fs::read(paths::remote_config())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
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
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemotePatch {
    pub enabled: Option<bool>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub allow_control: Option<bool>,
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
        })
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
        let urls: Vec<String> = match &*running {
            Some(r) if r.bind == "0.0.0.0" || r.bind == "::" => {
                local_addresses().into_iter().map(|ip| format!("http://{ip}:{}", r.port)).collect()
            }
            Some(r) => vec![format!("http://{}:{}", r.bind, r.port)],
            None => vec![],
        };
        json!({
            "enabled": cfg.enabled,
            "running": running.is_some(),
            "port": cfg.port,
            "bind": cfg.bind,
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
        if let Some(b) = patch.bind {
            if b.parse::<IpAddr>().is_err() {
                return Err(ApiErr::new(400, format!("invalid bind address {b:?}")));
            }
            cfg.bind = b;
        }
        if let Some(c) = patch.allow_control {
            cfg.allow_control = c;
        }
        if let Some(e) = patch.enabled {
            cfg.enabled = e;
        }
        if cfg.enabled && cfg.password_hash.is_empty() {
            return Err(ApiErr::new(400, "set a password before enabling remote access"));
        }
        save_config(&cfg).map_err(|e| ApiErr::new(500, format!("cannot save remote.json: {e}")))?;
        let creds_changed = cfg.username != old.username || cfg.password_hash != old.password_hash;
        *self.cfg.lock().unwrap() = cfg;
        if creds_changed {
            self.sessions.lock().unwrap().clear();
        }
        self.apply(app).await;
        Ok(self.status())
    }

    /// Make the listener match the config (called at boot and after every change).
    pub async fn apply(self: &Arc<Self>, app: &AppState) {
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
            return;
        }
        let addr = format!("{}:{}", cfg.bind, cfg.port);
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
        println!("{} remote dashboard listening on http://{addr}", paths::iso(now_ms()));
        *self.running.lock().unwrap() = Some(Running { port: cfg.port, bind: cfg.bind, task });
    }

    /// Start two-factor enrollment: returns a fresh secret and its otpauth:// URI. Login is unaffected
    /// until `totp_confirm` sees a valid code for it.
    pub fn totp_begin(&self) -> Value {
        let cfg = self.config();
        let secret = totp::new_secret();
        let uri = totp::uri(&secret, &format!("{}@{}", cfg.username, hostname()), "LStack");
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
        println!("{} remote: two-factor login enabled", paths::iso(now_ms()));
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
        println!("{} remote: two-factor login disabled", paths::iso(now_ms()));
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

#[derive(Clone)]
struct RState {
    app: AppState,
    remote: Arc<Remote>,
    gen: u64,
}

fn router(app: AppState, remote: Arc<Remote>, gen: u64) -> Router {
    let rs = RState { app, remote, gen };
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
    {
        let mut f = rs.remote.failures.lock().unwrap();
        f.retain(|_, (_, last)| now - *last < LOCKOUT_MS);
        if f.get(&ip).is_some_and(|(n, _)| *n >= MAX_FAILURES) {
            return ApiErr::new(429, "too many failed attempts, try again in a few minutes").into_response();
        }
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
        let n = {
            let mut f = rs.remote.failures.lock().unwrap();
            let e = f.entry(ip).or_insert((0, now));
            *e = (e.0 + 1, now);
            e.0
        };
        println!("{} remote: failed login from {ip} ({n}/{MAX_FAILURES})", paths::iso(now));
        tokio::time::sleep(Duration::from_millis(400)).await;
        let msg = if cfg.totp_secret.is_empty() { "invalid username or password" } else { "invalid username, password or code" };
        return ApiErr::new(401, msg).into_response();
    }
    rs.remote.failures.lock().unwrap().remove(&ip);
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

/// Environment values often hold secrets; never send them to remote browsers.
fn redact(mut s: ServiceState) -> ServiceState {
    if let Some(env) = s.config.env.as_mut() {
        for v in env.values_mut() {
            *v = "••••".into();
        }
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
    server::events_stream(rs.app, true).await
}

async fn action(State(rs): State<RState>, Path((name, action)): Path<(String, String)>) -> Result<Json<ServiceState>, ApiErr> {
    if !rs.remote.config().allow_control {
        return Err(ApiErr::new(403, "remote control is disabled on this machine"));
    }
    let sup = &rs.app.sup;
    let st = match action.as_str() {
        "start" => sup.start(&name).await?,
        "stop" => sup.stop(&name).await?,
        "restart" => sup.restart(&name).await?,
        _ => return Err(ApiErr::new(404, "unknown action")),
    };
    println!("{} remote: {action} {name}", paths::iso(now_ms()));
    Ok(Json(redact(st)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let s = ServiceState {
            name: "db".into(),
            kind: "docker".into(),
            description: None,
            autostart: false,
            tags: vec![],
            status: "running".into(),
            health: "none".into(),
            pid: None,
            started_at: None,
            exit_code: None,
            restarts: 0,
            restart_at: None,
            last_error: None,
            metrics: None,
            config: cfg,
        };
        assert_eq!(redact(s).config.env.unwrap()["PASSWORD"], "••••");
    }
}

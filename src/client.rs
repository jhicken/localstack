//! Blocking HTTP client for the daemon API (used by the CLI and TUI).
use crate::paths;
use crate::types::{DaemonInfo, LogLine, MetricSample, ServiceConfig, ServiceState};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::time::Duration;

pub type Res<T> = Result<T, String>;

pub fn read_info() -> Option<DaemonInfo> {
    serde_json::from_str(&std::fs::read_to_string(paths::daemon_info()).ok()?).ok()
}

pub fn ping(info: &DaemonInfo) -> bool {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1500))
        .build()
        .get(&format!("http://127.0.0.1:{}/api/health", info.port))
        .set("Authorization", &format!("Bearer {}", info.token))
        .call()
        .is_ok()
}

pub fn enc(s: &str) -> String {
    crate::daemon::docker_api::enc(s)
}

/// `api` → this machine; `api@studio` → the paired machine "studio".
pub fn split_machine(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('@') {
        Some((n, m)) if !n.is_empty() && !m.is_empty() => (n, Some(m)),
        _ => (name, None),
    }
}

/// API path of a service, honouring `name@machine`.
pub fn svc_path(name: &str) -> String {
    match split_machine(name) {
        (n, Some(m)) => format!("/api/machines/{}/services/{}", enc(m), enc(n)),
        (n, None) => format!("/api/services/{}", enc(n)),
    }
}

#[derive(Clone)]
pub struct Client {
    pub info: DaemonInfo,
    agent: ureq::Agent,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResp {
    pub services: Vec<ServiceState>,
    #[serde(default)]
    pub config_errors: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
pub struct ReloadResp {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub errors: Vec<String>,
}

impl Client {
    pub fn new(info: DaemonInfo) -> Self {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout_read(Duration::from_secs(180)).build();
        Client { info, agent }
    }

    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.info.port)
    }

    pub fn req<T: DeserializeOwned>(&self, method: &str, p: &str, body: Option<Value>) -> Res<T> {
        let r = self.agent.request(method, &self.url(p)).set("Authorization", &format!("Bearer {}", self.info.token));
        let res = match body {
            Some(b) => r.send_json(b),
            None => r.call(),
        };
        match res {
            Ok(resp) => resp.into_json::<T>().map_err(|e| e.to_string()),
            Err(ureq::Error::Status(code, resp)) => {
                let v: Value = resp.into_json().unwrap_or(Value::Null);
                let msg = v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {code}"));
                if code == 404 && msg.starts_with("no route for") {
                    return Err(format!("{msg} — the background service is older than this command; run `lstack daemon restart`"));
                }
                Err(msg)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn list(&self) -> Res<ListResp> {
        self.req("GET", "/api/services", None)
    }
    pub fn get(&self, name: &str) -> Res<ServiceState> {
        self.req("GET", &svc_path(name), None)
    }
    pub fn create(&self, cfg: &ServiceConfig, start: bool) -> Res<ServiceState> {
        let q = if start { "?start=1" } else { "" };
        self.req("POST", &format!("/api/services{q}"), Some(serde_json::to_value(cfg).unwrap()))
    }
    pub fn update(&self, name: &str, patch: Value) -> Res<ServiceState> {
        self.req("PATCH", &format!("/api/services/{}", enc(name)), Some(patch))
    }
    pub fn remove(&self, name: &str) -> Res<Value> {
        self.req("DELETE", &format!("/api/services/{}", enc(name)), None)
    }
    pub fn action(&self, name: &str, action: &str) -> Res<ServiceState> {
        self.req("POST", &format!("{}/{action}", svc_path(name)), None)
    }
    pub fn stdin(&self, name: &str, data: &str) -> Res<Value> {
        self.req("POST", &format!("/api/services/{}/stdin", enc(name)), Some(json!({ "data": data })))
    }
    pub fn logs(&self, name: &str, tail: usize) -> Res<Vec<LogLine>> {
        let v: Value = self.req("GET", &format!("{}/logs?tail={tail}", svc_path(name)), None)?;
        serde_json::from_value(v["lines"].clone()).map_err(|e| e.to_string())
    }
    pub fn clear_logs(&self, name: &str) -> Res<Value> {
        self.req("DELETE", &format!("/api/services/{}/logs", enc(name)), None)
    }
    pub fn metrics(&self, name: &str, range: &str, points: usize) -> Res<Vec<MetricSample>> {
        let v: Value = self.req("GET", &format!("{}/metrics?range={range}&points={points}", svc_path(name)), None)?;
        serde_json::from_value(v["samples"].clone()).map_err(|e| e.to_string())
    }
    pub fn reload(&self) -> Res<ReloadResp> {
        self.req("POST", "/api/reload", None)
    }
    pub fn shutdown(&self) -> Res<Value> {
        self.req("POST", "/api/shutdown", None)
    }

    /// Subscribe to a server-sent-event stream. `on_event` returns false to stop.
    pub fn stream(&self, p: &str, mut on_event: impl FnMut(&str, Value) -> bool) -> Res<()> {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).build();
        let resp = agent
            .get(&self.url(p))
            .set("Authorization", &format!("Bearer {}", self.info.token))
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    let v: Value = r.into_json().unwrap_or(Value::Null);
                    v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {code}"))
                }
                e => e.to_string(),
            })?;
        let reader = std::io::BufReader::new(resp.into_reader());
        let (mut event, mut data, mut seen) = (String::from("message"), String::new(), false);
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.is_empty() {
                // Comment-only blocks (keepalive pings) are delivered as "ping" so callers can bail out.
                if seen {
                    let (ev, v) = if data.is_empty() { ("ping", Value::Null) } else { (event.as_str(), serde_json::from_str(&data).unwrap_or(Value::Null)) };
                    if !on_event(ev, v) {
                        return Ok(());
                    }
                }
                event = "message".into();
                data.clear();
                seen = false;
                continue;
            }
            seen = true;
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ") {
                data.push_str(d);
            }
        }
        Ok(())
    }
}

/// Connect to the daemon, starting it in the background if needed.
pub fn connect(autostart: bool) -> Res<Client> {
    if let Some(info) = read_info() {
        if ping(&info) {
            warn_if_stale(&info);
            return Ok(Client::new(info));
        }
    }
    if !autostart {
        return Err("lstack daemon is not running (start it with `lstack daemon start`)".into());
    }
    spawn_daemon();
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(info) = read_info() {
            if info.running != Some(false) && ping(&info) {
                return Ok(Client::new(info));
            }
        }
    }
    Err(format!("daemon failed to start — see {}", paths::daemon_log().display()))
}

fn warn_if_stale(info: &DaemonInfo) {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return;
    }
    if let Some(why) = crate::setup::daemon_stale(info) {
        eprintln!("{}", crate::fmt::yellow(&format!("! {why} — run `lstack daemon restart` to update it")));
    }
}

pub fn spawn_daemon() {
    paths::ensure_dirs();
    // Prefer launchd if the agent is installed so there's only ever one supervisor.
    if crate::launchd::manages_this_home() {
        crate::launchd::kickstart();
        return;
    }
    let Ok(out) = std::fs::OpenOptions::new().create(true).append(true).open(paths::daemon_log()) else { return };
    let err = out.try_clone().ok();
    let mut cmd = std::process::Command::new(paths::self_exe());
    cmd.args(["daemon", "run"]).stdin(std::process::Stdio::null()).stdout(out);
    if let Some(e) = err {
        cmd.stderr(e);
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

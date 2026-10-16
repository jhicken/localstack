//! Owns every service: lifecycle, restart policy, health checks, metrics and logs.
use super::docker_api;
use super::health;
use super::logs::LogStore;
use super::procinfo::{self, CpuSampler, HostSampler};
use super::store::MetricsStore;
use crate::config;
use crate::paths::{self, now_ms};
use crate::types::{MetricSample, ServiceConfig, ServiceState, SystemSample};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};

const DEFAULT_MAX_RESTARTS: u32 = 10;
const STABLE_AFTER_MS: i64 = 30_000;
static NEXT_GEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub enum Event {
    Service(Box<ServiceState>),
    Removed(String),
    Metrics(String, MetricSample),
    System(SystemSample),
    /// Processes over the usage thresholds (whole machine, excluding service processes).
    Processes(Arc<Vec<crate::types::ProcInfo>>),
    /// Something changed on another machine (see `peers.rs`).
    Peer(Arc<super::peers::PeerMsg>),
    /// Something the user should be told about (the app shows a notification): `{title, body}`.
    Notice(Arc<serde_json::Value>),
}

/// ~15 minutes of whole-machine samples at the 3 s cadence.
const SYSTEM_HISTORY: usize = 300;

#[derive(Debug)]
pub struct SupErr {
    pub status: u16,
    pub msg: String,
}
impl SupErr {
    pub fn new(status: u16, msg: impl Into<String>) -> Self {
        SupErr { status, msg: msg.into() }
    }
}
pub type R<T> = Result<T, SupErr>;

#[derive(Serialize, Default, Debug)]
pub struct ReloadResult {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub errors: Vec<String>,
}

/// Driver runtime state. Replaced wholesale when a new config is attached.
pub struct Rt {
    pub status: &'static str,
    pub pid: Option<i32>,
    pub started_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub last_error: Option<String>,
    /// Driver-reported health (e.g. docker HEALTHCHECK).
    pub native_health: Option<&'static str>,
    /// Newest log timestamp we already have (avoid replaying logs when re-attaching).
    pub log_since: Option<i64>,
    // process
    pub stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
    pub stopping: bool,
    pub exited: Option<watch::Receiver<bool>>,
    // docker / compose
    pub wanted: bool,
    pub cancelled: bool,
    pub ids: Vec<String>,
    pub labels: HashMap<String, String>,
    pub followers: HashMap<String, AbortHandle>,
}

impl Default for Rt {
    fn default() -> Self {
        Rt {
            status: "stopped",
            pid: None,
            started_at: None,
            exit_code: None,
            last_error: None,
            native_health: None,
            log_since: None,
            stdin: None,
            stopping: false,
            exited: None,
            wanted: false,
            cancelled: false,
            ids: vec![],
            labels: HashMap::new(),
            followers: HashMap::new(),
        }
    }
}

impl Rt {
    pub(super) fn dispose(&mut self) {
        for (_, h) in self.followers.drain() {
            h.abort();
        }
    }
    pub fn live(&self) -> bool {
        matches!(self.status, "running" | "starting" | "stopping")
    }
}

#[derive(Clone, Copy)]
struct Net {
    rx: f64,
    tx: f64,
    ts: i64,
    rate_rx: f64,
    rate_tx: f64,
}

pub struct Managed {
    pub cfg: ServiceConfig,
    pub pending: Option<ServiceConfig>,
    pub logs: Arc<LogStore>,
    pub rt: Rt,
    pub gen: u64,
    want_running: bool,
    restarts: u32,
    restart_task: Option<AbortHandle>,
    /// When the pending crash-loop restart fires (ms), while one is scheduled.
    restart_at: Option<i64>,
    health: &'static str,
    health_task: Option<AbortHandle>,
    metrics: Option<MetricSample>,
    net: Option<Net>,
    pid_set: Vec<i32>,
    pub urls: Vec<crate::types::ServiceUrl>,
    pub heartbeat: Option<crate::types::HeartbeatStatus>,
    /// Published docker/compose ports: (host port, loopback-only).
    pub ext_ports: Vec<(u16, bool)>,
    /// Summary of unapproved 1Password changes in the service's env files (see `envimport.rs`).
    pub secrets_pending: Option<serde_json::Value>,
    /// Plain env-file values the running process started with (key → hash; never the values).
    pub env_applied: Option<BTreeMap<String, u64>>,
    /// Plain env-file keys that changed since the start (restart to apply).
    pub env_stale: Option<Vec<String>>,
    /// Every key in the env files (names only), for the menu bar.
    pub env_file_keys: Vec<String>,
}

impl Managed {
    pub fn system(&self, msg: impl Into<String>) {
        self.logs.push("system", msg.into());
    }
    fn sys(&self, msg: &str) {
        self.logs.push("system", format!("[lstack] {msg}"));
    }
    fn health_now(&self) -> &'static str {
        if self.rt.status != "running" {
            return "none";
        }
        if self.cfg.healthcheck.is_some() {
            return self.health;
        }
        self.rt.native_health.unwrap_or("none")
    }
    pub fn state(&self) -> ServiceState {
        ServiceState {
            name: self.cfg.name.clone(),
            kind: self.cfg.kind.clone(),
            description: self.cfg.description.clone(),
            autostart: self.cfg.autostart(),
            tags: self.cfg.tags.clone().unwrap_or_default(),
            status: self.rt.status.to_string(),
            health: self.health_now().to_string(),
            pid: self.rt.pid,
            started_at: if self.rt.status == "running" { self.rt.started_at } else { None },
            exit_code: self.rt.exit_code,
            restarts: self.restarts,
            restart_at: self.restart_at,
            last_error: self.rt.last_error.clone().map(crate::scrub::scrub_string),
            metrics: self.metrics,
            urls: if self.rt.status == "running" { self.urls.clone() } else { vec![] },
            heartbeat: self.heartbeat.clone(),
            machine: None,
            secrets_pending: self.secrets_pending.clone(),
            env_stale: if self.rt.status == "running" { self.env_stale.clone() } else { None },
            env_file_keys: self.env_file_keys.clone(),
            approval: super::approvals::pending_for(&self.cfg.name),
            config: self.pending.clone().unwrap_or_else(|| self.cfg.clone()),
        }
    }
    fn cancel_restart(&mut self) {
        self.restart_at = None;
        if let Some(h) = self.restart_task.take() {
            h.abort();
        }
    }
    fn abort_tasks(&mut self) {
        self.cancel_restart();
        if let Some(h) = self.health_task.take() {
            h.abort();
        }
    }
}

pub struct Inner {
    pub services: BTreeMap<String, Managed>,
    busy: HashSet<String>,
    pub config_errors: Vec<String>,
}

pub struct Sup {
    inner: Mutex<Inner>,
    pub events: broadcast::Sender<Event>,
    pub store: MetricsStore,
    cpu: Mutex<CpuSampler>,
    host: Mutex<(HostSampler, std::collections::VecDeque<SystemSample>)>,
    docker_prev: Mutex<HashMap<String, (f64, f64)>>,
    loops: Mutex<Vec<AbortHandle>>,
    /// Listening TCP sockets by pid, from the last `lsof` pass.
    pub listen: Mutex<HashMap<i32, Vec<procinfo::Listen>>>,
    all_cpu: Mutex<CpuSampler>,
    /// pid → (rx+tx cumulative bytes, ts) and the derived rate, from nettop.
    net_all: Mutex<HashMap<i32, (f64, i64, f64)>>,
    procs: Mutex<Arc<Vec<crate::types::ProcInfo>>>,
}

pub struct BusyGuard {
    sup: Arc<Sup>,
    name: String,
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.sup.lock().busy.remove(&self.name);
    }
}

impl Sup {
    /// Tell the user something (the app shows it as a notification; also logged).
    pub fn notice(&self, title: &str, body: &str) {
        println!("{} notice: {title}: {body}", super::paths::iso(super::now_ms()));
        let _ = self.events.send(Event::Notice(Arc::new(serde_json::json!({ "title": title, "body": body }))));
    }

    pub fn new() -> rusqlite::Result<Arc<Self>> {
        paths::ensure_dirs();
        let (events, _) = broadcast::channel(1024);
        Ok(Arc::new(Sup {
            inner: Mutex::new(Inner { services: BTreeMap::new(), busy: HashSet::new(), config_errors: vec![] }),
            events,
            store: MetricsStore::open(&paths::db())?,
            cpu: Mutex::new(CpuSampler::default()),
            host: Mutex::new((HostSampler::default(), std::collections::VecDeque::new())),
            docker_prev: Mutex::new(HashMap::new()),
            loops: Mutex::new(vec![]),
            listen: Mutex::new(HashMap::new()),
            all_cpu: Mutex::new(CpuSampler::default()),
            net_all: Mutex::new(HashMap::new()),
            procs: Mutex::new(Arc::new(vec![])),
        }))
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` on a service if it still exists (and, when given, still has runner generation `gen`).
    pub fn with<T>(&self, name: &str, gen: Option<u64>, f: impl FnOnce(&mut Managed) -> T) -> Option<T> {
        let mut g = self.lock();
        let m = g.services.get_mut(name)?;
        if gen.is_some_and(|x| x != m.gen) {
            return None;
        }
        Some(f(m))
    }

    pub fn changed(&self, m: &Managed) {
        let _ = self.events.send(Event::Service(Box::new(m.state())));
    }

    /// Re-announce a service's state (e.g. an approval it waited on was answered).
    pub fn touch(&self, name: &str) {
        self.with(name, None, |m| self.changed(m));
    }

    /// Meant to be running but isn't (e.g. its start was held back for an approval).
    pub fn wants_running(&self, name: &str) -> bool {
        self.with(name, None, |m| m.want_running && !m.rt.live()).unwrap_or(false)
    }

    pub fn set_status(&self, m: &mut Managed, s: &'static str) {
        if m.rt.status != s {
            m.rt.status = s;
            self.changed(m);
        }
    }

    fn get<T>(&self, name: &str, f: impl FnOnce(&mut Managed) -> T) -> R<T> {
        self.with(name, None, f).ok_or_else(|| SupErr::new(404, format!("no such service \"{name}\"")))
    }

    async fn wait_busy(self: &Arc<Self>, name: &str) -> R<BusyGuard> {
        let deadline = now_ms() + 60_000;
        loop {
            match self.try_busy(name) {
                Err(e) if e.status == 409 && now_ms() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
                r => return r,
            }
        }
    }

    fn try_busy(self: &Arc<Self>, name: &str) -> R<BusyGuard> {
        let mut g = self.lock();
        if !g.busy.insert(name.to_string()) {
            return Err(SupErr::new(409, format!("{name} is busy, try again")));
        }
        Ok(BusyGuard { sup: self.clone(), name: name.to_string() })
    }

    // ------------------------------------------------------------ lifecycle

    pub async fn boot(self: &Arc<Self>) {
        self.reload();
        self.cleanup_orphans();
        let names: Vec<(String, u64)> = self.lock().services.values().map(|m| (m.cfg.name.clone(), m.gen)).collect();
        let mut set = JoinSet::new();
        for (name, gen) in names {
            let sup = self.clone();
            set.spawn(async move {
                sup.with(&name, Some(gen), |m| m.rt.log_since = m.logs.last_ts());
                if sup.adopt(&name, gen).await {
                    sup.with(&name, Some(gen), |m| {
                        m.want_running = true;
                        m.sys("re-attached to running workload");
                    });
                    sup.schedule_health(&name, Some(1_000));
                }
            });
        }
        while set.join_next().await.is_some() {}
        let resume = take_resume();
        let auto: Vec<String> = self
            .lock()
            .services
            .values()
            .filter(|m| (m.cfg.autostart() || (m.cfg.kind == "process" && resume.contains(&m.cfg.name))) && m.rt.status != "running")
            .map(|m| m.cfg.name.clone())
            .collect();
        for name in auto {
            let sup = self.clone();
            tokio::spawn(async move {
                let _ = sup.start(&name).await;
            });
        }
        self.every(3_000, |s| async move {
            s.sample_processes().await;
            s.sample_system();
            let s2 = s.clone();
            let _ = tokio::task::spawn_blocking(move || s2.sample_all()).await;
        });
        self.every(5_000, |s| async move { s.sample_docker().await });
        self.every(30_000, |s| async move { s.sample_network().await });
        self.every(4_000, |s| async move { s.refresh_external().await });
        self.every(5_000, |s| async move { s.sample_ports().await });
        self.every(3_600_000, |s| async move { s.store.prune(now_ms()) });
    }

    fn every<F, Fut>(self: &Arc<Self>, ms: u64, f: F)
    where
        F: Fn(Arc<Sup>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let sup = self.clone();
        let h = tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(ms));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                f(sup.clone()).await;
            }
        });
        self.loops.lock().unwrap().push(h.abort_handle());
    }

    pub async fn shutdown(self: &Arc<Self>) {
        for h in self.loops.lock().unwrap().drain(..) {
            h.abort();
        }
        // Child processes die with the daemon; containers are left alone (they outlive us by design).
        let procs: Vec<String> = {
            let mut g = self.lock();
            g.services
                .values_mut()
                .filter(|m| m.cfg.kind == "process")
                .map(|m| {
                    m.abort_tasks();
                    m.want_running = false;
                    m.cfg.name.clone()
                })
                .collect()
        };
        let mut set = JoinSet::new();
        for name in procs {
            let sup = self.clone();
            set.spawn(async move { sup.process_stop(&name).await });
        }
        while set.join_next().await.is_some() {}
        let mut g = self.lock();
        for m in g.services.values_mut() {
            m.abort_tasks();
            m.rt.dispose();
            m.logs.close();
        }
        drop(g);
        self.write_state();
    }

    /// Re-read the config folder and reconcile.
    pub fn reload(self: &Arc<Self>) -> ReloadResult {
        let (services, errors) = config::load_services();
        let mut res = ReloadResult { errors: errors.clone(), ..Default::default() };
        let mut seen = HashSet::new();
        {
            let mut g = self.lock();
            g.config_errors = errors;
            for cfg in services {
                seen.insert(cfg.name.clone());
                match g.services.get_mut(&cfg.name) {
                    None => {
                        res.added.push(cfg.name.clone());
                        let m = self.make(cfg);
                        self.changed(&m);
                        g.services.insert(m.cfg.name.clone(), m);
                    }
                    Some(m) if m.cfg != cfg => {
                        res.updated.push(cfg.name.clone());
                        self.apply_config(m, cfg);
                    }
                    _ => {}
                }
            }
            res.removed = g.services.keys().filter(|n| !seen.contains(*n)).cloned().collect();
        }
        for name in res.removed.clone() {
            let sup = self.clone();
            tokio::spawn(async move { sup.remove(&name).await });
        }
        res
    }

    // ------------------------------------------------------------ CRUD

    pub async fn create(self: &Arc<Self>, cfg: ServiceConfig, start: bool) -> R<ServiceState> {
        {
            let mut g = self.lock();
            if g.services.contains_key(&cfg.name) {
                return Err(SupErr::new(409, format!("service \"{}\" already exists", cfg.name)));
            }
            config::save_service(&cfg).map_err(|e| SupErr::new(400, e))?;
            let m = self.make(cfg.clone());
            self.changed(&m);
            g.services.insert(cfg.name.clone(), m);
        }
        if start {
            let _ = self.start(&cfg.name).await;
        }
        self.get_state(&cfg.name)
    }

    pub fn update(self: &Arc<Self>, name: &str, patch: &serde_json::Value) -> R<ServiceState> {
        let mut g = self.lock();
        let m = g.services.get_mut(name).ok_or_else(|| SupErr::new(404, format!("no such service \"{name}\"")))?;
        let base = m.pending.clone().unwrap_or_else(|| m.cfg.clone());
        let cfg = config::merge_patch(&base, patch).map_err(|e| SupErr::new(400, e))?;
        config::save_service(&cfg).map_err(|e| SupErr::new(400, e))?;
        self.apply_config(m, cfg);
        Ok(m.state())
    }

    pub async fn destroy(self: &Arc<Self>, name: &str) -> R<()> {
        self.get(name, |_| ())?;
        self.remove(name).await;
        config::delete_service(name);
        self.store.forget(name);
        Ok(())
    }

    // ------------------------------------------------------------ control

    pub async fn start(self: &Arc<Self>, name: &str) -> R<ServiceState> {
        let live = self.get(name, |m| {
            m.cancel_restart();
            m.want_running = true;
            m.restarts = 0;
            matches!(m.rt.status, "running" | "starting")
        })?;
        if !live {
            let _g = self.try_busy(name)?;
            self.do_start(name).await;
        }
        self.get_state(name)
    }

    pub async fn stop(self: &Arc<Self>, name: &str) -> R<ServiceState> {
        let was_restarting = self.get(name, |m| {
            m.want_running = false;
            let r = m.restart_at.is_some();
            m.cancel_restart();
            if r {
                m.sys("crash-loop restart cancelled");
            }
            r
        })?;
        {
            // A start may be in flight (e.g. a crash-loop restart); wait for it rather than refusing.
            let _g = self.wait_busy(name).await?;
            self.driver_stop(name).await;
        }
        if was_restarting {
            self.with(name, None, |m| self.changed(m));
        }
        self.get(name, |m| {
            if let Some(p) = m.pending.take() {
                self.apply_config(m, p);
            }
            m.metrics = None;
            m.health = if m.cfg.healthcheck.is_some() { "unknown" } else { "none" };
            self.changed(m);
            m.state()
        })
    }

    pub async fn restart(self: &Arc<Self>, name: &str) -> R<ServiceState> {
        let cfg = self.get(name, |m| m.pending.clone().unwrap_or_else(|| m.cfg.clone()))?;
        // Don't take a running service down for a start that would be held back for an approval.
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || super::approvals::gate(&cfg)).await {
            self.get(name, |m| m.sys(&e))?;
            return self.get_state(name);
        }
        self.get(name, |m| m.sys("restart requested"))?;
        self.stop(name).await?;
        self.start(name).await
    }

    pub fn write(&self, name: &str, data: &str) -> R<bool> {
        self.get(name, |m| m.rt.stdin.as_ref().is_some_and(|tx| tx.send(data.as_bytes().to_vec()).is_ok()))
    }

    // ------------------------------------------------------------ queries

    pub fn list(&self) -> Vec<ServiceState> {
        self.lock().services.values().map(|m| m.state()).collect()
    }
    pub fn config_errors(&self) -> Vec<String> {
        self.lock().config_errors.clone()
    }
    pub fn get_state(&self, name: &str) -> R<ServiceState> {
        self.get(name, |m| m.state())
    }
    pub fn configs(&self) -> Vec<ServiceConfig> {
        self.lock().services.values().map(|m| m.cfg.clone()).collect()
    }
    /// Configs including changes queued until the next restart (what the service will run with).
    pub fn effective_configs(&self) -> Vec<ServiceConfig> {
        self.lock().services.values().map(|m| m.pending.clone().unwrap_or_else(|| m.cfg.clone())).collect()
    }

    /// Mask a newly added secret in logs already written (memory + files, including the daemon log).
    pub fn rescrub_logs(&self) {
        let stores: Vec<Arc<LogStore>> = self.lock().services.values().map(|m| m.logs.clone()).collect();
        for l in stores {
            l.rescrub();
        }
        if let Ok(rd) = std::fs::read_dir(paths::logs_dir()) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("_daemon") {
                    super::logs::rescrub_file(&e.path());
                }
            }
        }
    }

    pub fn logs(&self, name: &str) -> R<Arc<LogStore>> {
        self.get(name, |m| m.logs.clone())
    }

    // ------------------------------------------------------------ internals

    fn make(&self, cfg: ServiceConfig) -> Managed {
        Managed {
            logs: Arc::new(LogStore::new(&cfg.name)),
            health: if cfg.healthcheck.is_some() { "unknown" } else { "none" },
            cfg,
            pending: None,
            rt: Rt::default(),
            gen: NEXT_GEN.fetch_add(1, Ordering::Relaxed),
            want_running: false,
            restarts: 0,
            restart_task: None,
            restart_at: None,
            health_task: None,
            metrics: None,
            net: None,
            pid_set: vec![],
            urls: vec![],
            heartbeat: None,
            ext_ports: vec![],
            secrets_pending: None,
            env_applied: None,
            env_stale: None,
            env_file_keys: vec![],
        }
    }

    /// Apply a new config now if stopped; otherwise queue it until the next stop/restart.
    fn apply_config(self: &Arc<Self>, m: &mut Managed, cfg: ServiceConfig) {
        let live = m.rt.live();
        let strip = |c: &ServiceConfig| ServiceConfig { autostart: None, description: None, tags: None, healthcheck: None, ..c.clone() };
        if live && strip(&m.cfg) != strip(&cfg) {
            m.cfg.autostart = cfg.autostart;
            m.cfg.description = cfg.description.clone();
            m.cfg.tags = cfg.tags.clone();
            m.pending = Some(cfg);
            m.sys("configuration changed — restart to apply");
        } else {
            m.pending = None;
            m.cfg = cfg;
            if !live {
                m.rt.dispose();
                m.rt = Rt::default();
                m.gen = NEXT_GEN.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !m.cfg.secret_refs().is_empty() {
            // Ask now, while you're here, rather than at the next unattended restart.
            let c = m.pending.clone().unwrap_or_else(|| m.cfg.clone());
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                h.spawn_blocking(move || super::approvals::prompt_if_needed(&c));
            }
        }
        self.schedule_health_m(m, None);
        self.changed(m);
    }

    async fn remove(self: &Arc<Self>, name: &str) {
        if self.with(name, None, |m| {
            m.want_running = false;
            m.abort_tasks();
        })
        .is_none()
        {
            return;
        }
        self.driver_stop(name).await;
        let mut g = self.lock();
        if let Some(mut m) = g.services.remove(name) {
            m.abort_tasks();
            m.rt.dispose();
            m.logs.close();
        }
        drop(g);
        let _ = self.events.send(Event::Removed(name.to_string()));
    }

    async fn do_start(self: &Arc<Self>, name: &str) {
        let Some((kind, gen)) = self.with(name, None, |m| {
            if let Some(p) = m.pending.take() {
                self.apply_config(m, p);
            }
            m.health = if m.cfg.healthcheck.is_some() { "unknown" } else { "none" };
            (m.cfg.kind.clone(), m.gen)
        }) else {
            return;
        };
        let res = match kind.as_str() {
            "process" => self.process_start(name, gen).await,
            "docker" => self.docker_start(name, gen).await,
            "compose" => self.compose_start(name, gen).await,
            _ => self.fail(name, gen, "Kubernetes services are not supported yet (planned for Phase 3)"),
        };
        if let Err(e) = res {
            self.with(name, None, |m| m.sys(&format!("start failed: {e}")));
            // Held for an approval: not a crash. It starts when approved, so no retries meanwhile.
            if super::approvals::id_in(&e).is_some() {
                return;
            }
            self.on_exit(name, Some(1), false);
            return;
        }
        self.schedule_health(name, Some(1_000));
    }

    async fn driver_stop(self: &Arc<Self>, name: &str) {
        let Some(kind) = self.with(name, None, |m| m.cfg.kind.clone()) else { return };
        match kind.as_str() {
            "process" => self.process_stop(name).await,
            "docker" => self.docker_stop(name).await,
            "compose" => self.compose_stop(name).await,
            _ => {
                self.with(name, None, |m| self.set_status(m, "stopped"));
            }
        }
    }

    async fn adopt(self: &Arc<Self>, name: &str, gen: u64) -> bool {
        let Some(kind) = self.with(name, Some(gen), |m| m.cfg.kind.clone()) else { return false };
        match kind.as_str() {
            "docker" => self.docker_adopt(name, gen).await,
            "compose" => self.compose_adopt(name, gen).await,
            _ => false,
        }
    }

    /// Record a driver failure: sets lastError + errored status.
    pub fn fail(&self, name: &str, gen: u64, msg: &str) -> Result<(), String> {
        self.with(name, Some(gen), |m| {
            m.rt.last_error = Some(msg.to_string());
            m.system(msg);
            self.set_status(m, "errored");
        });
        Err(msg.to_string())
    }

    /// The workload stopped. Applies the restart policy.
    pub fn on_exit(self: &Arc<Self>, name: &str, code: Option<i32>, expected: bool) {
        self.with(name, None, |m| {
            if let Some(h) = m.health_task.take() {
                h.abort();
            }
            m.metrics = None;
            if expected || !m.want_running {
                return;
            }
            let policy = m.cfg.restart.as_deref().unwrap_or("on-failure");
            if policy == "never" || (policy == "on-failure" && code == Some(0)) {
                m.want_running = false;
                return;
            }
            if m.rt.started_at.is_some_and(|t| now_ms() - t > STABLE_AFTER_MS) {
                m.restarts = 0;
            }
            let max = m.cfg.max_restarts.unwrap_or(DEFAULT_MAX_RESTARTS);
            if m.restarts >= max {
                m.want_running = false;
                let msg = format!("gave up after {max} restarts");
                m.sys(&msg);
                m.rt.last_error = Some(msg);
                self.changed(m);
                return;
            }
            let delay = 30_000u64.min(1_000u64 << m.restarts.min(15));
            m.restarts += 1;
            m.sys(&format!("restarting in {}s (attempt {}/{max})", delay / 1000, m.restarts));
            let sup = self.clone();
            let n = name.to_string();
            let h = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let want = sup.with(&n, None, |m| {
                    m.restart_task = None;
                    m.restart_at = None;
                    m.want_running
                });
                if want != Some(true) {
                    return;
                }
                if let Ok(_g) = sup.try_busy(&n) {
                    sup.do_start(&n).await;
                }
            });
            m.restart_task = Some(h.abort_handle());
            m.restart_at = Some(now_ms() + delay as i64);
            self.changed(m);
        });
    }

    pub fn schedule_health(self: &Arc<Self>, name: &str, delay: Option<u64>) {
        self.with(name, None, |m| self.schedule_health_m(m, delay));
    }

    fn schedule_health_m(self: &Arc<Self>, m: &mut Managed, delay: Option<u64>) {
        if let Some(h) = m.health_task.take() {
            h.abort();
        }
        let Some(hc) = m.cfg.healthcheck.clone() else {
            m.health = "none";
            return;
        };
        let interval = (hc.interval.unwrap_or(10.0).max(0.5) * 1000.0) as u64;
        let mut wait = delay.unwrap_or(interval);
        let sup = self.clone();
        let name = m.cfg.name.clone();
        let h = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                if sup.with(&name, None, |m| m.rt.status == "running") != Some(true) {
                    return;
                }
                let next = if health::check(&hc).await { "healthy" } else { "unhealthy" };
                sup.with(&name, None, |m| {
                    if m.health != next {
                        m.health = next;
                        m.sys(&format!("health: {next}"));
                        sup.changed(m);
                    }
                });
                wait = interval;
            }
        });
        m.health_task = Some(h.abort_handle());
    }

    // ------------------------------------------------------------ sampling

    async fn refresh_external(self: &Arc<Self>) {
        let targets: Vec<(String, String, u64)> = self
            .lock()
            .services
            .values()
            .filter(|m| m.cfg.kind != "process" && (m.want_running || m.rt.status == "running"))
            .map(|m| (m.cfg.name.clone(), m.cfg.kind.clone(), m.gen))
            .collect();
        let mut set = JoinSet::new();
        for (name, kind, gen) in targets {
            let sup = self.clone();
            set.spawn(async move {
                match kind.as_str() {
                    "docker" => sup.docker_refresh(&name, gen).await,
                    "compose" => sup.compose_refresh(&name, gen).await,
                    _ => {}
                }
            });
        }
        while set.join_next().await.is_some() {}
    }

    async fn sample_processes(self: &Arc<Self>) {
        let roots: Vec<(String, i32)> = self
            .lock()
            .services
            .values()
            .filter(|m| m.cfg.kind == "process" && m.rt.status == "running")
            .filter_map(|m| m.rt.pid.map(|p| (m.cfg.name.clone(), p)))
            .collect();
        if roots.is_empty() {
            return;
        }
        let groups: Vec<Vec<i32>> = roots.iter().map(|(_, p)| procinfo::descendants(&[*p])).collect();
        let samples = self.cpu.lock().unwrap().sample(&groups);
        let now = now_ms();
        let mut g = self.lock();
        for (((name, _), pids), s) in roots.iter().zip(groups).zip(samples) {
            if let Some(m) = g.services.get_mut(name) {
                m.pid_set = pids;
                self.record(m, now, s.cpu, s.mem);
            }
        }
        drop(g);
        self.write_state();
    }

    /// Refresh listening sockets and recompute every service's URLs (events only on change).
    async fn sample_ports(self: &Arc<Self>) {
        let Some(map) = procinfo::listening_ports().await else { return };
        *self.listen.lock().unwrap() = map;
        let listen = self.listen.lock().unwrap().clone();
        let mut g = self.lock();
        for m in g.services.values_mut() {
            let urls = service_urls(m, &listen);
            if urls != m.urls {
                m.urls = urls;
                if m.rt.status == "running" {
                    self.changed(m);
                }
            }
        }
    }

    fn sample_system(&self) {
        let cores = procinfo::host_cores() as f64;
        let (ls_cpu, ls_mem) = self
            .lock()
            .services
            .values()
            .filter(|m| m.rt.status == "running")
            .filter_map(|m| m.metrics)
            .fold((0.0, 0.0), |a, s| (a.0 + s.cpu, a.1 + s.mem));
        let mut h = self.host.lock().unwrap();
        let Some(cpu) = h.0.cpu() else { return };
        let mem = procinfo::host_mem_used().unwrap_or(0.0);
        let s = SystemSample { ts: now_ms(), cpu, mem, ls_cpu: (ls_cpu / cores).min(cpu.max(0.0)), ls_mem: ls_mem.min(mem) };
        if h.1.len() >= SYSTEM_HISTORY {
            h.1.pop_front();
        }
        h.1.push_back(s);
        drop(h);
        let _ = self.events.send(Event::System(s));
    }

    /// Sample every visible process and publish the ones over the configured thresholds.
    fn sample_all(&self) {
        let th = super::terminal::load().processes;
        if !th.enabled {
            if !self.procs.lock().unwrap().is_empty() {
                *self.procs.lock().unwrap() = Arc::new(vec![]);
                let _ = self.events.send(Event::Processes(Arc::new(vec![])));
            }
            return;
        }
        let pids = procinfo::all_pids();
        let groups: Vec<Vec<i32>> = pids.iter().map(|p| vec![*p]).collect();
        let samples = self.all_cpu.lock().unwrap().sample(&groups);
        let owned: HashMap<i32, String> = self
            .lock()
            .services
            .values()
            .flat_map(|m| m.pid_set.iter().map(move |p| (*p, m.cfg.name.clone())))
            .collect();
        let me = std::process::id() as i32;
        let mem_limit = procinfo::host_mem_total() * th.mem / 100.0;
        let net = self.net_all.lock().unwrap().clone();
        let mut out: Vec<(f64, crate::types::ProcInfo)> = vec![];
        for (pid, s) in pids.iter().zip(samples) {
            if *pid == me || owned.contains_key(pid) || (s.cpu == 0.0 && s.mem == 0.0) {
                continue;
            }
            let rate = net.get(pid).map(|n| n.2);
            let score = [
                if th.cpu > 0.0 { s.cpu / th.cpu } else { 0.0 },
                if mem_limit > 0.0 && th.mem > 0.0 { s.mem / mem_limit } else { 0.0 },
                if th.net > 0.0 { rate.unwrap_or(0.0) / th.net } else { 0.0 },
            ]
            .into_iter()
            .fold(0.0, f64::max);
            if score < 1.0 {
                continue;
            }
            let name = procinfo::proc_name(*pid).unwrap_or_else(|| format!("pid {pid}"));
            out.push((score, crate::types::ProcInfo { pid: *pid, name, cpu: s.cpu, mem: s.mem, net: rate, service: None }));
        }
        out.sort_by(|a, b| b.0.total_cmp(&a.0));
        out.truncate(th.max.max(1));
        let list: Vec<_> = out.into_iter().map(|x| x.1).collect();
        let list = Arc::new(list);
        *self.procs.lock().unwrap() = list.clone();
        let _ = self.events.send(Event::Processes(list));
    }

    pub fn processes(&self) -> Arc<Vec<crate::types::ProcInfo>> {
        self.procs.lock().unwrap().clone()
    }

    pub fn system(&self) -> serde_json::Value {
        let h = self.host.lock().unwrap();
        serde_json::json!({
            "cores": procinfo::host_cores(),
            "memTotal": procinfo::host_mem_total(),
            "samples": h.1.iter().collect::<Vec<_>>(),
        })
    }

    async fn sample_docker(self: &Arc<Self>) {
        let targets: Vec<(String, Vec<String>)> = self
            .lock()
            .services
            .values()
            .filter(|m| m.cfg.kind != "process" && !m.rt.ids.is_empty())
            .map(|m| (m.cfg.name.clone(), m.rt.ids.clone()))
            .collect();
        if targets.is_empty() {
            return;
        }
        let ids: HashSet<String> = targets.iter().flat_map(|(_, i)| i.iter().cloned()).collect();
        let mut set = JoinSet::new();
        for id in ids {
            set.spawn(async move { (docker_api::stats(&id).await, id) });
        }
        let mut stats: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
        while let Some(Ok((v, id))) = set.join_next().await {
            if let Some(v) = v {
                stats.insert(id.clone(), self.docker_stat(&id, &v));
            }
        }
        let now = now_ms();
        let mut g = self.lock();
        for (name, ids) in targets {
            let rows: Vec<_> = ids.iter().filter_map(|i| stats.get(i)).collect();
            let Some(m) = g.services.get_mut(&name) else { continue };
            if rows.is_empty() {
                continue;
            }
            let (cpu, mem, rx, tx) = rows.iter().fold((0.0, 0.0, 0.0, 0.0), |a, r| (a.0 + r.0, a.1 + r.1, a.2 + r.2, a.3 + r.3));
            update_net(m, rx, tx, now);
            self.record(m, now, cpu, mem);
        }
    }

    /// (cpu %, mem bytes, net rx total, net tx total) from one `stats` document.
    fn docker_stat(&self, id: &str, v: &serde_json::Value) -> (f64, f64, f64, f64) {
        let f = |p: &str| v.pointer(p).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let total = f("/cpu_stats/cpu_usage/total_usage");
        let system = f("/cpu_stats/system_cpu_usage");
        let online = v
            .pointer("/cpu_stats/online_cpus")
            .and_then(|x| x.as_f64())
            .or_else(|| v.pointer("/cpu_stats/cpu_usage/percpu_usage").and_then(|x| x.as_array()).map(|a| a.len() as f64))
            .unwrap_or(1.0);
        let mut prev = self.docker_prev.lock().unwrap();
        let (pt, ps) = prev.get(id).copied().unwrap_or((f("/precpu_stats/cpu_usage/total_usage"), f("/precpu_stats/system_cpu_usage")));
        prev.insert(id.to_string(), (total, system));
        let cpu = if ps > 0.0 && system > ps && total >= pt { (total - pt) / (system - ps) * online * 100.0 } else { 0.0 };
        let usage = f("/memory_stats/usage");
        let cache = v
            .pointer("/memory_stats/stats/inactive_file")
            .or_else(|| v.pointer("/memory_stats/stats/total_inactive_file"))
            .or_else(|| v.pointer("/memory_stats/stats/cache"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let mem = if usage > cache { usage - cache } else { usage };
        let (mut rx, mut tx) = (0.0, 0.0);
        if let Some(nets) = v.get("networks").and_then(|n| n.as_object()) {
            for n in nets.values() {
                rx += n.get("rx_bytes").and_then(|x| x.as_f64()).unwrap_or(0.0);
                tx += n.get("tx_bytes").and_then(|x| x.as_f64()).unwrap_or(0.0);
            }
        }
        (cpu, mem, rx, tx)
    }

    async fn sample_network(self: &Arc<Self>) {
        let targets: Vec<(String, Vec<i32>)> = self
            .lock()
            .services
            .values()
            .filter(|m| m.cfg.kind == "process" && m.rt.status == "running")
            .map(|m| (m.cfg.name.clone(), if m.pid_set.is_empty() { m.rt.pid.into_iter().collect() } else { m.pid_set.clone() }))
            .collect();
        let breakdown = super::terminal::load().processes.enabled;
        if targets.is_empty() && !breakdown {
            return;
        }
        let Some(net) = procinfo::network_table().await else { return };
        let now = now_ms();
        {
            let mut all = self.net_all.lock().unwrap();
            let mut next = HashMap::new();
            for (pid, (rx, tx)) in &net {
                let total = rx + tx;
                let rate = match all.get(pid) {
                    Some(&(prev, ts, _)) if now > ts && total >= prev => (total - prev) / ((now - ts) as f64 / 1000.0),
                    _ => 0.0,
                };
                next.insert(*pid, (total, now, rate));
            }
            *all = next;
        }
        let mut g = self.lock();
        for (name, pids) in targets {
            let (rx, tx) = pids.iter().filter_map(|p| net.get(p)).fold((0.0, 0.0), |a, n| (a.0 + n.0, a.1 + n.1));
            if let Some(m) = g.services.get_mut(&name) {
                update_net(m, rx, tx, now);
            }
        }
    }

    fn record(&self, m: &mut Managed, ts: i64, cpu: f64, mem: f64) {
        let mut s = MetricSample { ts, cpu, mem, net_rx: None, net_tx: None };
        if let Some(n) = m.net {
            s.net_rx = Some(n.rate_rx);
            s.net_tx = Some(n.rate_tx);
        }
        m.metrics = Some(s);
        self.store.record(&m.cfg.name, &s);
        let _ = self.events.send(Event::Metrics(m.cfg.name.clone(), s));
    }

    // ------------------------------------------------ orphan handling (daemon crash recovery)

    pub fn write_state(&self) {
        let g = self.lock();
        let mut state = serde_json::Map::new();
        for m in g.services.values() {
            if let (true, Some(pid), Some(at)) = (m.cfg.kind == "process", m.rt.pid, m.rt.started_at) {
                state.insert(m.cfg.name.clone(), serde_json::json!({ "pid": pid, "startedAt": at }));
            }
        }
        drop(g);
        let _ = std::fs::write(paths::state_file(), serde_json::Value::Object(state).to_string());
    }

    /// If the daemon died without cleaning up, kill process groups it left behind (verified by start time).
    fn cleanup_orphans(&self) {
        let Ok(text) = std::fs::read_to_string(paths::state_file()) else { return };
        let state: HashMap<String, serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        for (name, v) in state {
            let (Some(pid), Some(at)) = (v["pid"].as_i64(), v["startedAt"].as_i64()) else { continue };
            let pid = pid as i32;
            if procinfo::start_time(pid).is_some_and(|st| (st - at).abs() < 5_000) {
                kill_group(pid, libc::SIGTERM);
                self.with(&name, None, |m| m.sys(&format!("stopped orphaned process group {pid} from a previous daemon")));
            }
        }
        let _ = std::fs::remove_file(paths::state_file());
    }
}

/// Scripts the previous daemon was running when it was restarted (see `setup::remember_running`).
/// Ignored if stale, so a later plain `daemon start` doesn't bring back old state.
fn take_resume() -> Vec<String> {
    let path = paths::resume_file();
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![] };
    let _ = std::fs::remove_file(&path);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if now_ms() - v["at"].as_i64().unwrap_or(0) > 5 * 60_000 {
        return vec![];
    }
    v["services"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
}

/// Configured `urls:` first, then detected ports (unless `detectUrls: false`) as http://localhost:PORT.
fn service_urls(m: &Managed, listen: &HashMap<i32, Vec<procinfo::Listen>>) -> Vec<crate::types::ServiceUrl> {
    use crate::types::ServiceUrl;
    if m.rt.status != "running" {
        return vec![];
    }
    let mut out: Vec<ServiceUrl> = m
        .cfg
        .urls
        .iter()
        .flatten()
        .map(|u| ServiceUrl {
            url: u.url().to_string(),
            label: u.label().map(String::from),
            port: url_port(u.url()),
            source: "config".into(),
            ..Default::default()
        })
        .collect();
    if m.cfg.detect_urls == Some(false) {
        return out;
    }
    let ports = if m.cfg.kind == "process" {
        let pids: Vec<i32> = if m.pid_set.is_empty() { m.rt.pid.into_iter().collect() } else { m.pid_set.clone() };
        procinfo::ports_of(pids.iter().filter_map(|p| listen.get(p)).flatten())
    } else {
        m.ext_ports.clone()
    };
    for (port, local_only) in ports {
        if let Some(c) = out.iter_mut().find(|u| u.port == Some(port)) {
            c.local_only = local_only;
            c.listening = true;
            continue;
        }
        out.push(ServiceUrl { url: format!("http://localhost:{port}"), port: Some(port), source: "detected".into(), local_only, listening: true, ..Default::default() });
    }
    out
}

pub fn url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    match host.rsplit_once(':') {
        Some((_, p)) if !p.contains(']') => p.parse().ok(),
        _ if url.starts_with("https://") => Some(443),
        _ if url.starts_with("http://") => Some(80),
        _ => None,
    }
}

/// Host ports published by a container (`NetworkSettings.Ports` or a `containers/json` row's `Ports`).
pub fn docker_ports(v: &serde_json::Value) -> Vec<(u16, bool)> {
    let mut l: Vec<procinfo::Listen> = vec![];
    if let Some(map) = v.pointer("/NetworkSettings/Ports").and_then(|x| x.as_object()) {
        for (k, binds) in map {
            if !k.ends_with("/tcp") {
                continue;
            }
            for b in binds.as_array().into_iter().flatten() {
                if let Some(port) = b["HostPort"].as_str().and_then(|p| p.parse().ok()) {
                    l.push(procinfo::Listen { ip: b["HostIp"].as_str().filter(|s| !s.is_empty()).unwrap_or("0.0.0.0").into(), port });
                }
            }
        }
    }
    for p in v["Ports"].as_array().into_iter().flatten() {
        if let (Some(port), Some("tcp")) = (p["PublicPort"].as_u64(), p["Type"].as_str()) {
            l.push(procinfo::Listen { ip: p["IP"].as_str().unwrap_or("0.0.0.0").into(), port: port as u16 });
        }
    }
    procinfo::ports_of(&l)
}

fn update_net(m: &mut Managed, rx: f64, tx: f64, now: i64) {
    let prev = m.net;
    let dt = prev.map(|p| (now - p.ts) as f64 / 1000.0).unwrap_or(0.0);
    let rate = |cur: f64, old: f64| if dt > 0.0 { ((cur - old) / dt).max(0.0) } else { 0.0 };
    m.net = Some(Net {
        rx,
        tx,
        ts: now,
        rate_rx: prev.map(|p| rate(rx, p.rx)).unwrap_or(0.0),
        rate_tx: prev.map(|p| rate(tx, p.tx)).unwrap_or(0.0),
    });
}

pub fn kill_group(pid: i32, sig: i32) {
    unsafe {
        if libc::kill(-pid, sig) != 0 {
            libc::kill(pid, sig);
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn ports_from_urls_and_docker() {
        assert_eq!(url_port("http://localhost:3000/app"), Some(3000));
        assert_eq!(url_port("https://example.test"), Some(443));
        assert_eq!(url_port("http://[::1]:8080"), Some(8080));
        let v = serde_json::json!({ "NetworkSettings": { "Ports": { "80/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "8080" }], "53/udp": [{ "HostPort": "53" }] } } });
        assert_eq!(docker_ports(&v), vec![(8080, true)]);
        let row = serde_json::json!({ "Ports": [{ "IP": "0.0.0.0", "PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp" }, { "PrivatePort": 9000, "Type": "tcp" }] });
        assert_eq!(docker_ports(&row), vec![(5432, false)]);
    }
}

//! `lstack top` — htop-style dashboard (ratatui).
use crate::client::{self, Client};
use crate::fmt as f;
use crate::types::{LogLine, MetricSample, ProcInfo, ServiceState};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LIST_W: u16 = 40;
const HISTORY: usize = 120;
const MAX_LOGS: usize = 2000;
/// Other processes shown per machine.
const PROC_MAX: usize = 5;

enum Msg {
    Snapshot(Vec<ServiceState>, Vec<String>, Option<String>),
    Service(Box<ServiceState>),
    Removed(String),
    Metrics(Option<String>, String, MetricSample),
    Machines(Option<String>, Vec<Value>),
    Peer(String, String, Value),
    Processes(Option<String>, Vec<ProcInfo>),
    Log(u64, LogLine),
    History(String, Vec<MetricSample>),
    Flash(String, bool),
    Connected(bool),
    Client(Client),
}

/// A paired machine (peer).
#[derive(Debug, Clone, PartialEq, Default)]
struct Machine {
    name: String,
    status: String,
    error: Option<String>,
    allow_control: bool,
}

impl Machine {
    fn from_json(v: &Value) -> Option<Machine> {
        Some(Machine {
            name: v["name"].as_str()?.to_string(),
            status: v["status"].as_str().unwrap_or("connecting").to_string(),
            error: v["error"].as_str().filter(|e| !e.is_empty()).map(String::from),
            allow_control: v["allowControl"].as_bool().unwrap_or(false),
        })
    }
}

/// One row of the services list: a machine header (None = this machine) or a service index.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Header(Option<String>),
    Service(usize),
}

/// `name` for local services, `name@machine` for a paired machine's (the API addressing form).
fn skey(s: &ServiceState) -> String {
    match &s.machine {
        Some(m) => format!("{}@{m}", s.name),
        None => s.name.clone(),
    }
}

/// This machine first, then peers by name; services by name within a machine.
fn svc_order(a: &ServiceState, b: &ServiceState) -> std::cmp::Ordering {
    (a.machine.as_deref(), a.name.as_str()).cmp(&(b.machine.as_deref(), b.name.as_str()))
}

/// Rows for the list. Machine headers only appear once at least one peer is known.
/// `services` must be sorted with `svc_order` so row order matches index order.
fn build_rows(services: &[ServiceState], machines: &[Machine]) -> Vec<Row> {
    if machines.is_empty() && services.iter().all(|s| s.machine.is_none()) {
        return (0..services.len()).map(Row::Service).collect();
    }
    let mut names: Vec<String> = machines.iter().map(|m| m.name.clone()).chain(services.iter().filter_map(|s| s.machine.clone())).collect();
    names.sort();
    names.dedup();
    let mut out = vec![];
    for g in std::iter::once(None).chain(names.into_iter().map(Some)) {
        let rows: Vec<Row> = services.iter().enumerate().filter(|(_, s)| s.machine == g).map(|(i, _)| Row::Service(i)).collect();
        out.push(Row::Header(g));
        out.extend(rows);
    }
    out
}

/// "5s", "2m", "3h", "1d".
fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn machine_color(status: &str) -> Color {
    match status {
        "online" => Color::Green,
        "connecting" | "repair" => Color::Yellow,
        _ => Color::Red,
    }
}

struct Ui {
    client: Client,
    tx: Sender<Msg>,
    services: Vec<ServiceState>,
    machines: Vec<Machine>,
    local_name: Option<String>,
    procs: HashMap<Option<String>, Vec<ProcInfo>>,
    selected: usize,
    history: HashMap<String, Vec<MetricSample>>,
    logs: VecDeque<LogLine>,
    log_scroll: usize,
    full_logs: bool,
    input_mode: bool,
    input: String,
    flash: Option<(String, Instant, bool)>,
    connected: bool,
    config_errors: Vec<String>,
    logs_for: Option<String>,
    log_gen: Arc<AtomicU64>,
    page: usize,
}

fn status_color(s: &str) -> Color {
    match s {
        "running" => Color::Green,
        "starting" | "stopping" => Color::Yellow,
        "crashed" | "errored" => Color::Red,
        _ => Color::DarkGray,
    }
}
fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(Color::DarkGray))
}

pub fn run(client: Client) -> Result<(), String> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err("`lstack top` needs an interactive terminal".into());
    }
    let (tx, rx) = channel();
    spawn_events(client.clone(), tx.clone());
    let mut ui = Ui {
        client,
        tx,
        services: vec![],
        machines: vec![],
        local_name: None,
        procs: HashMap::new(),
        selected: 0,
        history: HashMap::new(),
        logs: VecDeque::new(),
        log_scroll: 0,
        full_logs: false,
        input_mode: false,
        input: String::new(),
        flash: None,
        connected: false,
        config_errors: vec![],
        logs_for: None,
        log_gen: Arc::new(AtomicU64::new(0)),
        page: 10,
    };
    let mut term = ratatui::init();
    let res = main_loop(&mut term, &mut ui, &rx);
    ratatui::restore();
    res
}

fn spawn_events(client: Client, tx: Sender<Msg>) {
    std::thread::spawn(move || {
      let mut client = client;
      loop {
        let r = client.stream("/api/events", |ev, data| {
            let msg = match ev {
                "snapshot" => {
                    let mut s: Vec<ServiceState> = serde_json::from_value(data["services"].clone()).unwrap_or_default();
                    s.sort_by(svc_order);
                    let machine = data["machine"].as_str().map(String::from);
                    Msg::Snapshot(s, serde_json::from_value(data["configErrors"].clone()).unwrap_or_default(), machine)
                }
                "service" => match serde_json::from_value(data) {
                    Ok(s) => Msg::Service(Box::new(s)),
                    Err(_) => return true,
                },
                "removed" => Msg::Removed(data["name"].as_str().unwrap_or_default().to_string()),
                "metrics" => match serde_json::from_value(data["metrics"].clone()) {
                    Ok(m) => Msg::Metrics(None, data["name"].as_str().unwrap_or_default().to_string(), m),
                    Err(_) => return true,
                },
                "machines" => Msg::Machines(data["local"].as_str().map(String::from), data["machines"].as_array().cloned().unwrap_or_default()),
                "processes" => Msg::Processes(None, serde_json::from_value(data["processes"].clone()).unwrap_or_default()),
                "peer" => match (data["machine"].as_str(), data["event"].as_str()) {
                    (Some(m), Some(e)) => Msg::Peer(m.to_string(), e.to_string(), data["data"].clone()),
                    _ => return true,
                },
                _ => Msg::Connected(true),
            };
            let _ = tx.send(Msg::Connected(true));
            tx.send(msg).is_ok()
        });
        if tx.send(Msg::Connected(false)).is_err() {
            return;
        }
        if let Err(e) = r {
            let _ = tx.send(Msg::Flash(format!("lost connection to daemon: {e}"), true));
        }
        std::thread::sleep(Duration::from_secs(2));
        // The daemon may have restarted with a new port/token.
        if let Some(info) = client::read_info() {
            if info.token != client.info.token || info.port != client.info.port {
                client = Client::new(info);
                let _ = tx.send(Msg::Client(client.clone()));
            }
        }
      }
    });
}

impl Ui {
    fn current(&self) -> Option<&ServiceState> {
        self.services.get(self.selected)
    }

    fn flash(&mut self, msg: impl Into<String>, error: bool) {
        self.flash = Some((msg.into(), Instant::now() + Duration::from_secs(3), error));
    }

    fn machine(&self, name: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.name == name)
    }

    /// Flash an error and return true when the selected service lives on another machine.
    fn remote_only(&mut self, what: &str) -> bool {
        match self.current().and_then(|s| s.machine.clone()) {
            Some(m) => {
                self.flash(format!("{what} isn't available for services on {m}"), true);
                true
            }
            None => false,
        }
    }

    fn sort_and_reselect(&mut self, sel: Option<String>) {
        self.services.sort_by(svc_order);
        self.selected = sel
            .and_then(|k| self.services.iter().position(|x| skey(x) == k))
            .unwrap_or(self.selected.min(self.services.len().saturating_sub(1)));
        if self.logs_for.as_ref().is_some_and(|k| !self.services.iter().any(|s| &skey(s) == k)) {
            self.logs_for = None;
            self.logs.clear();
        }
        self.select_logs();
    }

    fn select_logs(&mut self) {
        let Some(name) = self.current().map(skey) else { return };
        if self.logs_for.as_deref() == Some(name.as_str()) {
            return;
        }
        let gen = self.log_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.logs_for = Some(name.clone());
        self.logs.clear();
        self.log_scroll = 0;
        let (cl, tx, g) = (self.client.clone(), self.tx.clone(), self.log_gen.clone());
        let n = name.clone();
        std::thread::spawn(move || {
            let _ = cl.stream(&format!("{}/logs/stream?tail=500", client::svc_path(&n)), |ev, v| {
                if g.load(Ordering::SeqCst) != gen {
                    return false;
                }
                if ev == "log" {
                    if let Ok(l) = serde_json::from_value(v) {
                        return tx.send(Msg::Log(gen, l)).is_ok();
                    }
                }
                true
            });
        });
        if !self.history.contains_key(&name) {
            let (cl, tx) = (self.client.clone(), self.tx.clone());
            std::thread::spawn(move || {
                if let Ok(s) = cl.metrics(&name, "10m", HISTORY) {
                    let _ = tx.send(Msg::History(name, s));
                }
            });
        }
    }

    fn upsert(&mut self, s: ServiceState) {
        let sel = self.current().map(skey);
        let mut s = s;
        if let Some(i) = self.services.iter().position(|x| x.name == s.name && x.machine == s.machine) {
            if s.metrics.is_none() && s.status == "running" {
                s.metrics = self.services[i].metrics.take();
            }
            self.services[i] = s;
        } else {
            self.services.push(s);
        }
        self.sort_and_reselect(sel);
    }

    /// Replace every service of one machine (None = this one).
    fn replace_services(&mut self, machine: Option<&str>, list: Vec<ServiceState>) {
        let sel = self.current().map(skey);
        self.services.retain(|s| s.machine.as_deref() != machine);
        self.services.extend(list.into_iter().map(|mut s| {
            if machine.is_some() {
                s.machine = machine.map(String::from);
            }
            s
        }));
        self.sort_and_reselect(sel);
    }

    fn remove_service(&mut self, machine: Option<&str>, name: &str) {
        let sel = self.current().map(skey);
        if let Some(i) = self.services.iter().position(|s| s.name == name && s.machine.as_deref() == machine) {
            self.history.remove(&skey(&self.services[i]));
            self.services.remove(i);
        }
        self.sort_and_reselect(sel);
    }

    fn metric(&mut self, machine: Option<&str>, n: &str, m: MetricSample) {
        let Some(s) = self.services.iter_mut().find(|s| s.name == n && s.machine.as_deref() == machine) else { return };
        s.metrics = Some(m);
        let h = self.history.entry(skey(s)).or_default();
        h.push(m);
        if h.len() > HISTORY {
            h.drain(..h.len() - HISTORY);
        }
    }

    fn upsert_machine(&mut self, m: Machine) {
        match self.machines.iter_mut().find(|x| x.name == m.name) {
            Some(x) => *x = m,
            None => {
                self.machines.push(m);
                self.machines.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }

    fn peer_event(&mut self, machine: String, ev: &str, data: Value) {
        let mk = Some(machine.as_str());
        match ev {
            "machine" => {
                if let Some(mut m) = Machine::from_json(&data) {
                    m.name = machine;
                    self.upsert_machine(m);
                }
            }
            "snapshot" => {
                let list: Vec<ServiceState> = serde_json::from_value(data["services"].clone()).unwrap_or_default();
                self.replace_services(mk, list);
            }
            "service" => {
                if let Ok(mut s) = serde_json::from_value::<ServiceState>(data) {
                    s.machine = Some(machine);
                    self.upsert(s);
                }
            }
            "removed" => self.remove_service(mk, data["name"].as_str().unwrap_or_default()),
            "metrics" => {
                if let Ok(m) = serde_json::from_value(data["metrics"].clone()) {
                    self.metric(mk, data["name"].as_str().unwrap_or_default(), m);
                }
            }
            "processes" => {
                self.procs.insert(Some(machine), serde_json::from_value(data["processes"].clone()).unwrap_or_default());
            }
            "removed-machine" => {
                self.machines.retain(|m| m.name != machine);
                self.procs.remove(&Some(machine.clone()));
                let suffix = format!("@{machine}");
                self.history.retain(|k, _| !k.ends_with(&suffix));
                self.replace_services(Some(&machine), vec![]);
            }
            _ => {}
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Snapshot(s, e, machine) => {
                self.config_errors = e;
                if machine.is_some() {
                    self.local_name = machine;
                }
                self.replace_services(None, s);
                let (cl, tx) = (self.client.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    if let Ok(v) = cl.req::<Value>("GET", "/api/processes", None) {
                        let _ = tx.send(Msg::Processes(None, serde_json::from_value(v["processes"].clone()).unwrap_or_default()));
                    }
                });
            }
            Msg::Service(s) => self.upsert(*s),
            Msg::Removed(n) => self.remove_service(None, &n),
            Msg::Metrics(machine, n, m) => self.metric(machine.as_deref(), &n, m),
            Msg::Machines(local, list) => {
                if local.is_some() {
                    self.local_name = local;
                }
                self.machines = list.iter().filter_map(Machine::from_json).collect();
                self.machines.sort_by(|a, b| a.name.cmp(&b.name));
                self.procs.retain(|k, _| k.is_none());
                let sel = self.current().map(skey);
                self.services.retain(|s| s.machine.is_none());
                for v in &list {
                    let Some(name) = v["name"].as_str() else { continue };
                    let svcs: Vec<ServiceState> = serde_json::from_value(v["services"].clone()).unwrap_or_default();
                    self.services.extend(svcs.into_iter().map(|mut s| {
                        s.machine = Some(name.to_string());
                        s
                    }));
                    self.procs.insert(Some(name.to_string()), serde_json::from_value(v["processes"].clone()).unwrap_or_default());
                }
                self.sort_and_reselect(sel);
            }
            Msg::Peer(machine, ev, data) => self.peer_event(machine, &ev, data),
            Msg::Processes(machine, p) => {
                self.procs.insert(machine, p);
            }
            Msg::Log(g, l) => {
                if g == self.log_gen.load(Ordering::SeqCst) {
                    self.logs.push_back(l);
                    if self.logs.len() > MAX_LOGS {
                        self.logs.pop_front();
                    }
                    if self.log_scroll > 0 {
                        self.log_scroll += 1;
                    }
                }
            }
            Msg::History(n, mut s) => {
                if let Some(live) = self.history.get(&n) {
                    let last = s.last().map(|x| x.ts).unwrap_or(0);
                    s.extend(live.iter().filter(|x| x.ts > last).cloned());
                }
                if s.len() > HISTORY {
                    s.drain(..s.len() - HISTORY);
                }
                self.history.insert(n, s);
            }
            Msg::Flash(m, e) => self.flash(m, e),
            Msg::Connected(c) => self.connected = c,
            Msg::Client(c) => {
                self.client = c;
                self.logs_for = None;
                self.select_logs();
            }
        }
    }

    fn act(&mut self, action: &'static str) {
        let Some(s) = self.current() else { return };
        let name = skey(s);
        if let Some(m) = s.machine.as_deref().and_then(|m| self.machine(m)) {
            if !m.allow_control {
                let msg = format!("{} lets this Mac view only — allow control there (Share this Mac, or `lstack remote grant <this-mac> control`) to {action} its services", m.name);
                self.flash(msg, true);
                return;
            }
        }
        self.flash(format!("{action} {name}…"), false);
        let (cl, tx) = (self.client.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let m = match cl.action(&name, action) {
                Ok(r) => {
                    let err = r.status == "errored" || r.status == "crashed";
                    let detail = match (&r.last_error, r.status.as_str()) {
                        (Some(e), s) if s != "running" => format!(" — {e}"),
                        _ => String::new(),
                    };
                    Msg::Flash(format!("{name}: {}{detail}", r.status), err)
                }
                Err(e) => Msg::Flash(e, true),
            };
            let _ = tx.send(m);
        });
    }

    /// Open the selected service's first URL reachable from here in the browser.
    fn open_url(&mut self) {
        let Some(s) = self.current() else { return };
        let remote = s.machine.is_some();
        let url = s.urls.iter().find(|u| u.via.is_some() || !(remote && (u.local_only || u.blocked.is_some()))).map(|u| u.url.clone());
        let msg = match (url, s.urls.is_empty()) {
            (Some(u), _) => match std::process::Command::new("/usr/bin/open")
                .arg(&u)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => (format!("opening {u}"), false),
                Err(e) => (format!("cannot open {u}: {e}"), true),
            },
            (None, true) => (format!("{} has no URLs", s.name), true),
            (None, false) => (format!("{}'s URLs are only reachable on {}", s.name, s.machine.as_deref().unwrap_or("that machine")), true),
        };
        self.flash(msg.0, msg.1);
    }

    fn suspend(&mut self, term: &mut DefaultTerminal, args: &[&str]) {
        ratatui::restore();
        let _ = std::process::Command::new(crate::paths::self_exe()).args(args).status();
        *term = ratatui::init();
        let _ = term.clear();
    }

    /// Returns false to quit.
    fn key(&mut self, k: KeyEvent, term: &mut DefaultTerminal) -> bool {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if self.input_mode {
            match k.code {
                KeyCode::Esc => self.input_mode = false,
                _ if ctrl_c => self.input_mode = false,
                KeyCode::Enter => {
                    if let Some(name) = self.current().filter(|s| s.machine.is_none()).map(|s| s.name.clone()) {
                        let line = std::mem::take(&mut self.input) + "\n";
                        let (cl, tx) = (self.client.clone(), self.tx.clone());
                        std::thread::spawn(move || {
                            if let Err(e) = cl.stdin(&name, &line) {
                                let _ = tx.send(Msg::Flash(e, true));
                            }
                        });
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            }
            return true;
        }
        if ctrl_c {
            return false;
        }
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.select_logs();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.services.len().saturating_sub(1));
                self.select_logs();
            }
            KeyCode::PageUp | KeyCode::Char('[') => self.log_scroll = (self.log_scroll + self.page).min(self.logs.len().saturating_sub(1)),
            KeyCode::PageDown | KeyCode::Char(']') => self.log_scroll = self.log_scroll.saturating_sub(self.page),
            KeyCode::End | KeyCode::Char('G') => self.log_scroll = 0,
            KeyCode::Char('s') => self.act("start"),
            KeyCode::Char('x') => self.act("stop"),
            KeyCode::Char('r') => self.act("restart"),
            KeyCode::Char('o') => self.open_url(),
            KeyCode::Char('a') if self.remote_only("autostart") => {}
            KeyCode::Char('i') if self.remote_only("stdin") => {}
            KeyCode::Char('e') if self.remote_only("shell") => {}
            KeyCode::Char('E') if self.remote_only("edit") => {}
            KeyCode::Char('c') if self.remote_only("clearing logs") => {}
            KeyCode::Char('a') => {
                if let Some(s) = self.current() {
                    let (cl, tx, name, on) = (self.client.clone(), self.tx.clone(), s.name.clone(), !s.autostart);
                    std::thread::spawn(move || {
                        let m = match cl.update(&name, serde_json::json!({ "autostart": on })) {
                            Ok(r) => Msg::Flash(format!("{}: autostart {}", r.name, if r.autostart { "on" } else { "off" }), false),
                            Err(e) => Msg::Flash(e, true),
                        };
                        let _ = tx.send(m);
                    });
                }
            }
            KeyCode::Char('l') | KeyCode::Enter => self.full_logs = !self.full_logs,
            KeyCode::Char('i') => match self.current() {
                Some(s) if s.kind == "process" && s.status == "running" => self.input_mode = true,
                Some(s) if s.kind != "process" => self.flash("press e to open a shell in this container", false),
                Some(_) => self.flash("service is not running", true),
                None => {}
            },
            KeyCode::Char('e') => {
                if let Some(s) = self.current() {
                    if s.kind == "process" {
                        self.flash("use i to send input to a process", false);
                    } else {
                        let n = s.name.clone();
                        self.suspend(term, &["exec", &n]);
                    }
                }
            }
            KeyCode::Char('E') => {
                if let Some(n) = self.current().map(|s| s.name.clone()) {
                    self.suspend(term, &["edit", &n]);
                }
            }
            KeyCode::Char('c') => {
                if let Some(n) = self.current().map(|s| s.name.clone()) {
                    let cl = self.client.clone();
                    std::thread::spawn(move || cl.clear_logs(&n));
                    self.logs.clear();
                    self.log_scroll = 0;
                }
            }
            KeyCode::Char('?') => self.flash("↑↓ select · s start · x stop · r restart · o open url · a autostart · l logs · i stdin · e shell · E edit · [ ] scroll · c clear · q quit", false),
            _ => {}
        }
        true
    }
}

fn main_loop(term: &mut DefaultTerminal, ui: &mut Ui, rx: &Receiver<Msg>) -> Result<(), String> {
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut dirty = true;
    loop {
        while let Ok(m) = rx.try_recv() {
            ui.handle(m);
            dirty = true;
        }
        if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
            term.draw(|fr| draw(fr, ui)).map_err(|e| e.to_string())?;
            last_draw = Instant::now();
            dirty = false;
        }
        if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if !ui.key(k, term) {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

// --------------------------------------------------------------------------- drawing

fn block(title: Line<'static>, focus: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focus { Color::Cyan } else { Color::DarkGray }))
        .title(title)
}

fn draw(fr: &mut Frame, ui: &mut Ui) {
    let area = fr.area();
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).split(area);
    ui.page = (area.height as usize).saturating_sub(14).max(5);

    // header
    let running: Vec<&ServiceState> = ui.services.iter().filter(|s| s.status == "running").collect();
    let local = running.iter().filter(|s| s.machine.is_none());
    let cpu: f64 = local.clone().filter_map(|s| s.metrics.as_ref()).map(|m| m.cpu).sum();
    let mem: f64 = local.filter_map(|s| s.metrics.as_ref()).map(|m| m.mem).sum();
    let mut left = vec![
        Span::raw(" "),
        Span::styled("◆ lstack", Style::new().fg(Color::Cyan).bold()),
        Span::raw("  "),
        Span::styled(format!("● {} running", running.len()), Style::new().fg(Color::Green)),
        Span::raw("  "),
        dim(format!("○ {} stopped", ui.services.len() - running.len())),
    ];
    if !ui.connected {
        left.push(Span::styled("  ✗ disconnected", Style::new().fg(Color::Red)));
    }
    let right = Line::from(vec![
        Span::raw("CPU "),
        Span::raw(f::pct(Some(cpu))).bold(),
        Span::raw("  MEM "),
        Span::raw(f::bytes(Some(mem))).bold(),
        Span::raw("  "),
        dim(chrono::Local::now().format("%H:%M:%S ").to_string()),
    ]);
    fr.render_widget(Paragraph::new(Line::from(left)), rows[0]);
    fr.render_widget(Paragraph::new(right).right_aligned(), rows[0]);

    let body = rows[1];
    let sel = ui.current().cloned();
    let scrolled = if ui.log_scroll > 0 { Span::styled(format!(" (scrolled {})", ui.log_scroll), Style::new().fg(Color::Yellow)) } else { Span::raw("") };

    let title = |s: &ServiceState| match &s.machine {
        Some(m) => Line::from(vec![Span::raw(format!(" {}", s.name)).bold(), dim(format!(" @ {m} "))]),
        None => Line::from(Span::raw(format!(" {} ", s.name)).bold()),
    };
    if let (true, Some(s)) = (ui.full_logs, &sel) {
        let b = block(Line::from(vec![Span::raw(format!(" {} · logs", skey(s))).bold(), scrolled, Span::raw(" ")]), true);
        let inner = b.inner(body);
        fr.render_widget(b, body);
        fr.render_widget(Paragraph::new(log_lines(ui, inner)), inner);
    } else {
        let list_w = LIST_W.min(body.width * 2 / 5);
        let cols = Layout::horizontal([Constraint::Length(list_w), Constraint::Min(10)]).split(body);
        draw_list(fr, ui, cols[0]);
        let detail_w = cols[1].width.saturating_sub(2) as usize;
        let info = match &sel {
            Some(s) => details(s, ui.history.get(&skey(s)).map(Vec::as_slice).unwrap_or(&[]), detail_w),
            None => vec![Line::from(dim(" select a service"))],
        };
        let info_h = ((info.len() + 2) as u16).min(body.height / 2);
        let right = Layout::vertical([Constraint::Length(info_h), Constraint::Min(3)]).split(cols[1]);
        let title = sel.as_ref().map(title).unwrap_or_default();
        let b = block(title, false);
        let inner = b.inner(right[0]);
        fr.render_widget(b, right[0]);
        fr.render_widget(Paragraph::new(info), inner);
        let b = block(Line::from(vec![Span::raw(" Logs").bold(), scrolled, Span::raw(" ")]), false);
        let inner = b.inner(right[1]);
        fr.render_widget(b, right[1]);
        fr.render_widget(Paragraph::new(log_lines(ui, inner)), inner);
    }

    // footer
    let footer = if ui.input_mode {
        Line::from(vec![Span::styled(" stdin › ", Style::new().fg(Color::Yellow)), Span::raw(ui.input.clone()), dim("▏  (enter to send, esc to exit)")])
    } else if let Some((m, _, err)) = ui.flash.as_ref().filter(|x| x.1 > Instant::now()) {
        Line::from(Span::styled(format!(" {m}"), Style::new().fg(if *err { Color::Red } else { Color::Yellow })))
    } else {
        let keys = [("↑↓", "select"), ("s", "start"), ("x", "stop"), ("r", "restart"), ("o", "open"), ("a", "autostart"), ("l", "logs"), ("i", "stdin"), ("e", "shell"), ("[ ]", "scroll"), ("?", "help"), ("q", "quit")];
        let mut v = vec![Span::raw(" ")];
        for (k, d) in keys {
            v.push(Span::styled(k, Style::new().fg(Color::Cyan).bold()));
            v.push(dim(format!(" {d}  ")));
        }
        Line::from(v)
    };
    fr.render_widget(Paragraph::new(footer), rows[2]);
}

fn draw_list(fr: &mut Frame, ui: &Ui, area: Rect) {
    let procs = proc_lines(ui, area.width.saturating_sub(2) as usize);
    let area = if procs.is_empty() || area.height < 14 {
        area
    } else {
        let h = ((procs.len() + 2) as u16).min(area.height / 3);
        let parts = Layout::vertical([Constraint::Min(5), Constraint::Length(h)]).split(area);
        let b = block(Line::from(Span::raw(" Other processes ").bold()), false);
        let inner = b.inner(parts[1]);
        fr.render_widget(b, parts[1]);
        fr.render_widget(Paragraph::new(procs), inner);
        parts[0]
    };
    let b = block(Line::from(Span::raw(" Services ").bold()), true);
    let inner = b.inner(area);
    fr.render_widget(b, area);
    let w = inner.width as usize;
    let name_w = w.saturating_sub(23).max(4);
    let mut lines: Vec<Line> = vec![];
    let mut sel_line = 0;
    for row in build_rows(&ui.services, &ui.machines) {
        let i = match row {
            Row::Header(None) => {
                let name = ui.local_name.clone().unwrap_or_else(|| "this machine".into());
                lines.push(Line::from(vec![Span::raw(" "), Span::styled(f::truncate(&name, w.saturating_sub(12)), Style::new().bold()), dim(" · this mac")]));
                continue;
            }
            Row::Header(Some(m)) => {
                let info = ui.machine(&m).cloned().unwrap_or(Machine { name: m.clone(), status: "connecting".into(), ..Default::default() });
                let mut v = vec![
                    Span::raw(" "),
                    Span::styled(f::truncate(&m, w.saturating_sub(16)), Style::new().bold()),
                    Span::raw(" "),
                    Span::styled(format!("● {}", info.status), Style::new().fg(machine_color(&info.status))),
                ];
                if !info.allow_control && info.status == "online" {
                    v.push(dim(" 🔒 view only"));
                } else if info.status == "online" {
                    v.push(dim(" 🔒"));
                }
                lines.push(Line::from(v));
                if info.status != "online" {
                    if let Some(e) = &info.error {
                        lines.push(Line::from(Span::styled(f::truncate(&format!("   {e}"), w), Style::new().fg(Color::Red))));
                    }
                }
                if !ui.services.iter().any(|s| s.machine.as_ref() == Some(&m)) && info.status == "online" {
                    lines.push(Line::from(dim("   no services")));
                }
                continue;
            }
            Row::Service(i) => i,
        };
        let s = &ui.services[i];
        let run = s.status == "running";
        let m = s.metrics.as_ref();
        let cpu = if run { f::pct(m.map(|m| m.cpu)) } else { String::new() };
        let mem = if run { f::bytes(m.map(|m| m.mem)) } else if s.restart_at.is_some() { "restarting".into() } else { s.status.clone() };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled("●", Style::new().fg(status_color(&s.status))),
            Span::raw(" "),
            Span::styled(if s.autostart { "⇡" } else { " " }, Style::new().fg(Color::Cyan)),
            Span::raw(" "),
            Span::raw(f::pad(&s.name, name_w)),
            Span::raw(format!(" {cpu:>6} ")),
            if run { Span::raw(f::pad(&mem, 8)) } else { dim(f::pad(&mem, 8)) },
        ]);
        if i == ui.selected {
            sel_line = lines.len();
            lines.push(line.style(Style::new().add_modifier(Modifier::REVERSED)));
        } else {
            lines.push(line);
        }
    }
    if ui.services.is_empty() {
        lines.extend([Line::from(dim(" no services")), Line::default(), Line::from(dim(" add one:")), Line::from(dim(" lstack add <name> -- <cmd>"))]);
    }
    for e in &ui.config_errors {
        lines.push(Line::from(Span::styled(f::truncate(&format!(" ⚠ {e}"), w), Style::new().fg(Color::Red))));
    }
    // Keep the selection visible when the list is taller than the pane.
    let h = inner.height as usize;
    let off = if sel_line >= h { sel_line + 1 - h } else { 0 };
    fr.render_widget(Paragraph::new(lines).scroll((off as u16, 0)), inner);
}

/// Busiest non-service processes over the daemon's thresholds, grouped by machine when peers exist.
fn proc_lines(ui: &Ui, w: usize) -> Vec<Line<'static>> {
    let mut groups: Vec<Option<String>> = vec![None];
    groups.extend(ui.machines.iter().map(|m| Some(m.name.clone())));
    let multi = groups.len() > 1;
    let name_w = w.saturating_sub(33).max(4);
    let mut out = vec![];
    for g in groups {
        let mut list: Vec<&ProcInfo> = ui.procs.get(&g).into_iter().flatten().filter(|p| p.service.is_none()).collect();
        if list.is_empty() {
            continue;
        }
        list.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        if multi {
            let name = g.or_else(|| ui.local_name.clone()).unwrap_or_else(|| "this machine".into());
            out.push(Line::from(Span::styled(f::truncate(&format!(" {name}"), w), Style::new().bold())));
        }
        for p in list.into_iter().take(PROC_MAX) {
            out.push(Line::from(vec![
                Span::raw(" "),
                Span::raw(f::pad(&p.name, name_w)),
                dim(format!(" {:>6}", p.pid)),
                Span::raw(format!(" {:>6}", f::pct(Some(p.cpu)))),
                Span::raw(format!(" {:>6}", f::bytes(Some(p.mem)))),
                dim(format!(" {:>9}", p.net.map(|n| f::rate(Some(n))).unwrap_or_default())),
            ]));
        }
    }
    out
}

fn details(s: &ServiceState, hist: &[MetricSample], w: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    let st = if s.restart_at.is_some() { Span::styled(format!("{} (x to stop)", s.status_label()), Style::new().fg(Color::Yellow)) } else { Span::styled(s.status.clone(), Style::new().fg(status_color(&s.status))) };
    let mut head = vec![dim(format!(" {}  ", s.kind)), st, Span::raw("  ")];
    match s.health.as_str() {
        "healthy" => head.push(Span::styled("♥ healthy  ", Style::new().fg(Color::Green))),
        "unhealthy" => head.push(Span::styled("♥ unhealthy  ", Style::new().fg(Color::Red))),
        "unknown" => head.push(Span::styled("♥ checking  ", Style::new().fg(Color::Yellow))),
        _ => {}
    }
    if let Some(p) = s.pid {
        head.push(dim(format!("pid {p}  ")));
    }
    if let Some(t) = s.started_at {
        head.push(dim(format!("up {}  ", f::duration(Some(crate::paths::now_ms() - t)))));
    }
    if s.restarts > 0 {
        head.push(Span::styled(format!("↺ {}  ", s.restarts), Style::new().fg(Color::Yellow)));
    }
    if s.autostart {
        head.push(Span::styled("⇡ autostart", Style::new().fg(Color::Cyan)));
    }
    out.push(Line::from(head));
    let c = &s.config;
    let what = match c.kind.as_str() {
        "process" => format!("$ {}", c.command.as_deref().unwrap_or("")),
        "docker" => format!(
            "docker  {}{}",
            c.image.clone().or(c.container.clone()).unwrap_or_default(),
            c.ports.as_ref().filter(|p| !p.is_empty()).map(|p| format!("  {}", p.join(" "))).unwrap_or_default()
        ),
        "compose" => format!("compose  {}", c.compose_file.as_deref().unwrap_or("")),
        _ => format!("k8s  {}", c.manifest.clone().or(c.selector.clone()).unwrap_or_default()),
    };
    out.push(Line::from(dim(format!(" {}", f::truncate(&what, w.saturating_sub(2))))));
    if let (Some(cwd), "process") = (&c.cwd, c.kind.as_str()) {
        out.push(Line::from(dim(format!(" {}", f::truncate(&format!("  in {cwd}"), w.saturating_sub(2))))));
    }
    if let (Some(e), false) = (&s.last_error, s.status == "running") {
        out.push(Line::from(Span::styled(format!(" {}", f::truncate(&format!("✗ {e}"), w.saturating_sub(2))), Style::new().fg(Color::Red))));
    }
    for u in s.urls.iter().take(3) {
        let mut v = vec![Span::raw(" "), Span::styled(f::truncate(&u.url, w.saturating_sub(4)), Style::new().fg(Color::Cyan))];
        if let Some(l) = &u.label {
            v.push(dim(format!("  {l}")));
        }
        if let Some(m) = &u.via {
            v.push(dim(format!("  🔒 via {m}")));
        } else if u.blocked.as_deref() == Some("untunneled") {
            v.push(dim("  (no tunnel: not reachable from here)"));
        } else if u.local_only {
            v.push(dim(if s.machine.is_some() { "  (not reachable from here)" } else { "  (localhost only)" }));
        }
        out.push(Line::from(v));
    }
    if s.urls.len() > 3 {
        out.push(Line::from(dim(format!(" +{} more", s.urls.len() - 3))));
    }
    if let Some(hb) = &c.heartbeat {
        let line = match (&s.heartbeat, hb.enabled) {
            (_, Some(false)) => dim(" heartbeat: paused"),
            (None, _) => dim(" heartbeat: waiting for first ping"),
            (Some(h), _) => {
                let when = ago(crate::paths::now_ms() - h.last_ping);
                if !h.ok {
                    let err = h.error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default();
                    Span::styled(f::truncate(&format!(" heartbeat: ✗ {} ping failed {when} ago{err}", h.kind), w), Style::new().fg(Color::Red))
                } else if h.kind == "fail" {
                    Span::styled(format!(" heartbeat: ⚠ failure reported {when} ago"), Style::new().fg(Color::Yellow))
                } else {
                    Span::styled(format!(" heartbeat: ✓ {} {when} ago", h.kind), Style::new().fg(Color::Green))
                }
            }
        };
        out.push(Line::from(line));
    }
    let mut secrets: Vec<String> = c.secret_refs().into_iter().map(|(_, n)| n).collect();
    secrets.sort();
    secrets.dedup();
    if !secrets.is_empty() {
        out.push(Line::from(vec![Span::raw(" 🔒 "), dim(f::truncate(&format!("secrets: {}", secrets.join(", ")), w.saturating_sub(5)))]));
    }
    out.push(Line::default());
    let sw = w.saturating_sub(32).max(10);
    let cpu: Vec<f64> = hist.iter().map(|h| h.cpu).collect();
    let mem: Vec<f64> = hist.iter().map(|h| h.mem).collect();
    let net: Vec<f64> = hist.iter().map(|h| h.net_rx.unwrap_or(0.0) + h.net_tx.unwrap_or(0.0)).collect();
    let max = |v: &[f64]| if v.is_empty() { None } else { Some(v.iter().cloned().fold(f64::MIN, f64::max)) };
    let m = s.metrics.as_ref();
    out.push(Line::from(vec![
        dim(" CPU "),
        Span::styled(f::sparkline(&cpu, sw), Style::new().fg(Color::Green)),
        Span::raw(format!(" {:<9}", f::pct(m.map(|m| m.cpu)))).bold(),
        dim(format!("max {}", f::pct(max(&cpu)))),
    ]));
    out.push(Line::from(vec![
        dim(" MEM "),
        Span::styled(f::sparkline(&mem, sw), Style::new().fg(Color::Blue)),
        Span::raw(format!(" {:<9}", f::bytes(m.map(|m| m.mem)))).bold(),
        dim(format!("max {}", f::bytes(max(&mem)))),
    ]));
    out.push(Line::from(vec![
        dim(" NET "),
        Span::styled(f::sparkline(&net, sw), Style::new().fg(Color::Magenta)),
        dim(format!(" ↓{} ↑{}", f::rate(m.and_then(|m| m.net_rx)), f::rate(m.and_then(|m| m.net_tx)))),
    ]));
    out
}

fn clean(s: &str) -> String {
    f::strip_ansi(s).replace('\t', "  ").chars().filter(|c| !c.is_control()).collect()
}

fn log_lines(ui: &Ui, area: Rect) -> Vec<Line<'static>> {
    let h = area.height as usize;
    let w = area.width as usize;
    let end = ui.logs.len().saturating_sub(ui.log_scroll);
    let start = end.saturating_sub(h);
    ui.logs
        .range(start..end)
        .map(|l| {
            let text = f::truncate(&clean(&l.line), w.saturating_sub(11));
            let style = match l.stream.as_str() {
                "stderr" => Style::new().fg(Color::Red),
                "system" => Style::new().fg(Color::Cyan),
                _ => Style::new(),
            };
            Line::from(vec![Span::raw(" "), dim(f::local_time(l.ts)), Span::raw(" "), Span::styled(text, style)])
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, machine: Option<&str>) -> ServiceState {
        ServiceState { name: name.into(), machine: machine.map(String::from), ..Default::default() }
    }

    #[test]
    fn rows_without_peers_have_no_headers() {
        let s = vec![svc("a", None), svc("b", None)];
        assert_eq!(build_rows(&s, &[]), vec![Row::Service(0), Row::Service(1)]);
    }

    #[test]
    fn rows_grouped_by_machine() {
        let mut s = vec![svc("web", Some("studio")), svc("b", None), svc("a", None), svc("api", Some("studio"))];
        s.sort_by(svc_order);
        assert_eq!(s.iter().map(skey).collect::<Vec<_>>(), ["a", "b", "api@studio", "web@studio"]);
        let m = vec![Machine { name: "studio".into(), status: "online".into(), ..Default::default() }, Machine { name: "air".into(), ..Default::default() }];
        assert_eq!(
            build_rows(&s, &m),
            vec![
                Row::Header(None),
                Row::Service(0),
                Row::Service(1),
                Row::Header(Some("air".into())),
                Row::Header(Some("studio".into())),
                Row::Service(2),
                Row::Service(3),
            ]
        );
    }

    #[test]
    fn ago_units() {
        assert_eq!(ago(5_000), "5s");
        assert_eq!(ago(120_000), "2m");
        assert_eq!(ago(7_200_000), "2h");
        assert_eq!(ago(-1), "0s");
    }
}

//! `lstack top` — htop-style dashboard (ratatui).
use crate::client::{self, Client};
use crate::fmt as f;
use crate::types::{LogLine, MetricSample, ServiceState};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, VecDeque};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LIST_W: u16 = 40;
const HISTORY: usize = 120;
const MAX_LOGS: usize = 2000;

enum Msg {
    Snapshot(Vec<ServiceState>, Vec<String>),
    Service(Box<ServiceState>),
    Removed(String),
    Metrics(String, MetricSample),
    Log(u64, LogLine),
    History(String, Vec<MetricSample>),
    Flash(String, bool),
    Connected(bool),
    Client(Client),
}

struct Ui {
    client: Client,
    tx: Sender<Msg>,
    services: Vec<ServiceState>,
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
                    s.sort_by(|a, b| a.name.cmp(&b.name));
                    Msg::Snapshot(s, serde_json::from_value(data["configErrors"].clone()).unwrap_or_default())
                }
                "service" => match serde_json::from_value(data) {
                    Ok(s) => Msg::Service(Box::new(s)),
                    Err(_) => return true,
                },
                "removed" => Msg::Removed(data["name"].as_str().unwrap_or_default().to_string()),
                "metrics" => match serde_json::from_value(data["metrics"].clone()) {
                    Ok(m) => Msg::Metrics(data["name"].as_str().unwrap_or_default().to_string(), m),
                    Err(_) => return true,
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

    fn select_logs(&mut self) {
        let Some(name) = self.current().map(|s| s.name.clone()) else { return };
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
            let _ = cl.stream(&format!("/api/services/{}/logs/stream?tail=500", client::enc(&n)), |ev, v| {
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
        let sel = self.current().map(|s| s.name.clone());
        let mut s = s;
        if let Some(i) = self.services.iter().position(|x| x.name == s.name) {
            if s.metrics.is_none() && s.status == "running" {
                s.metrics = self.services[i].metrics.take();
            }
            self.services[i] = s;
        } else {
            self.services.push(s);
            self.services.sort_by(|a, b| a.name.cmp(&b.name));
        }
        if let Some(sel) = sel {
            self.selected = self.services.iter().position(|x| x.name == sel).unwrap_or(0);
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Snapshot(s, e) => {
                let sel = self.current().map(|s| s.name.clone());
                self.services = s;
                self.config_errors = e;
                self.selected = sel.and_then(|n| self.services.iter().position(|x| x.name == n)).unwrap_or(self.selected.min(self.services.len().saturating_sub(1)));
                self.select_logs();
            }
            Msg::Service(s) => {
                self.upsert(*s);
                self.select_logs();
            }
            Msg::Removed(n) => {
                self.services.retain(|s| s.name != n);
                self.history.remove(&n);
                self.selected = self.selected.min(self.services.len().saturating_sub(1));
                if self.logs_for.as_deref() == Some(n.as_str()) {
                    self.logs_for = None;
                    self.logs.clear();
                }
                self.select_logs();
            }
            Msg::Metrics(n, m) => {
                if let Some(s) = self.services.iter_mut().find(|s| s.name == n) {
                    s.metrics = Some(m.clone());
                }
                let h = self.history.entry(n).or_default();
                h.push(m);
                if h.len() > HISTORY {
                    h.drain(..h.len() - HISTORY);
                }
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
        let Some(name) = self.current().map(|s| s.name.clone()) else { return };
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
                    if let Some(name) = self.current().map(|s| s.name.clone()) {
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
            KeyCode::Char('?') => self.flash("↑↓ select · s start · x stop · r restart · a autostart · l logs · i stdin · e shell · E edit · [ ] scroll · c clear · q quit", false),
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
    let cpu: f64 = running.iter().filter_map(|s| s.metrics.as_ref()).map(|m| m.cpu).sum();
    let mem: f64 = running.iter().filter_map(|s| s.metrics.as_ref()).map(|m| m.mem).sum();
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

    if let (true, Some(s)) = (ui.full_logs, &sel) {
        let b = block(Line::from(vec![Span::raw(format!(" {} · logs", s.name)).bold(), scrolled, Span::raw(" ")]), true);
        let inner = b.inner(body);
        fr.render_widget(b, body);
        fr.render_widget(Paragraph::new(log_lines(ui, inner)), inner);
    } else {
        let list_w = LIST_W.min(body.width * 2 / 5);
        let cols = Layout::horizontal([Constraint::Length(list_w), Constraint::Min(10)]).split(body);
        draw_list(fr, ui, cols[0]);
        let detail_w = cols[1].width.saturating_sub(2) as usize;
        let info = match &sel {
            Some(s) => details(s, ui.history.get(&s.name).map(Vec::as_slice).unwrap_or(&[]), detail_w),
            None => vec![Line::from(dim(" select a service"))],
        };
        let info_h = ((info.len() + 2) as u16).min(body.height / 2);
        let right = Layout::vertical([Constraint::Length(info_h), Constraint::Min(3)]).split(cols[1]);
        let title = sel.as_ref().map(|s| Line::from(Span::raw(format!(" {} ", s.name)).bold())).unwrap_or_default();
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
        let keys = [("↑↓", "select"), ("s", "start"), ("x", "stop"), ("r", "restart"), ("a", "autostart"), ("l", "logs"), ("i", "stdin"), ("e", "shell"), ("[ ]", "scroll"), ("?", "help"), ("q", "quit")];
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
    let b = block(Line::from(Span::raw(" Services ").bold()), true);
    let inner = b.inner(area);
    fr.render_widget(b, area);
    let w = inner.width as usize;
    let name_w = w.saturating_sub(23).max(4);
    let mut lines: Vec<Line> = ui
        .services
        .iter()
        .enumerate()
        .map(|(i, s)| {
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
                line.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();
    if ui.services.is_empty() {
        lines.extend([Line::from(dim(" no services")), Line::default(), Line::from(dim(" add one:")), Line::from(dim(" lstack add <name> -- <cmd>"))]);
    }
    for e in &ui.config_errors {
        lines.push(Line::from(Span::styled(f::truncate(&format!(" ⚠ {e}"), w), Style::new().fg(Color::Red))));
    }
    // Keep the selection visible when the list is taller than the pane.
    let h = inner.height as usize;
    let off = if ui.selected >= h { ui.selected + 1 - h } else { 0 };
    fr.render_widget(Paragraph::new(lines).scroll((off as u16, 0)), inner);
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


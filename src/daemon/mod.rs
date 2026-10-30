//! The long-running daemon (`lstack daemon run`), normally started by launchd at login.
pub mod approvals;
pub mod docker_api;
pub mod docker_import;
pub mod envimport;
mod drivers;
pub mod exec;
pub mod health;
pub mod forwards;
pub mod link;
pub mod heartbeat;
pub mod peers;
pub mod secretshare;
pub mod logs;
pub mod procinfo;
pub mod remote;
pub mod secrets;
pub mod server;
pub mod share;
pub mod store;
pub mod supervisor;
pub mod terminal;
pub mod totp;

use crate::paths::{self, now_ms, VERSION};
use crate::types::DaemonInfo;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

/// The daemon holds the secret store: no core dumps, and no debugger attaching to read its memory
/// (on top of the hardened runtime the app bundle is signed with).
fn harden() {
    let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        if std::env::var_os("LSTACK_ALLOW_DEBUGGER").is_none() {
            const PT_DENY_ATTACH: libc::c_int = 31;
            libc::ptrace(PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0);
        }
    }
}

/// Port of this daemon's local API (set once it's bound). Tunnels from paired Macs may never reach it.
static API_PORT: AtomicU16 = AtomicU16::new(0);

pub fn api_port() -> u16 {
    API_PORT.load(Ordering::Relaxed)
}

fn log(msg: &str) {
    println!("{} {}", paths::iso(now_ms()), crate::scrub::scrub(msg));
}

fn token() -> String {
    let mut buf = [0u8; 24];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn write_info(info: &DaemonInfo) {
    let tmp = paths::daemon_info().with_extension("json.tmp");
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(serde_json::to_string_pretty(info).unwrap_or_default().as_bytes()));
    if res.is_ok() {
        let _ = std::fs::rename(&tmp, paths::daemon_info());
    }
}

/// Run the daemon in the foreground (this is what launchd invokes).
pub fn run() -> i32 {
    paths::ensure_dirs();
    if let Some(info) = crate::client::read_info() {
        if info.pid != std::process::id() as i32 && crate::client::ping(&info) {
            eprintln!("lstack daemon already running (pid {}, port {})", info.pid, info.port);
            return 0;
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(serve())
}

async fn serve() -> i32 {
    harden();
    if let Some(e) = secrets::init() {
        log(&format!("secret store: {e}"));
    }
    let sup = match supervisor::Sup::new() {
        Ok(s) => s,
        Err(e) => {
            log(&format!("cannot open metrics database: {e}"));
            return 1;
        }
    };
    approvals::init(&sup);
    let base = paths::default_port();
    let mut bound = None;
    for port in base..base.saturating_add(10) {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            bound = Some((l, port));
            break;
        }
    }
    let Some((listener, port)) = bound else {
        log(&format!("could not bind 127.0.0.1:{base}-{}", base + 9));
        return 1;
    };
    API_PORT.store(port, Ordering::Relaxed);
    let token = token();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let remote = remote::Remote::new();
    let peers = peers::Peers::new(sup.clone(), tokio::runtime::Handle::current());
    let state = server::AppState { sup: sup.clone(), token: Arc::new(token.clone()), shutdown: shutdown.clone(), remote: remote.clone(), peers: peers.clone() };
    let remote_state = state.clone();
    let state_remote_stop = remote.clone();
    let info = DaemonInfo {
        pid: std::process::id() as i32,
        port,
        token,
        version: VERSION.into(),
        started_at: now_ms(),
        bin: Some(paths::self_exe().to_string_lossy().into_owned()),
        running: Some(true),
    };
    write_info(&info);
    log(&format!("lstack daemon v{VERSION} listening on 127.0.0.1:{port} (pid {})", info.pid));
    {
        // Redial machines that are down right after the Mac wakes up.
        let peers = peers.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            // The monotonic clock stops while the Mac sleeps; the wall clock doesn't.
            let mut mono = std::time::Instant::now();
            let mut wall = now_ms();
            loop {
                tick.tick().await;
                let slept = (now_ms() - wall) - mono.elapsed().as_millis() as i64 > 5_000;
                (mono, wall) = (std::time::Instant::now(), now_ms());
                if slept {
                    peers.reconnect_down();
                }
            }
        });
    }
    tokio::spawn(async move { remote.apply(&remote_state).await });
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, server::router(state)).await {
            log(&format!("server error: {e}"));
        }
    });
    let (mut term, mut int, mut hup) = (
        signal(SignalKind::terminate()).unwrap(),
        signal(SignalKind::interrupt()).unwrap(),
        signal(SignalKind::hangup()).unwrap(),
    );
    let (s2, p2, r2) = (sup.clone(), peers.clone(), state_remote_stop.clone());
    heartbeat::start(&sup);
    tokio::spawn(async move {
        s2.boot().await;
        envimport::spawn_watcher(s2.clone());
        secretshare::spawn_sync(s2.clone(), p2, r2);
        for e in s2.config_errors() {
            log(&format!("config error: {e}"));
        }
        log(&format!("managing {} service(s)", s2.list().len()));
    });
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = shutdown.notified() => break,
            _ = hup.recv() => log(&format!("reloaded: {:?}", sup.reload())),
        }
    }
    log("shutting down…");
    state_remote_stop.stop();
    peers.shutdown();
    sup.shutdown().await;
    if let Some(mut i) = crate::client::read_info() {
        if i.pid == info.pid {
            i.running = Some(false);
            write_info(&i);
        }
    }
    0
}

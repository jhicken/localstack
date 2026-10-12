//! The long-running daemon (`lstack daemon run`), normally started by launchd at login.
pub mod docker_api;
mod drivers;
pub mod health;
pub mod logs;
pub mod procinfo;
pub mod remote;
pub mod server;
pub mod store;
pub mod supervisor;
pub mod terminal;
pub mod totp;

use crate::paths::{self, now_ms, VERSION};
use crate::types::DaemonInfo;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

fn log(msg: &str) {
    println!("{} {msg}", paths::iso(now_ms()));
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
    let sup = match supervisor::Sup::new() {
        Ok(s) => s,
        Err(e) => {
            log(&format!("cannot open metrics database: {e}"));
            return 1;
        }
    };
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
    let token = token();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let remote = remote::Remote::new();
    let state = server::AppState { sup: sup.clone(), token: Arc::new(token.clone()), shutdown: shutdown.clone(), remote: remote.clone() };
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
    let s2 = sup.clone();
    tokio::spawn(async move {
        s2.boot().await;
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
    sup.shutdown().await;
    if let Some(mut i) = crate::client::read_info() {
        if i.pid == info.pid {
            i.running = Some(false);
            write_info(&i);
        }
    }
    0
}

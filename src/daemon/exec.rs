//! `lstack exec` relay. The CLI can't scrub secrets (only the daemon knows them), so the daemon
//! runs `docker exec` itself — on a pseudo-terminal when the user is on a terminal — and streams
//! scrubbed output back over an upgraded HTTP connection.
//!
//! Frames, both directions: 1 byte type + u32 BE length + payload.
use super::server::{ApiErr, AppState};
use crate::scrub::StreamScrubber;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const PROTOCOL: &str = "lstack-exec";
/// Data: stdout (daemon→CLI) or stdin (CLI→daemon).
pub const DATA: u8 = 0;
/// CLI→daemon: rows u16, cols u16.
pub const RESIZE: u8 = 1;
/// daemon→CLI: exit code i32, last frame.
pub const EXIT: u8 = 2;
/// daemon→CLI: stderr (non-TTY only; a TTY merges it into stdout).
pub const STDERR: u8 = 3;
/// CLI→daemon: stdin closed.
pub const EOF: u8 = 4;

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 5);
    f.push(kind);
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// Releases a partially matched secret after this pause if it's 1–2 bytes (typing stays snappy)…
const SHORT_IDLE: Duration = Duration::from_millis(25);
/// …and anything still held after this one (the rest of a secret isn't coming).
const LONG_IDLE: Duration = Duration::from_millis(300);

enum Out {
    Data(u8, Vec<u8>),
    Done(u8),
}

enum Child {
    Pty { master: std::fs::File, child: tokio::process::Child },
    Pipes { stdin: Option<tokio::process::ChildStdin>, child: tokio::process::Child },
}

pub async fn handler(State(st): State<AppState>, Path(name): Path<String>, Query(q): Query<HashMap<String, String>>, mut req: Request) -> Response {
    let s = match st.sup.get_state(&name) {
        Ok(s) => s,
        Err(e) => return ApiErr::from(e).into_response(),
    };
    let container = match tokio::task::spawn_blocking(move || crate::cli::exec_container(&s)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return ApiErr(400, e).into_response(),
        Err(e) => return ApiErr(500, e.to_string()).into_response(),
    };
    let cmd: Vec<String> = match q.get("cmd").map(|c| serde_json::from_str(c)) {
        Some(Ok(v)) => v,
        Some(Err(e)) => return ApiErr(400, format!("bad cmd: {e}")).into_response(),
        None => crate::cli::DEFAULT_SHELL.iter().map(|s| s.to_string()).collect(),
    };
    let tty = q.get("tty").is_some_and(|v| v == "1");
    let size = (q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(24u16), q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(80u16));
    let term = q.get("term").filter(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))).cloned();

    let child = match spawn(&container, &cmd, tty, size, term.as_deref()) {
        Ok(c) => c,
        Err(e) => return ApiErr(500, format!("docker exec: {e}")).into_response(),
    };
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match upgrade.await {
            Ok(io) => relay(TokioIo::new(io), child).await,
            Err(e) => eprintln!("exec upgrade failed: {e}"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, PROTOCOL)
        .body(Body::empty())
        .unwrap()
}

fn spawn(container: &str, cmd: &[String], tty: bool, (rows, cols): (u16, u16), term: Option<&str>) -> std::io::Result<Child> {
    let mut c = tokio::process::Command::new(super::docker_api::docker_bin());
    c.arg("exec").arg(if tty { "-it" } else { "-i" });
    if let Some(t) = term {
        c.args(["-e", &format!("TERM={t}")]);
    }
    c.arg(container).args(cmd).kill_on_drop(true);
    if !tty {
        use std::process::Stdio;
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        return Ok(Child::Pipes { stdin: child.stdin.take(), child });
    }
    let (mut m, mut s) = (0, 0);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    unsafe { libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC) };
    c.stdin(slave.try_clone()?).stdout(slave.try_clone()?).stderr(slave);
    unsafe {
        c.pre_exec(|| {
            // New session with the PTY as controlling terminal, so ^C etc. reach docker.
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = c.spawn()?;
    drop(c); // close our copies of the slave, so reads hit EOF when docker exits
    Ok(Child::Pty { master: std::fs::File::from(master), child })
}

fn reader(mut r: impl Read + Send + 'static, kind: u8, tx: mpsc::Sender<Out>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break, // a PTY master reports EIO once the child side closes
                Ok(n) => {
                    if tx.blocking_send(Out::Data(kind, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.blocking_send(Out::Done(kind));
    });
}

async fn input_loop(rd: &mut (impl tokio::io::AsyncRead + Unpin), in_tx: std::sync::mpsc::Sender<Option<Vec<u8>>>, master_fd: Option<i32>) {
    let mut head = [0u8; 5];
    loop {
        if rd.read_exact(&mut head).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > 1 << 20 {
            return;
        }
        let mut body = vec![0u8; len];
        if rd.read_exact(&mut body).await.is_err() {
            return;
        }
        match head[0] {
            DATA => {
                let _ = in_tx.send(Some(body));
            }
            EOF => {
                let _ = in_tx.send(None);
            }
            RESIZE if body.len() == 4 => {
                if let Some(fd) = master_fd {
                    let ws = libc::winsize { ws_row: u16::from_be_bytes([body[0], body[1]]), ws_col: u16::from_be_bytes([body[2], body[3]]), ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
                }
            }
            _ => {}
        }
    }
}

async fn relay<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static>(io: S, child: Child) {
    let (mut rd, mut wr) = tokio::io::split(io);
    let (tx, mut rx) = mpsc::channel::<Out>(64);
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    let (mut child, master_fd, mut streams) = match child {
        Child::Pty { master, child } => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
            let w = master.try_clone().ok();
            reader(master, DATA, tx.clone());
            if let Some(mut w) = w {
                std::thread::spawn(move || {
                    while let Ok(Some(b)) = in_rx.recv() {
                        if w.write_all(&b).is_err() {
                            break;
                        }
                    }
                });
            }
            (child, Some(fd), 1)
        }
        Child::Pipes { stdin, mut child } => {
            let out = child.stdout.take().and_then(|o| o.into_owned_fd().ok()).map(std::fs::File::from);
            let err = child.stderr.take().and_then(|o| o.into_owned_fd().ok()).map(std::fs::File::from);
            let mut streams = 0;
            for (f, k) in [(out, DATA), (err, STDERR)] {
                if let Some(f) = f {
                    reader(f, k, tx.clone());
                    streams += 1;
                }
            }
            if let Some(stdin) = stdin.and_then(|s| s.into_owned_fd().ok()) {
                let mut w = std::fs::File::from(stdin);
                std::thread::spawn(move || {
                    // None = EOF: dropping `w` closes docker's stdin.
                    while let Ok(Some(b)) = in_rx.recv() {
                        if w.write_all(&b).is_err() {
                            break;
                        }
                    }
                });
            }
            (child, None, streams)
        }
    };
    drop(tx);

    // Client → child. If the client goes away, hang up on docker like a closed terminal would.
    let pid = child.id();
    let input = tokio::spawn(async move {
        input_loop(&mut rd, in_tx, master_fd).await;
        if let Some(pid) = pid {
            unsafe { libc::kill(pid as i32, libc::SIGHUP) };
        }
    });

    // Child → client, scrubbed.
    let mut sc = [StreamScrubber::default(), StreamScrubber::default()];
    let slot = |k: u8| usize::from(k == STDERR);
    let mut idle: Option<(tokio::time::Instant, bool)> = None;
    let mut alive = true;
    while streams > 0 && alive {
        let ev = match idle {
            Some((at, _)) => tokio::select! {
                ev = rx.recv() => ev,
                _ = tokio::time::sleep_until(at) => {
                    let (_, long) = idle.take().unwrap();
                    for (i, s) in sc.iter_mut().enumerate() {
                        let out = s.flush(long);
                        if !out.is_empty() && wr.write_all(&frame(if i == 1 { STDERR } else { DATA }, &out)).await.is_err() {
                            alive = false;
                        }
                    }
                    if !long && sc.iter().any(|s| s.has_held()) {
                        idle = Some((tokio::time::Instant::now() + LONG_IDLE - SHORT_IDLE, true));
                    }
                    continue;
                }
            },
            None => rx.recv().await,
        };
        match ev {
            Some(Out::Data(k, b)) => {
                let out = sc[slot(k)].push(&b);
                if !out.is_empty() && wr.write_all(&frame(k, &out)).await.is_err() {
                    alive = false;
                }
                idle = sc.iter().any(|s| s.has_held()).then(|| (tokio::time::Instant::now() + SHORT_IDLE, false));
            }
            Some(Out::Done(k)) => {
                let out = sc[slot(k)].flush(true);
                if !out.is_empty() && wr.write_all(&frame(k, &out)).await.is_err() {
                    alive = false;
                }
                streams -= 1;
            }
            None => break,
        }
    }
    input.abort();
    if !alive {
        let _ = child.kill().await; // client went away
        return;
    }
    let code = match child.wait().await {
        Ok(st) => st.code().unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(1)),
        Err(_) => 1,
    };
    let _ = wr.write_all(&frame(EXIT, &code.to_be_bytes())).await;
    let _ = wr.shutdown().await;
}

/// CLI side: runs `cmd` in the service's container through the daemon. Returns the exit code.
pub fn run_client(info: &crate::types::DaemonInfo, name: &str, cmd: &[String]) -> Result<i32, String> {
    use std::io::IsTerminal;
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    let pct = crate::scrub::percent;
    let tty = std::io::stdin().is_terminal();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut q = format!("tty={}&rows={rows}&cols={cols}", u8::from(tty));
    if !cmd.is_empty() {
        q += &format!("&cmd={}", pct(&serde_json::to_string(cmd).unwrap_or_default()));
    }
    if let Ok(t) = std::env::var("TERM") {
        q += &format!("&term={}", pct(&t));
    }
    let mut sock = TcpStream::connect(("127.0.0.1", info.port)).map_err(|e| format!("daemon: {e}"))?;
    let _ = sock.set_nodelay(true);
    write!(
        sock,
        "GET /api/services/{}/exec?{q} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nConnection: Upgrade\r\nUpgrade: {PROTOCOL}\r\n\r\n",
        pct(name),
        info.token
    )
    .map_err(|e| e.to_string())?;

    // Response head.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut b).map_err(|e| e.to_string())? == 0 {
            return Err("daemon closed the connection".into());
        }
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    if status != 101 {
        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
        let mut body = vec![0u8; len.min(1 << 16)];
        let _ = sock.read_exact(&mut body);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        return Err(v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}")));
    }

    struct Raw(bool);
    impl Drop for Raw {
        fn drop(&mut self) {
            if self.0 {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }
    }
    let _raw = Raw(tty && crossterm::terminal::enable_raw_mode().is_ok());

    let w = Arc::new(Mutex::new(sock.try_clone().map_err(|e| e.to_string())?));
    let send = {
        let w = w.clone();
        move |kind: u8, p: &[u8]| w.lock().map(|mut s| s.write_all(&frame(kind, p)).is_ok()).unwrap_or(false)
    };
    {
        let send = send.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        send(EOF, &[]);
                        break;
                    }
                    Ok(n) => {
                        if !send(DATA, &buf[..n]) {
                            break;
                        }
                    }
                }
            }
        });
    }
    if tty {
        std::thread::spawn(move || {
            let mut last = (cols, rows);
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Ok(now) = crossterm::terminal::size() else { continue };
                if now != last {
                    last = now;
                    let mut p = now.1.to_be_bytes().to_vec();
                    p.extend_from_slice(&now.0.to_be_bytes());
                    if !send(RESIZE, &p) {
                        break;
                    }
                }
            }
        });
    }

    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut hdr = [0u8; 5];
    loop {
        if sock.read_exact(&mut hdr).is_err() {
            return Err("connection to daemon lost".into());
        }
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).map_err(|_| "connection to daemon lost".to_string())?;
        match hdr[0] {
            DATA => {
                let _ = out.write_all(&body).and_then(|_| out.flush());
            }
            STDERR => {
                let _ = err.write_all(&body).and_then(|_| err.flush());
            }
            EXIT if body.len() == 4 => return Ok(i32::from_be_bytes([body[0], body[1], body[2], body[3]])),
            _ => {}
        }
    }
}

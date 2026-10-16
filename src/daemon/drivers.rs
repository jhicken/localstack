//! Workload drivers: plain processes, single docker containers and compose projects.
use super::docker_api::{self, Demux};
use super::logs::{LineSplitter, LogStore};
use super::supervisor::{kill_group, Sup};
use crate::paths::{expand_home, now_ms};
use serde_json::Value;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

const KILL_TIMEOUT: Duration = Duration::from_secs(8);

fn signal_name(sig: i32) -> String {
    let n = match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGABRT => "SIGABRT",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGTERM => "SIGTERM",
        libc::SIGBUS => "SIGBUS",
        _ => return format!("signal {sig}"),
    };
    format!("signal {n}")
}

/// Pump a pipe into the log store, one entry per line.
async fn pump(mut r: impl AsyncRead + Unpin, logs: Arc<LogStore>, stream: &'static str, prefix: Option<String>) {
    let mut sp = LineSplitter::default();
    let mut buf = vec![0u8; 8192];
    let emit = |l: String| match &prefix {
        Some(p) => logs.push(stream, format!("{p}{l}")),
        None => logs.push(stream, l),
    };
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => sp.feed(&buf[..n], emit),
        }
    }
    sp.flush(emit);
}

/// Run a command, streaming its output into the logs. Returns (exit code, last stderr line).
async fn stream_cmd(logs: Arc<LogStore>, bin: &str, args: &[String], cwd: Option<&str>, env: &[(String, String)]) -> (i32, String) {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    cmd.envs(env.iter().cloned());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{bin}: {e}");
            logs.push("system", msg.clone());
            return (1, msg);
        }
    };
    let out = tokio::spawn(pump(child.stdout.take().unwrap(), logs.clone(), "stdout", None));
    let mut err_pipe = child.stderr.take().unwrap();
    let l2 = logs.clone();
    let err = tokio::spawn(async move {
        let mut sp = LineSplitter::default();
        let mut buf = vec![0u8; 8192];
        let mut last = String::new();
        loop {
            match err_pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sp.feed(&buf[..n], |l| {
                    if !l.trim().is_empty() {
                        last = l.trim().to_string();
                    }
                    l2.push("stderr", l)
                }),
            }
        }
        sp.flush(|l| {
            last = l.trim().to_string();
            l2.push("stderr", l)
        });
        last
    });
    let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(1);
    let _ = out.await;
    let last = err.await.unwrap_or_default();
    (code, last)
}

impl Sup {
    // ================================================================ process

    pub(crate) async fn process_start(self: &Arc<Self>, name: &str, gen: u64) -> Result<(), String> {
        let Some((cfg, logs, running)) = self.with(name, Some(gen), |m| (m.cfg.clone(), m.logs.clone(), m.rt.stdin.is_some() && m.rt.status == "running")) else {
            return Err("service was replaced".into());
        };
        if running {
            return Ok(());
        }
        let cwd = expand_home(cfg.cwd.as_deref().unwrap_or("~"));
        if !std::path::Path::new(&cwd).is_dir() {
            return self.fail(name, gen, &format!("cwd does not exist: {cwd}"));
        }
        let secret_env = match super::secrets::env_for(&cfg) {
            Ok(e) => e,
            Err(e) => return self.fail(name, gen, &e),
        };
        let file_env = match self.env_file_vars(name, &cfg) {
            Ok(e) => e,
            Err(e) => return self.fail(name, gen, &e),
        };
        let shell = cfg.shell.clone().filter(|s| !s.is_empty()).unwrap_or_else(crate::cmdline::user_shell);
        let command = cfg.command.clone().unwrap_or_default();
        self.with(name, Some(gen), |m| {
            m.rt.stopping = false;
            m.rt.last_error = None;
            m.rt.exit_code = None;
            self.set_status(m, "starting");
            m.system(format!("$ {command}  (cwd: {cwd})"));
        });
        let mut cmd = tokio::process::Command::new(&shell);
        cmd.args(crate::cmdline::shell_args(&shell, cfg.source.as_deref()))
            .current_dir(&cwd)
            .env("SHELL", &shell)
            .envs(cfg.env.clone().unwrap_or_default())
            .envs(file_env)
            .envs(secret_env)
            .env("LSTACK_SERVICE", name)
            .env("LSTACK_COMMAND", &command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0); // own process group so we can signal the whole tree
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                self.with(name, Some(gen), |m| {
                    m.rt.last_error = Some(msg.clone());
                    m.system(format!("failed to start: {msg}"));
                    self.set_status(m, "errored");
                });
                return Err(msg);
            }
        };
        let pid = child.id().map(|p| p as i32);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                if stdin.write_all(&d).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let (exit_tx, exit_rx) = watch::channel(false);
        self.with(name, Some(gen), |m| {
            m.rt.pid = pid;
            m.rt.started_at = Some(now_ms());
            m.rt.stdin = Some(tx);
            m.rt.exited = Some(exit_rx);
            self.set_status(m, "running");
        });
        self.write_state();
        let r1 = tokio::spawn(pump(child.stdout.take().unwrap(), logs.clone(), "stdout", None));
        let r2 = tokio::spawn(pump(child.stderr.take().unwrap(), logs.clone(), "stderr", None));
        let sup = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Let the pipes drain (bounded: a backgrounded grandchild may keep them open).
            let _ = tokio::time::timeout(Duration::from_millis(300), async {
                let _ = r1.await;
                let _ = r2.await;
            })
            .await;
            let (code, sig) = match &status {
                Ok(s) => (s.code(), s.signal()),
                Err(_) => (None, None),
            };
            let expected = sup
                .with(&name, Some(gen), |m| {
                    let expected = m.rt.stopping;
                    m.rt.exit_code = code;
                    m.rt.pid = None;
                    m.rt.stdin = None;
                    let how = match (code, sig) {
                        (_, Some(s)) => signal_name(s),
                        (Some(c), _) => format!("code {c}"),
                        _ => "code null".into(),
                    };
                    m.system(format!("process exited ({how})"));
                    sup.set_status(m, if expected || code == Some(0) { "stopped" } else { "crashed" });
                    expected
                })
                .unwrap_or(true);
            let _ = exit_tx.send(true);
            sup.write_state();
            sup.on_exit(&name, code, expected);
        });
        Ok(())
    }

    pub(crate) async fn process_stop(self: &Arc<Self>, name: &str) {
        let Some(target) = self.with(name, None, |m| match (m.rt.pid, m.rt.exited.clone()) {
            (Some(pid), Some(rx)) if !*rx.borrow() => {
                m.rt.stopping = true;
                self.set_status(m, "stopping");
                Some((pid, rx))
            }
            _ => {
                self.set_status(m, "stopped");
                None
            }
        }) else {
            return;
        };
        let Some((pid, mut rx)) = target else { return };
        kill_group(pid, libc::SIGTERM);
        if tokio::time::timeout(KILL_TIMEOUT, rx.wait_for(|v| *v)).await.is_err() {
            self.with(name, None, |m| m.system(format!("did not exit within {}s, sending SIGKILL", KILL_TIMEOUT.as_secs())));
            kill_group(pid, libc::SIGKILL);
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|v| *v)).await;
        }
    }

    // ================================================================ docker / compose shared

    fn cancelled(&self, name: &str, gen: u64) -> bool {
        self.with(name, Some(gen), |m| m.rt.cancelled).unwrap_or(true)
    }

    fn system_log(&self, name: &str, msg: &str) {
        self.with(name, None, |m| m.system(msg));
    }

    /// Plain values from the service's `envFiles` (references come from the secret store via the
    /// approved `secrets:` map). Logs warnings and any 1Password changes still waiting for review.
    fn env_file_vars(&self, name: &str, cfg: &crate::types::ServiceConfig) -> Result<Vec<(String, String)>, String> {
        let (vars, warnings) = super::envimport::plain_env(cfg)?;
        for w in warnings {
            self.system_log(name, &format!("[lstack] {w}"));
        }
        let applied = super::envimport::hashed(&vars);
        self.with(name, None, |m| {
            m.env_applied = Some(applied);
            m.env_stale = None;
        });
        if let Some(p) = self.with(name, None, |m| m.secrets_pending.clone()).flatten() {
            let keys: Vec<&str> = ["new", "changed", "missing", "link", "notStored"].iter().flat_map(|k| p[*k].as_array().into_iter().flatten()).filter_map(|v| v.as_str()).collect();
            if !keys.is_empty() {
                self.system_log(
                    name,
                    &format!("[lstack] env file secrets waiting for review ({}); starting with the last approved values (lstack env {name})", keys.join(", ")),
                );
            }
        }
        Ok(vars)
    }

    /// Start a log follower for every running container that doesn't have one yet.
    fn ensure_followers(self: &Arc<Self>, name: &str, gen: u64) {
        self.with(name, Some(gen), |m| {
            m.rt.followers.retain(|_, h| !h.is_finished());
            let new: Vec<String> = m.rt.ids.iter().filter(|id| !m.rt.followers.contains_key(*id)).cloned().collect();
            if new.is_empty() {
                return;
            }
            let since = m.rt.log_since.take().or_else(|| m.logs.last_ts());
            let compose = m.cfg.kind == "compose";
            for id in new {
                let prefix = if compose { m.rt.labels.get(&id).map(|s| format!("{s} | ")) } else { None };
                let h = tokio::spawn(follow(id.clone(), since, prefix, m.logs.clone()));
                m.rt.followers.insert(id, h.abort_handle());
            }
        });
    }

    // ================================================================ docker

    pub(crate) async fn docker_start(self: &Arc<Self>, name: &str, gen: u64) -> Result<(), String> {
        let Some(cfg) = self.with(name, Some(gen), |m| {
            m.rt.cancelled = false;
            m.rt.wanted = true;
            m.rt.last_error = None;
            self.set_status(m, "starting");
            m.cfg.clone()
        }) else {
            return Err("service was replaced".into());
        };
        let t0 = now_ms() - 2_000;
        let cname = cfg.container_name();
        if !docker_api::wait_for_docker(|m| self.system_log(name, m), || self.cancelled(name, gen)).await {
            return self.fail(name, gen, "Docker engine is not available");
        }
        let exists = match docker_api::inspect(&cname).await {
            Ok(v) => v.is_some(),
            Err(e) => return self.fail(name, gen, &e),
        };
        if exists {
            self.system_log(name, &format!("$ docker start {cname}"));
            match docker_api::call("POST", &format!("/containers/{}/start", docker_api::enc(&cname)), None, 60).await {
                Ok((204 | 304, _)) => {}
                Ok((s, v)) => return self.fail(name, gen, &docker_api::api_error(s, &v)),
                Err(e) => return self.fail(name, gen, &e),
            }
        } else if let Some(image) = &cfg.image {
            let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), cname.clone(), "--label".into(), format!("lstack.service={name}")];
            for p in cfg.ports.iter().flatten() {
                args.extend(["-p".into(), p.clone()]);
            }
            for v in cfg.volumes.iter().flatten() {
                args.extend(["-v".into(), expand_home(v)]);
            }
            let file_env = match self.env_file_vars(name, &cfg) {
                Ok(e) => e,
                Err(e) => return self.fail(name, gen, &e),
            };
            for (k, v) in cfg.env.iter().flatten() {
                if !file_env.iter().any(|(f, _)| f == k) {
                    args.extend(["-e".into(), format!("{k}={v}")]);
                }
            }
            // `-e NAME` without a value: docker reads it from its own environment, so secret
            // values (and env file values, which may be sensitive) never appear on a command line
            // or in the logged command.
            let secret_env = match super::secrets::env_for(&cfg) {
                Ok(e) => e,
                Err(e) => return self.fail(name, gen, &e),
            };
            let mut pass_env = file_env;
            pass_env.retain(|(k, _)| !secret_env.iter().any(|(s, _)| s == k));
            pass_env.extend(secret_env);
            let secret_env = pass_env;
            for (k, _) in &secret_env {
                args.extend(["-e".into(), k.clone()]);
            }
            args.extend(cfg.args.iter().flatten().cloned());
            args.push(image.clone());
            if let Some(c) = &cfg.command {
                args.extend(["sh".into(), "-c".into(), c.clone()]);
            }
            self.system_log(name, &format!("$ docker {}", args.join(" ")));
            let logs = self.with(name, None, |m| m.logs.clone()).unwrap();
            let bin = docker_api::docker_bin();
            let run = stream_cmd(logs, &bin, &args, None, &secret_env);
            match tokio::time::timeout(Duration::from_secs(15 * 60), run).await {
                Ok((0, _)) => {}
                Ok((code, err)) => {
                    let msg = if err.is_empty() { format!("docker exited with {code}") } else { err };
                    return self.fail(name, gen, &msg);
                }
                Err(_) => return self.fail(name, gen, "docker run timed out"),
            }
        } else {
            return self.fail(name, gen, &format!("container \"{cname}\" does not exist and no image is configured"));
        }
        self.with(name, Some(gen), |m| {
            self.set_status(m, "stopped"); // allow refresh to resolve the real state
            m.rt.log_since = Some(t0);
        });
        self.docker_refresh(name, gen).await;
        Ok(())
    }

    pub(crate) async fn docker_stop(self: &Arc<Self>, name: &str) {
        let Some((cname, gen)) = self.with(name, None, |m| {
            m.rt.cancelled = true;
            m.rt.wanted = false;
            self.set_status(m, "stopping");
            let c = m.cfg.container_name();
            m.system(format!("$ docker stop {c}"));
            (c, m.gen)
        }) else {
            return;
        };
        let res = docker_api::call("POST", &format!("/containers/{}/stop?t=10", docker_api::enc(&cname)), None, 60).await;
        if let Ok((s, v)) = &res {
            if !matches!(s, 204 | 304 | 404) {
                self.system_log(name, &docker_api::api_error(*s, v));
            }
        }
        self.with(name, None, |m| {
            m.rt.dispose();
            self.set_status(m, "stopped");
        });
        self.docker_refresh(name, gen).await;
    }

    pub(crate) async fn docker_adopt(self: &Arc<Self>, name: &str, gen: u64) -> bool {
        self.docker_refresh(name, gen).await;
        self.with(name, Some(gen), |m| {
            if m.rt.status == "running" {
                m.rt.wanted = true;
            }
            m.rt.status == "running"
        })
        .unwrap_or(false)
    }

    pub(crate) async fn docker_refresh(self: &Arc<Self>, name: &str, gen: u64) {
        let Some(Some(cname)) = self.with(name, Some(gen), |m| (!matches!(m.rt.status, "starting" | "stopping")).then(|| m.cfg.container_name())) else {
            return;
        };
        let info = docker_api::inspect(&cname).await;
        let exit = self.with(name, Some(gen), |m| {
            if matches!(m.rt.status, "starting" | "stopping") {
                return None;
            }
            let Ok(Some(v)) = &info else {
                m.rt.ids.clear();
                if m.rt.status == "running" {
                    self.set_status(m, "stopped");
                }
                return None;
            };
            let st = &v["State"];
            let state = st["Status"].as_str().unwrap_or("");
            m.rt.pid = st["Pid"].as_i64().filter(|p| *p > 0).map(|p| p as i32);
            m.rt.native_health = match st.pointer("/Health/Status").and_then(|h| h.as_str()) {
                Some("healthy") => Some("healthy"),
                Some("unhealthy") => Some("unhealthy"),
                _ => None,
            };
            if state == "running" || state == "restarting" {
                if let Some(t) = st["StartedAt"].as_str().and_then(crate::paths::parse_iso) {
                    m.rt.started_at = Some(t);
                }
                m.rt.ids = v["Id"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default();
                m.ext_ports = super::supervisor::docker_ports(v);
                if m.rt.status != "running" {
                    self.set_status(m, "running");
                } else {
                    self.changed(m); // pid/health may have changed
                }
                None
            } else {
                let was_running = m.rt.status == "running";
                let code = st["ExitCode"].as_i64().unwrap_or(0) as i32;
                m.rt.exit_code = Some(code);
                m.rt.ids.clear();
                self.set_status(m, if m.rt.wanted && code != 0 { "crashed" } else { "stopped" });
                was_running.then_some((code, !m.rt.wanted))
            }
        });
        if let Some(Some((code, expected))) = exit {
            self.on_exit(name, Some(code), expected);
        }
        self.ensure_followers(name, gen);
    }

    // ================================================================ compose

    fn compose_base(cfg: &crate::types::ServiceConfig) -> (String, Vec<String>, String) {
        let file = expand_home(cfg.compose_file.as_deref().unwrap_or(""));
        let mut base = vec!["compose".to_string(), "-f".into(), file.clone()];
        if let Some(p) = &cfg.project {
            base.extend(["-p".into(), p.clone()]);
        }
        let dir = std::path::Path::new(&file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into());
        let cwd = expand_home(cfg.cwd.as_deref().unwrap_or(&dir));
        (file, base, cwd)
    }

    pub(crate) async fn compose_start(self: &Arc<Self>, name: &str, gen: u64) -> Result<(), String> {
        let Some((cfg, logs)) = self.with(name, Some(gen), |m| {
            m.rt.wanted = true;
            m.rt.cancelled = false;
            m.rt.last_error = None;
            self.set_status(m, "starting");
            (m.cfg.clone(), m.logs.clone())
        }) else {
            return Err("service was replaced".into());
        };
        let t0 = now_ms() - 2_000;
        let (file, base, cwd) = Self::compose_base(&cfg);
        if !std::path::Path::new(&file).exists() {
            return self.fail(name, gen, &format!("compose file not found: {file}"));
        }
        if !docker_api::wait_for_docker(|m| self.system_log(name, m), || self.cancelled(name, gen)).await {
            return self.fail(name, gen, "Docker engine is not available");
        }
        let mut args = base.clone();
        args.extend(["up".into(), "-d".into()]);
        args.extend(cfg.args.iter().flatten().cloned());
        self.system_log(name, &format!("$ docker {}", args.join(" ")));
        let mut env: Vec<(String, String)> = cfg.env.clone().unwrap_or_default().into_iter().collect();
        match self.env_file_vars(name, &cfg) {
            Ok(e) => env.extend(e),
            Err(e) => return self.fail(name, gen, &e),
        }
        match super::secrets::env_for(&cfg) {
            Ok(e) => env.extend(e),
            Err(e) => return self.fail(name, gen, &e),
        }
        let (code, _) = stream_cmd(logs, &docker_api::docker_bin(), &args, Some(&cwd), &env).await;
        if code != 0 {
            return self.fail(name, gen, &format!("docker compose up exited with {code}"));
        }
        self.with(name, Some(gen), |m| {
            m.rt.started_at = Some(now_ms());
            m.rt.log_since = Some(t0);
            self.set_status(m, "stopped");
        });
        self.compose_refresh(name, gen).await;
        Ok(())
    }

    pub(crate) async fn compose_stop(self: &Arc<Self>, name: &str) {
        let Some((cfg, logs, gen)) = self.with(name, None, |m| {
            m.rt.wanted = false;
            m.rt.cancelled = true;
            self.set_status(m, "stopping");
            (m.cfg.clone(), m.logs.clone(), m.gen)
        }) else {
            return;
        };
        let (_, mut args, cwd) = Self::compose_base(&cfg);
        args.push("stop".into());
        self.system_log(name, &format!("$ docker {}", args.join(" ")));
        let mut env: Vec<(String, String)> = cfg.env.clone().unwrap_or_default().into_iter().collect();
        env.extend(super::envimport::plain_env(&cfg).map(|(e, _)| e).unwrap_or_default());
        env.extend(super::secrets::env_for_ungated(&cfg).unwrap_or_default());
        stream_cmd(logs, &docker_api::docker_bin(), &args, Some(&cwd), &env).await;
        self.with(name, None, |m| {
            m.rt.dispose();
            self.set_status(m, "stopped");
        });
        self.compose_refresh(name, gen).await;
    }

    pub(crate) async fn compose_adopt(self: &Arc<Self>, name: &str, gen: u64) -> bool {
        self.compose_refresh(name, gen).await;
        // Uptime = earliest StartedAt among the stack's running containers.
        let ids = self.with(name, Some(gen), |m| m.rt.ids.clone()).unwrap_or_default();
        let mut started: Option<i64> = None;
        for id in ids.iter().take(20) {
            if let Ok(Some(v)) = docker_api::inspect(id).await {
                if let Some(t) = v["State"]["StartedAt"].as_str().and_then(crate::paths::parse_iso) {
                    started = Some(started.map_or(t, |s| s.min(t)));
                }
            }
        }
        self.with(name, Some(gen), |m| {
            if m.rt.status == "running" {
                m.rt.wanted = true;
                if let Some(t) = started {
                    m.rt.started_at = Some(t);
                }
                m.rt.started_at.get_or_insert(now_ms());
            }
            m.rt.status == "running"
        })
        .unwrap_or(false)
    }

    pub(crate) async fn compose_refresh(self: &Arc<Self>, name: &str, gen: u64) {
        let Some(Some(cfg)) = self.with(name, Some(gen), |m| (!matches!(m.rt.status, "starting" | "stopping")).then(|| m.cfg.clone())) else {
            return;
        };
        let (file, _, _) = Self::compose_base(&cfg);
        if !std::path::Path::new(&file).exists() {
            return;
        }
        let Ok(all) = docker_api::compose_containers().await else { return };
        let canon = std::fs::canonicalize(&file).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let rows: Vec<&Value> = all.iter().filter(|c| compose_match(c, &file, &canon, cfg.project.as_deref())).collect();
        let exit = self.with(name, Some(gen), |m| {
            if matches!(m.rt.status, "starting" | "stopping") {
                return None;
            }
            let running: Vec<&&Value> = rows.iter().filter(|c| matches!(c["State"].as_str(), Some("running" | "restarting"))).collect();
            m.rt.ids = running.iter().filter_map(|c| c["Id"].as_str().map(String::from)).collect();
            let mut ports: Vec<(u16, bool)> = running.iter().flat_map(|c| super::supervisor::docker_ports(c)).collect();
            ports.sort();
            ports.dedup_by_key(|p| p.0);
            m.ext_ports = ports;
            m.rt.labels = rows
                .iter()
                .filter_map(|c| Some((c["Id"].as_str()?.to_string(), c.pointer("/Labels/com.docker.compose.service")?.as_str()?.to_string())))
                .collect();
            let unhealthy = rows.iter().any(|c| c["Status"].as_str().is_some_and(|s| s.contains("(unhealthy)")))
                || (!running.is_empty() && running.len() < rows.len());
            m.rt.native_health = if running.is_empty() { None } else if unhealthy { Some("unhealthy") } else { Some("healthy") };
            let was_running = m.rt.status == "running";
            if !running.is_empty() {
                if was_running {
                    self.changed(m);
                } else {
                    self.set_status(m, "running");
                }
                None
            } else {
                let crashed = m.rt.wanted && rows.iter().any(|c| exit_code_of(c) != 0);
                self.set_status(m, if crashed { "crashed" } else { "stopped" });
                was_running.then_some((if crashed { 1 } else { 0 }, !m.rt.wanted))
            }
        });
        if let Some(Some((code, expected))) = exit {
            self.on_exit(name, Some(code), expected);
        }
        self.ensure_followers(name, gen);
    }
}

fn compose_match(c: &Value, file: &str, canon: &str, project: Option<&str>) -> bool {
    let labels = &c["Labels"];
    if let Some(p) = project {
        return labels["com.docker.compose.project"].as_str() == Some(p);
    }
    labels["com.docker.compose.project.config_files"]
        .as_str()
        .is_some_and(|s| s.split(',').any(|f| f == file || (!canon.is_empty() && f == canon)))
}

/// "Exited (137) 2 minutes ago" -> 137
fn exit_code_of(c: &Value) -> i32 {
    c["Status"]
        .as_str()
        .and_then(|s| s.strip_prefix("Exited ("))
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Follow a container's logs through the Engine API until it stops.
async fn follow(id: String, since: Option<i64>, prefix: Option<String>, logs: Arc<LogStore>) {
    let window = match since {
        Some(ms) => {
            let ms = ms + 1;
            format!("since={}.{:09}", ms / 1000, (ms % 1000) * 1_000_000)
        }
        None => "tail=100".into(),
    };
    let path = format!("/containers/{}/logs?follow=1&stdout=1&stderr=1&{window}", docker_api::enc(&id));
    let Ok((200, mut body)) = docker_api::request("GET", &path, None).await else { return };
    let mut demux = Demux::default();
    let mut framed: Option<bool> = None;
    let (mut out, mut err) = (LineSplitter::default(), LineSplitter::default());
    let emit = |stream: &'static str, l: String| match &prefix {
        Some(p) => logs.push(stream, format!("{p}{l}")),
        None => logs.push(stream, l),
    };
    while let Ok(Some(chunk)) = body.next().await {
        // Multiplexed frames start with [stream(0-2), 0, 0, 0, size...]; TTY containers send raw bytes.
        let is_framed = *framed.get_or_insert_with(|| chunk.len() >= 8 && chunk[0] <= 2 && chunk[1..4] == [0, 0, 0]);
        if is_framed {
            demux.feed(&chunk, |is_err, payload| {
                if is_err {
                    err.feed(payload, |l| emit("stderr", l));
                } else {
                    out.feed(payload, |l| emit("stdout", l));
                }
            });
        } else {
            out.feed(&chunk, |l| emit("stdout", l));
        }
    }
    out.flush(|l| emit("stdout", l));
    err.flush(|l| emit("stderr", l));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compose_helpers() {
        let c = serde_json::json!({"Status": "Exited (137) 2 minutes ago", "Labels": {
            "com.docker.compose.project": "demo",
            "com.docker.compose.project.config_files": "/a/docker-compose.yml,/a/override.yml"}});
        assert_eq!(exit_code_of(&c), 137);
        assert!(compose_match(&c, "/a/override.yml", "", None));
        assert!(!compose_match(&c, "/b/docker-compose.yml", "", None));
        assert!(compose_match(&c, "/zzz", "", Some("demo")));
    }
}

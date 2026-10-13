//! Command-line interface.
use crate::client::{self, Client};
use crate::fmt as c;
use crate::types::{HealthCheck, LogLine, ServiceConfig, ServiceState};
use crate::{cmdline, config, launchd, paths, setup};
use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};

type R = Result<(), String>;

fn help() -> String {
    format!(
        "{} {} — run & watch your local stack (scripts, Docker, Compose)

{}
  lstack                         open the interactive dashboard (same as {})
  lstack ls [--json]             list services
  lstack show <name>             details, config and recent resource usage
  lstack add <name> [options] -- <command…>
  lstack add -f <service.yaml>   add a service from a YAML definition
  lstack edit <name>             edit a service definition in $EDITOR
  lstack rm <name> [-y]          stop and remove a service
  lstack start|stop|restart <name…|all>
  lstack enable|disable <name…>  toggle start-at-login (autostart)
  lstack logs <name> [-f] [-n 200]
  lstack attach <name>           follow logs and type into the process's stdin
  lstack exec <name> [cmd…]      shell into a docker/compose service (docker exec -it)
  lstack metrics <name> [--range 1h|24h|7d]
  lstack setup [--cli] [--login] [--app <path>]   install the CLI + start at login
  lstack setup status|repair|uninstall
  lstack remote [on|off|passwd|2fa|status]   web dashboard for other machines (see below)
  lstack terminal [terminal|iterm|cmux|ghostty]   app used to open the dashboard / attach
  lstack terminal cmux --fix     let LStack open cmux workspaces (socket password mode)
  lstack secret set|ls|rm|rotate-key|unlock   write-only secrets passed to services as env vars
  lstack ps [--machine <m>]      busy processes (over the CPU/mem/net thresholds), per machine
  lstack machine ls|add|rm|rename   watch other Macs (their services show up everywhere)
  lstack heartbeat [status|key|machine|set|off|test]   healthchecks.io-compatible pings
  lstack daemon run|start|stop|restart|status|logs
  lstack reload                  re-read {}

{}
  -t, --type process|docker|compose   (default: process)
  -c, --cwd <dir>        working directory (process)
  -e, --env KEY=VALUE    environment variable (repeatable)
  -a, --autostart        start at login / daemon boot
  -r, --restart always|on-failure|never   (default: on-failure)
  -d, --description <text>
      --image <image>    docker: image to run      --container <name>  docker: existing container
  -p, --port <h:c>       docker: port mapping (repeatable)
  -v, --volume <h:c>     docker: volume (repeatable)
      --file <path>      compose: compose file     --project <name>   compose: project name
      --health-url <url> HTTP health check         --health-port <n>  TCP health check
  -s, --source <file>    source this file before running (repeatable; default: your shell rc, e.g. ~/.zshrc)
      --no-source        don't source any shell config (scripts still run in a login shell)
      --no-split         keep the command as typed (by default a leading `cd dir &&`, `FOO=bar`,
                         `export …` and `source …` are moved into cwd / env / source)
      --now              start immediately after adding

{}
  lstack remote on [--port 7380] [--user admin] [--bind 0.0.0.0] [--allow-control|--read-only] [--password-stdin]
  lstack remote passwd [--user <name>] [--password-stdin]
  lstack remote 2fa on|off       authenticator-app codes (TOTP) at sign-in
  lstack remote off
  lstack remote pair             one-time code another Mac uses to watch this one
  lstack remote peers [rm <id>]  machines that watch this one (revoke access)

{}
  lstack machine add studio.local --code K7QP-3M9X [--name studio]
  lstack machine add 100.64.1.2 --user admin     sign in with its dashboard password (+2FA)
  lstack ls                      services grouped by machine; address them as name@machine
  lstack logs api@studio -f      lstack restart worker@studio

{}
  lstack heartbeat key                           healthchecks.io project ping key (stored encrypted)
  lstack heartbeat set api [--slug api | --url https://hc-ping.com/<uuid>] [--interval 60]
  lstack heartbeat machine <slug|url>            ping while this Mac + LStack are up
  lstack heartbeat test [service]

{}
  lstack add api -a -c ~/code/api -- npm run dev
  lstack add web -a -- 'cd ~/code/web && PORT=3000 bun run dev'     # → cwd, env PORT, command
  lstack add db -t docker --image postgres:16 -p 5432:5432 -e POSTGRES_PASSWORD=dev -a
  lstack add stack -t compose --file ~/code/app/docker-compose.yml -a
",
        c::bold("lstack"),
        c::dim(&format!("v{}", paths::VERSION)),
        c::bold("Usage"),
        c::cyan("lstack top"),
        paths::services_dir().display(),
        c::bold("Add options"),
        c::bold("Remote access"),
        c::bold("Other machines"),
        c::bold("Heartbeats (healthchecks.io)"),
        c::bold("Examples"),
    )
}

pub fn main(argv: Vec<String>) -> i32 {
    let tty = std::io::stdout().is_terminal();
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => (if tty { "top" } else { "ls" }, vec![]),
    };
    let res = match cmd {
        "-h" | "--help" | "help" => {
            println!("{}", help());
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("{}", paths::VERSION);
            Ok(())
        }
        "top" | "ui" | "dash" => client::connect(true).and_then(crate::tui::run),
        "ls" | "list" | "status" => cmd_list(&rest),
        "show" | "info" => cmd_show(&rest),
        "add" => cmd_add(&rest),
        "edit" => cmd_edit(&rest),
        "rm" | "remove" | "delete" => cmd_remove(&rest),
        "start" | "stop" | "restart" => cmd_action(cmd, &rest),
        "enable" | "disable" => cmd_autostart(cmd == "enable", &rest),
        "logs" | "log" => cmd_logs(&rest),
        "attach" => cmd_attach(&rest),
        "exec" | "shell" => cmd_exec(&rest),
        "metrics" => cmd_metrics(&rest),
        "setup" => setup::run(&rest),
        "remote" => cmd_remote(&rest),
        "terminal" => cmd_terminal(&rest),
        "secret" | "secrets" => crate::cli_more::cmd_secret(&rest),
        "machine" | "machines" => crate::cli_more::cmd_machine(&rest),
        "ps" | "processes" => crate::cli_more::cmd_ps(&rest),
        "heartbeat" | "heartbeats" | "hc" => crate::cli_more::cmd_heartbeat(&rest),
        "startup" => cmd_startup(&rest),
        "daemon" => return cmd_daemon(&rest),
        "reload" => (|| {
            let r = client::connect(true)?.reload()?;
            let j = |v: &Vec<String>| if v.is_empty() { "—".to_string() } else { v.join(", ") };
            println!("added: {}  updated: {}  removed: {}", j(&r.added), j(&r.updated), j(&r.removed));
            for e in r.errors {
                eprintln!("{}", c::red(&format!("config error: {e}")));
            }
            Ok(())
        })(),
        "config" => {
            println!("{}", paths::home().display());
            Ok(())
        }
        other => Err(format!("unknown command \"{other}\" — run lstack --help")),
    };
    match res {
        Ok(()) => 0,
        Err(e) if e.is_empty() => 1,
        Err(e) => {
            eprintln!("{}", c::red(&format!("error: {e}")));
            1
        }
    }
}

// ------------------------------------------------------------------ arg parsing

#[derive(Default)]
pub(crate) struct Args {
    pub pos: Vec<String>,
    flags: std::collections::HashMap<String, Vec<String>>,
}
impl Args {
    /// `spec`: (long, short, takes_value). Everything after `--` is positional.
    pub(crate) fn parse(args: &[String], spec: &[(&str, &str, bool)]) -> Result<Args, String> {
        let mut out = Args::default();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--" {
                out.pos.extend(args[i + 1..].iter().cloned());
                break;
            }
            let (key, inline) = if let Some(l) = a.strip_prefix("--") {
                match l.split_once('=') {
                    Some((k, v)) => (Some(k.to_string()), Some(v.to_string())),
                    None => (Some(l.to_string()), None),
                }
            } else if a.len() > 1 && a.starts_with('-') && !a[1..].starts_with(|c: char| c.is_ascii_digit()) {
                let s = &a[1..];
                let (short, rest) = s.split_at(s.chars().next().unwrap().len_utf8());
                let long = spec.iter().find(|x| x.1 == short).map(|x| x.0.to_string()).ok_or_else(|| format!("unknown option -{short}"))?;
                (Some(long), if rest.is_empty() { None } else { Some(rest.to_string()) })
            } else {
                (None, None)
            };
            match key {
                None => out.pos.push(a.clone()),
                Some(k) => {
                    let s = spec.iter().find(|x| x.0 == k).ok_or_else(|| format!("unknown option --{k}"))?;
                    let v = if s.2 {
                        match inline {
                            Some(v) => v,
                            None => {
                                i += 1;
                                args.get(i).cloned().ok_or_else(|| format!("--{k} needs a value"))?
                            }
                        }
                    } else {
                        "1".into()
                    };
                    out.flags.entry(k).or_default().push(v);
                }
            }
            i += 1;
        }
        Ok(out)
    }
    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).and_then(|v| v.last()).map(String::as_str)
    }
    pub(crate) fn all(&self, k: &str) -> Vec<String> {
        self.flags.get(k).cloned().unwrap_or_default()
    }
    pub(crate) fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
}

fn need(v: Option<&String>) -> Result<String, String> {
    v.cloned().ok_or_else(|| "missing service name".to_string())
}

fn uptime(s: &ServiceState) -> String {
    c::duration(s.started_at.map(|t| paths::now_ms() - t))
}

// ------------------------------------------------------------------ commands

/// First URL of a service, for tables.
fn url_cell(s: &ServiceState) -> String {
    match s.urls.first() {
        None => c::dim("—"),
        Some(u) => {
            let more = if s.urls.len() > 1 { c::dim(&format!(" +{}", s.urls.len() - 1)) } else { String::new() };
            let warn = if u.local_only && s.machine.is_some() { c::yellow(" (local only)") } else { String::new() };
            format!("{}{warn}{more}", c::cyan(&u.url))
        }
    }
}

fn cmd_list(args: &[String]) -> R {
    let a = Args::parse(args, &[("json", "", false), ("machine", "m", true), ("local", "", false)])?;
    let cl = client::connect(true)?;
    let l = cl.list()?;
    let machines: serde_json::Value = if a.has("local") { serde_json::Value::Null } else { cl.req("GET", "/api/machines", None).unwrap_or_default() };
    let local_name = machines["local"]["name"].as_str().unwrap_or("this mac").to_string();
    let mut groups: Vec<(String, Vec<ServiceState>, String, Option<String>)> = vec![(local_name.clone(), l.services, "online".into(), None)];
    for m in machines["machines"].as_array().into_iter().flatten() {
        groups.push((
            m["name"].as_str().unwrap_or("?").to_string(),
            serde_json::from_value(m["services"].clone()).unwrap_or_default(),
            m["status"].as_str().unwrap_or("?").to_string(),
            m["error"].as_str().map(String::from),
        ));
    }
    if let Some(want) = a.get("machine") {
        groups.retain(|g| g.0 == want);
        if groups.is_empty() {
            return Err(format!("no machine named {want:?}"));
        }
    }
    if a.has("json") {
        let all: Vec<&ServiceState> = groups.iter().flat_map(|g| g.1.iter()).collect();
        println!("{}", serde_json::to_string_pretty(&all).unwrap());
        return Ok(());
    }
    let multi = groups.len() > 1;
    for (name, services, status, err) in &groups {
        if multi {
            let st = match status.as_str() {
                "online" => String::new(),
                s => c::red(&format!(" ({s})")),
            };
            println!("{}{st}", c::bold(&format!("── {name} ")));
            if let Some(e) = err {
                println!("{}", c::dim(&format!("   {e}")));
            }
        }
        print_services(services, name == &local_name);
    }
    for e in l.config_errors {
        eprintln!("{}", c::red(&format!("config error: {e}")));
    }
    Ok(())
}

fn print_services(services: &[ServiceState], local: bool) {
    if services.is_empty() {
        println!("{}", c::dim(if local { "No services yet. Add one with: lstack add <name> -- <command>" } else { "  no services" }));
    } else {
        let with_urls = services.iter().any(|s| !s.urls.is_empty());
        let rows: Vec<Vec<String>> = services
            .iter()
            .map(|s| {
                let mut row = vec![
                    format!("{} {}", c::status_dot(&s.status), c::bold(&s.name)),
                    c::dim(&s.kind),
                    if s.restart_at.is_some() { c::yellow(&s.status_label()) } else { c::status_color(&s.status, &s.status) },
                    c::health_label(&s.health),
                    c::pct(s.metrics.as_ref().map(|m| m.cpu)),
                    c::bytes(s.metrics.as_ref().map(|m| m.mem)),
                    uptime(s),
                    s.restarts.to_string(),
                    if s.autostart { c::cyan("✓") } else { c::dim("·") },
                ];
                if with_urls {
                    row.push(url_cell(s));
                }
                row
            })
            .collect();
        let mut headers = vec!["NAME", "TYPE", "STATUS", "HEALTH", "CPU", "MEM", "UPTIME", "↺", "BOOT"];
        if with_urls {
            headers.push("URL");
        }
        println!("{}", c::table(&rows, &headers));
    }
}

fn cmd_show(args: &[String]) -> R {
    let name = need(args.first())?;
    let cl = client::connect(true)?;
    let s = cl.get(&name)?;
    let m = cl.metrics(&name, "1h", 60).unwrap_or_default();
    println!("{} {} {}  {}  {}", c::status_dot(&s.status), c::bold(&s.name), c::dim(&format!("({})", s.kind)), if s.restart_at.is_some() { c::yellow(&s.status_label()) } else { c::status_color(&s.status, &s.status) }, c::health_label(&s.health));
    if s.restart_at.is_some() {
        println!("{}", c::dim(&format!("crash loop: `lstack stop {}` cancels the pending restart", s.name)));
    }
    if let Some(d) = &s.description {
        println!("{}", c::dim(d));
    }
    let met = s.metrics.as_ref();
    let mut kv = vec![
        ("pid", s.pid.map(|p| p.to_string()).unwrap_or("—".into())),
        ("uptime", uptime(&s)),
        ("restarts", s.restarts.to_string()),
        ("autostart", if s.autostart { "yes" } else { "no" }.into()),
        ("cpu", c::pct(met.map(|m| m.cpu))),
        ("memory", c::bytes(met.map(|m| m.mem))),
        ("network", format!("↓ {}  ↑ {}", c::rate(met.and_then(|m| m.net_rx)), c::rate(met.and_then(|m| m.net_tx)))),
    ];
    if let Some(m) = &s.machine {
        kv.insert(0, ("machine", c::bold(m)));
    }
    for u in &s.urls {
        let label = u.label.as_deref().map(|l| format!(" {}", c::dim(l))).unwrap_or_default();
        let note = if u.local_only { c::dim(if s.machine.is_some() { " (bound to localhost there: not reachable from here)" } else { " (localhost only)" }) } else { String::new() };
        kv.push(("url", format!("{}{label}{note}", c::cyan(&u.url))));
    }
    if let Some(hb) = &s.config.heartbeat {
        let last = match &s.heartbeat {
            None => c::dim("no ping yet"),
            Some(h) if h.ok => c::green(&format!("✓ {} at {}", h.kind, c::local_time(h.last_ping))),
            Some(h) => c::red(&format!("✗ {} at {}: {}", h.kind, c::local_time(h.last_ping), h.error.as_deref().unwrap_or("?"))),
        };
        let on = if hb.enabled == Some(false) { c::dim("off · ") } else { String::new() };
        kv.push(("heartbeat", format!("{on}every {}s · {last}", hb.interval.unwrap_or(60.0))));
    }
    if let Some(e) = &s.last_error {
        kv.push(("last error", c::red(e)));
    }
    for (k, v) in kv {
        println!("  {}{v}", c::dim(&format!("{k:<11}")));
    }
    if !m.is_empty() {
        println!("\n  {}{}", c::dim("cpu 1h  "), c::green(&c::sparkline(&m.iter().map(|x| x.cpu).collect::<Vec<_>>(), 60)));
        println!("  {}{}", c::dim("mem 1h  "), c::blue(&c::sparkline(&m.iter().map(|x| x.mem).collect::<Vec<_>>(), 60)));
    }
    let src = match &s.machine {
        Some(m) => format!("# on {m}"),
        None => format!("# {}", config::service_file(&name).display()),
    };
    println!("\n{}\n{}", c::dim(&src), config::to_yaml(&s.config).trim_end());
    Ok(())
}

fn to_abs(p: &str) -> String {
    let e = paths::expand_home(p);
    if e.starts_with('/') {
        e
    } else {
        std::env::current_dir().map(|d| d.join(&e).to_string_lossy().into_owned()).unwrap_or(e)
    }
}

fn cmd_add(args: &[String]) -> R {
    let a = Args::parse(
        args,
        &[
            ("file", "f", true),
            ("type", "t", true),
            ("cwd", "c", true),
            ("env", "e", true),
            ("autostart", "a", false),
            ("restart", "r", true),
            ("description", "d", true),
            ("image", "", true),
            ("container", "", true),
            ("port", "p", true),
            ("volume", "v", true),
            ("project", "", true),
            ("health-url", "", true),
            ("health-port", "", true),
            ("source", "s", true),
            ("no-source", "", false),
            ("no-split", "", false),
            ("now", "", false),
        ],
    )?;
    let kind = a.get("type").unwrap_or("process").to_string();
    let mut cfg = if let (Some(f), false) = (a.get("file"), kind == "compose") {
        let text = std::fs::read_to_string(paths::expand_home(f)).map_err(|e| format!("{f}: {e}"))?;
        let mut cfg = config::parse_yaml(&text)?;
        if let Some(n) = a.pos.first() {
            cfg.name = n.clone();
        }
        cfg
    } else {
        let name = need(a.pos.first())?;
        let cmd = &a.pos[1..];
        let env = a.all("env");
        let mut cfg = ServiceConfig {
            name,
            kind: kind.clone(),
            description: a.get("description").map(String::from),
            autostart: Some(a.has("autostart")),
            restart: a.get("restart").map(String::from),
            command: if cmd.is_empty() { None } else { Some(cmdline::join_argv(cmd)) },
            cwd: match a.get("cwd") {
                Some(d) => Some(to_abs(d)),
                None if kind == "process" => std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()),
                None => None,
            },
            env: if env.is_empty() {
                None
            } else {
                Some(env.iter().map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).unwrap_or((kv.clone(), String::new()))).collect())
            },
            image: a.get("image").map(String::from),
            container: a.get("container").map(String::from),
            ports: Some(a.all("port")).filter(|v| !v.is_empty()),
            volumes: Some(a.all("volume").iter().map(|v| match v.split_once(':') {
                Some((h, rest)) if h.starts_with('.') || h.starts_with('~') || h.starts_with('/') => format!("{}:{rest}", to_abs(h)),
                _ => v.clone(),
            }).collect::<Vec<_>>())
            .filter(|v| !v.is_empty()),
            compose_file: a.get("file").map(to_abs),
            project: a.get("project").map(String::from),
            ..Default::default()
        };
        if let Some(u) = a.get("health-url") {
            cfg.healthcheck = Some(HealthCheck { kind: "http".into(), url: Some(u.into()), ..Default::default() });
        } else if let Some(p) = a.get("health-port") {
            cfg.healthcheck = Some(HealthCheck { kind: "tcp".into(), port: Some(p.parse().map_err(|_| "--health-port must be a number")?), ..Default::default() });
        }
        let sources = a.all("source");
        if a.has("no-source") {
            cfg.source = Some(vec![]);
        } else if !sources.is_empty() {
            cfg.source = Some(sources.iter().map(|s| if s.starts_with('~') { s.clone() } else { to_abs(s) }).collect());
        }
        if kind == "process" && !a.has("no-split") {
            split_command(&mut cfg);
        }
        cfg
    };
    cfg = config::validate(cfg)?;
    let s = client::connect(true)?.create(&cfg, a.has("now"))?;
    println!(
        "{} added {} {}{}",
        c::green("✓"),
        c::bold(&s.name),
        c::dim(&format!("→ {}", config::service_file(&s.name).display())),
        if s.autostart { c::cyan("  (starts at login)") } else { String::new() }
    );
    if a.has("now") {
        println!("  {} {}", c::status_dot(&s.status), s.status);
    }
    Ok(())
}

/// Move leading `cd …`, `FOO=bar`, `export …` and `source …` out of the command into
/// cwd / env / source, so the service definition is easy to read and edit.
fn split_command(cfg: &mut ServiceConfig) {
    let Some(command) = cfg.command.clone() else { return };
    let base = cfg.cwd.clone().unwrap_or_else(|| paths::user_home().to_string_lossy().into_owned());
    let sp = cmdline::split(&command, &base);
    if !sp.changed() {
        return;
    }
    let mut notes = vec![];
    if let Some(d) = &sp.cwd {
        notes.push(format!("cwd {d}"));
        cfg.cwd = Some(d.clone());
    }
    if !sp.env.is_empty() {
        notes.push(format!("env {}", sp.env.keys().cloned().collect::<Vec<_>>().join(", ")));
        let mut env = cfg.env.clone().unwrap_or_default();
        for (k, v) in &sp.env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        cfg.env = Some(env);
    }
    if !sp.source.is_empty() {
        notes.push(format!("source {}", sp.source.join(", ")));
        let shell = cfg.shell.clone().unwrap_or_else(cmdline::user_shell);
        let mut src = cfg.source.clone().unwrap_or_else(|| cmdline::default_source(&shell));
        src.extend(sp.source.iter().cloned());
        cfg.source = Some(src);
    }
    cfg.command = Some(sp.command.clone());
    println!("{} split out {}  {}", c::cyan("↳"), notes.join(" · "), c::dim("(--no-split to keep the command as typed)"));
}

fn cmd_edit(args: &[String]) -> R {
    let name = need(args.first())?;
    let cl = client::connect(true)?;
    cl.get(&name)?;
    let file = config::service_file(&name);
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "nano".into());
    let st = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("{editor} \"$1\""), "sh", &file.to_string_lossy()])
        .status()
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err(format!("{editor} exited with {}", st.code().unwrap_or(-1)));
    }
    let r = cl.reload()?;
    for e in &r.errors {
        eprintln!("{}", c::red(&format!("config error: {e}")));
    }
    match cl.get(&name) {
        Ok(s) if s.status == "running" && r.updated.contains(&name) => println!("{}", c::yellow(&format!("{name} is running — run `lstack restart {name}` to apply runtime changes"))),
        _ => println!("{}", c::green("✓ saved")),
    }
    Ok(())
}

fn confirm(q: &str) -> bool {
    print!("{q} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_lowercase().starts_with('y')
}

fn cmd_remove(args: &[String]) -> R {
    let a = Args::parse(args, &[("yes", "y", false)])?;
    let name = need(a.pos.first())?;
    if !a.has("yes") && std::io::stdin().is_terminal() && !confirm(&format!("Stop and remove {}?", c::bold(&name))) {
        return Ok(());
    }
    client::connect(true)?.remove(&name)?;
    println!("{} removed {name}", c::green("✓"));
    Ok(())
}

fn targets(cl: &Client, names: &[String]) -> Result<Vec<String>, String> {
    if names.is_empty() {
        return Err("missing service name (or `all`)".into());
    }
    if names.iter().any(|n| n == "all") {
        return Ok(cl.list()?.services.into_iter().map(|s| s.name).collect());
    }
    Ok(names.to_vec())
}

fn cmd_action(action: &str, args: &[String]) -> R {
    let cl = client::connect(true)?;
    let names = targets(&cl, args)?;
    let results: Vec<_> = std::thread::scope(|sc| {
        let hs: Vec<_> = names.iter().map(|n| sc.spawn(|| cl.action(n, action))).collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("panicked".into()))).collect()
    });
    let mut failed = false;
    for (n, r) in names.iter().zip(results) {
        match r {
            Ok(s) => println!(
                "{} {} {}{}",
                c::status_dot(&s.status),
                c::bold(&s.name),
                c::status_color(&s.status, &s.status),
                match (&s.last_error, s.status.as_str()) {
                    (Some(e), st) if st != "running" => c::red(&format!("  {e}")),
                    _ => String::new(),
                }
            ),
            Err(e) => {
                failed = true;
                eprintln!("{} {n}: {e}", c::red("✗"));
            }
        }
    }
    if failed { Err(String::new()) } else { Ok(()) }
}

fn cmd_autostart(on: bool, args: &[String]) -> R {
    let cl = client::connect(true)?;
    for n in targets(&cl, args)? {
        cl.update(&n, json!({ "autostart": on }))?;
        println!("{} {n} {}", if on { c::cyan("✓") } else { c::dim("·") }, if on { "will start at login" } else { "will not start at login" });
    }
    Ok(())
}

pub fn fmt_line(l: &LogLine) -> String {
    let body = match l.stream.as_str() {
        "stderr" => c::red(&l.line),
        "system" => c::cyan(&l.line),
        _ => l.line.clone(),
    };
    format!("{} {body}", c::dim(&c::local_time(l.ts)))
}

fn print_stream(cl: &Client, name: &str, tail: usize) -> R {
    cl.stream(&format!("{}/logs/stream?tail={tail}", client::svc_path(name)), |ev, v| {
        if ev != "log" {
            return true;
        }
        if let Ok(l) = serde_json::from_value::<LogLine>(v) {
            println!("{}", fmt_line(&l));
        }
        true
    })
}

fn cmd_logs(args: &[String]) -> R {
    let a = Args::parse(args, &[("follow", "f", false), ("lines", "n", true)])?;
    let name = need(a.pos.first())?;
    let n: usize = a.get("lines").unwrap_or("200").parse().map_err(|_| "-n must be a number")?;
    let cl = client::connect(true)?;
    if !a.has("follow") {
        for l in cl.logs(&name, n)? {
            println!("{}", fmt_line(&l));
        }
        return Ok(());
    }
    print_stream(&cl, &name, n)
}

fn cmd_attach(args: &[String]) -> R {
    let name = need(args.first())?;
    let cl = client::connect(true)?;
    let s = cl.get(&name)?;
    if s.kind != "process" {
        return cmd_exec(args);
    }
    println!("{}", c::dim(&format!("attached to {name} — lines you type are sent to stdin. Ctrl-C to detach (the process keeps running).")));
    let (cl2, n2) = (cl.clone(), name.clone());
    std::thread::spawn(move || {
        if let Err(e) = print_stream(&cl2, &n2, 50) {
            eprintln!("{}", c::red(&e));
        }
        std::process::exit(0);
    });
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Err(e) = cl.stdin(&name, &format!("{line}\n")) {
            eprintln!("{}", c::red(&e));
        }
    }
    Ok(())
}

/// The container to `docker exec` into for a docker/compose service.
pub fn exec_container(s: &ServiceState) -> Result<String, String> {
    match s.kind.as_str() {
        "docker" => Ok(s.config.container_name()),
        "compose" => {
            let mut cmd = std::process::Command::new(crate::daemon::docker_api::docker_bin());
            cmd.args(["compose", "-f", &paths::expand_home(s.config.compose_file.as_deref().unwrap_or(""))]);
            if let Some(p) = &s.config.project {
                cmd.args(["-p", p]);
            }
            let out = cmd.args(["ps", "-q"]).output().map_err(|e| e.to_string())?;
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(String::from).collect();
            if ids.len() > 1 {
                eprintln!("{}", c::dim(&format!("({} containers; using {} — use docker exec directly for others)", ids.len(), &ids[0][..12.min(ids[0].len())])));
            }
            ids.into_iter().next().ok_or_else(|| format!("{} has no running containers", s.name))
        }
        _ => Err(format!("exec is for docker/compose services; use `lstack attach {}` for processes", s.name)),
    }
}

pub const DEFAULT_SHELL: [&str; 3] = ["sh", "-c", "command -v bash >/dev/null && exec bash || exec sh"];

fn cmd_exec(args: &[String]) -> R {
    let name = need(args.first())?;
    let cl = client::connect(true)?;
    // Runs through the daemon so output gets the same secret scrubbing as logs.
    match crate::daemon::exec::run_client(&cl.info, &name, &args[1..])? {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

fn cmd_metrics(args: &[String]) -> R {
    let a = Args::parse(args, &[("range", "", true), ("json", "", false)])?;
    let name = need(a.pos.first())?;
    let range = a.get("range").unwrap_or("1h");
    let samples = client::connect(true)?.metrics(&name, range, 80)?;
    if a.has("json") {
        println!("{}", serde_json::to_string(&samples).unwrap());
        return Ok(());
    }
    if samples.is_empty() {
        println!("{}", c::dim("no samples in range"));
        return Ok(());
    }
    let cpu: Vec<f64> = samples.iter().map(|s| s.cpu).collect();
    let mem: Vec<f64> = samples.iter().map(|s| s.mem).collect();
    let rx: Vec<f64> = samples.iter().map(|s| s.net_rx.unwrap_or(0.0)).collect();
    let tx: Vec<f64> = samples.iter().map(|s| s.net_tx.unwrap_or(0.0)).collect();
    let stat = |v: &[f64], f: &dyn Fn(Option<f64>) -> String| {
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        let max = v.iter().cloned().fold(f64::MIN, f64::max);
        c::dim(&format!("avg {}  max {}", f(Some(avg)), f(Some(max))))
    };
    println!("{}", c::bold(&format!("{name} — last {range}")));
    println!("  cpu  {}  {}", c::green(&c::sparkline(&cpu, 80)), stat(&cpu, &c::pct));
    println!("  mem  {}  {}", c::blue(&c::sparkline(&mem, 80)), stat(&mem, &c::bytes));
    println!("  ↓    {}  {}", c::magenta(&c::sparkline(&rx, 80)), stat(&rx, &c::rate));
    println!("  ↑    {}  {}", c::yellow(&c::sparkline(&tx, 80)), stat(&tx, &c::rate));
    Ok(())
}

/// Back-compat alias for `setup` (login items only).
pub(crate) fn read_password(prompt: &str, stdin_only: bool) -> Result<String, String> {
    let stdin = std::io::stdin();
    if stdin_only || !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_line(&mut s).map_err(|e| e.to_string())?;
        return Ok(s.trim_end_matches(['\r', '\n']).to_string());
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let fd = libc::STDIN_FILENO;
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let have = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let saved = term;
    if have {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut s = String::new();
    let res = stdin.lock().read_line(&mut s);
    if have {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    eprintln!();
    res.map_err(|e| e.to_string())?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

fn ask_new_password(stdin_only: bool) -> Result<String, String> {
    let p = read_password("New remote password (min 8 chars): ", stdin_only)?;
    if !stdin_only && std::io::stdin().is_terminal() && read_password("Repeat password: ", false)? != p {
        return Err("passwords don't match".into());
    }
    Ok(p)
}

fn print_remote(v: &serde_json::Value) {
    let running = v["running"].as_bool().unwrap_or(false);
    let state = if running {
        c::green("● on")
    } else if v["enabled"].as_bool().unwrap_or(false) {
        c::red("● enabled but not listening")
    } else {
        c::dim("○ off")
    };
    println!("{state}  remote dashboard");
    println!("  {:<9} {}:{}", c::dim("listen"), v["bind"].as_str().unwrap_or("?"), v["port"]);
    let pw = if v["hasPassword"].as_bool().unwrap_or(false) { "" } else { "  (no password set)" };
    println!("  {:<9} {}{pw}", c::dim("login"), v["username"].as_str().unwrap_or("?"));
    let tfa = if v["totp"].as_bool().unwrap_or(false) { c::green("on (authenticator code required)") } else { c::dim("off — lstack remote 2fa on") };
    println!("  {:<9} {tfa}", c::dim("2fa"));
    let access = if v["allowControl"].as_bool().unwrap_or(false) { "view + start/stop/restart" } else { "read-only" };
    println!("  {:<9} {access}", c::dim("access"));
    if let Some(e) = v["error"].as_str() {
        println!("  {:<9} {}", c::dim("error"), c::red(e));
    }
    for u in v["urls"].as_array().into_iter().flatten() {
        println!("  {:<9} {}", c::dim("url"), c::cyan(u.as_str().unwrap_or("")));
    }
    if running {
        println!("{}", c::dim("  Plain HTTP: use on a trusted network or over a VPN such as Tailscale."));
    }
}

fn cmd_remote(args: &[String]) -> R {
    let a = Args::parse(
        args,
        &[
            ("port", "", true),
            ("user", "u", true),
            ("bind", "", true),
            ("allow-control", "", false),
            ("read-only", "", false),
            ("password-stdin", "", false),
            ("code", "", true),
            ("json", "", false),
        ],
    )?;
    let cl = client::connect(true)?;
    let sub = a.pos.first().map(String::as_str).unwrap_or("status");
    if matches!(sub, "pair" | "peers") {
        return crate::cli_more::cmd_remote_peers(&cl, sub, &a.pos[1..], a.has("json"));
    }
    if matches!(sub, "2fa" | "totp" | "otp") {
        return remote_2fa(&cl, a.pos.get(1).map(String::as_str).unwrap_or("status"), a.get("code"));
    }
    let current: serde_json::Value = cl.req("GET", "/api/remote", None)?;
    let mut patch = serde_json::Map::new();
    if let Some(p) = a.get("port") {
        patch.insert("port".into(), json!(p.parse::<u16>().map_err(|_| format!("invalid port {p}"))?));
    }
    if let Some(u) = a.get("user") {
        patch.insert("username".into(), json!(u));
    }
    if let Some(b) = a.get("bind") {
        patch.insert("bind".into(), json!(b));
    }
    if a.has("allow-control") {
        patch.insert("allowControl".into(), json!(true));
    }
    if a.has("read-only") {
        patch.insert("allowControl".into(), json!(false));
    }
    match sub {
        "status" if patch.is_empty() => {
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&current).unwrap_or_default());
            } else {
                print_remote(&current);
            }
            return Ok(());
        }
        "status" | "set" => {}
        "on" | "enable" | "start" => {
            patch.insert("enabled".into(), json!(true));
            if !current["hasPassword"].as_bool().unwrap_or(false) || a.has("password-stdin") {
                patch.insert("password".into(), json!(ask_new_password(a.has("password-stdin"))?));
            }
        }
        "off" | "disable" | "stop" => {
            patch.insert("enabled".into(), json!(false));
        }
        "passwd" | "password" => {
            patch.insert("password".into(), json!(ask_new_password(a.has("password-stdin"))?));
        }
        other => return Err(format!("unknown remote command {other:?} (on, off, passwd, status)")),
    }
    let v: serde_json::Value = cl.req("PUT", "/api/remote", Some(serde_json::Value::Object(patch)))?;
    print_remote(&v);
    Ok(())
}

fn remote_2fa(cl: &client::Client, sub: &str, code: Option<&str>) -> R {
    match sub {
        "status" => {
            let v: serde_json::Value = cl.req("GET", "/api/remote", None)?;
            let on = v["totp"].as_bool().unwrap_or(false);
            println!("two-factor sign-in: {}", if on { c::green("on") } else { c::dim("off") });
            Ok(())
        }
        "on" | "enable" | "setup" => {
            let v: serde_json::Value = cl.req("POST", "/api/remote/totp", None)?;
            let (secret, uri) = (v["secret"].as_str().unwrap_or(""), v["uri"].as_str().unwrap_or(""));
            println!("Scan this with an authenticator app (1Password, Google Authenticator, Authy…):\n");
            if let Some(qr) = qr_lines(uri) {
                println!("{qr}");
            }
            let grouped: Vec<String> = secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
            println!("or enter the key manually: {}\n", c::bold(&grouped.join(" ")));
            let code = match code {
                Some(c) => c.to_string(),
                None => {
                    eprint!("Enter the 6-digit code to confirm: ");
                    let mut s = String::new();
                    std::io::stdin().lock().read_line(&mut s).map_err(|e| e.to_string())?;
                    s.trim().to_string()
                }
            };
            let v: serde_json::Value = cl.req("POST", "/api/remote/totp/confirm", Some(json!({ "code": code })))?;
            println!("{} two-factor sign-in is on; remote browsers were signed out", c::green("✓"));
            print_remote(&v);
            Ok(())
        }
        "off" | "disable" => {
            let v: serde_json::Value = cl.req("DELETE", "/api/remote/totp", None)?;
            println!("{} two-factor sign-in is off", c::green("✓"));
            print_remote(&v);
            Ok(())
        }
        other => Err(format!("unknown 2fa command {other:?} (on, off, status)")),
    }
}

/// Renders a QR code with half-block characters and explicit black/white colours, so it scans on
/// dark and light terminal themes alike.
fn qr_lines(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let quiet = 2usize;
    let dark = |x: isize, y: isize| -> bool {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < w && colors[y as usize * w + x as usize] == qrcode::Color::Dark
    };
    let size = (w + 2 * quiet) as isize;
    let mut out = String::new();
    let mut y = -(quiet as isize);
    while y < size - quiet as isize {
        out.push_str("  ");
        for x in -(quiet as isize)..size - quiet as isize {
            let fg = if dark(x, y) { 30 } else { 97 };
            let bg = if dark(x, y + 1) { 40 } else { 107 };
            out.push_str(&format!("\x1b[{fg};{bg}m▀"));
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    Some(out)
}

fn cmd_terminal(args: &[String]) -> R {
    let cl = client::connect(true)?;
    if args.first().map(String::as_str) == Some("cmux") && args.iter().any(|a| a == "--fix") {
        let v: serde_json::Value = cl.req("POST", "/api/terminal/cmux/enable-password", None)?;
        println!("{} cmux socket control set to password mode; LStack keeps the password in its secret store", c::green("✓"));
        if let Some(b) = v["backup"].as_str() {
            println!("  {:<8} {b}", c::dim("backup"));
        }
        if let Some(n) = v["note"].as_str() {
            println!("  {}", c::yellow(n));
        }
        println!("{}", c::dim("  You can change or reset the password any time in cmux Settings → Automation."));
    }
    let args: Vec<String> = args.iter().filter(|a| *a != "--fix").cloned().collect();
    let v: serde_json::Value = match args.first() {
        Some(t) => cl.req("PUT", "/api/settings", Some(json!({ "terminal": t })))?,
        None => cl.req("GET", "/api/settings", None)?,
    };
    let cur = v["terminal"].as_str().unwrap_or("terminal");
    for t in v["terminals"].as_array().into_iter().flatten() {
        let id = t["id"].as_str().unwrap_or("");
        let installed = t["installed"].as_bool().unwrap_or(false);
        let mark = if id == cur { c::green("●") } else { c::dim("○") };
        let note = if installed { String::new() } else { c::dim("  (not installed)") };
        println!("{mark} {:<9} {}{note}", id, t["name"].as_str().unwrap_or(""));
    }
    let cm = &v["cmux"];
    if cm["installed"].as_bool().unwrap_or(false) {
        let mode = cm["mode"].as_str().unwrap_or("?");
        let ok = mode == "allowAll" || (mode == "password" && cm["hasPassword"].as_bool().unwrap_or(false));
        let note = if ok { c::green("LStack can open workspaces") } else { c::yellow("LStack may be blocked — fix with: lstack terminal cmux --fix") };
        println!("{}", c::dim(&format!("cmux socket control: {mode}")) + "  " + &note);
    }
    if args.is_empty() {
        println!("{}", c::dim("change with: lstack terminal <id>"));
    }
    Ok(())
}

fn cmd_startup(args: &[String]) -> R {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    match sub {
        "install" => {
            let mut a = vec!["install".to_string(), "--login".into()];
            a.extend(args[1..].iter().cloned());
            setup::run(&a)
        }
        "uninstall" => {
            for n in launchd::uninstall() {
                println!("{}", c::dim(&n));
            }
            println!("{} removed login items (the daemon was stopped)", c::green("✓"));
            Ok(())
        }
        "print" => {
            print!("{}", launchd::render_daemon_plist(&paths::self_exe()));
            Ok(())
        }
        _ => setup::run(&["status".into()]),
    }
}

fn cmd_daemon(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    if sub == "run" {
        return crate::daemon::run();
    }
    let info = client::read_info();
    let alive = info.as_ref().is_some_and(client::ping);
    let res: R = (|| {
        match sub {
            "start" => {
                let cl = client::connect(true)?;
                println!("{} daemon running (pid {}, port {})", c::green("●"), cl.info.pid, cl.info.port);
            }
            "stop" | "restart" => {
                if launchd::status().loaded && launchd::manages_this_home() {
                    launchd::bootout_daemon();
                } else if alive {
                    let _ = Client::new(info.clone().unwrap()).shutdown();
                }
                if let Some(i) = &info {
                    for _ in 0..50 {
                        if !client::ping(i) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
                println!("{}", c::dim("daemon stopped"));
                if sub == "restart" {
                    let cl = client::connect(true)?;
                    println!("{} daemon running (pid {}, port {})", c::green("●"), cl.info.pid, cl.info.port);
                }
            }
            "logs" => {
                let _ = std::process::Command::new("tail").args(["-n", "100", "-f"]).arg(paths::daemon_log()).status();
            }
            _ => match (&info, alive) {
                (Some(i), true) => println!(
                    "{} daemon running  pid {}  port {}  up {}  {}",
                    c::green("●"),
                    i.pid,
                    i.port,
                    c::duration(Some(paths::now_ms() - i.started_at)),
                    c::dim(&paths::home().to_string_lossy())
                ),
                _ => println!("{} daemon not running", c::gray("●")),
            },
        }
        Ok(())
    })();
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", c::red(&format!("error: {e}")));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn args() {
        let v: Vec<String> = ["api", "-a", "-e", "A=1", "-eB=2", "--cwd=/x", "--", "npm", "-v"].iter().map(|s| s.to_string()).collect();
        let a = Args::parse(&v, &[("autostart", "a", false), ("env", "e", true), ("cwd", "c", true)]).unwrap();
        assert_eq!(a.pos, vec!["api", "npm", "-v"]);
        assert!(a.has("autostart"));
        assert_eq!(a.all("env"), vec!["A=1", "B=2"]);
        assert_eq!(a.get("cwd"), Some("/x"));
        assert!(Args::parse(&["--nope".into()], &[]).is_err());
    }
}

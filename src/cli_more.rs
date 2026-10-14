//! CLI commands for secrets, heartbeats, machines and processes.
use crate::cli::{confirm, read_password, Args};
use crate::client;
use crate::fmt as c;
use serde_json::{json, Value};
use std::io::{IsTerminal, Read};

type R = Result<(), String>;

// ------------------------------------------------------------------ secrets

fn print_secrets(v: &Value) {
    if v["locked"].as_bool().unwrap_or(false) {
        println!("{}", c::red(&format!("secret store is locked: {}", v["error"].as_str().unwrap_or("?"))));
        println!("{}", c::dim("  Try `lstack secret unlock` (e.g. after the login Keychain was locked)."));
    }
    let items: Vec<&Value> = v["secrets"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    if items.is_empty() {
        println!("{}", c::dim("No secrets yet. Add one with: lstack secret set <NAME>"));
        return;
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|s| {
            let state = if s["missing"].as_bool().unwrap_or(false) {
                c::red("not set")
            } else {
                c::green("set ✓")
            };
            let set_at = s["setAt"].as_i64().map(c::local_time).unwrap_or_else(|| "—".into());
            let used: Vec<&str> = s["usedBy"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let used = if s["internal"].as_bool().unwrap_or(false) { c::dim("(LStack)") } else if used.is_empty() { c::dim("—") } else { used.join(", ") };
            vec![c::bold(s["name"].as_str().unwrap_or("?")), state, set_at, used]
        })
        .collect();
    println!("{}", c::table(&rows, &["NAME", "VALUE", "UPDATED", "USED BY"]));
    println!("{}", c::dim(&format!("  Encrypted at rest; key split between {} and secrets.key. Values can't be read back.", if v["storage"] == "keychain" { "the login Keychain" } else { "secrets.share2" })));
}

pub fn cmd_secret(args: &[String]) -> R {
    let a = Args::parse(args, &[("stdin", "", false), ("json", "", false), ("yes", "y", false)])?;
    let cl = client::connect(true)?;
    let sub = a.pos.first().map(String::as_str).unwrap_or("ls");
    let name = a.pos.get(1).cloned();
    let v: Value = match sub {
        "ls" | "list" | "status" => cl.req("GET", "/api/secrets", None)?,
        "set" | "add" => {
            let name = name.ok_or("usage: lstack secret set <NAME> [--stdin]")?;
            let value = if a.has("stdin") || !std::io::stdin().is_terminal() {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
                s.trim_end_matches(['\r', '\n']).to_string()
            } else {
                let v = read_password(&format!("Value for {name} (hidden): "), false)?;
                if read_password("Repeat: ", false)? != v {
                    return Err("values don't match".into());
                }
                v
            };
            if value.len() < crate::scrub::MIN_LEN {
                eprintln!("{}", c::yellow(&format!("warning: values shorter than {} characters are not scrubbed from logs", crate::scrub::MIN_LEN)));
            }
            let r = cl.req("PUT", &format!("/api/secrets/{}", client::enc(&name)), Some(json!({ "value": value })))?;
            println!("{} {name} saved", c::green("✓"));
            r
        }
        "rm" | "remove" | "delete" | "unset" => {
            let name = name.ok_or("usage: lstack secret rm <NAME>")?;
            let r = cl.req("DELETE", &format!("/api/secrets/{}", client::enc(&name)), None)?;
            println!("{} {name} removed", c::green("✓"));
            r
        }
        "unlock" => cl.req("POST", "/api/secrets/unlock", None)?,
        "rotate-key" | "rotate" => {
            let r = cl.req("POST", "/api/secrets/rotate", None)?;
            println!("{} new encryption key and key shares written", c::green("✓"));
            r
        }
        "get" | "show" | "reveal" => return Err("secrets are write-only: values can't be read back (set it again to change it)".into()),
        other => return Err(format!("unknown secret command {other:?} (set, ls, rm, rotate-key, unlock)")),
    };
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else if matches!(sub, "ls" | "list" | "status" | "unlock") {
        print_secrets(&v);
    }
    Ok(())
}

// ------------------------------------------------------------------ machines

fn machine_status(s: &str) -> String {
    match s {
        "online" => c::green("● online"),
        "connecting" => c::yellow("◌ connecting"),
        "unauthorized" => c::red("● unauthorized"),
        "repair" => c::yellow("● pair again"),
        _ => c::red(&format!("○ {s}")),
    }
}

pub fn cmd_machine(args: &[String]) -> R {
    let a = Args::parse(
        args,
        &[("code", "", true), ("name", "", true), ("totp", "", true), ("json", "", false), ("yes", "y", false)],
    )?;
    let cl = client::connect(true)?;
    let sub = a.pos.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "list" | "status" => {
            let v: Value = cl.req("GET", "/api/machines", None)?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let local: Value = cl.req("GET", "/api/services", None).unwrap_or_default();
            let local = local["services"].as_array().cloned().unwrap_or_default();
            let running = local.iter().filter(|s| s["status"] == "running").count();
            let mut rows = vec![vec![
                c::bold(v["local"]["name"].as_str().unwrap_or("this mac")),
                c::dim("this machine"),
                machine_status("online"),
                format!("{running}/{} running", local.len()),
                c::cyan("control"),
            ]];
            for m in v["machines"].as_array().into_iter().flatten() {
                let n = m["services"].as_array().map(Vec::len).unwrap_or(0);
                let running = m["services"].as_array().into_iter().flatten().filter(|s| s["status"] == "running").count();
                rows.push(vec![
                    c::bold(m["name"].as_str().unwrap_or("?")),
                    m["address"].as_str().unwrap_or("").to_string(),
                    machine_status(m["status"].as_str().unwrap_or("?")),
                    format!("{running}/{n} running"),
                    if m["grants"].is_null() { c::dim("—") } else { grants_text(&m["grants"]) },
                ]);
            }
            println!("{}", c::table(&rows, &["MACHINE", "ADDRESS", "STATUS", "SERVICES", "ACCESS"]));
            for m in v["machines"].as_array().into_iter().flatten() {
                if let Some(e) = m["error"].as_str() {
                    eprintln!("{}", c::dim(&format!("  {}: {e}", m["name"].as_str().unwrap_or("?"))));
                }
            }
            if v["machines"].as_array().is_none_or(Vec::is_empty) {
                println!("{}", c::dim("  Watch another Mac: run `lstack remote pair` there, then `lstack machine pair <address> --code XXXX-XXXX` here."));
            }
            Ok(())
        }
        "add" | "pair" => {
            let addr = a.pos.get(1).ok_or("usage: lstack machine pair <address> --code XXXX-XXXX [--totp 123456] [--name <alias>]")?;
            let code = match a.get("code") {
                Some(c) => c.to_string(),
                None if std::io::stdin().is_terminal() => {
                    eprint!("Pairing code shown on {addr} (Share this Mac → Show pairing code): ");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).map_err(|e| e.to_string())?;
                    s.trim().to_string()
                }
                None => return Err("--code required (run `lstack remote pair` on that Mac)".into()),
            };
            let mut body = json!({ "address": addr, "code": code });
            if let Some(n) = a.get("name") {
                body["name"] = json!(n);
            }
            let totp = match a.get("totp") {
                Some(t) => Some(t.to_string()),
                None if std::io::stdin().is_terminal() => {
                    eprint!("2FA code from {addr}'s authenticator (Enter if it doesn't use 2FA): ");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).map_err(|e| e.to_string())?;
                    Some(s.trim().to_string()).filter(|s| !s.is_empty())
                }
                None => None,
            };
            if let Some(t) = totp {
                body["totp"] = json!(t);
            }
            let m: Value = cl.req("POST", "/api/machines", Some(body))?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&m).unwrap_or_default());
                return Ok(());
            }
            println!("{} paired with {} ({})", c::green("✓"), c::bold(m["name"].as_str().unwrap_or("?")), m["address"].as_str().unwrap_or(""));
            println!("  {:<12} {}", c::dim("phrase"), c::bold(m["phrase"].as_str().unwrap_or("?")));
            println!("  {:<12} {}", c::dim("fingerprint"), m["fingerprint"].as_str().unwrap_or("?"));
            println!("  {:<12} {}", c::dim("access"), grants_text(&m["grants"]));
            println!("{}", c::dim("  The other Mac shows the same phrase. Its services now show up in `lstack ls`, the menu bar and the web\n  dashboard (address them as name@machine); services it shares are forwarded to localhost — see `lstack forwards`."));
            Ok(())
        }
        "id" | "identity" | "whoami" => {
            let v: Value = cl.req("GET", "/api/identity", None)?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            println!("{}  {}", c::bold(v["name"].as_str().unwrap_or("?")), v["fingerprint"].as_str().unwrap_or("?"));
            println!("{}", c::dim("  This install's link key fingerprint. Paired Macs pin it; it never leaves this Mac's secret store."));
            Ok(())
        }
        "rekey" => {
            if !a.has("yes") && !confirm("New link key for this Mac? Every pairing (both ways) must be made again.") {
                return Ok(());
            }
            let v: Value = cl.req("POST", "/api/identity/rekey", None)?;
            println!("{} new key {}", c::green("✓"), v["fingerprint"].as_str().unwrap_or("?"));
            println!("{}", c::dim("  Pair again with each Mac: `lstack machine pair <address> --code …`."));
            Ok(())
        }
        "rm" | "remove" | "forget" => {
            let n = a.pos.get(1).ok_or("usage: lstack machine rm <name>")?;
            let _: Value = cl.req("DELETE", &format!("/api/machines/{}", client::enc(n)), None)?;
            println!("{} forgot {n}", c::green("✓"));
            println!("{}", c::dim("  To cut it off on that Mac too: `lstack remote revoke <name>` there."));
            Ok(())
        }
        "rename" | "mv" => {
            let (Some(n), Some(to)) = (a.pos.get(1), a.pos.get(2)) else { return Err("usage: lstack machine rename <name> <new-name>".into()) };
            let _: Value = cl.req("PATCH", &format!("/api/machines/{}", client::enc(n)), Some(json!({ "name": to })))?;
            println!("{} {n} → {to}", c::green("✓"));
            Ok(())
        }
        other => Err(format!("unknown machine command {other:?} (ls, pair, id, rekey, rm, rename)")),
    }
}

/// "control + tunnel all" etc. from `{control, tunnel: "all" | "none" | [..]}`.
pub fn grants_text(g: &Value) -> String {
    let tunnel = match &g["tunnel"] {
        Value::String(s) if s == "all" => "tunnel all".to_string(),
        Value::Array(a) if !a.is_empty() => format!("tunnel {}", a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")),
        _ => "no tunnels".to_string(),
    };
    let control = if g["control"].as_bool().unwrap_or(false) { c::cyan("view + control") } else { "view".to_string() };
    format!("{control}, {tunnel}")
}

/// `lstack remote pair|peers|grant|revoke|activity` (this Mac's side of pairing).
pub fn cmd_remote_peers(cl: &client::Client, sub: &str, rest: &[String], tunnel: Option<&str>, json_out: bool) -> R {
    let out = |v: &Value| println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    match sub {
        "pair" => {
            let v: Value = cl.req("POST", "/api/remote/pair-code", None)?;
            if json_out {
                out(&v);
                return Ok(());
            }
            println!("Pairing code (valid 10 minutes, one use): {}\n", c::bold(v["code"].as_str().unwrap_or("?")));
            println!("On the Mac that should pair with this one, run:");
            let urls: Vec<&str> = v["urls"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            for u in if urls.is_empty() { vec!["<this-mac-address>"] } else { urls } {
                println!("  lstack machine pair {u} --code {}", v["code"].as_str().unwrap_or(""));
            }
            let tfa = if v["totp"].as_bool().unwrap_or(false) { "  It also asks for a 2FA code from this Mac's authenticator app.\n" } else { "" };
            println!("{}", c::dim(&format!("{tfa}  New pairings may view and tunnel to services; allow start/stop with `lstack remote grant <mac> control`.")));
            Ok(())
        }
        "grant" => {
            let who = rest.first().ok_or("usage: lstack remote grant <mac> [control|no-control] [--tunnel all|none|svc,svc]")?;
            let mut patch = json!({});
            for w in &rest[1..] {
                match w.as_str() {
                    "control" => patch["control"] = json!(true),
                    "no-control" | "view" => patch["control"] = json!(false),
                    other => return Err(format!("unknown grant {other:?} (control, no-control; tunnels with --tunnel)")),
                }
            }
            if let Some(t) = tunnel {
                patch["tunnel"] = match t {
                    "all" | "none" => json!(t),
                    list => json!(list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()),
                };
            }
            if patch.as_object().is_some_and(|o| o.is_empty()) {
                return Err("say what to change: control, no-control and/or --tunnel all|none|svc,svc".into());
            }
            let v: Value = cl.req("PATCH", &format!("/api/remote/peers/{}", client::enc(who)), Some(patch))?;
            let p = v.as_array().into_iter().flatten().find(|p| p["id"] == json!(who) || p["name"] == json!(who));
            println!("{} {who}: {}", c::green("✓"), p.map(|p| grants_text(&p["grants"])).unwrap_or_default());
            Ok(())
        }
        "revoke" => {
            let who = rest.first().ok_or("usage: lstack remote revoke <mac>")?;
            let _: Value = cl.req("DELETE", &format!("/api/remote/peers/{}", client::enc(who)), None)?;
            println!("{} unpaired {who}; its link and tunnels are closed", c::green("✓"));
            Ok(())
        }
        "activity" => {
            let v: Value = cl.req("GET", "/api/remote/activity", None)?;
            if json_out {
                out(&v);
                return Ok(());
            }
            let events: Vec<&Value> = v["events"].as_array().into_iter().flatten().collect();
            if events.is_empty() {
                println!("{}", c::dim("No link activity yet."));
            }
            for e in events.iter().take(40).rev() {
                let mut detail: Vec<String> = vec![];
                for k in ["peer", "service", "port", "address", "reason", "bytes"] {
                    if let Some(x) = e.get(k).filter(|x| !x.is_null()) {
                        detail.push(format!("{k}={}", x.as_str().map(String::from).unwrap_or_else(|| x.to_string())));
                    }
                }
                println!("{}  {:<14} {}", c::dim(&e["at"].as_i64().map(c::local_time).unwrap_or_default()), e["event"].as_str().unwrap_or("?"), detail.join("  "));
            }
            let tunnels: Vec<&Value> = v["tunnels"].as_array().into_iter().flatten().collect();
            if !tunnels.is_empty() {
                println!();
                let rows: Vec<Vec<String>> = tunnels
                    .iter()
                    .map(|t| vec![c::bold(t["peer"].as_str().unwrap_or("?")), format!("{}:{}", t["service"].as_str().unwrap_or("?"), t["port"]), t["connections"].to_string(), c::bytes(t["bytes"].as_f64()), t["lastAt"].as_i64().map(c::local_time).unwrap_or_default()])
                    .collect();
                println!("{}", c::table(&rows, &["MAC", "TUNNEL", "CONNS", "BYTES", "LAST USED"]));
            }
            Ok(())
        }
        _ => {
            if matches!(rest.first().map(String::as_str), Some("rm" | "revoke" | "remove")) {
                return cmd_remote_peers(cl, "revoke", &rest[1..], None, json_out);
            }
            let v: Value = cl.req("GET", "/api/remote/peers", None)?;
            if json_out {
                out(&v);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = v
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    vec![
                        c::bold(p["name"].as_str().unwrap_or("?")),
                        if p["online"].as_bool().unwrap_or(false) { c::green("● linked") } else { c::dim("○ offline") },
                        grants_text(&p["grants"]),
                        p["fingerprint"].as_str().unwrap_or("").to_string(),
                        p["lastSeen"].as_i64().map(c::local_time).unwrap_or_else(|| c::dim("never")),
                    ]
                })
                .collect();
            if rows.is_empty() {
                println!("{}", c::dim("No Macs are paired with this one. Pair one with: lstack remote pair"));
            } else {
                println!("{}", c::table(&rows, &["MAC", "LINK", "ACCESS", "KEY", "LAST SEEN"]));
            }
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ forwards

/// `lstack forwards [pause|resume svc@machine]`: paired Macs' services on localhost.
pub fn cmd_forwards(args: &[String]) -> R {
    let a = Args::parse(args, &[("json", "", false)])?;
    let cl = client::connect(true)?;
    match a.pos.first().map(String::as_str) {
        Some(verb @ ("pause" | "resume")) => {
            let target = a.pos.get(1).ok_or(format!("usage: lstack forwards {verb} <service>@<machine>"))?;
            let (svc, m) = target.split_once('@').ok_or("give the service as service@machine")?;
            let _: Value = cl.req("POST", &format!("/api/forwards/{verb}"), Some(json!({ "machine": m, "service": svc })))?;
            println!("{} {verb}d forwarding {target}", c::green("✓"));
            Ok(())
        }
        Some(other) if other != "ls" && other != "list" => Err(format!("unknown forwards command {other:?} (ls, pause, resume)")),
        _ => {
            let v: Value = cl.req("GET", "/api/forwards", None)?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let rows: Vec<Vec<String>> = v
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let state = if f["paused"].as_bool().unwrap_or(false) {
                        c::dim("paused")
                    } else if let Some(e) = f["error"].as_str() {
                        c::red(e)
                    } else if f["online"].as_bool().unwrap_or(false) {
                        c::green("● ready")
                    } else {
                        c::yellow("○ machine offline")
                    };
                    vec![
                        c::bold(&format!("{}@{}", f["service"].as_str().unwrap_or("?"), f["machine"].as_str().unwrap_or("?"))),
                        f["port"].to_string(),
                        f["url"].as_str().map(c::cyan).unwrap_or_else(|| c::dim("—")),
                        state,
                        f["connections"].to_string(),
                    ]
                })
                .collect();
            if rows.is_empty() {
                println!("{}", c::dim("Nothing forwarded. Services a paired Mac lets you tunnel to appear here while they listen on a port."));
            } else {
                println!("{}", c::table(&rows, &["SERVICE", "REMOTE PORT", "LOCAL URL", "STATE", "CONNS"]));
                println!("{}", c::dim("  Only reachable from this Mac, by your user; traffic rides the encrypted link."));
            }
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ processes

pub fn print_procs(procs: &[Value]) {
    let rows: Vec<Vec<String>> = procs
        .iter()
        .map(|p| {
            vec![
                p["pid"].to_string(),
                c::bold(p["name"].as_str().unwrap_or("?")),
                c::pct(p["cpu"].as_f64()),
                c::bytes(p["mem"].as_f64()),
                c::rate(p["net"].as_f64()),
            ]
        })
        .collect();
    println!("{}", c::table(&rows, &["PID", "PROCESS", "CPU", "MEM", "NET"]));
}

pub fn cmd_ps(args: &[String]) -> R {
    let a = Args::parse(args, &[("machine", "m", true), ("json", "", false)])?;
    let cl = client::connect(true)?;
    let local: Value = cl.req("GET", "/api/processes", None)?;
    let machines: Value = cl.req("GET", "/api/machines", None).unwrap_or(Value::Null);
    let mut groups: Vec<(String, Vec<Value>, Option<String>)> = vec![(
        machines["local"]["name"].as_str().unwrap_or("this mac").to_string(),
        local["processes"].as_array().cloned().unwrap_or_default(),
        None,
    )];
    for m in machines["machines"].as_array().into_iter().flatten() {
        groups.push((
            m["name"].as_str().unwrap_or("?").to_string(),
            m["processes"].as_array().cloned().unwrap_or_default(),
            (m["status"] != "online").then(|| m["status"].as_str().unwrap_or("offline").to_string()),
        ));
    }
    if let Some(want) = a.get("machine") {
        groups.retain(|g| g.0 == want);
        if groups.is_empty() {
            return Err(format!("no machine named {want:?}"));
        }
    }
    if a.has("json") {
        let v: serde_json::Map<String, Value> = groups.into_iter().map(|(n, p, _)| (n, Value::Array(p))).collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(());
    }
    let t = &local["thresholds"];
    let multi = groups.len() > 1;
    for (name, procs, status) in &groups {
        if multi {
            println!("{}{}", c::bold(&format!("── {name} ")), status.as_deref().map(|s| c::red(&format!("({s})"))).unwrap_or_default());
        }
        if procs.is_empty() {
            println!("{}", c::dim("  nothing above the thresholds"));
        } else {
            print_procs(procs);
        }
    }
    if t["enabled"] == json!(false) {
        println!("{}", c::dim("  Process breakdown is off (settings → processes.enabled)."));
    } else {
        println!(
            "{}",
            c::dim(&format!(
                "  Showing processes over CPU {}% · mem {}% · net {}/s (max {}); service processes are in `lstack ls`.",
                t["cpu"].as_f64().unwrap_or(10.0),
                t["mem"].as_f64().unwrap_or(10.0),
                c::bytes(t["net"].as_f64()),
                t["max"].as_u64().unwrap_or(15)
            ))
        );
    }
    Ok(())
}

// ------------------------------------------------------------------ heartbeats

fn hb_line(v: &Value) -> String {
    if v.is_null() {
        return c::dim("no ping yet");
    }
    let when = v["lastPing"].as_i64().map(c::local_time).unwrap_or_default();
    let kind = v["kind"].as_str().unwrap_or("");
    if v["ok"].as_bool().unwrap_or(false) {
        c::green(&format!("✓ {kind} ping at {when}"))
    } else {
        c::red(&format!("✗ {kind} ping failed at {when}: {}", v["error"].as_str().unwrap_or("?")))
    }
}

pub fn cmd_heartbeat(args: &[String]) -> R {
    let a = Args::parse(
        args,
        &[("url", "", true), ("slug", "", true), ("interval", "i", true), ("base-url", "", true), ("log-lines", "", true), ("no-start", "", false), ("no-fail", "", false), ("no-logs", "", false), ("stdin", "", false), ("json", "", false)],
    )?;
    let cl = client::connect(true)?;
    let sub = a.pos.first().map(String::as_str).unwrap_or("status");
    let interval = a.get("interval").map(|s| s.trim_end_matches('s').parse::<f64>().map_err(|_| format!("invalid interval {s} (seconds)"))).transpose()?;
    let hb_body = |a: &Args| {
        let mut hb = json!({});
        if let Some(u) = a.get("url") {
            hb["url"] = json!(u);
        }
        if let Some(s) = a.get("slug") {
            hb["slug"] = json!(s);
        }
        if let Some(i) = interval {
            hb["interval"] = json!(i);
        }
        if a.has("no-start") {
            hb["start"] = json!(false);
        }
        if a.has("no-fail") {
            hb["fail"] = json!(false);
        }
        if a.has("no-logs") {
            hb["logs"] = json!(false);
        }
        hb
    };
    match sub {
        "status" | "ls" => {
            let st: Value = cl.req("GET", "/api/settings", None)?;
            let hc = &st["healthchecks"];
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(hc).unwrap_or_default());
                return Ok(());
            }
            println!("{}  {}", c::dim(&format!("{:<11}", "ping key")), if hc["hasPingKey"].as_bool().unwrap_or(false) { c::green("set ✓") } else { c::dim("not set (lstack heartbeat key)") });
            println!("{}  {}", c::dim(&format!("{:<11}", "base url")), hc["baseUrl"].as_str().unwrap_or(""));
            let m = &hc["machine"];
            let mdesc = if m.is_null() {
                c::dim("off (lstack heartbeat machine <url|slug>)")
            } else {
                format!("{} every {}s — {}", m["url"].as_str().or(m["slug"].as_str()).unwrap_or("<host>-lstack"), m["interval"].as_f64().unwrap_or(60.0), hb_line(&hc["machineStatus"]))
            };
            println!("{}  {mdesc}", c::dim(&format!("{:<11}", "machine")));
            let l = cl.list()?;
            let rows: Vec<Vec<String>> = l
                .services
                .iter()
                .filter_map(|s| {
                    let hb = s.config.heartbeat.as_ref()?;
                    let target = hb.url.clone().map(crate::scrub::scrub_string).or(hb.slug.clone()).unwrap_or_else(|| s.name.clone());
                    let on = hb.enabled != Some(false);
                    Some(vec![
                        c::bold(&s.name),
                        if on { target } else { c::dim(&format!("{target} (off)")) },
                        format!("{}s", hb.interval.unwrap_or(60.0)),
                        hb_line(&serde_json::to_value(&s.heartbeat).unwrap_or_default()),
                    ])
                })
                .collect();
            if rows.is_empty() {
                println!("{}", c::dim("\n  No service heartbeats. Add one: lstack heartbeat set <service> [--url <ping-url> | --slug <slug>]"));
            } else {
                println!("\n{}", c::table(&rows, &["SERVICE", "CHECK", "EVERY", "LAST PING"]));
            }
            Ok(())
        }
        "key" => {
            let key = if a.has("stdin") || !std::io::stdin().is_terminal() {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
                s.trim().to_string()
            } else {
                read_password("healthchecks.io project ping key (hidden): ", false)?
            };
            let _: Value = cl.req("PUT", "/api/settings", Some(json!({ "healthchecks": { "pingKey": key } })))?;
            println!("{} ping key {}", c::green("✓"), if key.is_empty() { "removed" } else { "saved (in the encrypted secret store)" });
            Ok(())
        }
        "config" => {
            let mut hc = json!({});
            if let Some(b) = a.get("base-url") {
                hc["baseUrl"] = json!(b);
            }
            if let Some(n) = a.get("log-lines") {
                hc["logLines"] = json!(n.parse::<u32>().map_err(|_| "--log-lines must be a number")?);
            }
            let _: Value = cl.req("PUT", "/api/settings", Some(json!({ "healthchecks": hc })))?;
            println!("{} saved", c::green("✓"));
            Ok(())
        }
        "machine" => {
            let target = a.pos.get(1).map(String::as_str);
            let m = match target {
                Some("off" | "disable") => Value::Null,
                Some(t) if t.starts_with("http://") || t.starts_with("https://") => json!({ "url": t, "interval": interval.unwrap_or(60.0) }),
                Some(t) => json!({ "slug": t, "interval": interval.unwrap_or(60.0) }),
                None => json!({ "interval": interval.unwrap_or(60.0) }),
            };
            let _: Value = cl.req("PUT", "/api/settings", Some(json!({ "healthchecks": { "machine": m } })))?;
            println!("{} machine heartbeat {}", c::green("✓"), if m.is_null() { "off" } else { "on" });
            Ok(())
        }
        "set" | "on" => {
            let n = a.pos.get(1).ok_or("usage: lstack heartbeat set <service> [--url <ping-url> | --slug <slug>] [--interval 60]")?;
            let mut hb = serde_json::to_value(cl.get(n)?.config.heartbeat).ok().filter(Value::is_object).unwrap_or(json!({}));
            for (k, v) in hb_body(&a).as_object().cloned().unwrap_or_default() {
                hb[k] = v;
            }
            if a.has("url") {
                hb.as_object_mut().map(|o| o.remove("slug"));
            } else if a.has("slug") {
                hb.as_object_mut().map(|o| o.remove("url"));
            }
            hb["enabled"] = json!(true);
            let s = cl.update(n, json!({ "heartbeat": hb }))?;
            println!("{} {} pings {}", c::green("✓"), c::bold(&s.name), s.config.heartbeat.as_ref().and_then(|h| h.url.clone().map(crate::scrub::scrub_string).or(h.slug.clone())).unwrap_or_else(|| format!("slug {}", s.name)));
            Ok(())
        }
        "off" | "rm" => {
            let n = a.pos.get(1).ok_or("usage: lstack heartbeat off <service>")?;
            cl.update(n, json!({ "heartbeat": null }))?;
            println!("{} {n} heartbeat removed", c::green("✓"));
            Ok(())
        }
        "test" => {
            let body = a.pos.get(1).map(|n| json!({ "name": n })).unwrap_or(json!({}));
            let v: Value = cl.req("POST", "/api/heartbeat/test", Some(body))?;
            println!("{}", hb_line(&v));
            Ok(())
        }
        other => Err(format!("unknown heartbeat command {other:?} (status, key, config, machine, set, off, test)")),
    }
}

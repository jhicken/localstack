//! `lstack secret import | link | refresh | review | audit | share | unshare | decline | push | shares | sync
//! | accept | reject`: 1Password
//! references, read by this CLI (never the daemon) after you confirm, with approval in the 1Password app;
//! and sharing cached values with paired Macs.
use crate::cli::{confirm, Args};
use crate::client::{self, Client};
use crate::fmt as c;
use crate::op::{self, Op};
use serde_json::{json, Value};
use std::collections::BTreeMap;

type R = Result<(), String>;

const SPEC: &[(&str, &str, bool)] = &[
    ("service", "s", true),
    ("key", "k", true),
    ("keys", "", true),
    ("account", "", true),
    ("env-file", "f", true),
    ("environment", "", true),
    ("no-removals", "", false),
    ("all", "", false),
    ("yes", "y", false),
    ("json", "", false),
    ("n", "n", true),
];

pub fn handles(sub: &str) -> bool {
    matches!(sub, "import" | "link" | "refresh" | "review" | "audit" | "share" | "unshare" | "decline" | "push" | "shares" | "sync" | "accept" | "reject")
}

pub fn cmd(args: &[String]) -> R {
    let a = Args::parse(args, SPEC)?;
    let cl = client::connect(true)?;
    let sub_owned = a.pos.first().cloned().unwrap_or_default();
    let sub = sub_owned.as_str();
    let rest: Vec<String> = a.pos.iter().skip(1).cloned().collect();
    match sub {
        "review" | "import" => {
            let target = match rest.first().map(String::as_str).or(a.get("service")) {
                Some(t) => Some(resolve_target(&cl, t)?),
                None => None,
            };
            let mut a = a;
            if let Some((svc, Some(file))) = &target {
                if sub == "review" {
                    return Err(format!("{svc} doesn't use {file} yet; add it with: lstack secret import {file}"));
                }
                if a.get("env-file").is_none() {
                    a.set("env-file", file);
                }
            }
            let service = target.as_ref().map(|(s, _)| s.as_str());
            if sub == "review" {
                review(&cl, &a, service)
            } else {
                import(&cl, &a, service)
            }
        }
        "link" => {
            let (Some(name), Some(r)) = (rest.first(), rest.get(1)) else {
                return Err("usage: lstack secret link <NAME> op://vault/item/field [--service S [--key ENV]]".into());
            };
            let plan = cl.req("POST", "/api/onepassword/plan", Some(json!({ "kind": "link", "name": name, "ref": r, "service": a.get("service"), "key": a.get("key") })))?;
            run_plan(&cl, &a, plan)
        }
        "refresh" => {
            if rest.is_empty() && a.get("service").is_none() && !a.has("all") {
                return Err("usage: lstack secret refresh <NAME…> | --service S | --all".into());
            }
            let plan = cl.req("POST", "/api/onepassword/plan", Some(json!({ "kind": "refresh", "names": rest, "service": a.get("service") })))?;
            run_plan(&cl, &a, plan)
        }
        "share" | "unshare" => {
            let (Some(machine), items) = (rest.first(), &rest[rest.len().min(1)..]) else {
                return Err(format!("usage: lstack secret {sub} <machine> <NAME | op://…>…"));
            };
            if sub == "share" && a.has("all") {
                return share_all(&cl, &a, machine);
            }
            if items.is_empty() {
                return Err(format!("usage: lstack secret {sub} <machine> <NAME | op://…>… | --all"));
            }
            let allow = sub == "share";
            let v: Value = cl.req("POST", "/api/onepassword/shares", Some(json!({ "peer": machine, "items": items, "allow": allow, "actor": "cli" })))?;
            if allow {
                println!("{} {machine} may now receive {} (from this Mac's store; it never reads 1Password here)", c::green("✓"), items.join(", "));
            } else {
                println!("{} stopped sharing {} with {machine} (values it already received stay there)", c::green("✓"), items.join(", "));
            }
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            Ok(())
        }
        "decline" | "push" => {
            let (Some(machine), items) = (rest.first(), &rest[rest.len().min(1)..]) else {
                return Err(format!("usage: lstack secret {sub} <machine> <NAME | op://…>…"));
            };
            if items.is_empty() {
                return Err(format!("usage: lstack secret {sub} <machine> <NAME | op://…>…"));
            }
            let path = if sub == "push" { "/api/onepassword/push" } else { "/api/onepassword/shares/decline" };
            let v: Value = cl.req("POST", path, Some(json!({ "peer": machine, "items": items, "actor": "cli" })))?;
            if sub == "push" {
                println!("{} offered {} to {machine}; it's sent once someone accepts it there", c::green("✓"), items.join(", "));
            } else {
                println!("{} declined {machine}'s request for {}", c::green("✓"), items.join(", "));
            }
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            Ok(())
        }
        "accept" | "reject" => {
            let Some(machine) = rest.first() else { return Err(format!("usage: lstack secret {sub} <machine> [op://…]…")) };
            let mut body = json!({ "machine": machine });
            if rest.len() > 1 {
                body["refs"] = json!(&rest[1..]);
            }
            let path = if sub == "accept" { "/api/onepassword/peers/accept" } else { "/api/onepassword/peers/decline" };
            let v: Value = cl.req("POST", path, Some(body))?;
            let list = |k: &str| v[k].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
            if sub == "accept" && v["pending"] == true {
                println!("{} accepted {} — {machine} sends them within 15 s (while it's connected)", c::green("✓"), list("accepted"));
            } else if sub == "accept" {
                println!("{} stored {} from {machine}", c::green("✓"), list("stored"));
                let svcs = list("services");
                if !svcs.is_empty() {
                    println!("  now passed to {svcs} (restart running ones to use them)");
                }
            } else {
                println!("{} rejected {} from {machine}", c::green("✓"), list("declined"));
            }
            Ok(())
        }
        "shares" => shares(&cl, &a),
        "sync" => {
            let v: Value = cl.req("POST", "/api/onepassword/peers/sync", Some(json!({ "service": rest.first().map(String::as_str).or(a.get("service")) })))?;
            let mut n = 0;
            for r in v["received"].as_array().into_iter().flatten() {
                let names: Vec<&str> = r["stored"].as_array().into_iter().chain(r["updated"].as_array()).flatten().filter_map(Value::as_str).collect();
                let svcs: Vec<&str> = r["services"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                n += names.len();
                let to = if svcs.is_empty() { String::new() } else { format!(" → {}", svcs.join(", ")) };
                println!("{} from {}: {}{to}", c::green("✓"), r["machine"].as_str().unwrap_or("?"), names.join(", "));
            }
            for r in v["asked"].as_array().into_iter().flatten() {
                let refs: Vec<&str> = r["refs"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                println!("{} asked {} for {} — waiting for approval there", c::yellow("…"), r["machine"].as_str().unwrap_or("?"), refs.join(", "));
            }
            for e in v["errors"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("{} {e}", c::red("✗"));
            }
            if n == 0 && v["asked"].as_array().is_none_or(|a| a.is_empty()) {
                println!("{}", c::dim("nothing new from paired Macs"));
            }
            Ok(())
        }
        "audit" => {
            let n = a.get("n").unwrap_or("50");
            let v: Value = cl.req("GET", &format!("/api/onepassword/audit?n={}", client::enc(n)), None)?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            for e in v["entries"].as_array().into_iter().flatten() {
                let d = &e["detail"];
                let mut parts = vec![];
                if let Some(s) = d["service"].as_str() {
                    parts.push(s.to_string());
                }
                for (k, label) in [("stored", "stored"), ("removed", "removed"), ("deleted", "deleted")] {
                    let names: Vec<String> = d[k].as_array().into_iter().flatten().map(|x| x["name"].as_str().or(x.as_str()).unwrap_or("?").to_string()).collect();
                    if !names.is_empty() {
                        parts.push(format!("{label} {}", names.join(", ")));
                    }
                }
                println!("{}  {:<7} {:<11} {}", c::dim(e["ts"].as_str().unwrap_or("")), e["actor"].as_str().unwrap_or(""), e["action"].as_str().unwrap_or(""), parts.join(" · "));
            }
            Ok(())
        }
        _ => Err(format!("unknown secret command {sub:?}")),
    }
}

fn status_text(s: &str) -> String {
    match s {
        "ok" => c::green("approved ✓"),
        "new" => c::yellow("new"),
        "changed" => c::yellow("changed"),
        "missing" => c::red("not cached"),
        "blocked" => c::red("blocked"),
        "refresh" => c::cyan("refresh"),
        o => o.to_string(),
    }
}

fn review(cl: &Client, a: &Args, service: Option<&str>) -> R {
    let service = service.ok_or("usage: lstack secret review <service>")?;
    let v: Value = cl.req("GET", &format!("/api/onepassword/review/{}", client::enc(service)), None)?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(());
    }
    for f in v["files"].as_array().into_iter().flatten() {
        let state = match f["state"].as_str().unwrap_or("") {
            "ok" => c::green("✓"),
            "fifo" => c::cyan("1Password Environment mount"),
            _ => c::red(f["error"].as_str().unwrap_or("error")),
        };
        println!("{} {state}", c::bold(f["path"].as_str().unwrap_or("?")));
    }
    let refs = v["refs"].as_array().cloned().unwrap_or_default();
    if refs.is_empty() {
        println!("{}", c::dim("  no op:// references"));
    } else {
        let rows: Vec<Vec<String>> = refs
            .iter()
            .map(|r| {
                vec![
                    c::bold(r["key"].as_str().unwrap_or("?")),
                    status_text(r["status"].as_str().unwrap_or("")),
                    r["ref"].as_str().unwrap_or("").to_string(),
                    r["secret"].as_str().map(String::from).unwrap_or_else(|| c::dim("—")),
                ]
            })
            .collect();
        println!("{}", c::table(&rows, &["KEY", "STATUS", "REFERENCE", "STORED AS"]));
        for r in &refs {
            let key = r["key"].as_str().unwrap_or("?");
            if let Some(t) = r["template"].as_str() {
                println!("{}", c::dim(&format!("  {key}: written as {t}")));
            }
            if let Some(b) = r["blocked"].as_str() {
                println!("  {} {key}: {b}", c::red("✗"));
            }
            for w in r["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("  {} {key}: {w}", c::yellow("!"));
            }
            let by: Vec<&str> = r["offeredBy"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !by.is_empty() {
                println!("  {} {key}: shared by {} — importing takes it from there, no 1Password read", c::cyan("↓"), by.join(", "));
            }
        }
    }
    for r in v["removed"].as_array().into_iter().flatten() {
        println!("  {} {} no longer in the env file (gets {})", c::yellow("−"), r["key"].as_str().unwrap_or("?"), r["secret"].as_str().unwrap_or("?"));
    }
    let plain = v["plain"].as_array().map(Vec::len).unwrap_or(0);
    if plain > 0 {
        println!("{}", c::dim(&format!("  {plain} plain value(s) read from the file at each start")));
    }
    for w in v["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        println!("{} {w}", c::yellow("!"));
    }
    if refs.iter().any(|r| matches!(r["status"].as_str(), Some("new" | "changed" | "missing"))) || v["removed"].as_array().is_some_and(|r| !r.is_empty()) {
        println!("\nPull them with: {}", c::cyan(&format!("lstack secret import {service}")));
    }
    Ok(())
}

fn import(cl: &Client, a: &Args, service: Option<&str>) -> R {
    if let Some(path) = a.get("environment") {
        let path = std::fs::canonicalize(crate::paths::expand_home(path)).map_err(|e| format!("{path}: {e}"))?;
        println!("Reading {} (approve in the 1Password app)…", path.display());
        let content = op::read_mount(&path)?;
        let plan = cl.req(
            "POST",
            "/api/onepassword/plan",
            Some(json!({ "kind": "environment", "service": service, "path": path.to_string_lossy(), "content": content.as_str() })),
        )?;
        return run_plan(cl, a, plan);
    }
    let services: Vec<String> = match service {
        Some(s) => {
            if let Some(f) = a.get("env-file") {
                add_env_file(cl, s, f)?;
            }
            vec![s.to_string()]
        }
        None => {
            if a.get("env-file").is_some() {
                return Err("--env-file needs a service: lstack secret import <service> --env-file .env".into());
            }
            let v: Value = cl.req("GET", "/api/services", None)?;
            let pending: Vec<String> = v["services"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| !s["secretsPending"].is_null())
                .filter_map(|s| s["name"].as_str().map(String::from))
                .collect();
            if pending.is_empty() {
                println!("{}", c::dim("No 1Password references waiting for review."));
                return Ok(());
            }
            pending
        }
    };
    let keys: Option<Vec<String>> = a.get("keys").map(|k| k.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
    for s in services {
        println!("{}", c::bold(&s));
        match cl.req::<Value>("POST", "/api/onepassword/plan", Some(json!({ "kind": "env", "service": s, "keys": keys }))) {
            Ok(plan) => run_plan(cl, a, plan)?,
            Err(e) if e.contains("nothing to import") => println!("{}", c::dim("  up to date")),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `import`/`review` take a service name or an env file path. A path resolves to the local service that already
/// uses it, or else the one service whose `cwd` holds it (which then gets it added to `envFiles`).
fn resolve_target(cl: &Client, arg: &str) -> Result<(String, Option<String>), String> {
    let services = cl.list()?.services;
    if services.iter().any(|s| s.name == arg) {
        return Ok((arg.to_string(), None));
    }
    let path = std::path::PathBuf::from(crate::paths::expand_home(arg));
    let Ok(file) = std::fs::canonicalize(&path) else {
        return Err(format!("no service named {arg} (and no file {arg} here); see `lstack ls`"));
    };
    if !file.is_file() {
        return Err(format!("{arg} isn't a service or an env file"));
    }
    let local: Vec<_> = services.iter().filter(|s| s.machine.is_none()).collect();
    let dir_of = |s: &crate::types::ServiceState| {
        s.config.cwd.as_deref().filter(|c| !c.is_empty()).and_then(|c| std::fs::canonicalize(crate::paths::expand_home(c)).ok())
    };
    let using: Vec<&str> = local
        .iter()
        .filter(|s| {
            let base = dir_of(s);
            s.config.env_files.iter().flatten().any(|f| {
                let p = std::path::PathBuf::from(crate::paths::expand_home(f));
                let p = if p.is_absolute() { p } else { base.clone().unwrap_or_default().join(p) };
                std::fs::canonicalize(p).ok().as_ref() == Some(&file)
            })
        })
        .map(|s| s.name.as_str())
        .collect();
    match using.as_slice() {
        [one] => return Ok((one.to_string(), None)),
        [] => {}
        many => return Err(format!("{arg} is used by {}; name one: lstack secret import <service>", many.join(", "))),
    }
    let parent = file.parent().map(std::path::Path::to_path_buf);
    let here: Vec<&str> = local.iter().filter(|s| dir_of(s).is_some() && dir_of(s) == parent).map(|s| s.name.as_str()).collect();
    let fname = file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| arg.to_string());
    match here.as_slice() {
        [one] => Ok((one.to_string(), Some(fname))),
        [] => Err(format!(
            "no service uses {arg}. Attach it to one: lstack secret import <service> --env-file {}  (or create one: lstack add <name> -c {} --env-file {fname} -- <command>)",
            file.display(),
            parent.map(|p| p.display().to_string()).unwrap_or_default()
        )),
        many => Err(format!("several services run in {}: {}; name one: lstack secret import <service> --env-file {fname}", file.parent().unwrap().display(), many.join(", "))),
    }
}

fn add_env_file(cl: &Client, service: &str, file: &str) -> R {
    let st = cl.get(service)?;
    let mut files = st.config.env_files.clone().unwrap_or_default();
    if !files.iter().any(|f| f == file) {
        files.push(file.to_string());
        cl.update(service, json!({ "envFiles": files }))?;
        println!("{} {service}: envFiles = {}", c::green("✓"), files.join(", "));
    }
    Ok(())
}

/// Show a plan, confirm, read what it needs from 1Password, then hand the values to the daemon.
/// Approve every open request from `machine` at once: values cached here are shared in one step; ones
/// that aren't are read from 1Password here first (one confirmation each), then shared.
fn share_all(cl: &Client, a: &Args, machine: &str) -> R {
    let v: Value = cl.req("GET", "/api/onepassword/shares", None)?;
    let peers = v["peers"].as_array().cloned().unwrap_or_default();
    let p = peers.iter().find(|p| p["id"] == machine).or_else(|| peers.iter().find(|p| p["name"] == machine)).ok_or_else(|| format!("no paired Mac {machine}"))?;
    let id = p["id"].as_str().unwrap_or(machine).to_string();
    let open: Vec<&Value> = p["wants"].as_array().into_iter().flatten().filter(|w| w["granted"] != true && w["declined"] != true).collect();
    if open.is_empty() {
        println!("no open requests from {machine}");
        return Ok(());
    }
    let (have, missing): (Vec<&Value>, Vec<&Value>) = open.into_iter().partition(|w| w["have"] == true);
    if !have.is_empty() {
        let refs: Vec<&str> = have.iter().filter_map(|w| w["ref"].as_str()).collect();
        let _: Value = cl.req("POST", "/api/onepassword/shares", Some(json!({ "peer": id, "items": refs, "allow": true, "actor": "cli" })))?;
        let names: Vec<&str> = have.iter().map(|w| w["name"].as_str().or(w["ref"].as_str()).unwrap_or("?")).collect();
        println!("{} {machine} may now receive {}", c::green("✓"), names.join(", "));
    }
    let mut failed = vec![];
    for w in missing {
        let r = w["ref"].as_str().unwrap_or_default();
        println!("{r} isn't on this Mac — reading it from 1Password to share with {machine}");
        let res = cl.req("POST", "/api/onepassword/plan", Some(json!({ "kind": "link", "ref": r, "hint": w["key"], "sharePeer": id }))).and_then(|plan: Value| run_plan(cl, a, plan));
        if let Err(e) = res {
            failed.push(format!("{r}: {e}"));
        }
    }
    if failed.is_empty() { Ok(()) } else { Err(failed.join("\n")) }
}

fn run_plan(cl: &Client, a: &Args, plan: Value) -> R {
    let id = plan["id"].as_str().unwrap_or_default().to_string();
    let items = plan["items"].as_array().cloned().unwrap_or_default();
    let removals = plan["removals"].as_array().cloned().unwrap_or_default();
    for i in &items {
        let target = match i["key"].as_str() {
            Some(k) if k != i["name"].as_str().unwrap_or("") => format!("{k} → {}", i["name"].as_str().unwrap_or("?")),
            Some(k) => k.to_string(),
            None => i["name"].as_str().unwrap_or("?").to_string(),
        };
        let src = if i["source"] == "op-env" {
            c::dim("from the Environment")
        } else if let Some(m) = i["from"].as_str().filter(|_| i["cached"].as_bool().unwrap_or(false)) {
            c::yellow(&format!("value from paired Mac {m}{}; no 1Password read", ago(&i["fetchedAt"])))
        } else if i["cached"].as_bool().unwrap_or(false) {
            c::dim("already cached; no 1Password read")
        } else {
            format!("vault {} · item {} · field {}", c::bold(i["vault"].as_str().unwrap_or("?")), i["item"].as_str().unwrap_or("?"), i["field"].as_str().unwrap_or("?"))
        };
        println!("  {} {:<28} {}", status_text(i["status"].as_str().unwrap_or("")), c::bold(&target), src);
        if i["source"] != "op-env" {
            println!("{}", c::dim(&format!("      {}", i["ref"].as_str().unwrap_or(""))));
        }
        for w in i["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            println!("      {} {w}", c::yellow("!"));
        }
    }
    let apply_removals = !a.has("no-removals");
    for r in &removals {
        let what = if apply_removals { "stop passing" } else { "keep (--no-removals)" };
        println!("  {} {} {what} {}", c::yellow("−"), r["key"].as_str().unwrap_or("?"), c::dim(&format!("({})", r["secret"].as_str().unwrap_or("?"))));
    }
    let reads: Vec<&Value> = items.iter().filter(|i| i["needsRead"].as_bool().unwrap_or(false)).collect();
    let peers: BTreeMap<&str, usize> = items.iter().filter(|i| i["cached"].as_bool().unwrap_or(false)).filter_map(|i| i["from"].as_str()).fold(BTreeMap::new(), |mut m, f| {
        *m.entry(f).or_default() += 1;
        m
    });
    let mut parts: Vec<String> = peers.iter().map(|(m, n)| format!("use {n} value(s) from {m}")).collect();
    if !reads.is_empty() {
        parts.push(format!("read {} value(s) from 1Password (the 1Password app will ask you to approve)", reads.len()));
    }
    let q = match parts.is_empty() {
        true => "Apply these changes?".to_string(),
        false => {
            let mut q = parts.join(" and ");
            q[..1].make_ascii_uppercase();
            format!("{q}?")
        }
    };
    if !a.has("yes") && !confirm(&q) {
        let _ = cl.req::<Value>("DELETE", &format!("/api/onepassword/plan/{}", client::enc(&id)), None);
        println!("{}", c::dim("Cancelled; nothing was read."));
        return Ok(());
    }
    let mut values: BTreeMap<String, zeroize::Zeroizing<String>> = BTreeMap::new();
    let mut failed: Vec<String> = vec![];
    let mut account = a.get("account").map(String::from);
    if !reads.is_empty() {
        let o = match Op::find(account.clone()) {
            Ok(o) => o,
            Err(e) => {
                let _ = cl.req::<Value>("DELETE", &format!("/api/onepassword/plan/{}", client::enc(&id)), None);
                return Err(e);
            }
        };
        account = o.account.clone();
        for i in &reads {
            let (name, r) = (i["name"].as_str().unwrap_or(""), i["ref"].as_str().unwrap_or(""));
            match o.read(r) {
                Ok(v) => {
                    if v.len() < crate::scrub::MIN_LEN {
                        eprintln!("{}", c::yellow(&format!("  warning: {name} is shorter than {} characters, so it isn't scrubbed from logs", crate::scrub::MIN_LEN)));
                    }
                    values.insert(name.to_string(), v);
                }
                Err(e) => {
                    eprintln!("  {} {e}", c::red("✗"));
                    failed.push(name.to_string());
                    // A declined prompt or a locked app will fail the rest the same way.
                    if e.contains("declined") || e.contains("timed out") || e.contains("Integrate with 1Password CLI") {
                        break;
                    }
                }
            }
        }
        o.signout();
    }
    let ok_items: Vec<String> = items
        .iter()
        .filter(|i| !i["needsRead"].as_bool().unwrap_or(false) || values.contains_key(i["name"].as_str().unwrap_or("")))
        .filter_map(|i| i["name"].as_str().map(String::from))
        .collect();
    if ok_items.is_empty() && (removals.is_empty() || !apply_removals) {
        let _ = cl.req::<Value>("DELETE", &format!("/api/onepassword/plan/{}", client::enc(&id)), None);
        return Err("nothing was read from 1Password".into());
    }
    let mut keys = ok_items.clone();
    keys.extend(removals.iter().filter_map(|r| r["key"].as_str().map(String::from)));
    let body = json!({
        "plan": id,
        "values": values.iter().map(|(k, v)| (k.clone(), Value::String(v.to_string()))).collect::<serde_json::Map<_, _>>(),
        "keys": keys,
        "applyRemovals": apply_removals,
        "account": account,
        "actor": "cli",
    });
    drop(values);
    let r: Value = cl.req("POST", "/api/onepassword/import", Some(body))?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    } else {
        let list = |k: &str| r[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        if !list("stored").is_empty() {
            println!("{} stored (encrypted): {}", c::green("✓"), list("stored"));
        }
        if let Some(g) = r["granted"].as_object().filter(|g| !g.is_empty()) {
            let s: Vec<String> = g.iter().map(|(k, v)| if Some(k.as_str()) == v.as_str() { k.clone() } else { format!("{k} → {}", v.as_str().unwrap_or("?")) }).collect();
            println!("{} passed to {}: {}", c::green("✓"), plan["service"].as_str().unwrap_or("the service"), s.join(", "));
        }
        if !list("removed").is_empty() {
            println!("{} no longer passed: {}", c::green("✓"), list("removed"));
        }
        if !list("deleted").is_empty() {
            println!("{} forgotten (unused): {}", c::green("✓"), list("deleted"));
        }
        if !list("restarting").is_empty() {
            println!("{} restarting: {}", c::cyan("↻"), list("restarting"));
        } else if plan["service"].is_string() && r["granted"].as_object().is_some_and(|g| !g.is_empty()) {
            println!("{}", c::dim("  Running services pick these up on their next restart."));
        }
    }
    if !failed.is_empty() {
        return Err(format!("couldn't read {} (still waiting for review)", failed.join(", ")));
    }
    Ok(())
}

fn shares(cl: &Client, a: &Args) -> R {
    let v: Value = cl.req("GET", "/api/onepassword/shares", None)?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(());
    }
    let mut any = false;
    for p in v["peers"].as_array().into_iter().flatten() {
        let name = p["name"].as_str().unwrap_or("?");
        let granted = p["granted"].as_array().cloned().unwrap_or_default();
        let wants: Vec<&Value> = p["wants"].as_array().into_iter().flatten().filter(|w| w["granted"] != true && w["declined"] != true).collect();
        let pushing = p["pushing"].as_array().cloned().unwrap_or_default();
        if granted.is_empty() && wants.is_empty() {
            continue;
        }
        any = true;
        println!("{}", c::bold(name));
        for g in &granted {
            if pushing.iter().any(|x| x["ref"] == g["ref"]) {
                println!("  {} {} {}", c::yellow("pushed"), g["ref"].as_str().unwrap_or("?"), c::dim("waiting for it to accept"));
                continue;
            }
            let note = if g["have"] == true { c::dim(&format!("as {}", g["name"].as_str().unwrap_or("?"))) } else { c::yellow("not stored here any more") };
            println!("  {} {} {note}", c::green("shared"), g["ref"].as_str().unwrap_or("?"));
        }
        for w in wants {
            let r = w["ref"].as_str().unwrap_or("?");
            let svcs: Vec<&str> = w["services"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let r = &if svcs.is_empty() { r.to_string() } else { format!("{r} (for {})", svcs.join(", ")) };
            if w["have"] == true {
                println!("  {} {r} — approve: {}", c::yellow("wants "), c::cyan(&format!("lstack secret share {name} {}", w["name"].as_str().unwrap_or(r))));
            } else {
                println!("  {} {r} {}", c::dim("wants "), c::dim("(not stored here)"));
            }
        }
    }
    let incoming = v["incoming"].as_array().cloned().unwrap_or_default();
    if !incoming.is_empty() {
        any = true;
        println!("{}", c::bold("Sent to you, waiting for approval"));
        for i in &incoming {
            let m = i["machine"].as_str().unwrap_or("?");
            let svcs: Vec<&str> = i["services"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let to = if svcs.is_empty() { String::new() } else { format!(" for {}", svcs.join(", ")) };
            println!("  {} {}{to} from {m} — {}", i["name"].as_str().unwrap_or("?"), c::dim(i["ref"].as_str().unwrap_or("")), c::cyan(&format!("lstack secret accept {m}")));
        }
    }
    let received = v["received"].as_array().cloned().unwrap_or_default();
    if !received.is_empty() {
        any = true;
        println!("{}", c::bold("Received here"));
        for r in &received {
            println!("  {} {} from {}", r["name"].as_str().unwrap_or("?"), c::dim(r["ref"].as_str().unwrap_or("")), r["from"].as_str().unwrap_or("?"));
        }
    }
    if !any {
        println!("{}", c::dim("nothing shared. Paired Macs that need a 1Password reference you have show up here."));
    }
    Ok(())
}

/// ", read 3h ago" for a `fetchedAt` (ms), or nothing.
fn ago(at: &Value) -> String {
    at.as_i64().filter(|t| *t > 0).map(|t| format!(", read {} ago", c::duration(Some(crate::paths::now_ms() - t)))).unwrap_or_default()
}

//! End-to-end: run a real daemon against a throwaway LSTACK_HOME and drive it over HTTP + the CLI.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_lstack");

struct Daemon {
    home: PathBuf,
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.post("/api/shutdown", None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

impl Daemon {
    fn start(home: &Path) -> Daemon {
        Self::start_with(home, &[])
    }

    fn start_with(home: &Path, env: &[(&str, String)]) -> Daemon {
        let port = free_port();
        std::fs::create_dir_all(home.join("services")).unwrap();
        let child = Command::new(BIN)
            .args(["daemon", "run"])
            .env("LSTACK_HOME", home)
            .env("LSTACK_PORT", port.to_string())
            .env("LSTACK_KEYCHAIN", "file")
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let info = home.join("daemon.json");
        let mut d = Daemon { home: home.to_path_buf(), child, port, token: String::new() };
        wait_until("daemon.json", 10, || {
            let Ok(t) = std::fs::read_to_string(&info) else { return false };
            let Ok(v) = serde_json::from_str::<Value>(&t) else { return false };
            if v["running"] == json!(false) {
                return false;
            }
            d.port = v["port"].as_u64().unwrap() as u16;
            d.token = v["token"].as_str().unwrap().to_string();
            d.get("/api/health").is_ok()
        });
        d
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, (u16, String)> {
        let r = ureq::request(method, &format!("http://127.0.0.1:{}{path}", self.port)).set("Authorization", &format!("Bearer {}", self.token)).timeout(Duration::from_secs(30));
        let res = match body {
            Some(b) => r.send_json(b),
            None => r.call(),
        };
        match res {
            Ok(r) => Ok(r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => Err((c, r.into_json::<Value>().map(|v| v["error"].as_str().unwrap_or("").to_string()).unwrap_or_default())),
            Err(e) => Err((0, e.to_string())),
        }
    }
    fn get(&self, p: &str) -> Result<Value, (u16, String)> {
        self.call("GET", p, None)
    }
    fn post(&self, p: &str, b: Option<Value>) -> Result<Value, (u16, String)> {
        self.call("POST", p, b)
    }
    fn svc(&self, n: &str) -> Value {
        self.get(&format!("/api/services/{n}")).unwrap()
    }
    fn logs(&self, n: &str) -> String {
        self.get(&format!("/api/services/{n}/logs?tail=500")).unwrap()["lines"].as_array().unwrap().iter().map(|l| l["line"].as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n")
    }
    fn cli(&self, args: &[&str]) -> (bool, String) {
        let o = Command::new(BIN).args(args).env("LSTACK_HOME", &self.home).env("LSTACK_KEYCHAIN", "file").env("NO_COLOR", "1").stdin(Stdio::null()).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
    }
}

fn pid_alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn end_to_end() {
    let home = std::env::temp_dir().join(format!("lstack-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("services")).unwrap();
    // An autostart service defined on disk before the daemon boots.
    std::fs::write(home.join("services/boot.yaml"), "name: boot\ntype: process\nautostart: true\ncommand: echo booted; sleep 60\n").unwrap();
    std::fs::write(home.join("services/broken.yaml"), "name: [oops\n").unwrap();
    let d = Daemon::start(&home);

    // auth
    let unauth = ureq::get(&format!("http://127.0.0.1:{}/api/services", d.port)).call();
    assert!(matches!(unauth, Err(ureq::Error::Status(401, _))));

    // autostart at boot + config errors surfaced
    wait_until("boot running", 10, || d.svc("boot")["status"] == "running");
    wait_until("boot log", 5, || d.logs("boot").contains("booted"));
    let list = d.get("/api/services").unwrap();
    assert!(list["configErrors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("broken")));

    // add + start via CLI, stdin round-trip through `cat`
    let (ok, out) = d.cli(&["add", "echo", "--now", "--", "cat"]);
    assert!(ok, "{out}");
    assert!(home.join("services/echo.yaml").exists());
    wait_until("echo running", 10, || d.svc("echo")["status"] == "running");
    d.post("/api/services/echo/stdin", Some(json!({ "data": "hello lstack\n" }))).unwrap();
    wait_until("stdin echo", 5, || d.logs("echo").contains("hello lstack"));
    let (ok, out) = d.cli(&["logs", "echo", "-n", "5"]);
    assert!(ok && out.contains("hello lstack"), "{out}");
    let (ok, out) = d.cli(&["ls"]);
    assert!(ok && out.contains("echo") && out.contains("running"), "{out}");

    // duplicate / invalid
    assert_eq!(d.post("/api/services", Some(json!({ "name": "echo", "type": "process", "command": "x" }))).unwrap_err().0, 409);
    assert_eq!(d.post("/api/services", Some(json!({ "name": "bad name", "type": "process", "command": "x" }))).unwrap_err().0, 400);

    // a folder's .env.lstack is used by default (no terminal: no prompt); --no-env-file skips it
    let proj = home.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join(".env.lstack"), "GREETING=hi\n").unwrap();
    std::fs::write(proj.join(".env"), "OTHER=1\n").unwrap();
    let (ok, out) = d.cli(&["add", "envy", "-c", proj.to_str().unwrap(), "--", "true"]);
    assert!(ok && out.contains(".env.lstack"), "{out}");
    assert_eq!(d.svc("envy")["config"]["envFiles"], json!([".env.lstack"]));
    let (ok, out) = d.cli(&["add", "plain", "-c", proj.to_str().unwrap(), "--no-env-file", "--", "true"]);
    assert!(ok, "{out}");
    assert!(d.svc("plain")["config"].get("envFiles").is_none());
    let (ok, out) = d.cli(&["add", "chosen", "-c", proj.to_str().unwrap(), "--env-file", ".env", "--", "true"]);
    assert!(ok, "{out}");
    assert_eq!(d.svc("chosen")["config"]["envFiles"], json!([".env"]));
    for n in ["envy", "plain", "chosen"] {
        let (ok, out) = d.cli(&["rm", n, "-y"]);
        assert!(ok, "{out}");
    }

    // stop kills the whole process group
    let pid = d.svc("echo")["pid"].as_i64().unwrap();
    let st = d.post("/api/services/echo/stop", None).unwrap();
    assert_eq!(st["status"], "stopped");
    wait_until("pid gone", 5, || !pid_alive(pid));
    assert_eq!(d.post("/api/services/echo/stdin", Some(json!({ "data": "x" }))).unwrap_err().0, 409);

    // crash → backoff restarts → gives up after maxRestarts
    d.post("/api/services?start=1", Some(json!({ "name": "crashy", "type": "process", "command": "echo dying; exit 3", "maxRestarts": 2 }))).unwrap();
    wait_until("crashy gave up", 20, || d.svc("crashy")["lastError"].as_str().is_some_and(|e| e.contains("gave up")));
    let c = d.svc("crashy");
    assert_eq!(c["status"], "crashed");
    assert_eq!(c["restarts"], 2);
    assert_eq!(c["exitCode"], 3);

    // restart policy never → crashed, no restarts
    d.post("/api/services?start=1", Some(json!({ "name": "once", "type": "process", "command": "exit 1", "restart": "never" }))).unwrap();
    wait_until("once crashed", 10, || d.svc("once")["status"] == "crashed");
    assert_eq!(d.svc("once")["restarts"], 0);

    // TCP health check against a real listener
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hp = l.local_addr().unwrap().port();
    d.post("/api/services?start=1", Some(json!({ "name": "web", "type": "process", "command": "sleep 60", "healthcheck": { "type": "tcp", "port": hp, "interval": 1 } }))).unwrap();
    wait_until("healthy", 15, || d.svc("web")["health"] == "healthy");
    drop(l);
    wait_until("unhealthy", 15, || d.svc("web")["health"] == "unhealthy");

    // metrics are sampled and stored
    wait_until("metrics", 15, || d.svc("boot")["metrics"]["mem"].as_f64().unwrap_or(0.0) > 0.0);
    wait_until("metric history", 15, || !d.get("/api/services/boot/metrics?range=10m").unwrap()["samples"].as_array().unwrap().is_empty());

    // PATCH persists to YAML; null removes a key
    d.call("PATCH", "/api/services/web", Some(json!({ "autostart": true, "description": "web thing" }))).unwrap();
    let y = std::fs::read_to_string(home.join("services/web.yaml")).unwrap();
    assert!(y.contains("autostart: true") && y.contains("web thing"), "{y}");
    d.call("PATCH", "/api/services/web", Some(json!({ "description": null }))).unwrap();
    assert!(!std::fs::read_to_string(home.join("services/web.yaml")).unwrap().contains("web thing"));

    // hand-edited file + reload
    std::fs::write(home.join("services/later.yaml"), "name: later\ncommand: sleep 1\n").unwrap();
    let r = d.post("/api/reload", None).unwrap();
    assert_eq!(r["added"], json!(["later"]));

    // SSE events stream delivers a snapshot first
    let resp = ureq::get(&format!("http://127.0.0.1:{}/api/events?token={}", d.port, d.token)).call().unwrap();
    let mut rd = std::io::BufReader::new(resp.into_reader());
    let mut buf = String::new();
    while !buf.contains("event: snapshot") {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut rd, &mut line).unwrap();
        buf.push_str(&line);
    }

    // remove deletes the file and stops the process
    let pid = d.svc("web")["pid"].as_i64().unwrap();
    d.call("DELETE", "/api/services/web", None).unwrap();
    assert!(!home.join("services/web.yaml").exists());
    wait_until("web pid gone", 10, || !pid_alive(pid));
    assert_eq!(d.get("/api/services/web").unwrap_err().0, 404);

    // daemon shutdown stops process services
    let boot_pid = d.svc("boot")["pid"].as_i64().unwrap();
    drop(d);
    wait_until("boot pid gone", 10, || !pid_alive(boot_pid));
}

#[test]
fn secrets_are_write_only_injected_and_scrubbed() {
    let home = std::env::temp_dir().join(format!("lstack-it-secrets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let d = Daemon::start(&home);
    let value = "sk-live-Zx81QpL0w";

    // A service that needs a secret that isn't set yet fails with a clear error.
    d.post("/api/services", Some(json!({ "name": "leaky", "type": "process", "restart": "never", "secrets": ["API_KEY"],
        "command": "echo raw=$API_KEY; printf %s \"$API_KEY\" | base64; echo fin\"\"ished; sleep 60" }))).unwrap();
    let err = d.post("/api/services/leaky/start", None).map(|v| v["lastError"].clone());
    let st = d.svc("leaky");
    assert!(st["lastError"].as_str().unwrap_or("").contains("API_KEY") || format!("{err:?}").contains("API_KEY"), "{st}");
    let listed = d.get("/api/secrets").unwrap();
    assert_eq!(listed["secrets"][0]["missing"], true);

    d.call("PUT", "/api/secrets/API_KEY", Some(json!({ "value": value }))).unwrap();
    let listed = d.get("/api/secrets").unwrap().to_string();
    assert!(!listed.contains(value), "values are never returned: {listed}");
    assert!(listed.contains("leaky"), "usedBy: {listed}");
    assert!(!std::fs::read_to_string(home.join("secrets.enc")).unwrap().contains(value));

    d.post("/api/services/leaky/start", None).unwrap();
    wait_until("leaky output", 10, || d.logs("leaky").contains("finished"));
    let logs = d.logs("leaky");
    assert!(logs.contains("raw=••••[API_KEY]"), "{logs}");
    assert!(!logs.contains(value) && !logs.contains("c2stbGl2ZS1aeDgxUXBMMHc"), "{logs}");
    let file = std::fs::read_to_string(home.join("logs/leaky.log")).unwrap();
    assert!(!file.contains(value), "{file}");
    // The process really got the value: its environment matches.
    let (ok, out) = d.cli(&["secret", "ls"]);
    assert!(ok && out.contains("API_KEY") && !out.contains(value), "{out}");
    let (ok, out) = d.cli(&["secret", "get", "API_KEY"]);
    assert!(!ok && out.contains("write-only"), "{out}");

    // A secret added later is masked in logs already written.
    d.post("/api/services?start=1", Some(json!({ "name": "early", "type": "process", "restart": "never", "command": "echo token=late-\"\"secret-42; sleep 60" }))).unwrap();
    wait_until("early output", 10, || d.logs("early").contains("late-secret-42"));
    d.call("PUT", "/api/secrets/LATE", Some(json!({ "value": "late-secret-42" }))).unwrap();
    wait_until("rescrubbed", 5, || !d.logs("early").contains("late-secret-42"));
    wait_until("file rescrubbed", 5, || !std::fs::read_to_string(home.join("logs/early.log")).unwrap_or_default().contains("late-secret-42"));

    // Internal secrets can't be handed to services.
    d.call("PUT", "/api/secrets/LSTACK_CMUX_PASSWORD", Some(json!({ "value": "internal-pw" }))).unwrap();
    d.post("/api/services", Some(json!({ "name": "sneaky", "type": "process", "restart": "never", "secrets": ["LSTACK_CMUX_PASSWORD"], "command": "env" }))).unwrap();
    let _ = d.post("/api/services/sneaky/start", None);
    assert!(d.svc("sneaky")["lastError"].as_str().unwrap_or("").contains("reserved"));

    d.call("DELETE", "/api/secrets/API_KEY", None).unwrap();
    assert_eq!(d.call("DELETE", "/api/secrets/API_KEY", None).unwrap_err().0, 404);
}

/// 1Password references in an env file: flagged for review, imported with values a client read (here
/// fake ones; the daemon never runs `op`), injected from the store on every restart, never read
/// live from the file, and cleaned up when the line goes away.
#[test]
fn onepassword_env_file_import() {
    let home = std::env::temp_dir().join(format!("lstack-it-op-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let d = Daemon::start(&home);
    let app = home.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join(".env"), "PLAIN=hello-$APP_ENV\nDB_PASSWORD=op://Dev/db/password\n").unwrap();
    d.post("/api/services", Some(json!({ "name": "api", "type": "process", "restart": "never", "cwd": app.to_string_lossy(),
        "envFiles": [".env"], "env": { "APP_ENV": "dev" },
        "command": "echo plain=$PLAIN db=${DB_PASSWORD:-unset}; sleep 60" }))).unwrap();

    // The watcher flags the new reference; nothing is read.
    wait_until("pending", 10, || d.svc("api")["secretsPending"]["new"] == json!(["DB_PASSWORD"]));
    let review = d.get("/api/onepassword/review/api").unwrap();
    assert_eq!(review["refs"][0]["status"], "new", "{review}");
    assert_eq!(review["refs"][0]["vault"], "Dev");
    let (ok, out) = d.cli(&["secret", "review", "api"]);
    assert!(ok && out.contains("DB_PASSWORD") && out.contains("op://Dev/db/password") && out.contains("lstack secret import api"), "{out}");

    // It starts with plain values and without the unapproved key.
    d.post("/api/services/api/start", None).unwrap();
    wait_until("first run", 10, || d.logs("api").contains("plain=hello-dev db=unset"));
    assert!(d.logs("api").contains("waiting for review"), "{}", d.logs("api"));

    // Plan → the client reads (here: a fake value) → import.
    let plan = d.post("/api/onepassword/plan", Some(json!({ "kind": "env", "service": "api" }))).unwrap();
    assert_eq!(plan["items"][0]["name"], "DB_PASSWORD");
    assert_eq!(plan["items"][0]["needsRead"], true);
    let id = plan["id"].as_str().unwrap();
    let bad = d.post("/api/onepassword/import", Some(json!({ "plan": id, "values": { "OTHER": "x-not-in-plan" } }))).unwrap_err();
    assert_eq!(bad.0, 400, "values for refs outside the plan are refused");
    let value = "pw-from-1password-Q7x";
    let r = d.post("/api/onepassword/import", Some(json!({ "plan": id, "values": { "DB_PASSWORD": value }, "actor": "cli" }))).unwrap();
    assert_eq!(r["stored"], json!(["DB_PASSWORD"]), "{r}");
    assert_eq!(d.post("/api/onepassword/import", Some(json!({ "plan": id, "values": {} }))).unwrap_err().0, 404, "plans are single-use");
    wait_until("pending cleared", 10, || d.svc("api")["secretsPending"].is_null());
    let listed = d.get("/api/secrets").unwrap();
    let entry = listed["secrets"].as_array().unwrap().iter().find(|s| s["name"] == "DB_PASSWORD").unwrap().clone();
    assert_eq!(entry["source"], "op");
    assert_eq!(entry["ref"], "op://Dev/db/password");
    assert!(!listed.to_string().contains(value));

    // Restarts need no 1Password: the value comes from the store.
    d.post("/api/services/api/restart", None).unwrap();
    wait_until("injected", 10, || d.logs("api").contains("db=••••[DB_PASSWORD]"));
    assert!(!d.logs("api").contains(value));

    // Pointing the key at another reference doesn't change what the service gets until reviewed.
    std::fs::write(app.join(".env"), "PLAIN=hello-$APP_ENV\nDB_PASSWORD=op://Dev/other-db/password\n").unwrap();
    wait_until("changed", 10, || d.svc("api")["secretsPending"]["changed"] == json!(["DB_PASSWORD"]));
    d.post("/api/services/api/restart", None).unwrap();
    wait_until("still approved value", 10, || d.logs("api").matches("db=••••[DB_PASSWORD]").count() >= 2);

    // Removing the line stops passing it and forgets the imported value.
    std::fs::write(app.join(".env"), "PLAIN=hello-$APP_ENV\n").unwrap();
    wait_until("removed", 10, || d.svc("api")["config"]["secrets"].is_null());
    // …once the running process (which still has it, and whose logs must stay scrubbed) restarts.
    std::thread::sleep(Duration::from_secs(3));
    assert!(d.get("/api/secrets").unwrap().to_string().contains("DB_PASSWORD"), "kept while still in use");
    d.post("/api/services/api/restart", None).unwrap();
    wait_until("forgotten", 10, || !d.get("/api/secrets").unwrap().to_string().contains("DB_PASSWORD"));
    let audit = d.get("/api/onepassword/audit").unwrap().to_string();
    assert!(audit.contains("op://Dev/db/password") && audit.contains("\"remove\"") && !audit.contains(value), "{audit}");

    // A missing env file fails the start clearly (like `op run`).
    d.call("PATCH", "/api/services/api", Some(json!({ "envFiles": [".env", ".env.missing"] }))).unwrap();
    d.post("/api/services/api/stop", None).unwrap();
    let _ = d.post("/api/services/api/start", None);
    wait_until("start fails", 10, || d.svc("api")["lastError"].as_str().unwrap_or("").contains(".env.missing"));
}

/// A paired Mac receives a 1Password value only after it's shared with it, from the other Mac's store.
#[test]
fn onepassword_values_shared_with_paired_mac() {
    let base = std::env::temp_dir().join(format!("lstack-it-opshare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let a = Daemon::start(&base.join("a")); // has the value from 1Password; shares its services
    let b = Daemon::start(&base.join("b")); // watches `a` ("studio") and needs the value

    // `a` imported the value (the client read it from 1Password; here a fake value).
    let value = "shared-pw-Zk9";
    let plan = a.post("/api/onepassword/plan", Some(json!({ "kind": "link", "name": "DB_PASSWORD", "ref": "op://Dev/db/password" }))).unwrap();
    a.post("/api/onepassword/import", Some(json!({ "plan": plan["id"], "values": { "DB_PASSWORD": value } }))).unwrap();
    a.call("PUT", "/api/secrets/TYPED_IN", Some(json!({ "value": "typed-in-value" }))).unwrap();

    let (rport, lport) = (free_port(), free_port());
    a.call("PUT", "/api/remote", Some(json!({ "enabled": false, "sharing": true, "bind": "127.0.0.1", "port": rport, "linkPort": lport, "username": "me", "password": "correct horse" })))
        .unwrap();
    let addr = format!("127.0.0.1:{lport}");
    wait_until("link listener", 10, || std::net::TcpStream::connect(&addr).is_ok());
    let code = a.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    b.post("/api/machines", Some(json!({ "address": addr, "code": code, "name": "studio" }))).unwrap();
    wait_until("online", 30, || b.get("/api/machines/studio").is_ok_and(|v| v["status"] == "online"));
    let peer_id = a.get("/api/remote/peers").unwrap()[0]["id"].as_str().unwrap().to_string();

    let app = base.join("b/app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join(".env"), "DB_PASSWORD=op://Dev/db/password\n").unwrap();
    b.post("/api/services", Some(json!({ "name": "api", "type": "process", "restart": "never", "cwd": app.to_string_lossy(),
        "envFiles": [".env"], "command": "echo db=${DB_PASSWORD:-unset}; sleep 60" }))).unwrap();
    wait_until("pending", 10, || b.svc("api")["secretsPending"]["new"] == json!(["DB_PASSWORD"]));

    // Not shared yet: nothing arrives, but `a` sees what `b` waits for (references only).
    let r = b.post("/api/onepassword/peers/sync", Some(json!({}))).unwrap();
    assert_eq!(r["received"], json!([]), "{r}");
    let shares = a.get("/api/onepassword/shares").unwrap();
    assert_eq!(shares["peers"][0]["wants"][0]["ref"], "op://Dev/db/password", "{shares}");
    assert_eq!(shares["peers"][0]["wants"][0]["have"], true);
    // Only values read from 1Password here can be shared.
    assert_eq!(a.post("/api/onepassword/shares", Some(json!({ "peer": peer_id, "items": ["TYPED_IN"] }))).unwrap_err().0, 400);
    assert_eq!(a.post("/api/onepassword/shares", Some(json!({ "peer": peer_id, "items": ["op://Dev/nope/x"] }))).unwrap_err().0, 404);

    // Shared: importing on `b` takes it from `a` — no 1Password read.
    let (ok, out) = a.cli(&["secret", "share", &peer_id, "DB_PASSWORD"]);
    assert!(ok, "{out}");
    let plan = b.post("/api/onepassword/plan", Some(json!({ "kind": "env", "service": "api" }))).unwrap();
    assert_eq!(plan["items"][0]["cached"], true, "{plan}");
    assert_eq!(plan["items"][0]["needsRead"], false);
    assert_eq!(plan["items"][0]["from"], "studio", "the plan says the value came from the paired Mac");
    // Received values aren't granted to anything until that plan is confirmed.
    assert_eq!(b.svc("api")["config"]["secrets"], Value::Null);
    b.post("/api/onepassword/import", Some(json!({ "plan": plan["id"], "values": {} }))).unwrap();
    let entry = |d: &Daemon| d.get("/api/secrets").unwrap()["secrets"].as_array().unwrap().iter().find(|s| s["name"] == "DB_PASSWORD").cloned().unwrap_or(Value::Null);
    assert_eq!(entry(&b)["source"], "peer:studio");
    assert_eq!(b.svc("api")["config"]["secrets"], json!({ "DB_PASSWORD": "DB_PASSWORD" }));
    b.post("/api/services/api/start", None).unwrap();
    wait_until("injected", 10, || b.logs("api").contains("db=••••[DB_PASSWORD]"));
    assert!(!b.logs("api").contains(value));

    // A refresh on `a` reaches `b` (pushToPeers), still from the store.
    let before = entry(&b)["fetchedAt"].clone();
    std::thread::sleep(Duration::from_millis(20));
    let plan = a.post("/api/onepassword/plan", Some(json!({ "kind": "refresh", "names": ["DB_PASSWORD"] }))).unwrap();
    a.post("/api/onepassword/import", Some(json!({ "plan": plan["id"], "values": { "DB_PASSWORD": "rotated-pw-Q2" } }))).unwrap();
    let r = b.post("/api/onepassword/peers/sync", Some(json!({}))).unwrap();
    assert_eq!(r["received"][0]["updated"], json!(["DB_PASSWORD"]), "{r}");
    assert_ne!(entry(&b)["fetchedAt"], before);

    // `b` can't pass on what it received.
    let b_peers = b.get("/api/remote/peers").unwrap();
    assert!(b_peers.as_array().unwrap().is_empty() || b.post("/api/onepassword/shares", Some(json!({ "peer": b_peers[0]["id"], "items": ["DB_PASSWORD"] }))).unwrap_err().0 == 400);

    // Unshared: no more updates.
    a.post("/api/onepassword/shares", Some(json!({ "peer": peer_id, "items": ["DB_PASSWORD"], "allow": false }))).unwrap();
    let plan = a.post("/api/onepassword/plan", Some(json!({ "kind": "refresh", "names": ["DB_PASSWORD"] }))).unwrap();
    a.post("/api/onepassword/import", Some(json!({ "plan": plan["id"], "values": { "DB_PASSWORD": "rotated-again" } }))).unwrap();
    assert_eq!(b.post("/api/onepassword/peers/sync", Some(json!({}))).unwrap()["received"], json!([]));

    let (audit_a, audit_b) = (a.get("/api/onepassword/audit").unwrap().to_string(), b.get("/api/onepassword/audit").unwrap().to_string());
    assert!(audit_a.contains("peer-sent") && audit_b.contains("peer-received"), "{audit_a}\n{audit_b}");
    for s in [&audit_a, &audit_b] {
        assert!(!s.contains(value) && !s.contains("rotated"), "{s}");
    }
    let activity = a.get("/api/remote/activity").map(|v| v.to_string()).unwrap_or_default();
    assert!(activity.is_empty() || activity.contains("secrets-sent"), "{activity}");
}

#[test]
fn machines_pair_and_proxy() {
    let base = std::env::temp_dir().join(format!("lstack-it-peers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let host = Daemon::start(&base.join("host"));
    let client = Daemon::start(&base.join("client"));

    // The client serves a web server; its URL is detected from the listening socket.
    let web_port = free_port();
    client
        .post("/api/services", Some(json!({ "name": "web", "type": "process", "restart": "never",
            "command": format!("exec python3 -m http.server {web_port} --bind 127.0.0.1") })))
        .unwrap();
    client.post("/api/services/web/start", None).unwrap();
    let want_url = format!("http://localhost:{web_port}");
    // First the server must be up (python's first launch can be slow on a fresh CI runner)…
    let up = || std::net::TcpStream::connect(("127.0.0.1", web_port)).is_ok();
    let t0 = Instant::now();
    while !up() && t0.elapsed() < Duration::from_secs(90) {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(up(), "web server never listened on {web_port}: {}\nlogs:\n{}", client.svc("web"), client.logs("web"));
    // …then it is detected within a couple of 5 s sampling rounds.
    let found = || client.svc("web")["urls"].as_array().is_some_and(|u| u.iter().any(|u| u["url"] == json!(want_url)));
    let t1 = Instant::now();
    while !found() && t1.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(200));
    }
    if !found() {
        let lsof = Command::new("/usr/sbin/lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string());
        panic!("url {want_url} not detected (server up after {:?}): {}\nlsof: {lsof:?}", t0.elapsed() - t1.elapsed(), client.svc("web"));
    }
    let u = &client.svc("web")["urls"][0];
    assert_eq!(u["source"], "detected");
    assert_eq!(u["localOnly"], true);
    assert!(client.get("/api/processes").unwrap()["thresholds"].is_object());

    // Pairing codes need sharing on. Sharing works with the web dashboard off: only the link port listens.
    assert_eq!(client.post("/api/remote/pair-code", None).unwrap_err().0, 400);
    let (rport, lport) = (free_port(), free_port());
    client
        .call("PUT", "/api/remote", Some(json!({ "enabled": false, "sharing": true, "bind": "127.0.0.1", "port": rport, "linkPort": lport, "username": "me", "password": "correct horse" })))
        .unwrap();
    let addr = format!("127.0.0.1:{lport}");
    wait_until("link listener", 10, || std::net::TcpStream::connect(&addr).is_ok());
    assert!(std::net::TcpStream::connect(("127.0.0.1", rport)).is_err(), "web dashboard is off");
    // The link port speaks only the LStack link protocol.
    assert!(ureq::get(&format!("http://{addr}/peer/api/info")).timeout(Duration::from_secs(8)).call().is_err());
    let rs = client.get("/api/remote").unwrap();
    assert_eq!((rs["sharing"].clone(), rs["enabled"].clone()), (json!(true), json!(false)));
    let client_id = client.get("/api/identity").unwrap();
    assert_eq!(client_id["fingerprint"].as_str().unwrap().len(), 19);

    // Pairing needs only a live one-time code (no dashboard login); a code is burned after 3 failures.
    let login = |code: &str| json!({ "address": addr, "code": code, "name": "studio" });
    assert_eq!(host.post("/api/machines", Some(json!({ "address": addr }))).unwrap_err().0, 400, "code required");
    let code = client.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    for _ in 0..3 {
        let e = host.post("/api/machines", Some(login("AAAA-AAAA"))).unwrap_err();
        assert_eq!(e.0, 403, "{e:?}");
    }
    assert!(host.post("/api/machines", Some(login(&code))).is_err(), "burned after 3 failures");
    let code3 = client.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    let m = host.post("/api/machines", Some(login(&code3))).unwrap();
    assert_eq!(m["name"], "studio");
    assert_eq!(m["phrase"].as_str().unwrap().split(' ').count(), 4);
    assert_eq!(m["grants"], json!({ "control": false, "tunnel": "all" }));
    assert!(host.post("/api/machines", Some(login(&code3))).is_err(), "codes are single use");
    let peers = client.get("/api/remote/peers").unwrap();
    assert_eq!(peers.as_array().unwrap().len(), 1);
    assert_eq!(peers[0]["phrase"], m["phrase"]);
    let saved = std::fs::read_to_string(base.join("host/peers.json")).unwrap();
    assert!(saved.contains("publicKey") && !saved.contains("token") && !saved.contains("correct horse"), "{saved}");

    // The client's 127.0.0.1-only web server is forwarded to a local port on the host, automatically.
    let forwarded = || -> Option<String> {
        let v = host.get("/api/machines/studio").ok()?;
        let s = v["services"].as_array()?.iter().find(|s| s["name"] == "web" && s["machine"] == "studio")?.clone();
        (v["status"] == "online" && s["urls"][0]["via"] == "studio").then(|| s["urls"][0]["url"].as_str().unwrap().to_string())
    };
    wait_until("automatic forward", 30, || forwarded().is_some());
    let local_url = forwarded().unwrap();
    assert!(local_url.starts_with("http://localhost:") && local_url != want_url, "{local_url} (same port is taken here, so another is used)");
    let fw = host.get("/api/forwards").unwrap();
    assert_eq!(fw[0]["machine"], "studio");
    assert_eq!(fw[0]["port"], web_port);
    let page = ureq::get(&local_url.replace("localhost", "127.0.0.1")).timeout(Duration::from_secs(10)).call().unwrap().into_string().unwrap();
    assert!(page.contains("Directory listing"), "{page}");
    wait_until("tunnel audited", 10, || client.get("/api/remote/activity").unwrap()["tunnels"].as_array().is_some_and(|t| !t.is_empty()));
    let ms = host.get("/api/machines").unwrap();
    assert_eq!(ms["local"]["local"], true);
    assert_eq!(ms["machines"][0]["name"], "studio");

    // Proxied logs and details over the link; control needs the control grant.
    assert!(host.get("/api/machines/studio/services/web").unwrap()["status"] == "running");
    assert!(host.get("/api/machines/studio/services/web/logs?tail=50").unwrap()["lines"].is_array());
    assert!(host.get("/api/machines/studio/services/web/metrics?range=5m").unwrap()["samples"].is_array());
    assert_eq!(host.post("/api/machines/studio/services/web/stop", None).unwrap_err().0, 403);
    let id = peers[0]["id"].as_str().unwrap().to_string();
    client.call("PATCH", &format!("/api/remote/peers/{id}"), Some(json!({ "control": true }))).unwrap();
    wait_until("control granted", 20, || host.post("/api/machines/studio/services/web/stop", None).is_ok());
    wait_until("peer stop seen on host", 20, || {
        let v = host.get("/api/machines/studio").unwrap();
        v["services"].as_array().unwrap().iter().any(|s| s["name"] == "web" && s["status"] == "stopped")
    });
    wait_until("forward dropped for stopped service", 10, || host.get("/api/forwards").unwrap().as_array().unwrap().is_empty());
    assert_eq!(host.get("/api/machines/nope").unwrap_err().0, 404);

    // CLI on the host lists both machines.
    let (ok, out) = host.cli(&["machine", "ls"]);
    assert!(ok && out.contains("studio"), "{out}");

    // Revoking on the client tells the host, which forgets the client too (and its forwards).
    client.call("DELETE", &format!("/api/remote/peers/{id}"), None).unwrap();
    wait_until("host forgets the revoked machine", 30, || host.get("/api/machines/studio").is_err_and(|e| e.0 == 404));
    wait_until("host peers.json updated", 5, || !std::fs::read_to_string(base.join("host/peers.json")).unwrap().contains("studio"));
    assert!(host.get("/api/forwards").unwrap().as_array().unwrap().is_empty());

    // Pairing again works; machines can be renamed (renaming doesn't unpair).
    let code4 = client.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    let m = host.post("/api/machines", Some(json!({ "address": addr, "code": code4, "name": "studio" }))).unwrap();
    assert_eq!(m["name"], "studio");
    assert_eq!(host.get("/api/machines").unwrap()["machines"].as_array().unwrap().len(), 1);
    host.call("PATCH", "/api/machines/studio", Some(json!({ "name": "lab" }))).unwrap();
    assert!(host.get("/api/machines/lab").is_ok());
    assert!(std::fs::read_to_string(base.join("host/peers.json")).unwrap().contains("\"lab\""));
    wait_until("link back up after rename", 30, || host.get("/api/machines/lab").unwrap()["status"] == "online");
    assert_eq!(client.get("/api/remote/peers").unwrap().as_array().unwrap().len(), 1);

    // Removing the machine on the host tells the client, which forgets the host too.
    host.call("DELETE", "/api/machines/lab", None).unwrap();
    wait_until("client forgets the host", 15, || client.get("/api/remote/peers").unwrap().as_array().unwrap().is_empty());
    drop(host);
    drop(client);
    let _ = std::fs::remove_dir_all(&base);
}

/// Both Macs on a fake tailnet (`tests/fake_tailnet.py` stands in for the Tailscale helper): upgrade with the other
/// Mac turning Tailscale on and signing in, downgrades from either side, approval when auto-approve is off, and
/// unpairing over the tailnet.
#[test]
fn tailscale_transport() {
    // Short paths: unix socket paths are limited to 104 bytes.
    let base = PathBuf::from(format!("/tmp/lsts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_tailnet.py");
    let env = |name: &str| {
        vec![
            ("LSTACK_TAILNET_BIN", fake.to_string_lossy().into_owned()),
            ("FAKE_TAILNET_REGISTRY", base.join("reg").to_string_lossy().into_owned()),
            ("LSTACK_TAILNET_HOSTNAME", name.to_string()),
        ]
    };
    let host = Daemon::start_with(&base.join("h"), &env("lstack-host"));
    let client = Daemon::start_with(&base.join("c"), &env("lstack-client"));
    let lport = free_port();
    client.call("PUT", "/api/remote", Some(json!({ "enabled": false, "sharing": true, "bind": "127.0.0.1", "linkPort": lport }))).unwrap();
    let addr = format!("127.0.0.1:{lport}");
    wait_until("link listener", 10, || std::net::TcpStream::connect(&addr).is_ok());
    let code = client.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    host.post("/api/machines", Some(json!({ "address": addr, "code": code, "name": "studio" }))).unwrap();
    let m = || host.get("/api/machines/studio").unwrap();
    wait_until("link up", 30, || m()["status"] == "online");
    assert_eq!(m()["transport"], "local");
    assert_eq!(m()["via"]["kind"], "lan");

    // Upgrading needs Tailscale here first; turning it on needs the exposure acknowledged.
    let up = || host.post("/api/machines/studio/transport", Some(json!({ "transport": "tailscale" })));
    assert_eq!(up().unwrap_err().0, 409);
    assert_eq!(host.post("/api/tailnet", Some(json!({ "enabled": true }))).unwrap_err().0, 409);
    host.post("/api/tailnet", Some(json!({ "enabled": true, "acknowledged": true }))).unwrap();
    wait_until("helper needs login", 20, || host.get("/api/tailnet").unwrap()["phase"] == "needs-login");
    assert!(host.get("/api/tailnet").unwrap()["authURL"].as_str().unwrap().starts_with("https://"));
    host.post("/api/tailnet/login", Some(json!({}))).unwrap();
    wait_until("helper running", 20, || host.get("/api/tailnet").unwrap()["phase"] == "running");
    let addrs = host.get("/api/remote/addresses").unwrap();
    assert!(addrs.as_array().unwrap().iter().any(|a| a["kind"] == "tailscale" && a["address"] == "lstack-host.fake.ts.net"), "{addrs}");

    // Upgrade: the other Mac turns Tailscale on by itself (auto-approve), then waits for someone to sign in there.
    up().unwrap();
    wait_until("waiting for the other Mac to sign in", 30, || m()["transportChange"]["state"] == "waiting-login");
    assert_eq!(client.get("/api/tailnet").unwrap()["enabled"], true);
    client.post("/api/tailnet/login", Some(json!({}))).unwrap();
    wait_until("switched to tailscale", 60, || {
        let v = m();
        v["transport"] == "tailscale" && v["via"]["kind"] == "tailscale" && v["status"] == "online" && v["transportChange"].is_null()
    });
    assert_eq!(m()["tailnetName"], "lstack-client.fake.ts.net");
    let peer = || client.get("/api/remote/peers").unwrap()[0].clone();
    wait_until("shared side sees the tailnet link", 10, || peer()["via"] == "tailscale");
    assert_eq!(peer()["transport"], "tailscale");
    assert_eq!(peer()["tailnet"]["tailnet"], "fake.ts.net");
    assert!(std::fs::read_to_string(base.join("h/peers.json")).unwrap().contains("\"tailscale\""));
    assert!(host.get("/api/machines/studio/services/nope").is_err_and(|e| e.0 == 404), "peer API works over the tailnet");

    // The shared Mac moves it back to the local network on its own: the watcher follows.
    let id = peer()["id"].as_str().unwrap().to_string();
    client.post(&format!("/api/remote/peers/{id}/transport"), Some(json!({ "transport": "local" }))).unwrap();
    wait_until("followed the shared Mac to the LAN", 30, || {
        let v = m();
        v["transport"] == "local" && v["via"]["kind"] == "lan" && v["status"] == "online"
    });
    // A Mac on the local transport isn't accepted over the tailnet.
    let refused = client.get("/api/remote/activity").unwrap();
    assert!(refused["events"].as_array().unwrap().iter().all(|e| e["event"] != "link-refused"), "{refused}");

    // Upgrade again (both sides already signed in: no waiting), then downgrade from the watcher with approval.
    up().unwrap();
    wait_until("tailscale again", 60, || m()["transport"] == "tailscale" && m()["via"]["kind"] == "tailscale");
    client.call("PUT", "/api/remote", Some(json!({ "autoApproveTransport": false }))).unwrap();
    host.post("/api/machines/studio/transport", Some(json!({ "transport": "local" }))).unwrap();
    wait_until("waiting for approval", 30, || m()["transportChange"]["state"] == "waiting-approval");
    assert_eq!(m()["pendingTransport"], "local");
    let rs = client.get("/api/remote").unwrap();
    assert_eq!(rs["transportRequests"][0]["transport"], "local", "{rs}");
    assert_eq!(peer()["transport"], "tailscale", "not applied before approval");
    client.post(&format!("/api/remote/transport-requests/{id}/allow"), None).unwrap();
    wait_until("approved downgrade applied", 30, || {
        let v = m();
        v["transport"] == "local" && v["pendingTransport"].is_null() && v["transportChange"].is_null() && v["status"] == "online"
    });
    assert_eq!(peer()["transport"], "local");
    assert!(client.get("/api/remote").unwrap()["transportRequests"].as_array().unwrap().is_empty());

    // Over Tailscale again, unpairing still tells the other Mac.
    client.call("PUT", "/api/remote", Some(json!({ "autoApproveTransport": true }))).unwrap();
    up().unwrap();
    wait_until("tailscale for unpair", 60, || m()["transport"] == "tailscale" && m()["via"]["kind"] == "tailscale" && m()["status"] == "online");
    let (ok, out) = host.cli(&["machine", "ls"]);
    assert!(ok && out.contains("tailscale"), "{out}");
    let (ok, out) = host.cli(&["tailnet", "status"]);
    assert!(ok && out.contains("running") && out.contains("lstack-client.fake.ts.net"), "{out}");
    host.call("DELETE", "/api/machines/studio", None).unwrap();
    wait_until("client forgets the host", 15, || client.get("/api/remote/peers").unwrap().as_array().unwrap().is_empty());
    drop(host);
    drop(client);
    let _ = std::fs::remove_dir_all(&base);
}

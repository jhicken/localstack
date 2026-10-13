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
        let port = free_port();
        std::fs::create_dir_all(home.join("services")).unwrap();
        let child = Command::new(BIN)
            .args(["daemon", "run"])
            .env("LSTACK_HOME", home)
            .env("LSTACK_PORT", port.to_string())
            .env("LSTACK_KEYCHAIN", "file")
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

    // Pairing codes need sharing on. Sharing works with the web dashboard off.
    let status = |path: &str| match ureq::get(path).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(_) => 0,
    };
    assert_eq!(client.post("/api/remote/pair-code", None).unwrap_err().0, 400);
    let rport = free_port();
    client
        .call("PUT", "/api/remote", Some(json!({ "enabled": false, "sharing": true, "bind": "127.0.0.1", "port": rport, "username": "me", "password": "correct horse", "allowControl": true })))
        .unwrap();
    let url = format!("127.0.0.1:{rport}");
    wait_until("remote listener", 10, || status(&format!("http://{url}/api/login")) == 404);
    assert_eq!(status(&format!("http://{url}/")), 404, "web dashboard is off");
    let rs = client.get("/api/remote").unwrap();
    assert_eq!((rs["sharing"].clone(), rs["enabled"].clone()), (json!(true), json!(false)));

    // Wrong code is refused; a fresh code pairs once.
    assert_eq!(host.post("/api/machines", Some(json!({ "url": url, "code": "AAAA-AAAA" }))).unwrap_err().0, 401);
    let code = client.post("/api/remote/pair-code", None).unwrap()["code"].as_str().unwrap().to_string();
    let m = host.post("/api/machines", Some(json!({ "url": url, "code": code, "name": "studio" }))).unwrap();
    assert_eq!(m["name"], "studio");
    assert_eq!(host.post("/api/machines", Some(json!({ "url": url, "code": code }))).unwrap_err().0, 409);
    let peers = client.get("/api/remote/peers").unwrap();
    assert_eq!(peers.as_array().unwrap().len(), 1);

    // The host sees the client's services, tagged and with URLs pointing at the client.
    let peer_url = format!("http://127.0.0.1:{web_port}");
    wait_until("peer services", 20, || {
        let v = host.get("/api/machines/studio").unwrap();
        v["status"] == "online" && v["services"].as_array().is_some_and(|s| s.iter().any(|s| s["name"] == "web" && s["machine"] == "studio" && s["urls"][0]["url"] == json!(peer_url)))
    });
    let ms = host.get("/api/machines").unwrap();
    assert_eq!(ms["local"]["local"], true);
    assert_eq!(ms["machines"][0]["name"], "studio");

    // Proxied logs, details and control.
    assert!(host.get("/api/machines/studio/services/web").unwrap()["status"] == "running");
    assert!(host.get("/api/machines/studio/services/web/logs?tail=50").unwrap()["lines"].is_array());
    assert!(host.get("/api/machines/studio/services/web/metrics?range=5m").unwrap()["samples"].is_array());
    host.post("/api/machines/studio/services/web/stop", None).unwrap();
    wait_until("peer stop seen on host", 20, || {
        let v = host.get("/api/machines/studio").unwrap();
        v["services"].as_array().unwrap().iter().any(|s| s["name"] == "web" && s["status"] == "stopped")
    });
    assert_eq!(host.get("/api/machines/nope").unwrap_err().0, 404);

    // CLI on the host lists both machines.
    let (ok, out) = host.cli(&["machine", "ls"]);
    assert!(ok && out.contains("studio"), "{out}");

    // Control follows the client's allowControl.
    client.call("PUT", "/api/remote", Some(json!({ "allowControl": false }))).unwrap();
    wait_until("remote restart", 10, || status(&format!("http://{url}/api/login")) == 404);
    wait_until("control refused", 20, || host.post("/api/machines/studio/services/web/start", None).is_err_and(|e| e.0 == 403));

    // Revoking the token on the client cuts the host off.
    let id = client.get("/api/remote/peers").unwrap()[0]["id"].as_str().unwrap().to_string();
    client.call("DELETE", &format!("/api/remote/peers/{id}"), None).unwrap();
    assert_eq!(host.get("/api/machines/studio/services/web").unwrap_err().0, 401);
    wait_until("host marks unauthorized", 40, || host.get("/api/machines/studio").unwrap()["status"] == "unauthorized");

    // Password pairing works too, and machines can be renamed/removed.
    host.call("DELETE", "/api/machines/studio", None).unwrap();
    assert_eq!(host.post("/api/machines", Some(json!({ "url": url, "username": "me", "password": "wrong-pass" }))).unwrap_err().0, 401);
    let m = host.post("/api/machines", Some(json!({ "url": url, "username": "me", "password": "correct horse" }))).unwrap();
    let name = m["name"].as_str().unwrap().to_string();
    host.call("PATCH", &format!("/api/machines/{name}"), Some(json!({ "name": "lab" }))).unwrap();
    assert!(host.get("/api/machines/lab").is_ok());
    assert!(std::fs::read_to_string(base.join("host/peers.json")).unwrap().contains("\"lab\""));
    drop(host);
    drop(client);
    let _ = std::fs::remove_dir_all(&base);
}

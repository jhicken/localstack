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
        let o = Command::new(BIN).args(args).env("LSTACK_HOME", &self.home).env("NO_COLOR", "1").stdin(Stdio::null()).output().unwrap();
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

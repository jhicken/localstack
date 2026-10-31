//! Daemon-wide preferences (settings.json) and opening `lstack …` in the user's terminal app.
use crate::paths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Terminal app for the dashboard / attach / shell buttons: terminal, iterm, cmux, ghostty.
    /// Empty (or not installed) = Terminal.
    pub terminal: String,
    /// healthchecks.io-compatible pings (the ping key lives in the secret store).
    pub healthchecks: Healthchecks,
    /// Which non-service processes show up in the process breakdown.
    pub processes: ProcessThresholds,
    /// How 1Password references in services' env files are handled (see `envimport.rs`).
    pub onepassword: OnePassword,
    /// Keep LStack's services in step with `docker ps -a` (see `docker_import::spawn_sync`).
    pub docker_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct OnePassword {
    /// Watch imported env files and flag new or changed `op://` references for review.
    pub watch: bool,
    /// When a reference disappears from an env file, stop passing it and forget the cached value
    /// without asking (it only reduces what the service gets).
    pub auto_apply_removals: bool,
    /// Restart running services whose values changed after an import or refresh.
    pub restart_on_change: bool,
    /// Only allow references into these vaults (case-insensitive). Empty = any vault.
    pub allowed_vaults: Vec<String>,
    /// After a refresh here, send changed values to paired Macs that were granted them.
    pub push_to_peers: bool,
    /// When a service needs a value that came from a paired Mac and it's missing, ask that Mac again
    /// (served from its encrypted cache; 1Password is never contacted).
    pub pull_from_peers: bool,
}

impl Default for OnePassword {
    fn default() -> Self {
        OnePassword { watch: true, auto_apply_removals: true, restart_on_change: false, allowed_vaults: vec![], push_to_peers: true, pull_from_peers: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Healthchecks {
    /// Ping server, e.g. https://hc-ping.com or a self-hosted Healthchecks instance.
    pub base_url: String,
    /// Log lines attached to `/fail` pings.
    pub log_lines: usize,
    /// Whole-machine heartbeat (pinged while the daemon runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<crate::types::Heartbeat>,
}

impl Default for Healthchecks {
    fn default() -> Self {
        Healthchecks { base_url: "https://hc-ping.com".into(), log_lines: 20, machine: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProcessThresholds {
    pub enabled: bool,
    /// % of one core.
    pub cpu: f64,
    /// % of physical memory.
    pub mem: f64,
    /// Bytes/s (rx + tx).
    pub net: f64,
    /// Rows per machine.
    pub max: usize,
}

impl Default for ProcessThresholds {
    fn default() -> Self {
        ProcessThresholds { enabled: true, cpu: 10.0, mem: 10.0, net: 1_048_576.0, max: 15 }
    }
}

/// (id, display name, app bundle name)
pub const TERMINALS: &[(&str, &str, &str)] = &[
    ("terminal", "Terminal", "Terminal.app"),
    ("iterm", "iTerm", "iTerm.app"),
    ("cmux", "cmux", "cmux.app"),
    ("ghostty", "Ghostty", "Ghostty.app"),
];

pub fn load() -> Settings {
    std::fs::read(paths::settings_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    let path = paths::settings_file();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

fn app_path(bundle: &str) -> Option<PathBuf> {
    if bundle == "Terminal.app" {
        return Some(PathBuf::from("/System/Applications/Utilities/Terminal.app"));
    }
    [PathBuf::from("/Applications"), paths::user_home().join("Applications")]
        .into_iter()
        .map(|d| d.join(bundle))
        .find(|p| p.exists())
}

pub fn is_known(id: &str) -> bool {
    TERMINALS.iter().any(|(t, _, _)| *t == id)
}

/// The configured terminal if it's still installed, else Terminal.
pub fn current() -> &'static str {
    let want = load().terminal;
    TERMINALS
        .iter()
        .find(|(id, _, bundle)| *id == want && app_path(bundle).is_some())
        .map(|(id, _, _)| *id)
        .unwrap_or("terminal")
}

/// Whether the daemon (and so every service it starts) has Full Disk Access. Without it macOS hides
/// other apps' data — e.g. the 1Password CLI can't reach the 1Password app.
pub fn full_disk_access() -> bool {
    // Files only readable with Full Disk Access; the per-user TCC.db doesn't exist on every macOS.
    let probes = [
        std::path::PathBuf::from("/Library/Application Support/com.apple.TCC/TCC.db"),
        paths::user_home().join("Library/Application Support/com.apple.TCC/TCC.db"),
        paths::user_home().join("Library/Safari/Bookmarks.plist"),
    ];
    for p in &probes {
        match std::fs::File::open(p) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        }
    }
    false
}

pub fn status() -> Value {
    let settings = load();
    let terminals: Vec<Value> = TERMINALS
        .iter()
        .map(|(id, name, bundle)| json!({ "id": id, "name": name, "installed": app_path(bundle).is_some() }))
        .collect();
    json!({
        "terminal": current(),
        "terminals": terminals,
        "fullDiskAccess": full_disk_access(),
        "helperPath": paths::self_exe(),
        "cmux": cmux_status(),
        "healthchecks": super::heartbeat::settings_status(&settings.healthchecks),
        "processes": settings.processes,
        "onepassword": settings.onepassword,
    })
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.:@%+=,".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// LSTACK_* settings the daemon runs with (non-default home/port), passed explicitly to the
/// launched command instead of through the environment, so a terminal app cold-launched by
/// the daemon doesn't inherit them into every later tab.
fn lstack_env() -> Vec<String> {
    ["LSTACK_HOME", "LSTACK_PORT", "LSTACK_DOCKER"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
        .collect()
}

fn spawn(cmd: &mut Command) -> Result<(), String> {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("LSTACK_") {
            cmd.env_remove(&k);
        }
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn osascript(script: &str) -> Result<(), String> {
    spawn(Command::new("/usr/bin/osascript").args(["-e", script]))
}

/// Open the preferred terminal app running `lstack <args>`; `term` overrides the configured app
/// (e.g. "terminal" as a fallback when cmux refuses LStack).
pub fn open_with(args: &[&str], term: Option<&str>) -> Result<(), String> {
    let exe = paths::self_exe().to_string_lossy().into_owned();
    let env = lstack_env();
    let mut argv: Vec<&str> = Vec::new();
    if !env.is_empty() {
        argv.push("/usr/bin/env");
        argv.extend(env.iter().map(String::as_str));
    }
    argv.push(&exe);
    argv.extend(args.iter().copied());
    let line: Vec<String> = argv.iter().copied().map(shell_quote).collect();
    let line = line.join(" ");
    let term: &str = term.and_then(|t| TERMINALS.iter().find(|x| x.0 == t)).map(|x| x.0).unwrap_or_else(current);
    let res = match term {
        "iterm" => osascript(&format!(
            "tell application \"iTerm\"\n  activate\n  set w to (create window with default profile)\n  tell current session of w to write text {}\nend tell",
            applescript_str(&line)
        )),
        "cmux" => open_cmux(args, &line),
        "ghostty" => {
            let mut c = Command::new("/usr/bin/open");
            c.args(["-na", "Ghostty", "--args", "-e"]).args(&argv);
            spawn(&mut c)
        }
        _ => osascript(&format!(
            "tell application \"Terminal\"\n  activate\n  do script {}\nend tell",
            applescript_str(&line)
        )),
    };
    res.map_err(|e| if e.starts_with(CMUX_DENIED) { e } else { format!("couldn't open {term}: {e}") })
}

// ------------------------------------------------------------------ cmux

/// Prefix of the error returned when cmux refuses LStack's socket connection.
pub const CMUX_DENIED: &str = "cmux-denied";
/// Internal secret holding the cmux socket password LStack set up.
pub const CMUX_SECRET: &str = "LSTACK_CMUX_PASSWORD";

fn cmux_cli() -> Option<PathBuf> {
    app_path("cmux.app").map(|a| a.join("Contents/Resources/bin/cmux")).filter(|p| p.exists())
}

fn cmux_config_path() -> PathBuf {
    paths::user_home().join(".config/cmux/cmux.json")
}

fn cmux_cmd(cli: &PathBuf) -> Command {
    let mut c = Command::new(cli);
    if let Some(pw) = super::secrets::internal_get(CMUX_SECRET) {
        c.env("CMUX_SOCKET_PASSWORD", pw);
    }
    c
}

fn is_denied(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    e.contains("access denied") || e.contains("only processes started inside cmux") || e.contains("password") || e.contains("unauthorized")
}

fn open_cmux(args: &[&str], line: &str) -> Result<(), String> {
    let cli = cmux_cli().ok_or("cmux isn't installed")?;
    let app = app_path("cmux.app").unwrap_or_default();
    let title = format!("lstack {}", args.join(" "));
    let create = || spawn(cmux_cmd(&cli).args(["workspace", "create", "--name", title.trim(), "--focus", "true", "--command", line]));
    let res = match create() {
        // Not running yet: launch it, give the socket a moment, try once more.
        Err(e) if !is_denied(&e) => {
            let _ = spawn(Command::new("/usr/bin/open").arg(&app));
            std::thread::sleep(std::time::Duration::from_secs(2));
            create()
        }
        r => r,
    };
    match res {
        Ok(()) => spawn(Command::new("/usr/bin/open").arg(&app)),
        Err(e) if is_denied(&e) => Err(format!(
            "{CMUX_DENIED}: cmux only accepts connections from processes it started (Settings → Automation → Socket control). \
             Allow LStack with a socket password, or use another terminal app. ({e})"
        )),
        Err(e) => Err(e),
    }
}

/// cmux's socket-control mode and whether LStack holds a password for it.
pub fn cmux_status() -> Value {
    let cfg: Value = std::fs::read(cmux_config_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    json!({
        "installed": cmux_cli().is_some(),
        "mode": cfg["automation"]["socketControlMode"].as_str().unwrap_or("cmuxOnly"),
        "hasPassword": super::secrets::internal_get(CMUX_SECRET).is_some(),
        "configPath": cmux_config_path(),
    })
}

/// Switch cmux to password socket mode with a random password LStack keeps in its secret store.
/// Backs up cmux.json first. Returns status plus whether cmux picked up the change live.
pub fn cmux_enable_password() -> Result<Value, String> {
    let cli = cmux_cli().ok_or("cmux isn't installed")?;
    let path = cmux_config_path();
    let mut cfg: Value = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| {
            format!("{} isn't plain JSON ({e}); set Automation → Socket control to “Password” in cmux Settings instead", path.display())
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if !cfg.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()));
    }
    let mut backup = None;
    if path.exists() {
        let b = path.with_file_name(format!("cmux.json.{}.bak", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        std::fs::copy(&path, &b).map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
        backup = Some(b);
    }
    let pw = super::remote::random_hex(24);
    super::secrets::set(CMUX_SECRET, &pw)?;
    if !cfg["automation"].is_object() {
        cfg["automation"] = json!({});
    }
    cfg["automation"]["socketControlMode"] = json!("password");
    cfg["automation"]["socketPassword"] = json!(pw);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.lstack-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&cfg).unwrap_or_default()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    let reloaded = spawn(cmux_cmd(&cli).arg("reload-config")).is_ok();
    let mut st = cmux_status();
    st["reloaded"] = json!(reloaded);
    st["backup"] = json!(backup);
    if !reloaded {
        st["note"] = json!("Saved. If cmux still refuses, reload its config from the command palette or restart cmux.");
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/Applications/LStack.app/Contents/Helpers/lstack"), "/Applications/LStack.app/Contents/Helpers/lstack");
        assert_eq!(shell_quote("my app"), "'my app'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(applescript_str(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}

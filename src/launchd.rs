//! launchd integration: LaunchAgents that start the daemon (and the menu bar app) at login.
use crate::paths;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DAEMON_LABEL: &str = "dev.lstack.daemon";
pub const APP_LABEL: &str = "dev.lstack.menubar";

pub fn agents_dir() -> PathBuf {
    paths::user_home().join("Library/LaunchAgents")
}
pub fn daemon_plist() -> PathBuf {
    agents_dir().join(format!("{DAEMON_LABEL}.plist"))
}
pub fn app_plist() -> PathBuf {
    agents_dir().join(format!("{APP_LABEL}.plist"))
}
fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> (bool, String) {
    match Command::new("/bin/launchctl").args(args).stdin(std::process::Stdio::null()).output() {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()),
        Err(_) => (false, String::new()),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
fn unesc(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn plist(label: &str, args: &[String], extra: &str) -> String {
    let mut env = vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".into())),
        ("SHELL".to_string(), std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())),
    ];
    // Make sure docker & friends are reachable even when launched from Finder (minimal PATH).
    for extra_dir in ["/usr/local/bin", "/opt/homebrew/bin"] {
        if !env[0].1.split(':').any(|p| p == extra_dir) {
            env[0].1.push(':');
            env[0].1.push_str(extra_dir);
        }
    }
    for k in ["LSTACK_HOME", "LSTACK_PORT"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    let args_xml: Vec<String> = args.iter().map(|a| format!("    <string>{}</string>", esc(a))).collect();
    let env_xml: Vec<String> = env.iter().map(|(k, v)| format!("    <key>{k}</key><string>{}</string>", esc(v))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{}
  </dict>
  <key>RunAtLoad</key><true/>
{extra}
</dict>
</plist>
"#,
        args_xml.join("\n"),
        env_xml.join("\n")
    )
}

pub fn render_daemon_plist(bin: &Path) -> String {
    let log = esc(&paths::daemon_log().to_string_lossy());
    plist(
        DAEMON_LABEL,
        &[bin.to_string_lossy().into_owned(), "daemon".into(), "run".into()],
        &format!(
            "  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>"
        ),
    )
}

pub fn render_app_plist(exe: &Path) -> String {
    plist(APP_LABEL, &[exe.to_string_lossy().into_owned()], "  <key>LimitLoadToSessionType</key><string>Aqua</string>")
}

fn plist_strings(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

/// First ProgramArguments entry of a plist.
pub fn plist_program(file: &Path) -> Option<String> {
    let xml = plist_strings(file)?;
    let after = &xml[xml.find("<key>ProgramArguments</key>")?..];
    let s = after.find("<string>")? + 8;
    let e = after[s..].find("</string>")? + s;
    Some(unesc(&after[s..e]))
}

pub fn is_installed() -> bool {
    daemon_plist().exists()
}

/// True if the installed LaunchAgent supervises the current LSTACK_HOME (so sandboxes never touch the real daemon).
pub fn manages_this_home() -> bool {
    let Some(xml) = plist_strings(&daemon_plist()) else { return false };
    let home = xml
        .find("<key>LSTACK_HOME</key>")
        .and_then(|i| {
            let rest = &xml[i..];
            let s = rest.find("<string>")? + 8;
            let e = rest[s..].find("</string>")? + s;
            Some(unesc(&rest[s..e]))
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::user_home().join(".localstack"));
    let norm = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    norm(home) == norm(paths::home())
}

pub fn kickstart() {
    let target = format!("{}/{DAEMON_LABEL}", domain());
    if launchctl(&["kickstart", &target]).0 {
        return;
    }
    // bootout is asynchronous; bootstrap fails with EIO until the old job is fully gone, so retry.
    let plist = daemon_plist().to_string_lossy().into_owned();
    for _ in 0..60 {
        if launchctl(&["bootstrap", &domain(), &plist]).0 || launchctl(&["kickstart", &target]).0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Unload a job and wait until launchd has fully removed it.
pub fn bootout(label: &str) {
    let target = format!("{}/{label}", domain());
    launchctl(&["bootout", &target]);
    for _ in 0..60 {
        if !launchctl(&["print", &target]).0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub fn bootout_daemon() {
    bootout(DAEMON_LABEL);
}

pub fn app_executable(app: &Path) -> PathBuf {
    let stem = app.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "LStack".into());
    app.join("Contents/MacOS").join(stem)
}

/// Write the LaunchAgent(s). Does not load them.
pub fn install(bin: &Path, app: Option<&Path>) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(agents_dir()).map_err(|e| e.to_string())?;
    let mut notes = vec![];
    bootout_daemon();
    std::fs::write(daemon_plist(), render_daemon_plist(bin)).map_err(|e| e.to_string())?;
    notes.push(format!("wrote {}", daemon_plist().display()));
    if let Some(app) = app {
        let exe = app_executable(app);
        if !exe.exists() {
            return Err(format!("menu bar app executable not found: {}", exe.display()));
        }
        let xml = render_app_plist(&exe);
        if plist_strings(&app_plist()).as_deref() != Some(xml.as_str()) {
            bootout(APP_LABEL);
            std::fs::write(app_plist(), xml).map_err(|e| e.to_string())?;
            notes.push(format!("wrote {}", app_plist().display()));
        }
    }
    Ok(notes)
}

/// Bootstrap the daemon agent. The menu bar agent is left for the next login so it never launches a second copy.
pub fn load() {
    let target = format!("{}/{DAEMON_LABEL}", domain());
    let f = daemon_plist().to_string_lossy().into_owned();
    for _ in 0..20 {
        if launchctl(&["bootstrap", &domain(), &f]).0 || launchctl(&["print", &target]).0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub fn uninstall() -> Vec<String> {
    let mut notes = vec![];
    for (label, file) in [(DAEMON_LABEL, daemon_plist()), (APP_LABEL, app_plist())] {
        bootout(label);
        if file.exists() && std::fs::remove_file(&file).is_ok() {
            notes.push(format!("removed {}", file.display()));
        }
    }
    notes
}

pub struct Status {
    pub installed: bool,
    pub app_installed: bool,
    pub loaded: bool,
    pub pid: Option<i32>,
}

pub fn status() -> Status {
    let (loaded, out) = launchctl(&["print", &format!("{}/{DAEMON_LABEL}", domain())]);
    let pid = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = "))
        .and_then(|p| p.trim().parse().ok());
    Status { installed: is_installed(), app_installed: app_plist().exists(), loaded, pid }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plist_roundtrip() {
        let xml = render_daemon_plist(Path::new("/Applications/L & S.app/Contents/Helpers/lstack"));
        assert!(xml.contains("<string>daemon</string>"));
        assert!(xml.contains("L &amp; S.app"));
        let dir = std::env::temp_dir().join(format!("lstack-plist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.plist");
        std::fs::write(&f, xml).unwrap();
        assert_eq!(plist_program(&f).unwrap(), "/Applications/L & S.app/Contents/Helpers/lstack");
        std::fs::remove_dir_all(dir).ok();
    }
}

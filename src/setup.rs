//! Self-installation: put `lstack` on PATH, register login items, migrate/repair existing installs.
use crate::fmt as c;
use crate::{client, launchd, paths};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The `.app` bundle this binary lives in (e.g. /Applications/LStack.app/Contents/Helpers/lstack).
pub fn bundle_app() -> Option<PathBuf> {
    paths::self_exe().ancestors().find(|a| a.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

fn dir_writable(d: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()) else { return false };
    d.is_dir() && unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// PATH as seen by the user's login shell (GUI apps get a minimal PATH, so env alone is misleading).
pub fn shell_path() -> Vec<PathBuf> {
    static P: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let child = std::process::Command::new(&shell)
            .args(["-ilc", "printf '\\n__LSTACK_PATH__%s__LSTACK_PATH__\\n' \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut found = None;
        if let Ok(mut ch) = child {
            for _ in 0..50 {
                if let Ok(Some(_)) = ch.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if ch.try_wait().ok().flatten().is_none() {
                let _ = ch.kill();
            }
            if let Ok(out) = ch.wait_with_output() {
                let s = String::from_utf8_lossy(&out.stdout);
                found = s.split("__LSTACK_PATH__").nth(1).map(String::from);
            }
        }
        let raw = found.or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        raw.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect()
    })
    .clone()
}

fn local_bin() -> PathBuf {
    paths::user_home().join(".local/bin")
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/usr/local/bin"), local_bin(), PathBuf::from("/opt/homebrew/bin")];
    for d in shell_path() {
        if !v.contains(&d) {
            v.push(d);
        }
    }
    v
}

pub struct CliStatus {
    pub link: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub ok: bool,
    pub on_path: bool,
    pub shadowed_by: Option<PathBuf>,
}

pub fn cli_status() -> CliStatus {
    let me = canon(&paths::self_exe());
    let mut link = None;
    let mut target = None;
    for d in candidate_dirs() {
        let p = d.join("lstack");
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            let t = if meta.file_type().is_symlink() { std::fs::canonicalize(&p).ok() } else { Some(canon(&p)) };
            if t.as_deref() == Some(me.as_path()) {
                link = Some(p);
                target = t;
                break;
            }
            if link.is_none() && target.is_none() {
                target = t;
            }
        }
    }
    let path = shell_path();
    // The first `lstack` the shell would actually run.
    let first = path.iter().map(|d| d.join("lstack")).find(|p| p.exists());
    let on_path = link.as_ref().is_some_and(|l| l.parent().is_some_and(|d| path.iter().any(|p| p == d)));
    let shadowed_by = match (&first, &link) {
        (Some(f), Some(_)) if canon(f) != me => Some(f.clone()),
        _ => None,
    };
    CliStatus { ok: link.is_some(), link, target, on_path, shadowed_by }
}

/// An `lstack` link that's dangling or points into another copy of LStack.app (moved/updated app).
fn has_stale_cli_link() -> bool {
    candidate_dirs().iter().map(|d| d.join("lstack")).any(|p| {
        std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
            && (!p.exists() || std::fs::read_link(&p).is_ok_and(|t| t.to_string_lossy().contains(".app/Contents/Helpers/lstack")))
    })
}

/// Symlink this binary into a bin directory. Returns human-readable notes.
pub fn install_cli(dir: Option<&Path>) -> Result<Vec<String>, String> {
    let me = paths::self_exe();
    let mut notes = vec![];
    // Clean up dangling `lstack` links (e.g. from an old install) so they can't shadow the new one.
    for d in candidate_dirs() {
        let p = d.join("lstack");
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) && !p.exists() && std::fs::remove_file(&p).is_ok() {
            notes.push(format!("removed stale link {}", p.display()));
        }
    }
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None if dir_writable(Path::new("/usr/local/bin")) => PathBuf::from("/usr/local/bin"),
        None => local_bin(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let link = dir.join("lstack");
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if !meta.file_type().is_symlink() && canon(&link) != canon(&me) {
            return Err(format!("{} exists and is not a symlink — remove it first", link.display()));
        }
        std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    std::os::unix::fs::symlink(&me, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    notes.push(format!("linked {} → {}", link.display(), me.display()));
    if !shell_path().iter().any(|p| p == &dir) {
        if let Some(n) = add_to_shell_path(&dir) {
            notes.push(n);
        }
    }
    Ok(notes)
}

/// Append an `export PATH=…` line to the user's shell profile (only once).
fn add_to_shell_path(dir: &Path) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let home = paths::user_home();
    let rc = if shell.ends_with("bash") { home.join(".bash_profile") } else if shell.ends_with("fish") { return Some(format!("add {} to your fish PATH: fish_add_path {}", dir.display(), dir.display())) } else { home.join(".zprofile") };
    let shown = dir.strip_prefix(&home).map(|r| format!("$HOME/{}", r.display())).unwrap_or_else(|_| dir.display().to_string());
    let line = format!("export PATH=\"{shown}:$PATH\"  # added by lstack");
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(&line) {
        return None;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&rc).ok()?;
    writeln!(f, "\n{line}").ok()?;
    Some(format!("added {shown} to PATH in {} (open a new terminal to pick it up)", rc.display()))
}

pub fn uninstall_cli() -> Vec<String> {
    let me = canon(&paths::self_exe());
    let mut notes = vec![];
    for d in candidate_dirs() {
        let p = d.join("lstack");
        let is_link = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && (!p.exists() || canon(&p) == me) && std::fs::remove_file(&p).is_ok() {
            notes.push(format!("removed {}", p.display()));
        }
    }
    notes
}

/// Before restarting the daemon: note which scripts are running so the new daemon starts them again
/// (a restart shouldn't drop scripts that aren't set to autostart).
pub fn remember_running(info: &crate::types::DaemonInfo) {
    let Ok(v) = client::Client::new(info.clone()).req::<serde_json::Value>("GET", "/api/services", None) else { return };
    let names: Vec<&str> = v["services"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == "process" && s["status"] == "running")
        .filter_map(|s| s["name"].as_str())
        .collect();
    if !names.is_empty() {
        let _ = std::fs::write(paths::resume_file(), json!({ "at": paths::now_ms(), "services": names }).to_string());
    }
}

fn stop_running_daemon() {
    if let Some(info) = client::read_info() {
        if client::ping(&info) {
            remember_running(&info);
            let _ = client::Client::new(info.clone()).shutdown();
            for _ in 0..50 {
                if !client::ping(&info) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Register (or re-register) the login agents to run this binary. No-op if already correct and running.
pub fn install_login(app: Option<&Path>, force: bool) -> Result<Vec<String>, String> {
    let me = paths::self_exe();
    let want = launchd::render_daemon_plist(&me);
    let current = std::fs::read_to_string(launchd::daemon_plist()).ok();
    let app_ok = match app {
        Some(a) => std::fs::read_to_string(launchd::app_plist()).ok().as_deref() == Some(launchd::render_app_plist(&launchd::app_executable(a)).as_str()),
        None => true,
    };
    let running_me = client::read_info().filter(client::ping).is_some_and(|i| i.bin.as_deref().map(|b| canon(Path::new(b))) == Some(canon(&me)));
    if !force && current.as_deref() == Some(want.as_str()) && app_ok && launchd::status().loaded && running_me {
        return Ok(vec![]);
    }
    // An older daemon (possibly a different build) may be running outside launchd — stop it gracefully first.
    if !running_me || current.as_deref() != Some(want.as_str()) {
        stop_running_daemon();
    } else if let Some(i) = client::read_info().filter(client::ping) {
        // launchd::install boots the running daemon out too.
        remember_running(&i);
    }
    let mut notes = launchd::install(&me, app)?;
    launchd::load();
    for _ in 0..50 {
        if client::read_info().filter(client::ping).is_some() {
            notes.push("daemon running under launchd".into());
            return Ok(notes);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("login item installed but the daemon did not start — see {}", paths::daemon_log().display()))
}

pub fn status_json() -> serde_json::Value {
    let cli = cli_status();
    let st = launchd::status();
    let program = launchd::plist_program(&launchd::daemon_plist());
    let me = paths::self_exe();
    let info = client::read_info();
    let alive = info.as_ref().is_some_and(client::ping);
    let stale = info.as_ref().filter(|_| alive).and_then(daemon_stale);
    json!({
        "version": paths::VERSION,
        "bin": me,
        "app": bundle_app(),
        "home": paths::home(),
        "cli": {
            "installed": cli.ok,
            "path": cli.link,
            "target": cli.target,
            "onPath": cli.on_path,
            "shadowedBy": cli.shadowed_by,
        },
        "login": {
            "installed": st.installed,
            "appInstalled": st.app_installed,
            "loaded": st.loaded,
            "pid": st.pid,
            "program": program,
            "current": program.as_deref().map(|p| canon(Path::new(p))) == Some(canon(&me)),
        },
        "daemon": {
            "running": alive,
            "pid": info.as_ref().filter(|_| alive).map(|i| i.pid),
            "port": info.as_ref().filter(|_| alive).map(|i| i.port),
            "version": info.as_ref().filter(|_| alive).map(|i| i.version.clone()),
            "protocol": info.as_ref().filter(|_| alive).map(|i| i.protocol),
            "upgradable": info.as_ref().filter(|_| alive).is_some_and(daemon_upgradable),
            "bin": info.as_ref().filter(|_| alive).and_then(|i| i.bin.clone()),
            "stale": stale.is_some(),
            "staleReason": stale,
        }
    })
}

/// Why the running daemon can't be used by this build, if it can't: it predates (or is below) the
/// protocol this app needs. A different app version or a replaced binary is NOT a reason: the daemon
/// keeps running (and so do your services) until you choose to restart it. Set
/// `LSTACK_STRICT_DAEMON=1` (dev builds) to also require the same version and an unchanged binary.
pub fn daemon_stale(i: &crate::types::DaemonInfo) -> Option<String> {
    if i.protocol < paths::MIN_DAEMON_PROTOCOL {
        return Some(format!("the background service (v{}, protocol {}) is too old for this version (needs protocol {})", i.version, i.protocol, paths::MIN_DAEMON_PROTOCOL));
    }
    std::env::var_os("LSTACK_STRICT_DAEMON")?;
    if i.version != paths::VERSION {
        return Some(format!("the background service is running v{} but this is v{}", i.version, paths::VERSION));
    }
    // The daemon's own binary changed since it started (an app update or rebuild), whichever copy this is.
    let bin = PathBuf::from(i.bin.as_deref()?);
    let modified = std::fs::metadata(&bin).and_then(|m| m.modified()).ok()?;
    let modified_ms = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
    (modified_ms > i.started_at + 2_000).then(|| "LStack was updated after the background service started".to_string())
}

/// The daemon works but a newer protocol exists (new features need a restart; nothing is forced).
pub fn daemon_upgradable(i: &crate::types::DaemonInfo) -> bool {
    i.protocol < paths::PROTOCOL
}

fn print_notes(notes: &[String]) {
    for n in notes {
        println!("  {}", c::dim(n));
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut sub = "install";
    let (mut want_cli, mut want_login, mut json_out, mut quiet) = (false, false, false, false);
    let mut app: Option<PathBuf> = None;
    let mut no_app = false;
    let mut keep_daemon = false;
    let mut bin_dir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "status" | "repair" | "uninstall" | "install" => sub = a.as_str(),
            "--cli" => want_cli = true,
            "--login" => want_login = true,
            "--json" => json_out = true,
            "-q" | "--quiet" => quiet = true,
            "--no-app" => no_app = true,
            "--keep-daemon" => keep_daemon = true,
            "--app" => app = Some(PathBuf::from(paths::expand_home(it.next().ok_or("--app needs a path")?))),
            "--bin-dir" => bin_dir = Some(PathBuf::from(paths::expand_home(it.next().ok_or("--bin-dir needs a path")?))),
            o => return Err(format!("unknown setup option {o:?}")),
        }
    }
    if !no_app && app.is_none() {
        app = bundle_app().or_else(|| Some(PathBuf::from("/Applications/LStack.app")).filter(|p| p.exists()));
    }
    if no_app {
        app = None;
    }
    match sub {
        "status" => {
            let s = status_json();
            if json_out {
                println!("{}", serde_json::to_string_pretty(&s).unwrap());
                return Ok(());
            }
            let yes = |b: bool, t: &str| if b { c::green(t) } else { c::yellow("no") };
            println!("{} {}", c::bold("lstack"), c::dim(&format!("v{}  {}", paths::VERSION, s["bin"].as_str().unwrap_or(""))));
            println!("  cli on PATH   {}  {}", yes(s["cli"]["installed"].as_bool().unwrap_or(false), "yes"), c::dim(s["cli"]["path"].as_str().unwrap_or("")));
            if let Some(sh) = s["cli"]["shadowedBy"].as_str() {
                println!("                {}", c::yellow(&format!("shadowed by {sh}")));
            }
            println!(
                "  login item    {}  {}",
                yes(s["login"]["installed"].as_bool().unwrap_or(false), "yes"),
                if s["login"]["installed"].as_bool() == Some(true) && s["login"]["current"].as_bool() != Some(true) {
                    c::yellow(&format!("points at {} — run `lstack setup repair`", s["login"]["program"].as_str().unwrap_or("?")))
                } else {
                    String::new()
                }
            );
            println!("  menu bar app  {}", yes(s["login"]["appInstalled"].as_bool().unwrap_or(false), "starts at login"));
            println!(
                "  daemon        {}",
                if s["daemon"]["running"].as_bool() == Some(true) {
                    let up = if s["daemon"]["upgradable"].as_bool() == Some(true) { c::dim("  — newer features available after `lstack daemon restart`") } else { String::new() };
                    format!("{}{up}", c::green(&format!("running v{} (pid {}, port {})", s["daemon"]["version"].as_str().unwrap_or("?"), s["daemon"]["pid"], s["daemon"]["port"])))
                } else {
                    c::gray("not running")
                }
            );
            Ok(())
        }
        "uninstall" => {
            let both = !want_cli && !want_login;
            let mut notes = vec![];
            if both || want_login {
                notes.extend(launchd::uninstall());
            }
            if both || want_cli {
                notes.extend(uninstall_cli());
            }
            print_notes(&notes);
            let what = match (both || want_login, both || want_cli) {
                (true, true) => "login items and CLI link",
                (true, false) => "login items",
                _ => "CLI link",
            };
            println!("{} removed {what} (config in {} kept)", c::green("✓"), paths::home().display());
            Ok(())
        }
        "repair" => {
            // Fix things that are installed but point at a stale binary (e.g. the app moved or was updated).
            let s = status_json();
            let mut notes = vec![];
            // --keep-daemon: never stop or replace a running daemon (the app uses it until it has Full Disk Access).
            let keep = keep_daemon && client::read_info().filter(client::ping).is_some();
            if s["login"]["installed"].as_bool() == Some(true) && !keep {
                notes.extend(install_login(if s["login"]["appInstalled"].as_bool() == Some(true) { app.as_deref() } else { None }, false)?);
            }
            if s["cli"]["installed"].as_bool() != Some(true) && has_stale_cli_link() {
                notes.extend(install_cli(bin_dir.as_deref())?);
            }
            if !quiet {
                print_notes(&notes);
                println!("{} {}", c::green("✓"), if notes.is_empty() { "everything looks good" } else { "repaired" });
            }
            Ok(())
        }
        _ => {
            if !want_cli && !want_login {
                want_cli = true;
                want_login = true;
            }
            let mut failed = None;
            if want_cli {
                match install_cli(bin_dir.as_deref()) {
                    Ok(n) => {
                        print_notes(&n);
                        println!("{} `lstack` command installed", c::green("✓"));
                    }
                    Err(e) => failed = Some(e),
                }
            }
            if want_login {
                let n = install_login(app.as_deref(), true)?;
                print_notes(&n);
                println!("{} lstack starts at login{}", c::green("✓"), if app.is_some() { " (with the menu bar app)" } else { "" });
                println!("  {}", c::dim("services marked autostart (lstack enable <name>) start automatically"));
            }
            match failed {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DaemonInfo;

    fn info(version: &str, protocol: u32) -> DaemonInfo {
        DaemonInfo { pid: 1, port: 1, token: String::new(), version: version.into(), protocol, started_at: 0, bin: Some("/nonexistent".into()), running: Some(true) }
    }

    #[test]
    fn daemon_is_stale_only_below_min_protocol() {
        if std::env::var_os("LSTACK_STRICT_DAEMON").is_some() {
            return;
        }
        assert!(daemon_stale(&info("0.9.0", 0)).is_some());
        assert!(daemon_stale(&info("0.1.0", paths::MIN_DAEMON_PROTOCOL)).is_none());
        assert!(!daemon_upgradable(&info("0.1.0", paths::PROTOCOL)));
    }
}

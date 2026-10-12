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
    let terminals: Vec<Value> = TERMINALS
        .iter()
        .map(|(id, name, bundle)| json!({ "id": id, "name": name, "installed": app_path(bundle).is_some() }))
        .collect();
    json!({
        "terminal": current(),
        "terminals": terminals,
        "fullDiskAccess": full_disk_access(),
        "helperPath": paths::self_exe(),
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

/// Open the preferred terminal app running `lstack <args>`.
pub fn open(args: &[&str]) -> Result<(), String> {
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
    let term = current();
    let res = match term {
        "iterm" => osascript(&format!(
            "tell application \"iTerm\"\n  activate\n  set w to (create window with default profile)\n  tell current session of w to write text {}\nend tell",
            applescript_str(&line)
        )),
        "cmux" => {
            let app = app_path("cmux.app").unwrap_or_default();
            let cli = app.join("Contents/Resources/bin/cmux");
            let title = format!("lstack {}", args.join(" "));
            spawn(Command::new(&cli).args(["new-workspace", "--name", title.trim(), "--focus", "true", "--command", &line]))
                .and_then(|_| spawn(Command::new("/usr/bin/open").arg(&app)))
        }
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
    res.map_err(|e| format!("couldn't open {term}: {e}"))
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

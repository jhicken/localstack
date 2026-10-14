use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Config folder: `$LSTACK_HOME` or `~/.localstack`.
pub fn home() -> PathBuf {
    match std::env::var_os("LSTACK_HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => user_home().join(".localstack"),
    }
}

pub fn services_dir() -> PathBuf {
    home().join("services")
}
pub fn logs_dir() -> PathBuf {
    home().join("logs")
}
pub fn db() -> PathBuf {
    home().join("metrics.db")
}
pub fn daemon_info() -> PathBuf {
    home().join("daemon.json")
}
pub fn daemon_log() -> PathBuf {
    logs_dir().join("_daemon.log")
}
pub fn remote_config() -> PathBuf {
    home().join("remote.json")
}
pub fn settings_file() -> PathBuf {
    home().join("settings.json")
}
pub fn state_file() -> PathBuf {
    home().join("state.json")
}
/// Scripts that were running when the daemon was restarted (not stopped), so the next daemon starts them again.
pub fn resume_file() -> PathBuf {
    home().join("resume.json")
}

pub fn default_port() -> u16 {
    std::env::var("LSTACK_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(7317)
}

pub fn ensure_dirs() {
    for d in [home(), services_dir(), logs_dir()] {
        let _ = std::fs::create_dir_all(d);
    }
}

pub fn expand_home(p: &str) -> String {
    if p == "~" {
        return user_home().to_string_lossy().into_owned();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return user_home().join(rest).to_string_lossy().into_owned();
    }
    p.to_string()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Canonical path of the running binary (resolves symlinks such as ~/.local/bin/lstack).
pub fn self_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lstack"));
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

pub fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn parse_iso(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

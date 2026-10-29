//! Client-side 1Password CLI runner. Only the `lstack` CLI uses this, after a person runs a command:
//! the daemon has no code that can reach 1Password. Before every batch it checks that `op` is
//! signed by AgileBits, runs it with a minimal environment (so a stray service-account token can't
//! skip the approval prompt), and reads values from stdout pipes only.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use zeroize::Zeroizing;

/// AgileBits Inc. (1Password) Apple Developer Team ID.
pub const AGILEBITS_TEAM: &str = "2BUA8C4S2C";
const READ_TIMEOUT: Duration = Duration::from_secs(180);

pub struct Op {
    pub bin: PathBuf,
    pub account: Option<String>,
}

fn candidates() -> Vec<PathBuf> {
    // Relative PATH entries (`.`, `bin`) would resolve against wherever the command runs: skipped.
    let mut v: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).map(|d| d.join("op")).collect()).unwrap_or_default();
    v.extend(["/opt/homebrew/bin/op", "/usr/local/bin/op"].map(PathBuf::from));
    v
}

/// Code signing requirement: a certificate chain issued by Apple whose leaf is AgileBits' Team ID. Without the
/// `anchor apple generic` part a self-made certificate claiming that Team ID would pass.
fn requirement() -> String {
    format!("=anchor apple generic and certificate leaf[subject.OU] = \"{AGILEBITS_TEAM}\"")
}

/// `codesign` says it's intact and signed by AgileBits (Apple-issued certificate).
pub fn verify(bin: &Path) -> Result<(), String> {
    let ok = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(format!("-R{}", requirement()))
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !ok.is_ok_and(|s| s.success()) {
        return Err(format!("{} isn't validly signed by 1Password ({AGILEBITS_TEAM}); refusing to run it", bin.display()));
    }
    let out = Command::new("/usr/bin/codesign").args(["-d", "--verbose=2"]).arg(bin).output().map_err(|e| format!("codesign: {e}"))?;
    let info = String::from_utf8_lossy(&out.stderr);
    let team = info.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).unwrap_or("none");
    if team != AGILEBITS_TEAM {
        return Err(format!("{} is signed by team {team}, not 1Password ({AGILEBITS_TEAM}); refusing to run it", bin.display()));
    }
    Ok(())
}

impl Op {
    /// Find and verify `op`. `account` (or `$OP_ACCOUNT`) picks the 1Password account.
    pub fn find(account: Option<String>) -> Result<Op, String> {
        let bin = candidates()
            .into_iter()
            .find(|p| p.is_file())
            .ok_or("1Password CLI (op) not found. Install it (brew install 1password-cli) and turn on Settings › Developer › Integrate with 1Password CLI in the 1Password app.")?;
        let bin = std::fs::canonicalize(&bin).unwrap_or(bin);
        verify(&bin)?;
        let account = account.or_else(|| std::env::var("OP_ACCOUNT").ok()).filter(|a| !a.is_empty());
        Ok(Op { bin, account })
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        // Minimal env: no OP_SERVICE_ACCOUNT_TOKEN / OP_CONNECT_* / OP_SESSION_*, so every read goes
        // through the 1Password app and its approval prompt.
        c.env_clear().env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        for k in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "TERM", "OP_CONFIG_DIR", "XDG_CONFIG_HOME"] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        if let Some(a) = &self.account {
            c.env("OP_ACCOUNT", a);
        }
        c
    }

    /// `op read --no-newline <ref>`. The reference is in argv (it's not secret); the value only
    /// travels through a pipe into a zeroizing buffer.
    pub fn read(&self, reference: &str) -> Result<Zeroizing<String>, String> {
        let mut child = self
            .command()
            .args(["read", "--no-newline", "--", reference])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("op: {e}"))?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let pid = child.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Zeroizing::new(Vec::new());
            let r = stdout.read_to_end(&mut buf);
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            let _ = tx.send((r.map(|_| buf), err));
        });
        let res = rx.recv_timeout(READ_TIMEOUT);
        if res.is_err() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        let (buf, err) = res.map_err(|_| format!("{reference}: timed out waiting for 1Password (approve the request in the 1Password app)"))?;
        let buf = buf.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("{reference}: {}", explain(&err)));
        }
        let s = String::from_utf8(buf.to_vec()).map_err(|_| format!("{reference}: value isn't UTF-8 text"))?;
        Ok(Zeroizing::new(s))
    }

    /// End the CLI's app-integration session so a later `op` call needs a new approval.
    pub fn signout(&self) {
        let _ = self.command().arg("signout").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// Turn `op` errors into something actionable.
fn explain(stderr: &str) -> String {
    let mut e = stderr.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).next_back().unwrap_or("op failed");
    // "[ERROR] 2024/01/01 12:00:00 message" → "message"
    if let Some(r) = e.strip_prefix("[ERROR]") {
        e = r.trim();
        let parts: Vec<&str> = e.splitn(3, ' ').collect();
        if parts.len() == 3 && parts[0].contains('/') && parts[1].contains(':') {
            e = parts[2];
        }
    }
    let low = stderr.to_lowercase();
    let hint = if low.contains("dismissed") || low.contains("denied") || low.contains("cancel") {
        " (the request was declined in 1Password)"
    } else if low.contains("not signed in") || low.contains("no accounts configured") || low.contains("connecting to desktop app") {
        " (open the 1Password app, unlock it, and turn on Settings › Developer › Integrate with 1Password CLI)"
    } else if low.contains("isn't a vault") || low.contains("isn't an item") || low.contains("could not find") || low.contains("no item") {
        " (check the vault, item and field names)"
    } else if low.contains("multiple accounts") || low.contains("account") && low.contains("specify") {
        " (pick one with --account or $OP_ACCOUNT)"
    } else {
        ""
    };
    format!("{e}{hint}")
}

/// Read a 1Password Environment mount (a named pipe the 1Password app serves; reading asks for
/// approval). Gives up after a while instead of hanging if nobody approves.
pub fn read_mount(path: &Path) -> Result<Zeroizing<String>, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if md.uid() != unsafe { libc::getuid() } {
        return Err(format!("{} isn't owned by you", path.display()));
    }
    if !(md.file_type().is_fifo() || md.is_file()) {
        return Err(format!("{} isn't a file or 1Password Environment mount", path.display()));
    }
    let p = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = std::fs::File::open(&p).and_then(|f| {
            let mut buf = Zeroizing::new(Vec::new());
            f.take(1 << 20).read_to_end(&mut buf).map(|_| buf)
        });
        let _ = tx.send(r);
    });
    let buf = rx
        .recv_timeout(READ_TIMEOUT)
        .map_err(|_| format!("{}: timed out waiting for 1Password (approve the request in the 1Password app)", path.display()))?
        .map_err(|e| format!("{}: {e}", path.display()))?;
    String::from_utf8(buf.to_vec()).map(Zeroizing::new).map_err(|_| "environment isn't UTF-8 text".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_unsigned_binaries() {
        // /bin/ls is signed by Apple, not AgileBits.
        let e = verify(Path::new("/bin/ls")).unwrap_err();
        assert!(e.contains("isn't validly signed by 1Password"), "{e}");
        let d = std::env::temp_dir().join("lstack-fake-op");
        std::fs::write(&d, "#!/bin/sh\necho hi\n").unwrap();
        assert!(verify(&d).unwrap_err().contains("refusing"));
    }

    #[test]
    fn explains_errors() {
        assert!(explain("[ERROR] 2024/01/01 authorization prompt dismissed, please try again").contains("declined"));
        assert!(explain("[ERROR] could not read secret: \"Nope\" isn't a vault in this account").contains("vault, item"));
    }
}

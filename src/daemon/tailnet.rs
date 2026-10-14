//! The Tailscale transport: installs, verifies, runs and talks to `lstack-tailnet`, LStack's own userspace
//! Tailscale node (see `tailnet/` and docs/TAILSCALE.md).
//!
//! - **Install/update:** the signed manifest from the latest GitHub release names the helper tarball and its
//!   SHA-256. The manifest must be signed by [`PUBLIC_KEY_HEX`]; the binary must pass `codesign --verify` as
//!   `dev.lstack.tailnet` and report the manifest's version. Older versions are never installed.
//!   Binaries live in `~/.localstack/bin/lstack-tailnet-<ver>` with `lstack-tailnet` linking the current one.
//! - **Supervise:** the helper runs while the Tailscale transport is on and restarts with backoff. It exits
//!   on its own when its stdin (held here) closes.
//! - **Talk:** HTTP over `~/.localstack/tailnet/ctl.sock` (status, login, CONNECT to dial a tailnet address);
//!   inbound tailnet links arrive on `link.sock` behind a one-line identity header and go through the same
//!   accept path as LAN links (`share::connection`).
use super::remote::{Remote, TailnetId};
use super::server::{ApiErr, AppState};
use super::share::Via;
use crate::paths::{self, now_ms, VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Notify};

/// Signs the helper's release manifest (`tailnet/build.sh`; the private key never leaves the release Mac/CI).
pub const PUBLIC_KEY_HEX: &str = "196ad304edf3c198475168e70ae6087fbad9c79565cc5baada10aeb5dfde0843";
const RELEASES: &str = "https://github.com/jhicken/localstack/releases/latest/download";
const UPDATE_EVERY_MS: i64 = 24 * 3_600_000;
/// Port the helper listens on inside the tailnet (same as the LAN link).
pub const TAILNET_PORT: u16 = 7381;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TailnetConfig {
    /// Run the helper (the Tailscale transport is available).
    pub enabled: bool,
    /// Install helper updates automatically (missing = on).
    pub auto_update: Option<bool>,
    /// Installed helper version.
    pub helper_version: Option<String>,
    /// The exposure warning was shown and accepted.
    pub acknowledged: bool,
    /// When updates were last checked (ms).
    pub last_check: Option<i64>,
}

impl TailnetConfig {
    pub fn auto_update(&self) -> bool {
        self.auto_update.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: String,
    pub asset: String,
    pub sha256: String,
    pub min_l_stack: String,
}

pub fn dir() -> PathBuf {
    paths::home().join("tailnet")
}
pub fn ctl_sock() -> PathBuf {
    dir().join("ctl.sock")
}
pub fn link_sock() -> PathBuf {
    dir().join("link.sock")
}
fn bin_dir() -> PathBuf {
    paths::home().join("bin")
}
fn current_link() -> PathBuf {
    bin_dir().join("lstack-tailnet")
}

/// Compare dotted/dashed versions numerically ("1.104.1-2" > "1.104.1-1" > "1.98.0").
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> { s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(a).cmp(&parts(b))
}

fn public_key() -> String {
    #[cfg(debug_assertions)]
    if let Ok(k) = std::env::var("LSTACK_TAILNET_PUBKEY") {
        return k;
    }
    PUBLIC_KEY_HEX.to_string()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Check the manifest's signature and that it's an acceptable upgrade from `installed`.
/// Ok(None) = signed and valid but nothing newer.
pub fn verify_manifest(body: &[u8], sig_hex: &str, public_hex: &str, installed: Option<&str>, lstack: &str) -> Result<Option<Manifest>, String> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let pk: [u8; 32] = unhex(public_hex).and_then(|v| v.try_into().ok()).ok_or("bad embedded public key")?;
    let sig: [u8; 64] = unhex(sig_hex).and_then(|v| v.try_into().ok()).ok_or("the helper manifest signature is malformed")?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|_| "bad embedded public key")?;
    vk.verify_strict(body, &Signature::from_bytes(&sig)).map_err(|_| "the helper manifest isn't signed by LStack — refusing to install it")?;
    let m: Manifest = serde_json::from_slice(body).map_err(|e| format!("the helper manifest is invalid: {e}"))?;
    let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !safe(&m.version) || !safe(&m.asset) || unhex(&m.sha256).is_none_or(|v| v.len() != 32) {
        return Err("the helper manifest is invalid".into());
    }
    if version_cmp(&m.min_l_stack, lstack).is_gt() {
        return Err(format!("helper {} needs LStack {} or newer — update LStack first", m.version, m.min_l_stack));
    }
    match installed {
        Some(cur) if version_cmp(&m.version, cur).is_le() => Ok(None),
        _ => Ok(Some(m)),
    }
}

fn curl(url: &str, out: &Path) -> Result<(), String> {
    let proto = if url.starts_with("file://") { "=file" } else { "=https" };
    let o = std::process::Command::new("/usr/bin/curl")
        .args(["-fsSL", "--proto", proto, "--max-time", "300", "--retry", "2", "-o"])
        .arg(out)
        .arg(url)
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !o.status.success() {
        return Err(format!("download failed ({url}): {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(())
}

fn releases_base() -> String {
    std::env::var("LSTACK_TAILNET_RELEASES").unwrap_or_else(|_| RELEASES.to_string())
}

/// Download, verify and install the latest helper if it's newer than `installed` (blocking).
/// Returns the newly installed version, or None when already up to date.
pub fn install_latest(installed: Option<&str>) -> Result<Option<String>, String> {
    use sha2::{Digest, Sha256};
    let base = releases_base();
    let work = dir().join("download");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create {}: {e}", work.display()))?;
    let result = (|| {
        let (mf, sf) = (work.join("manifest.json"), work.join("manifest.sig"));
        curl(&format!("{base}/tailnet-manifest.json"), &mf)?;
        curl(&format!("{base}/tailnet-manifest.sig"), &sf)?;
        let body = std::fs::read(&mf).map_err(|e| e.to_string())?;
        let sig = std::fs::read_to_string(&sf).map_err(|e| e.to_string())?;
        // A helper that's missing on disk is reinstalled even at the same version.
        let have = installed.filter(|_| current_link().exists());
        let Some(m) = verify_manifest(&body, &sig, &public_key(), have, VERSION)? else { return Ok(None) };
        let tgz = work.join(&m.asset);
        curl(&format!("{base}/{}", m.asset), &tgz)?;
        let data = std::fs::read(&tgz).map_err(|e| e.to_string())?;
        let got: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        if !got.eq_ignore_ascii_case(&m.sha256) {
            return Err(format!("the helper download is corrupt (sha256 {got}, expected {})", m.sha256));
        }
        let x = std::process::Command::new("/usr/bin/tar").arg("-xzf").arg(&tgz).arg("-C").arg(&work).arg("lstack-tailnet").output().map_err(|e| e.to_string())?;
        let bin = work.join("lstack-tailnet");
        if !x.status.success() || !bin.is_file() {
            return Err(format!("cannot unpack the helper: {}", String::from_utf8_lossy(&x.stderr).trim()));
        }
        verify_binary(&bin, &m.version)?;
        std::fs::create_dir_all(bin_dir()).map_err(|e| e.to_string())?;
        let dest = bin_dir().join(format!("lstack-tailnet-{}", m.version));
        std::fs::rename(&bin, &dest).map_err(|e| format!("cannot install {}: {e}", dest.display()))?;
        let tmp_link = bin_dir().join(".lstack-tailnet.new");
        let _ = std::fs::remove_file(&tmp_link);
        std::os::unix::fs::symlink(dest.file_name().unwrap(), &tmp_link).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp_link, current_link()).map_err(|e| e.to_string())?;
        prune_old();
        Ok(Some(m.version))
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Codesigned as LStack's helper, and actually the version the manifest promised.
fn verify_binary(bin: &Path, version: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755));
    let cs = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--test-requirement==identifier \"dev.lstack.tailnet\""])
        .arg(bin)
        .output()
        .map_err(|e| e.to_string())?;
    if !cs.status.success() {
        return Err(format!("the helper's code signature is invalid: {}", String::from_utf8_lossy(&cs.stderr).trim()));
    }
    let v = std::process::Command::new(bin).arg("version").output().map_err(|e| format!("cannot run the helper: {e}"))?;
    let got = String::from_utf8_lossy(&v.stdout).trim().to_string();
    if got != version {
        return Err(format!("the helper reports version {got:?}, expected {version}"));
    }
    Ok(())
}

/// Keep the current helper and the one before it (for rollback).
fn prune_old() {
    let Ok(rd) = std::fs::read_dir(bin_dir()) else { return };
    let mut vers: Vec<String> = rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_prefix("lstack-tailnet-").map(str::to_string)).filter(|v| v != "dev").collect();
    vers.sort_by(|a, b| version_cmp(b, a));
    for v in vers.iter().skip(2) {
        let _ = std::fs::remove_file(bin_dir().join(format!("lstack-tailnet-{v}")));
    }
}

/// The helper to run: `LSTACK_TAILNET_BIN`, else the installed one.
fn helper_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LSTACK_TAILNET_BIN").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let p = current_link();
    p.exists().then_some(p)
}

fn device_name() -> String {
    if let Ok(n) = std::env::var("LSTACK_TAILNET_HOSTNAME") {
        return n;
    }
    let lh = super::remote::scutil(&["--get", "LocalHostName"]);
    let base = if lh.trim().is_empty() { super::remote::hostname() } else { lh.trim().to_string() };
    let clean: String = base.split('.').next().unwrap_or("mac").to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    format!("lstack-{}", clean.trim_matches('-'))
}

/// One HTTP request to the helper's control socket.
pub async fn ctl(method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
    let fut = async {
        let mut s = UnixStream::connect(ctl_sock()).await.map_err(|e| format!("the Tailscale helper isn't running ({e})"))?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let req = format!("{method} {path} HTTP/1.1\r\nHost: lstack-tailnet\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        s.take(4 << 20).read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf);
        let (head, rest) = text.split_once("\r\n\r\n").ok_or("bad reply from the Tailscale helper")?;
        let code: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("bad reply from the Tailscale helper")?;
        let rest = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_string() };
        Ok((code, serde_json::from_str(rest.trim()).unwrap_or(Value::Null)))
    };
    tokio::time::timeout(Duration::from_secs(10), fut).await.map_err(|_| "the Tailscale helper didn't answer".to_string())?
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((len, after)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(len.trim(), 16) else { break };
        if n == 0 || after.len() < n {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].strip_prefix("\r\n").unwrap_or(&after[n..]);
    }
    out
}

/// A raw stream to `host:port` inside the tailnet (CONNECT through the helper).
pub async fn dial(target: &str) -> Result<UnixStream, String> {
    let fut = async {
        let mut s = UnixStream::connect(ctl_sock()).await.map_err(|_| "Tailscale is off on this Mac (its helper isn't running)".to_string())?;
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        // Read the reply head byte by byte so nothing after it is consumed.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 8192 || s.read_exact(&mut b).await.is_err() {
                return Err("bad reply from the Tailscale helper".to_string());
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head);
        if head.split_whitespace().nth(1) == Some("200") {
            return Ok(s);
        }
        let mut rest = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_string(&mut rest)).await;
        let msg = serde_json::from_str::<Value>(rest.trim()).ok().and_then(|v| v["error"].as_str().map(str::to_string));
        Err(msg.unwrap_or_else(|| format!("cannot reach {target} over Tailscale")))
    };
    tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| format!("timed out reaching {target} over Tailscale"))?
}

fn same_user(s: &UnixStream) -> bool {
    s.peer_cred().is_ok_and(|c| c.uid() == unsafe { libc::getuid() })
}

#[derive(Deserialize, Default)]
struct Header {
    src: String,
    node: String,
    user: String,
    tailnet: String,
}

async fn read_header(s: &mut UnixStream) -> Option<Header> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() > 4096 {
            return None;
        }
        s.read_exact(&mut b).await.ok()?;
        line.push(b[0]);
    }
    serde_json::from_slice(&line).ok()
}

/// Inbound tailnet links, from the helper.
async fn accept_links(l: tokio::net::UnixListener, app: AppState, remote: Arc<Remote>) {
    loop {
        let Ok((mut s, _)) = l.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if !same_user(&s) {
            continue;
        }
        let (app, remote) = (app.clone(), remote.clone());
        tokio::spawn(async move {
            let Ok(Some(h)) = tokio::time::timeout(Duration::from_secs(5), read_header(&mut s)).await else { return };
            if !remote.config().sharing() {
                return;
            }
            let addr = h.src.parse().unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
            let via = Via::Tailnet(TailnetId { tailnet: h.tailnet, user: h.user, node: h.node });
            super::share::connection(s, addr, via, app, remote).await;
        });
    }
}

#[derive(Default)]
struct St {
    phase: &'static str,
    error: Option<String>,
    /// Last `/status` from the helper.
    helper: Value,
    running_version: Option<String>,
    checking: bool,
}

/// Runs the helper while the Tailscale transport is on.
pub struct Tailnet {
    st: Mutex<St>,
    wake: Notify,
    restart: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// New helper version after an update was installed (paired Macs are asked to follow).
    pub updated: broadcast::Sender<String>,
}

impl Tailnet {
    pub fn new() -> Arc<Self> {
        Arc::new(Tailnet { st: Mutex::new(St { phase: "off", ..Default::default() }), wake: Notify::new(), restart: AtomicBool::new(false), task: Mutex::new(None), updated: broadcast::channel(8).0 })
    }

    /// Start the supervisor if it isn't running, and make it re-read the config.
    pub fn apply(self: &Arc<Self>, app: &AppState, remote: &Arc<Remote>) {
        let mut task = self.task.lock().unwrap();
        if task.as_ref().is_none_or(|t| t.is_finished()) {
            let (me, app, remote) = (self.clone(), app.clone(), remote.clone());
            *task = Some(tokio::spawn(async move { me.run(app, remote).await }));
        } else {
            self.wake.notify_one();
        }
    }

    pub fn stop(&self) {
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
        let _ = std::fs::remove_file(link_sock());
    }

    /// Restart the helper (after an update, or to pick up a new binary).
    pub fn restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn set(&self, phase: &'static str, error: Option<String>) {
        let mut st = self.st.lock().unwrap();
        st.phase = phase;
        st.error = error;
        if matches!(phase, "off" | "installing" | "error") {
            st.helper = Value::Null;
        }
    }

    /// The helper is up and signed in.
    pub fn running(&self) -> bool {
        self.st.lock().unwrap().phase == "running"
    }

    /// This Mac's tailnet DNS name and IPs, when running.
    pub fn self_addrs(&self) -> Option<(String, Vec<String>)> {
        let st = self.st.lock().unwrap();
        let s = st.helper.get("self")?;
        let dns = s["dnsName"].as_str().filter(|d| !d.is_empty())?.to_string();
        let ips = s["ips"].as_array().map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Some((dns, ips))
    }

    pub fn version(&self) -> Option<String> {
        self.st.lock().unwrap().running_version.clone()
    }

    pub fn phase(&self) -> &'static str {
        self.st.lock().unwrap().phase
    }

    /// The tailnet this Mac's helper is signed in to (its display name, e.g. "jeff@github").
    pub fn tailnet_name(&self) -> Option<String> {
        self.st.lock().unwrap().helper["tailnet"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
    }

    /// The tailnet as connections through the helper identify it: this Mac's MagicDNS domain
    /// ("lstack-studio.tail1234.ts.net" → "tail1234.ts.net"), the same as the helper's per-connection header.
    pub fn tailnet_id(&self) -> Option<String> {
        let st = self.st.lock().unwrap();
        st.helper["self"]["dnsName"].as_str().and_then(tailnet_of)
    }

    pub fn status(&self, cfg: &TailnetConfig) -> Value {
        let st = self.st.lock().unwrap();
        let h = &st.helper;
        let peers: Vec<&Value> = h["peers"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        json!({
            "enabled": cfg.enabled,
            "phase": st.phase,
            "error": st.error,
            "autoUpdate": cfg.auto_update(),
            "acknowledged": cfg.acknowledged,
            "installedVersion": cfg.helper_version,
            "version": st.running_version,
            "lastCheck": cfg.last_check,
            "checking": st.checking,
            "devBinary": std::env::var_os("LSTACK_TAILNET_BIN").is_some(),
            "deviceName": device_name(),
            "authURL": h["authURL"],
            "self": h["self"],
            "tailnet": h["tailnet"],
            "user": h["user"],
            "health": h["health"].as_array().cloned().unwrap_or_default(),
            "peers": peers,
        })
    }

    /// Check for (and install) a newer helper. Ok(Some(v)) when one was installed; the helper restarts.
    pub async fn check_update(self: &Arc<Self>, remote: &Arc<Remote>) -> Result<Option<String>, String> {
        if std::env::var_os("LSTACK_TAILNET_BIN").is_some() {
            return Ok(None);
        }
        {
            let mut st = self.st.lock().unwrap();
            if st.checking {
                return Err("already checking for an update".into());
            }
            st.checking = true;
        }
        let installed = remote.config().tailnet.helper_version;
        let res = tokio::task::spawn_blocking(move || install_latest(installed.as_deref())).await.unwrap_or_else(|e| Err(e.to_string()));
        self.st.lock().unwrap().checking = false;
        let _ = remote.set_tailnet(|t| {
            t.last_check = Some(now_ms());
            if let Ok(Some(v)) = &res {
                t.helper_version = Some(v.clone());
            }
        });
        if let Ok(Some(v)) = &res {
            log(&format!("installed helper {v}"));
            self.restart();
            let _ = self.updated.send(v.clone());
        }
        res
    }

    async fn run(self: Arc<Self>, app: AppState, remote: Arc<Remote>) {
        let mut backoff = 1u64;
        loop {
            let cfg = remote.config().tailnet;
            self.restart.store(false, Ordering::SeqCst);
            if !cfg.enabled {
                self.set("off", None);
                self.st.lock().unwrap().running_version = None;
                let _ = std::fs::remove_file(link_sock());
                self.wake.notified().await;
                continue;
            }
            let bin = match helper_bin() {
                Some(b) => b,
                None => {
                    self.set("installing", None);
                    let res = self.check_update(&remote).await;
                    // Nothing is running yet, so the update's restart request doesn't apply.
                    self.restart.store(false, Ordering::SeqCst);
                    match res {
                        Ok(_) => match helper_bin() {
                            Some(b) => b,
                            None => {
                                self.set("error", Some("no Tailscale helper is available for this LStack version".into()));
                                self.pause(&mut backoff).await;
                                continue;
                            }
                        },
                        Err(e) => {
                            log(&format!("install failed: {e}"));
                            self.set("error", Some(e));
                            self.pause(&mut backoff).await;
                            continue;
                        }
                    }
                }
            };
            let started = now_ms();
            let clean = self.run_once(&bin, &app, &remote).await;
            if clean || now_ms() - started > 60_000 {
                backoff = 1;
            }
            if !clean {
                self.pause(&mut backoff).await;
            }
        }
    }

    /// Wait before retrying (or until something changes).
    async fn pause(&self, backoff: &mut u64) {
        let _ = tokio::time::timeout(Duration::from_secs(*backoff), self.wake.notified()).await;
        *backoff = (*backoff * 2).min(120);
    }

    /// Run the helper until it exits (false) or is asked to stop/restart (true).
    async fn run_once(self: &Arc<Self>, bin: &Path, app: &AppState, remote: &Arc<Remote>) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_file(link_sock());
        let listener = match tokio::net::UnixListener::bind(link_sock()) {
            Ok(l) => l,
            Err(e) => {
                self.set("error", Some(format!("cannot listen on {}: {e}", link_sock().display())));
                return false;
            }
        };
        let _ = std::fs::set_permissions(link_sock(), std::fs::Permissions::from_mode(0o600));
        let accept = tokio::spawn(accept_links(listener, app.clone(), remote.clone()));
        let _guard = AbortOnDrop(accept);

        self.set("starting", None);
        let ver = std::process::Command::new(bin).arg("version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        self.st.lock().unwrap().running_version = ver.filter(|v| !v.is_empty());
        let mut child = match tokio::process::Command::new(bin)
            .arg("--dir")
            .arg(&d)
            .args(["--hostname", &device_name(), "--port", &TAILNET_PORT.to_string()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.set("error", Some(format!("cannot start the Tailscale helper: {e}")));
                return false;
            }
        };
        log(&format!("helper started (pid {})", child.id().unwrap_or(0)));
        let last_err = Arc::new(Mutex::new(None::<String>));
        if let Some(err) = child.stderr.take() {
            let last = last_err.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let v: Value = serde_json::from_str(&l).unwrap_or(json!({ "msg": l }));
                    let ev = v["event"].as_str().unwrap_or("");
                    if ev == "fatal" {
                        *last.lock().unwrap() = v["error"].as_str().map(str::to_string);
                    }
                    let msg = v.get("msg").or(v.get("error")).and_then(|m| m.as_str()).unwrap_or("");
                    // Sign-in links are one-time credentials: shown in the app, not kept in the log.
                    if msg.contains("login.tailscale.com") || msg.contains("To authenticate") {
                        continue;
                    }
                    match ev {
                        "ready" => log(&format!("helper {} ready", v["version"].as_str().unwrap_or("?"))),
                        "" => log(&l),
                        _ => log(&format!("{ev}: {msg}")),
                    }
                }
            });
        }
        let _stdin = child.stdin.take();
        let mut poll_every = Duration::from_millis(500);
        loop {
            tokio::select! {
                st = child.wait() => {
                    let why = last_err.lock().unwrap().clone().unwrap_or_else(|| format!("the Tailscale helper exited ({})", st.map(|s| s.to_string()).unwrap_or_default()));
                    log(&why);
                    self.set("error", Some(why));
                    return false;
                }
                _ = self.wake.notified() => {
                    if self.restart.load(Ordering::SeqCst) || !remote.config().tailnet.enabled {
                        let _ = child.kill().await;
                        log("helper stopped");
                        return true;
                    }
                }
                _ = tokio::time::sleep(poll_every) => {
                    self.poll(remote).await;
                    poll_every = if self.running() { Duration::from_secs(5) } else { Duration::from_secs(1) };
                }
            }
        }
    }

    async fn poll(self: &Arc<Self>, remote: &Arc<Remote>) {
        let Ok((200, v)) = ctl("GET", "/status", None).await else { return };
        let phase = match v["state"].as_str().unwrap_or("") {
            "Running" => "running",
            "NeedsLogin" | "NeedsMachineAuth" => "needs-login",
            "Stopped" => "needs-login",
            _ => "starting",
        };
        let was = self.st.lock().unwrap().phase;
        {
            let mut st = self.st.lock().unwrap();
            st.helper = v;
            st.phase = phase;
            st.error = None;
        }
        if phase == "running" && was != "running" {
            log("connected to the tailnet");
        }
        let cfg = remote.config().tailnet;
        if phase == "running" && cfg.auto_update() && now_ms() - cfg.last_check.unwrap_or(0) > UPDATE_EVERY_MS && !self.st.lock().unwrap().checking {
            let (me, remote) = (self.clone(), remote.clone());
            tokio::spawn(async move {
                if let Err(e) = me.check_update(&remote).await {
                    log(&format!("update check failed: {e}"));
                }
            });
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn log(msg: &str) {
    println!("{} tailnet: {msg}", paths::iso(now_ms()));
}

/// `POST /api/tailnet` body.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TailnetPatch {
    pub enabled: Option<bool>,
    pub auto_update: Option<bool>,
    pub acknowledged: Option<bool>,
}

pub async fn api_update(remote: &Arc<Remote>, app: &AppState, p: TailnetPatch) -> Result<Value, ApiErr> {
    if p.enabled == Some(true) && !remote.config().tailnet.acknowledged && p.acknowledged != Some(true) {
        return Err(ApiErr::new(409, "turning on Tailscale exposes this Mac's machine link to your tailnet (beyond the local network) — confirm with acknowledged: true"));
    }
    let cfg = remote.set_tailnet(|t| {
        if let Some(e) = p.enabled {
            t.enabled = e;
        }
        if let Some(a) = p.auto_update {
            t.auto_update = Some(a);
        }
        if let Some(a) = p.acknowledged {
            t.acknowledged = t.acknowledged || a;
        }
    })?;
    remote.tailnet.apply(app, remote);
    if p.enabled.is_some() {
        log(if cfg.enabled { "turned on" } else { "turned off" });
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok(remote.tailnet.status(&cfg))
}

pub async fn api_login(remote: &Arc<Remote>, body: &Value) -> Result<Value, ApiErr> {
    let key = body["authKey"].as_str().unwrap_or("").trim().to_string();
    let (code, v) = ctl("POST", "/login", Some(&json!({ "authKey": key }))).await.map_err(|e| ApiErr::new(503, e))?;
    if code != 200 {
        return Err(ApiErr::new(400, v["error"].as_str().unwrap_or("sign-in failed").to_string()));
    }
    // The auth URL (or the Running state, with a key) shows up a moment later.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        remote.tailnet.poll(remote).await;
        let st = remote.tailnet.status(&remote.config().tailnet);
        if st["authURL"].as_str().is_some_and(|u| !u.is_empty()) || st["phase"] == "running" {
            return Ok(st);
        }
    }
    Ok(remote.tailnet.status(&remote.config().tailnet))
}

pub async fn api_logout(remote: &Arc<Remote>) -> Result<Value, ApiErr> {
    let (code, v) = ctl("POST", "/logout", None).await.map_err(|e| ApiErr::new(503, e))?;
    if code != 200 {
        return Err(ApiErr::new(500, v["error"].as_str().unwrap_or("sign-out failed").to_string()));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    remote.tailnet.poll(remote).await;
    Ok(remote.tailnet.status(&remote.config().tailnet))
}

pub async fn api_check_update(remote: &Arc<Remote>) -> Result<Value, ApiErr> {
    let updated = remote.tailnet.check_update(remote).await.map_err(|e| ApiErr::new(502, e))?;
    let mut st = remote.tailnet.status(&remote.config().tailnet);
    st["updated"] = json!(updated);
    Ok(st)
}

/// The tailnet part of a MagicDNS name, as the helper computes it.
fn tailnet_of(dns_name: &str) -> Option<String> {
    dns_name.trim_end_matches('.').split_once('.').map(|(_, d)| d.to_string()).filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_of_dns_name() {
        assert_eq!(tailnet_of("lstack-studio.tail1234.ts.net.").as_deref(), Some("tail1234.ts.net"));
        assert_eq!(tailnet_of("studio"), None);
    }
    use ed25519_dalek::{Signer, SigningKey};

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn manifest_verification() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = hex(sk.verifying_key().as_bytes());
        let body = br#"{"version":"1.104.1-2","asset":"lstack-tailnet-1.104.1-2-darwin-universal.tar.gz","sha256":"7d9bdc333493e503479d83ec1c84ede2863194832a403caf31819decac32e8ba","minLStack":"0.4.0"}"#;
        let sig = hex(&sk.sign(body).to_bytes());
        let m = verify_manifest(body, &sig, &pk, None, "0.4.2").unwrap().unwrap();
        assert_eq!(m.version, "1.104.1-2");
        assert!(verify_manifest(body, &sig, &pk, Some("1.104.1-1"), "0.4.2").unwrap().is_some());
        // Same or newer installed: nothing to do (never downgrade).
        assert!(verify_manifest(body, &sig, &pk, Some("1.104.1-2"), "0.4.2").unwrap().is_none());
        assert!(verify_manifest(body, &sig, &pk, Some("1.110.0-1"), "0.4.2").unwrap().is_none());
        // Needs a newer LStack.
        assert!(verify_manifest(body, &sig, &pk, None, "0.3.9").unwrap_err().contains("needs LStack"));
        // Tampered body, wrong key, garbage signature.
        let mut bad = body.to_vec();
        bad[15] = b'9';
        assert!(verify_manifest(&bad, &sig, &pk, None, "0.4.2").is_err());
        let other = hex(SigningKey::from_bytes(&[8u8; 32]).verifying_key().as_bytes());
        assert!(verify_manifest(body, &sig, &other, None, "0.4.2").is_err());
        assert!(verify_manifest(body, "zz", &pk, None, "0.4.2").is_err());
        // Signed but unsafe asset name.
        let evil = br#"{"version":"9","asset":"../../x","sha256":"7d9bdc333493e503479d83ec1c84ede2863194832a403caf31819decac32e8ba","minLStack":"0"}"#;
        assert!(verify_manifest(evil, &hex(&sk.sign(evil).to_bytes()), &pk, None, "0.4.2").is_err());
    }

    #[test]
    fn versions_compare_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("1.104.1-2", "1.104.1-1"), Greater);
        assert_eq!(version_cmp("1.98.0", "1.104.1-1"), Less);
        assert_eq!(version_cmp("0.4.2", "0.4.2"), Equal);
        assert_eq!(dechunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), "hello world");
    }
}

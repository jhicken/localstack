//! Write-only secret store. Values are encrypted at rest with ChaCha20-Poly1305 in `secrets.enc`.
//! The 256-bit data key is never stored whole: it is split into two XOR shares, one in a 0600 file
//! (`secrets.key`) and one in the login Keychain, and both are needed to decrypt. The daemon
//! rebuilds the key once at boot and keeps it in locked memory, so restarting services never
//! touches the Keychain. Values can be set, replaced or deleted but are never returned by any API;
//! they only reach the services that list them, as environment variables, and are scrubbed from logs.
use crate::paths;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

const AAD: &[u8] = b"lstack-secrets-v1";
const KEYCHAIN_SERVICE: &str = "dev.lstack.secrets";
const MAX_VALUE: usize = 64 * 1024;
/// Internal entry holding the Touch ID approval state (see `approvals.rs`). Never scrubbed or injected.
pub const APPROVALS_NAME: &str = "LSTACK_APPROVALS";
/// Names starting with this are used by LStack itself (cmux socket password, healthchecks ping key).
pub const INTERNAL_PREFIX: &str = "LSTACK_";

/// 32-byte key in mlock'd memory, wiped on drop.
struct Key(Box<[u8; 32]>);

impl Key {
    fn new(bytes: [u8; 32]) -> Key {
        let k = Key(Box::new(bytes));
        unsafe { libc::mlock(k.0.as_ptr() as *const libc::c_void, 32) };
        k
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
        unsafe { libc::munlock(self.0.as_ptr() as *const libc::c_void, 32) };
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    value: String,
    set_at: i64,
    #[serde(flatten)]
    meta: Meta,
}

/// Where a value came from. Never secret: shown in `lstack secret ls`, the app and the audit log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    /// `local` (typed in), `op` (1Password reference), `op-env` (1Password Environment), `peer:<machine>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The resolved `op://…` reference (or Environment mount `path#KEY`).
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The reference as written in the env file, before `$VAR` expansion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// 1Password account it was read from (`--account`), when not the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Absolute path of the env file that introduced it (for clean-up when the line is removed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// When the value was last read from 1Password (or received from a paired Mac), ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    /// For `peer:<machine>` values: that Mac's pinned link key (hex). Only it may update the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_key: Option<String>,
}

impl Meta {
    /// Pulled from 1Password on this Mac (and so shareable with a paired Mac that's been granted it).
    pub fn is_from_1password(&self) -> bool {
        matches!(self.source.as_deref(), Some("op" | "op-env"))
    }
    /// Received from a paired Mac (it read it from 1Password there). Never shared on.
    pub fn is_from_peer(&self) -> bool {
        self.source.as_deref().is_some_and(|s| s.starts_with("peer:"))
    }
    /// Cached for an env-file reference: from 1Password here or from a paired Mac.
    pub fn imported(&self) -> bool {
        self.is_from_1password() || self.is_from_peer()
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

/// A value as the running daemon holds it: encrypted under a per-boot key that never leaves locked
/// memory, opened only when it's injected, sent to a paired Mac or written to disk.
struct Held {
    nonce: [u8; 12],
    ct: Vec<u8>,
    set_at: i64,
    meta: Meta,
}

impl Held {
    fn seal(mem: &Key, name: &str, value: &str, set_at: i64, meta: Meta) -> Result<Held, String> {
        let nonce = random::<12>();
        let ct = ChaCha20Poly1305::new_from_slice(&mem.0[..])
            .map_err(|e| e.to_string())?
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: value.as_bytes(), aad: name.as_bytes() })
            .map_err(|_| "encryption failed".to_string())?;
        Ok(Held { nonce, ct, set_at, meta })
    }

    fn open(&self, mem: &Key, name: &str) -> Option<Zeroizing<String>> {
        let pt = ChaCha20Poly1305::new_from_slice(&mem.0[..])
            .ok()?
            .decrypt(Nonce::from_slice(&self.nonce), Payload { msg: &self.ct, aad: name.as_bytes() })
            .ok()?;
        String::from_utf8(pt).map(Zeroizing::new).map_err(|e| e.into_bytes().zeroize()).ok()
    }
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    v: u32,
    nonce: String,
    ct: String,
}

fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    buf
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn share32(s: &str) -> Option<[u8; 32]> {
    unhex(s).and_then(|v| v.try_into().ok())
}

fn write_private(path: &PathBuf, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 128 && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Where the second key share lives. Tests and sandboxes use a file instead of the Keychain.
enum ShareB {
    Keychain { account: String },
    File(PathBuf),
}

impl ShareB {
    fn read(&self) -> Result<Option<[u8; 32]>, String> {
        match self {
            ShareB::File(p) => match std::fs::read_to_string(p) {
                Ok(s) => share32(&s).map(Some).ok_or_else(|| "keychain share file is corrupt".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            ShareB::Keychain { account } => {
                // Created and read through /usr/bin/security, so the item's access list trusts that
                // (Apple-signed, stable) binary: no prompts, even after LStack is updated.
                let out = Command::new("/usr/bin/security")
                    .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", account, "-w"])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| e.to_string())?;
                if out.status.success() {
                    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                    let r = share32(&s).map(Some).ok_or_else(|| "Keychain share is corrupt".to_string());
                    s.zeroize();
                    r
                } else if out.status.code() == Some(44) {
                    Ok(None) // errSecItemNotFound
                } else {
                    Err(format!("Keychain: {}", String::from_utf8_lossy(&out.stderr).trim()))
                }
            }
        }
    }

    fn write(&self, share: &[u8; 32]) -> Result<(), String> {
        let mut h = hex(share);
        let r = match self {
            ShareB::File(p) => write_private(p, h.as_bytes()).map_err(|e| e.to_string()),
            ShareB::Keychain { account } => {
                // The share goes to `security -i` on stdin, not argv, so `ps` can't see it while it runs.
                if account.contains(['"', '\\', '\n', '\r']) {
                    return Err(format!("Keychain: can't use account name {account:?}"));
                }
                let mut cmd = format!(
                    "add-generic-password -U -s \"{KEYCHAIN_SERVICE}\" -a \"{account}\" -l \"LStack secret store key share\" -w {h}\n"
                );
                let mut child = Command::new("/usr/bin/security")
                    .arg("-i")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                let wrote = child.stdin.take().map(|mut i| std::io::Write::write_all(&mut i, cmd.as_bytes()));
                cmd.zeroize();
                let out = child.wait_with_output().map_err(|e| e.to_string())?;
                // `security -i` exits 0 even when a command fails: check by reading it back.
                match (wrote, self.read()) {
                    (Some(Ok(())), Ok(Some(back))) if back == *share => Ok(()),
                    _ => Err(format!("Keychain: couldn't store the key share {}", String::from_utf8_lossy(&out.stderr).trim())),
                }
            }
        };
        h.zeroize();
        r
    }
}

pub struct Vault {
    dir: PathBuf,
    share_b: ShareB,
    key: Option<Key>,
    /// Seals `values` in memory; random per process.
    mem: Key,
    values: BTreeMap<String, Held>,
    /// Why the store can't be read (missing share, Keychain locked, tampered file…).
    error: Option<String>,
    /// Bumped on every change, so watchers can tell when to re-check.
    generation: u64,
    /// The generation the log scrubber last saw (it's rebuilt only when values change).
    scrubbed: Option<u64>,
}

impl Vault {
    fn enc_file(&self) -> PathBuf {
        self.dir.join("secrets.enc")
    }
    fn share_file(&self) -> PathBuf {
        self.dir.join("secrets.key")
    }

    pub fn open(dir: PathBuf, keychain: bool) -> Vault {
        let share_b = if keychain {
            ShareB::Keychain { account: dir.to_string_lossy().into_owned() }
        } else {
            ShareB::File(dir.join("secrets.share2"))
        };
        let mut v = Vault { dir, share_b, key: None, mem: Key::new(random::<32>()), values: BTreeMap::new(), error: None, generation: 0, scrubbed: None };
        if let Err(e) = v.load() {
            v.error = Some(e);
        }
        v
    }

    fn load(&mut self) -> Result<(), String> {
        self.key = None;
        self.values.clear();
        self.generation += 1;
        let exists = self.enc_file().exists();
        let a = match std::fs::read_to_string(self.share_file()) {
            Ok(s) => Some(share32(&s).ok_or("secrets.key is corrupt")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot read secrets.key: {e}")),
        };
        if !exists {
            return Ok(()); // nothing stored yet; shares are created on the first `set`
        }
        let b = self.share_b.read()?;
        let (Some(a), Some(b)) = (a, b) else {
            return Err("secret store key share is missing (secrets.key or Keychain item); secrets cannot be decrypted".into());
        };
        let mut k = [0u8; 32];
        for i in 0..32 {
            k[i] = a[i] ^ b[i];
        }
        let key = Key::new(k);
        k.zeroize();
        let sealed: Sealed = serde_json::from_slice(&std::fs::read(self.enc_file()).map_err(|e| e.to_string())?)
            .map_err(|_| "secrets.enc is corrupt".to_string())?;
        let nonce = unhex(&sealed.nonce).filter(|n| n.len() == 12).ok_or("secrets.enc is corrupt")?;
        let ct = unhex(&sealed.ct).ok_or("secrets.enc is corrupt")?;
        let cipher = ChaCha20Poly1305::new_from_slice(&key.0[..]).map_err(|e| e.to_string())?;
        let mut pt = cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: AAD })
            .map_err(|_| "secrets.enc failed authentication (wrong key share or tampered file)".to_string())?;
        let parsed = serde_json::from_slice::<BTreeMap<String, Entry>>(&pt);
        pt.zeroize();
        for (name, e) in parsed.map_err(|_| "secrets.enc is corrupt".to_string())? {
            let h = Held::seal(&self.mem, &name, &e.value, e.set_at, e.meta.clone())?;
            self.values.insert(name, h);
        }
        self.key = Some(key);
        Ok(())
    }

    /// Make a new random key, split it into fresh shares and write both.
    fn new_key(&mut self) -> Result<(), String> {
        let mut k = random::<32>();
        let a = random::<32>();
        let mut b = [0u8; 32];
        for i in 0..32 {
            b[i] = k[i] ^ a[i];
        }
        // Keychain first: if it fails, the existing file share is still intact.
        self.share_b.write(&b)?;
        b.zeroize();
        let mut ah = hex(&a);
        let r = write_private(&self.share_file(), ah.as_bytes()).map_err(|e| format!("cannot write secrets.key: {e}"));
        ah.zeroize();
        r?;
        self.key = Some(Key::new(k));
        k.zeroize();
        Ok(())
    }

    fn save(&mut self) -> Result<(), String> {
        if self.key.is_none() {
            self.new_key()?;
        }
        let key = self.key.as_ref().unwrap();
        let cipher = ChaCha20Poly1305::new_from_slice(&key.0[..]).map_err(|e| e.to_string())?;
        let nonce = random::<12>();
        let mut plain = BTreeMap::new();
        for (name, h) in &self.values {
            let v = h.open(&self.mem, name).ok_or("secret store memory is corrupt")?;
            plain.insert(name.as_str(), Entry { value: v.to_string(), set_at: h.set_at, meta: h.meta.clone() });
        }
        let mut pt = serde_json::to_vec(&plain).map_err(|e| e.to_string())?;
        drop(plain);
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: &pt, aad: AAD });
        pt.zeroize();
        let ct = ct.map_err(|_| "encryption failed".to_string())?;
        let sealed = Sealed { v: 1, nonce: hex(&nonce), ct: hex(&ct) };
        write_private(&self.enc_file(), serde_json::to_string(&sealed).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())
    }

    fn ready(&self) -> Result<(), String> {
        match &self.error {
            Some(e) => Err(format!("secret store is locked: {e}")),
            None => Ok(()),
        }
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.ready()?;
        if !valid_name(name) {
            return Err(format!("invalid secret name {name:?} (letters, digits and _, not starting with a digit)"));
        }
        if value.is_empty() {
            return Err("secret value is empty".into());
        }
        if value.len() > MAX_VALUE {
            return Err("secret value is too large (64 KB max)".into());
        }
        self.set_many(vec![(name.to_string(), value.to_string(), Meta { source: Some("local".into()), ..Default::default() })])
    }

    fn check(name: &str, value: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("invalid secret name {name:?} (letters, digits and _, not starting with a digit)"));
        }
        if value.is_empty() {
            return Err(format!("{name}: secret value is empty"));
        }
        if value.len() > MAX_VALUE {
            return Err(format!("{name}: secret value is too large (64 KB max)"));
        }
        if value.contains('\0') {
            return Err(format!("{name}: secret value contains a NUL byte, which can't be passed as an environment variable"));
        }
        Ok(())
    }

    /// Set several values in one encrypted write: all or nothing.
    pub fn set_many(&mut self, items: Vec<(String, String, Meta)>) -> Result<(), String> {
        self.ready()?;
        for (n, v, _) in &items {
            Self::check(n, v)?;
        }
        let mut prev = Vec::new();
        for (name, mut value, meta) in items {
            let h = Held::seal(&self.mem, &name, &value, paths::now_ms(), meta);
            value.zeroize();
            let p = self.values.insert(name.clone(), h?);
            prev.push((name, p));
        }
        if let Err(e) = self.save() {
            for (name, p) in prev.into_iter().rev() {
                match p {
                    Some(p) => self.values.insert(name, p),
                    None => self.values.remove(&name),
                };
            }
            return Err(e);
        }
        self.generation += 1;
        Ok(())
    }

    /// Remove several names in one encrypted write. Returns how many existed.
    pub fn remove_many(&mut self, names: &[String]) -> Result<usize, String> {
        self.ready()?;
        let mut prev = Vec::new();
        for n in names {
            if let Some(p) = self.values.remove(n) {
                prev.push((n.clone(), p));
            }
        }
        if prev.is_empty() {
            return Ok(0);
        }
        if let Err(e) = self.save() {
            for (n, p) in prev {
                self.values.insert(n, p);
            }
            return Err(e);
        }
        self.generation += 1;
        Ok(prev.len())
    }

    pub fn remove(&mut self, name: &str) -> Result<bool, String> {
        self.ready()?;
        Ok(self.remove_many(&[name.to_string()])? == 1)
    }

    pub fn rotate(&mut self) -> Result<(), String> {
        self.ready()?;
        if self.values.is_empty() && !self.enc_file().exists() {
            return Ok(());
        }
        self.new_key()?;
        self.save()
    }

    pub fn reload(&mut self) {
        self.error = self.load().err();
        self.generation += 1;
    }

    pub fn get(&self, name: &str) -> Option<Zeroizing<String>> {
        self.values.get(name).and_then(|h| h.open(&self.mem, name))
    }

    fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub fn names(&self) -> Vec<(String, i64)> {
        self.values.iter().map(|(k, e)| (k.clone(), e.set_at)).collect()
    }

    pub fn meta(&self, name: &str) -> Option<Meta> {
        self.values.get(name).map(|e| e.meta.clone())
    }

    /// Every vault access goes through here: only rebuild the scrubber's patterns (plaintext forms of every
    /// value) when something changed, so stale copies don't pile up on the heap.
    fn sync_scrubber(&mut self) {
        if self.scrubbed == Some(self.generation) {
            return;
        }
        let open: Vec<(&str, Zeroizing<String>)> =
            self.values.iter().filter(|(k, _)| !k.starts_with(APPROVALS_NAME)).filter_map(|(k, h)| Some((k.as_str(), h.open(&self.mem, k)?))).collect();
        crate::scrub::set_secrets(open.iter().map(|(k, v)| (*k, v.as_str())));
        self.scrubbed = Some(self.generation);
    }
}

// ------------------------------------------------------------------ daemon-wide store

static VAULT: Mutex<Option<Vault>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Vault) -> T) -> T {
    let mut g = VAULT.lock().unwrap_or_else(|e| e.into_inner());
    let v = g.get_or_insert_with(|| Vault::open(paths::home(), std::env::var("LSTACK_KEYCHAIN").as_deref() != Ok("file")));
    let r = f(v);
    v.sync_scrubber();
    r
}

/// Open the store (once, at daemon boot) so the scrubber knows every value before any log line.
pub fn init() -> Option<String> {
    with(|v| v.error.clone())
}

pub fn unlock() -> Result<(), String> {
    with(|v| {
        v.reload();
        v.ready()
    })
}

pub fn set(name: &str, value: &str) -> Result<(), String> {
    with(|v| v.set(name, value))
}

pub fn remove(name: &str) -> Result<bool, String> {
    with(|v| v.remove(name))
}

pub fn rotate() -> Result<(), String> {
    with(|v| v.rotate())
}

pub fn set_many(items: Vec<(String, String, Meta)>) -> Result<(), String> {
    with(|v| v.set_many(items))
}

pub fn remove_many(names: &[String]) -> Result<usize, String> {
    with(|v| v.remove_many(names))
}

/// Metadata (never values) for every stored secret.
pub fn all_meta() -> BTreeMap<String, Meta> {
    with(|v| v.values.iter().map(|(k, e)| (k.clone(), e.meta.clone())).collect())
}

/// Changes whenever the store does (set, remove, unlock).
pub fn generation() -> u64 {
    with(|v| v.generation)
}

pub fn is_locked() -> bool {
    with(|v| v.error.is_some())
}

/// For the daemon's own use only (env injection, cmux password, ping key). Never sent to clients.
pub fn internal_get(name: &str) -> Option<String> {
    with(|v| v.get(name).map(|s| s.to_string()))
}

/// HMAC of a stored value under `key` (for approvals: notices a changed value without revealing it).
pub fn keyed_digest(name: &str, key: &[u8]) -> Option<String> {
    with(|v| v.get(name).map(|s| super::approvals::keyed(key, name, s.as_bytes())))
}

/// Environment variables for a service's `secrets:` references: (env name, value). Withheld until
/// the service's configuration is approved (see `approvals`).
pub fn env_for(cfg: &crate::types::ServiceConfig) -> Result<Vec<(String, String)>, String> {
    super::approvals::gate(cfg)?;
    env_for_ungated(cfg)
}

/// Only for stopping (compose down needs the same variables to parse its file); never to start anything.
pub fn env_for_ungated(cfg: &crate::types::ServiceConfig) -> Result<Vec<(String, String)>, String> {
    let refs = cfg.secret_refs();
    if refs.is_empty() {
        return Ok(vec![]);
    }
    if let Some((_, s)) = refs.iter().find(|(_, s)| s.starts_with(INTERNAL_PREFIX)) {
        return Err(format!("secret {s} is reserved for LStack and can't be passed to services"));
    }
    with(|v| {
        v.ready()?;
        let missing: Vec<&str> = refs.iter().filter(|(_, s)| !v.has(s)).map(|(_, s)| s.as_str()).collect();
        if !missing.is_empty() {
            return Err(format!("secret(s) not set: {} (lstack secret set <NAME>)", missing.join(", ")));
        }
        refs.iter().map(|(e, s)| Ok((e.clone(), v.get(s).ok_or("secret store memory is corrupt")?.to_string()))).collect()
    })
}

/// Names (never values) plus which services use each one.
pub fn status(services: &[crate::types::ServiceConfig]) -> Value {
    with(|v| {
        let mut items: Vec<Value> = v
            .names()
            .into_iter()
            .map(|(name, set_at)| {
                let used_by: Vec<&str> =
                    services.iter().filter(|c| c.secret_refs().iter().any(|(_, s)| *s == name)).map(|c| c.name.as_str()).collect();
                let m = v.meta(&name).unwrap_or_default();
                json!({
                    "name": name, "setAt": set_at, "usedBy": used_by, "internal": name.starts_with(INTERNAL_PREFIX),
                    "source": m.source.as_deref().unwrap_or("local"), "ref": m.reference, "template": m.template,
                    "account": m.account, "origin": m.origin, "fetchedAt": m.fetched_at,
                })
            })
            .collect();
        // Referenced but not set yet, so the UI can prompt for them.
        for c in services {
            for (_, s) in c.secret_refs() {
                if !v.has(&s) && !items.iter().any(|i| i["name"] == s.as_str()) {
                    let used_by: Vec<&str> =
                        services.iter().filter(|c| c.secret_refs().iter().any(|(_, x)| *x == s)).map(|c| c.name.as_str()).collect();
                    items.push(json!({ "name": s, "setAt": null, "usedBy": used_by, "internal": false, "missing": true }));
                }
            }
        }
        json!({
            "locked": v.error.is_some(),
            "error": v.error,
            "storage": match v.share_b { ShareB::Keychain { .. } => "keychain", ShareB::File(_) => "file" },
            "secrets": items,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("lstack-secrets-{}", hex(&random::<6>())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_split_key() {
        let dir = tmp();
        let mut v = Vault::open(dir.clone(), false);
        assert!(v.error.is_none());
        v.set("API_TOKEN", "tok-123456").unwrap();
        v.set("OTHER", "x".repeat(10).as_str()).unwrap();
        assert!(v.set("1BAD", "v").is_err());
        assert!(v.set("EMPTY", "").is_err());
        let raw = std::fs::read_to_string(dir.join("secrets.enc")).unwrap();
        assert!(!raw.contains("tok-123456") && !raw.contains("API_TOKEN"), "names and values are encrypted");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("secrets.enc")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.join("secrets.key")).unwrap().permissions().mode() & 0o777, 0o600);
        // neither share alone is the key
        let a = std::fs::read_to_string(dir.join("secrets.key")).unwrap();
        let b = std::fs::read_to_string(dir.join("secrets.share2")).unwrap();
        assert_ne!(a, b);

        let v2 = Vault::open(dir.clone(), false);
        assert!(v2.error.is_none());
        assert_eq!(v2.get("API_TOKEN").as_deref().map(String::as_str), Some("tok-123456"));
        assert!(!v2.values["API_TOKEN"].ct.windows(10).any(|w| w == b"tok-123456"), "sealed in memory");

        // rotation keeps values, changes shares
        let mut v3 = Vault::open(dir.clone(), false);
        v3.rotate().unwrap();
        assert_ne!(std::fs::read_to_string(dir.join("secrets.key")).unwrap(), a);
        assert_eq!(Vault::open(dir.clone(), false).get("API_TOKEN").as_deref().map(String::as_str), Some("tok-123456"));

        assert!(v3.remove("API_TOKEN").unwrap());
        assert!(!v3.remove("API_TOKEN").unwrap());
        assert!(Vault::open(dir.clone(), false).get("API_TOKEN").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_or_wrong_share_locks() {
        let dir = tmp();
        let mut v = Vault::open(dir.clone(), false);
        v.set("A_SECRET", "value-1").unwrap();
        std::fs::remove_file(dir.join("secrets.share2")).unwrap();
        let mut locked = Vault::open(dir.clone(), false);
        assert!(locked.error.as_deref().unwrap().contains("missing"));
        assert!(locked.set("B", "vvvv").unwrap_err().contains("locked"));
        std::fs::write(dir.join("secrets.share2"), hex(&random::<32>())).unwrap();
        assert!(Vault::open(dir.clone(), false).error.unwrap().contains("authentication"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn names() {
        assert!(valid_name("GITHUB_TOKEN"));
        assert!(valid_name("_x1"));
        assert!(!valid_name("9A"));
        assert!(!valid_name("A-B"));
        assert!(!valid_name(""));
    }
}

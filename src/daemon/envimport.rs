//! A service's env files (`envFiles:`): plain values, 1Password references cached in the secret
//! store, and `lstack://NAME` references to secrets stored in LStack.
//!
//! Security model:
//! - This module never talks to 1Password. Only a client (the `lstack` CLI or LStack.app) runs `op`,
//!   after a person asks it to, and the 1Password app asks them to approve it. The client sends the
//!   values here, tied to a short-lived plan that lists exactly which references were reviewed.
//! - What a service gets is its `secrets:` map (env var → stored name, written only by an approved
//!   import) plus the stored values. References in the file are never read at runtime, so editing an
//!   env file can't hand a service a different secret until someone reviews and pulls it.
//! - The watcher only `lstat`s env files and reads regular files it owns (never FIFOs: a 1Password
//!   Environment mount asks for approval when read).
//! - `lstack://NAME` lines work the same way: a hand-edited one waits for approval before the service
//!   gets that stored secret (a cloned repo's `.env` shouldn't be able to ask for your tokens). Lines
//!   the app or CLI writes (Convert to Secret, Use Stored Secret) are approved as they're written.

use super::secrets::{self, Meta, INTERNAL_PREFIX};
use super::supervisor::Sup;
use super::terminal::OnePassword;
use crate::dotenv::{self, Var};
use crate::paths::{self, expand_home, now_ms};
use crate::types::ServiceConfig;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zeroize::Zeroize;

const MAX_FILE: u64 = 1 << 20;
const MAX_VALUE: usize = 1 << 20;
const PLAN_TTL_MS: i64 = 15 * 60 * 1000;
const MAX_PLANS: usize = 64;

pub type Res<T> = Result<T, (u16, String)>;

fn bad<T>(msg: impl Into<String>) -> Res<T> {
    Err((400, msg.into()))
}

// ------------------------------------------------------------------ reading env files

pub enum FileRead {
    Text(String),
    /// A named pipe, e.g. a 1Password Environment mount. Never opened by the daemon.
    Fifo,
    Missing,
    Error(String),
}

/// Directory relative `envFiles` are resolved against: `cwd`, else the compose file's folder, else ~.
fn base_dir(cfg: &ServiceConfig) -> PathBuf {
    if let Some(c) = cfg.cwd.as_deref().filter(|c| !c.is_empty()) {
        return PathBuf::from(expand_home(c));
    }
    if let Some(f) = cfg.compose_file.as_deref() {
        if let Some(p) = Path::new(&expand_home(f)).parent() {
            return p.to_path_buf();
        }
    }
    paths::user_home()
}

pub fn file_paths(cfg: &ServiceConfig) -> Vec<PathBuf> {
    let base = base_dir(cfg);
    cfg.env_files.iter().flatten().filter(|f| !f.trim().is_empty()).map(|f| base.join(expand_home(f.trim()))).collect()
}

/// Read a regular file owned by this user, at most 1 MB, without ever opening a FIFO or device.
pub fn read_safe(p: &Path) -> FileRead {
    use std::io::Read;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    let md = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileRead::Missing,
        Err(e) => return FileRead::Error(e.to_string()),
    };
    if md.file_type().is_fifo() {
        return FileRead::Fifo;
    }
    if !md.is_file() {
        return FileRead::Error("not a regular file".into());
    }
    if md.uid() != unsafe { libc::getuid() } {
        return FileRead::Error("not owned by you".into());
    }
    if md.len() > MAX_FILE {
        return FileRead::Error("larger than 1 MB".into());
    }
    let f = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(p) {
        Ok(f) => f,
        Err(e) => return FileRead::Error(e.to_string()),
    };
    // Swapped between the check and the open (e.g. for a FIFO)?
    match f.metadata() {
        Ok(m2) if m2.is_file() && m2.ino() == md.ino() && m2.dev() == md.dev() => {}
        _ => return FileRead::Error("changed while being read".into()),
    }
    let mut buf = Vec::new();
    if let Err(e) = f.take(MAX_FILE + 1).read_to_end(&mut buf) {
        return FileRead::Error(e.to_string());
    }
    if buf.len() as u64 > MAX_FILE {
        return FileRead::Error("larger than 1 MB".into());
    }
    match String::from_utf8(buf) {
        Ok(s) => FileRead::Text(s),
        Err(_) => FileRead::Error("not UTF-8 text".into()),
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub path: String,
    /// `ok`, `fifo` (1Password Environment mount), `missing`, `error`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Parsed {
    pub files: Vec<FileInfo>,
    /// Final variables (later files win), with the file each came from.
    pub vars: Vec<(Var, String)>,
}

impl Parsed {
    pub fn errors(&self) -> Vec<String> {
        self.files.iter().filter_map(|f| f.error.as_ref().map(|e| format!("{}: {e}", f.path))).collect()
    }
}

/// `$VAR`s in references may use these from LStack's own environment. Anything else there (tokens a
/// shell exported, …) stays out: a resolved reference is shown, audited and may be asked of paired Macs.
const HOST_VARS: &[&str] = &["HOME", "USER", "LOGNAME"];

fn host_var(n: &str) -> Option<String> {
    HOST_VARS.contains(&n).then(|| std::env::var(n).ok()).flatten()
}

/// Parse every env file the way `op run --env-file a --env-file b` would. `$VAR`s come from earlier
/// keys, then the service's `env:`, then `HOME`/`USER`/`LOGNAME`.
pub fn parse_files(cfg: &ServiceConfig) -> Parsed {
    let mut files = vec![];
    let mut vars: Vec<(Var, String)> = vec![];
    for p in file_paths(cfg) {
        let path = p.to_string_lossy().into_owned();
        let (state, error, text) = match read_safe(&p) {
            FileRead::Text(t) => ("ok", None, Some(t)),
            FileRead::Fifo => ("fifo", None, None),
            FileRead::Missing => ("missing", Some("file not found".to_string()), None),
            FileRead::Error(e) => ("error", Some(e), None),
        };
        let mut info = FileInfo { path: path.clone(), state, error };
        if let Some(mut t) = text {
            let earlier: HashMap<String, String> = vars.iter().map(|(v, _)| (v.key.clone(), v.value.clone())).collect();
            let lookup = |n: &str| {
                earlier.get(n).cloned().or_else(|| cfg.env.as_ref().and_then(|e| e.get(n).cloned())).or_else(|| host_var(n))
            };
            match dotenv::parse(&t, &lookup) {
                Ok(vs) => {
                    for v in vs {
                        vars.retain(|(x, _)| x.key != v.key);
                        vars.push((v, path.clone()));
                    }
                }
                Err(e) => {
                    info.state = "error";
                    info.error = Some(e);
                }
            }
            t.zeroize();
        }
        files.push(info);
    }
    Parsed { files, vars }
}

/// Variables that make a runtime or the dynamic linker load extra code. From a `.env` they're
/// dropped for services holding secrets: a cloned repo's `.env` shouldn't be able to run its own code
/// inside a process that has your 1Password values. Set them in the service's `env:` instead.
const CODE_VARS: &[&str] = &[
    "NODE_OPTIONS", "BASH_ENV", "ENV", "ZDOTDIR", "PYTHONSTARTUP", "PYTHONPATH", "PYTHONHOME", "PERL5OPT",
    "PERL5LIB", "RUBYOPT", "RUBYLIB", "JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "PATH",
];

pub fn loads_code(key: &str) -> bool {
    key.starts_with("DYLD_") || key.starts_with("LD_") || CODE_VARS.contains(&key)
}

fn code_var_warning(key: &str) -> String {
    format!("{key} from an env file is ignored for services with secrets (it can load code); set it in the service's env: instead")
}

/// Plain (non-`op://`) values from the env files, for a service start. Missing or unreadable files
/// fail the start (like `op run`); FIFO mounts are skipped with a warning.
pub fn plain_env(cfg: &ServiceConfig) -> Result<(Vec<(String, String)>, Vec<String>), String> {
    if cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
        return Ok((vec![], vec![]));
    }
    let p = parse_files(cfg);
    if let Some(e) = p.errors().into_iter().next() {
        return Err(format!("env file {e}"));
    }
    let mut warn = vec![];
    for f in p.files.iter().filter(|f| f.state == "fifo") {
        warn.push(format!(
            "{} is a 1Password Environment mount; not read at start (it would need approval). Its imported values are passed instead.",
            f.path
        ));
    }
    let has_secrets = !cfg.secret_refs().is_empty();
    let mut out = vec![];
    for (v, _) in p.vars {
        if !v.is_plain() {
            continue;
        }
        if has_secrets && loads_code(&v.key) {
            warn.push(code_var_warning(&v.key));
            continue;
        }
        if dotenv::has_inject_template(&v.value) {
            warn.push(format!("{}: {{{{ op://… }}}} templates aren't resolved by `op run`; passed as written", v.key));
        }
        out.push((v.key, v.value));
    }
    Ok((out, warn))
}

/// Env files in a folder (for "add service"): names, and per file which keys are 1Password references
/// and which are plain. Never values.
pub fn detect(dir: &str) -> Value {
    let dir = PathBuf::from(expand_home(dir));
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.retain(|n| n == ".env" || n.starts_with(".env.") || n.ends_with(".env"));
    names.sort_by_key(|n| (n != ".env.lstack", n != ".env", n.clone()));
    names.truncate(20);
    let files: Vec<Value> = names
        .into_iter()
        .map(|n| {
            let example = [".example", ".sample", ".template", ".dist"].iter().any(|s| n.ends_with(s));
            let mut out = json!({ "name": n, "example": example });
            match read_safe(&dir.join(&n)) {
                FileRead::Text(mut t) => {
                    match dotenv::parse(&t, &host_var) {
                        Ok(vars) => {
                            let refs: Vec<Value> = vars.iter().filter_map(|v| v.op_ref().map(|r| json!({ "key": v.key, "ref": r }))).collect();
                            let stored: Vec<Value> = vars.iter().filter_map(|v| v.lstack_ref().map(|n| json!({ "key": v.key, "name": n }))).collect();
                            let plain: Vec<&str> = vars.iter().filter(|v| v.is_plain()).map(|v| v.key.as_str()).collect();
                            out["refs"] = json!(refs);
                            out["stored"] = json!(stored);
                            out["plain"] = json!(plain);
                        }
                        Err(e) => out["error"] = json!(e),
                    }
                    t.zeroize();
                }
                FileRead::Fifo => out["fifo"] = json!(true),
                FileRead::Missing => out["error"] = json!("file not found"),
                FileRead::Error(e) => out["error"] = json!(e),
            }
            out
        })
        .collect();
    json!({ "dir": dir.to_string_lossy(), "files": files })
}

// ------------------------------------------------------------------ review

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RefItem {
    pub key: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// As written, when it had `$VAR`s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub file: String,
    pub line: usize,
    /// `ok` (approved and cached), `new`, `changed` (points somewhere else now), `missing` (approved
    /// but no cached value), `blocked` (can't be imported; see `blocked`).
    pub status: &'static str,
    /// Stored name the service currently gets this key from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub vault: String,
    pub item: String,
    pub field: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Removed {
    pub key: String,
    pub secret: String,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Was an `lstack://` line (always stops right away: passing less is safe).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stored: bool,
}

/// An `lstack://NAME` line.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StoredItem {
    pub key: String,
    pub name: String,
    pub file: String,
    pub line: usize,
    /// `ok` (the service gets it), `new`/`changed` (waiting for approval), `notStored` (no secret by
    /// that name yet), `blocked` (reserved name).
    pub status: &'static str,
    /// Stored name the service currently gets this key from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub service: String,
    pub files: Vec<FileInfo>,
    pub refs: Vec<RefItem>,
    /// `lstack://NAME` lines.
    pub stored: Vec<StoredItem>,
    /// Keys the service gets from an earlier import whose line is gone from the env files.
    pub removed: Vec<Removed>,
    /// Plain keys (set from the file at each start).
    pub plain: Vec<String>,
    pub warnings: Vec<String>,
    pub locked: bool,
}

const PERSONAL_VAULTS: &[&str] = &["private", "personal", "employee"];

fn ref_warnings(vault: &str, known_vaults: &BTreeSet<String>) -> Vec<String> {
    let mut w = vec![];
    if PERSONAL_VAULTS.contains(&vault.to_ascii_lowercase().as_str()) {
        w.push(format!("{vault} is a personal vault"));
    }
    if !known_vaults.contains(&vault.to_ascii_lowercase()) {
        w.push(format!("first reference into vault {vault}"));
    }
    w
}

/// Why a reference can't be imported, if it can't.
fn block_reason(key: &str, r: &str, template: Option<&str>, op: &OnePassword) -> Option<String> {
    if key.starts_with(INTERNAL_PREFIX) {
        return Some(format!("{INTERNAL_PREFIX} names are reserved for LStack"));
    }
    if !secrets::valid_name(key) {
        return Some("key must be letters, digits and _".into());
    }
    if dotenv::is_totp_ref(r) {
        return Some("one-time codes change every 30 s and can't be cached".into());
    }
    let Some((vault, _, _)) = dotenv::ref_parts(r) else {
        return Some(if template.is_some_and(|t| t.contains('$')) {
            "incomplete after expanding variables (is a $VAR unset?)".into()
        } else {
            "not a complete op://vault/item/field reference".into()
        });
    };
    if !op.allowed_vaults.is_empty() && !op.allowed_vaults.iter().any(|v| v.eq_ignore_ascii_case(&vault)) {
        return Some(format!("vault {vault} isn't in the allowed vaults (Settings › 1Password)"));
    }
    None
}

fn known_vaults(metas: &BTreeMap<String, Meta>) -> BTreeSet<String> {
    metas
        .values()
        .filter(|m| m.from_1password())
        .filter_map(|m| m.reference.as_deref().and_then(dotenv::ref_parts).map(|(v, _, _)| v.to_ascii_lowercase()))
        .collect()
}

pub fn review(cfg: &ServiceConfig, metas: &BTreeMap<String, Meta>, op: &OnePassword) -> Review {
    let parsed = parse_files(cfg);
    let mapping: BTreeMap<String, String> = cfg.secret_refs().into_iter().collect();
    let vaults = known_vaults(metas);
    let locked = secrets::is_locked();
    let file_keys: BTreeSet<String> = cfg.file_secrets.iter().flatten().cloned().collect();
    let mut refs = vec![];
    let mut stored = vec![];
    let mut plain = vec![];
    let mut warnings = vec![];
    for (v, file) in &parsed.vars {
        if let Some(n) = v.lstack_ref() {
            let secret = mapping.get(&v.key).cloned();
            let status = if n.starts_with(INTERNAL_PREFIX) {
                "blocked"
            } else if !locked && !metas.contains_key(n) {
                "notStored"
            } else if secret.as_deref() == Some(n) {
                "ok"
            } else if secret.is_none() {
                "new"
            } else {
                "changed"
            };
            stored.push(StoredItem { key: v.key.clone(), name: n.to_string(), file: file.clone(), line: v.line, status, secret });
            continue;
        }
        let Some(r) = v.op_ref() else {
            if dotenv::has_inject_template(&v.value) {
                warnings.push(format!("{}: {{{{ op://… }}}} is `op inject` syntax; `op run` and LStack pass it as written", v.key));
            }
            if loads_code(&v.key) {
                warnings.push(code_var_warning(&v.key));
            }
            plain.push(v.key.clone());
            continue;
        };
        let template = (v.template != v.value).then(|| v.template.clone());
        let (vault, item, field) = dotenv::ref_parts(r).unwrap_or_default();
        let blocked = block_reason(&v.key, r, template.as_deref(), op);
        let secret = mapping.get(&v.key).cloned();
        let status = if blocked.is_some() {
            "blocked"
        } else {
            match secret.as_ref().map(|n| metas.get(n)) {
                None => "new",
                Some(None) => "missing",
                Some(Some(m)) if m.reference.as_deref() == Some(r) => "ok",
                Some(Some(_)) => "changed",
            }
        };
        let warnings = if status == "ok" || blocked.is_some() { vec![] } else { ref_warnings(&vault, &vaults) };
        refs.push(RefItem {
            key: v.key.clone(),
            reference: r.to_string(),
            template,
            file: file.clone(),
            line: v.line,
            status,
            secret,
            vault,
            item,
            field,
            warnings,
            blocked,
        });
    }
    // Only offer removals when every file was read: a missing file shouldn't strip a service's secrets.
    let mut removed = vec![];
    if parsed.files.iter().all(|f| f.state == "ok") && !locked {
        let ref_keys: BTreeSet<&str> = refs.iter().map(|r| r.key.as_str()).chain(stored.iter().map(|s| s.key.as_str())).collect();
        for (k, n) in &mapping {
            if ref_keys.contains(k.as_str()) {
                continue;
            }
            if file_keys.contains(k) {
                removed.push(Removed { key: k.clone(), secret: n.clone(), reference: None, stored: true });
            } else if let Some(m) = metas.get(n).filter(|m| m.imported() && m.origin.is_some()) {
                removed.push(Removed { key: k.clone(), secret: n.clone(), reference: m.reference.clone(), stored: false });
            }
        }
    }
    if locked {
        warnings.push("the secret store is locked (lstack secret unlock)".into());
    }
    if parsed.files.iter().any(|f| f.state == "fifo") {
        warnings.push("1Password Environment mounts are imported from the app or `lstack secret import`, which read them with your approval".into());
    }
    for s in stored.iter().filter(|s| s.status == "blocked") {
        warnings.push(format!("{}: {INTERNAL_PREFIX} names are reserved for LStack", s.key));
    }
    if refs.iter().any(|r| r.status != "ok") || stored.iter().any(|s| s.status != "ok") {
        warnings.push(
            "keys waiting for review aren't set when the service starts; if the app loads this .env itself it will see the op:// text".into(),
        );
    }
    Review { service: cfg.name.clone(), files: parsed.files, refs, stored, removed, plain, warnings, locked }
}

/// What the service status shows (keys and references only). `None` when nothing needs attention.
pub fn pending_summary(r: &Review, op: &OnePassword) -> Option<Value> {
    let keys = |s: &str| r.refs.iter().filter(|x| x.status == s).map(|x| x.key.clone()).collect::<Vec<_>>();
    let (new, changed, missing, mut blocked) = (keys("new"), keys("changed"), keys("missing"), keys("blocked"));
    let skeys = |f: &dyn Fn(&str) -> bool| r.stored.iter().filter(|x| f(x.status)).map(|x| x.key.clone()).collect::<Vec<_>>();
    let (link, not_stored) = (skeys(&|s| s == "new" || s == "changed"), skeys(&|s| s == "notStored"));
    blocked.extend(skeys(&|s| s == "blocked"));
    let removed: Vec<String> =
        r.removed.iter().filter(|x| !x.stored && !op.auto_apply_removals).map(|x| x.key.clone()).collect();
    let errors: Vec<String> = r.files.iter().filter_map(|f| f.error.as_ref().map(|e| format!("{}: {e}", f.path))).collect();
    let quiet = [&new, &changed, &missing, &blocked, &removed, &errors, &link, &not_stored].iter().all(|v| v.is_empty());
    if r.locked || quiet {
        return None;
    }
    Some(json!({
        "count": new.len() + changed.len() + missing.len() + removed.len() + link.len() + not_stored.len(),
        "new": new, "changed": changed, "missing": missing, "blocked": blocked, "removed": removed, "errors": errors,
        "link": link, "notStored": not_stored,
    }))
}

// ------------------------------------------------------------------ naming

fn service_prefix(service: &str) -> String {
    let mut s: String = service.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, 'S');
    }
    s
}

/// Stored name for `key` → `r` in `service`: reuse an entry that already caches this reference, else
/// the key itself, else `SERVICE_KEY`, `SERVICE_KEY_2`, … Never takes a name typed in by hand, used by
/// another service, or reserved for LStack.
pub fn allocate(
    service: &str,
    key: &str,
    r: &str,
    metas: &BTreeMap<String, Meta>,
    all: &[ServiceConfig],
    taken: &HashMap<String, String>,
) -> Option<String> {
    if key.starts_with(INTERNAL_PREFIX) {
        return None;
    }
    if let Some((n, _)) = metas.iter().find(|(n, m)| m.imported() && m.reference.as_deref() == Some(r) && !n.starts_with(INTERNAL_PREFIX)) {
        if taken.get(n).is_none_or(|t| t == r) {
            return Some(n.clone());
        }
    }
    let pre = service_prefix(service);
    let cands = std::iter::once(key.to_string()).chain(std::iter::once(format!("{pre}_{key}"))).chain((2..100).map(|i| format!("{pre}_{key}_{i}")));
    for c in cands {
        if !secrets::valid_name(&c) || c.starts_with(INTERNAL_PREFIX) {
            continue;
        }
        if let Some(t) = taken.get(&c) {
            if t == r {
                return Some(c);
            }
            continue;
        }
        if metas.get(&c).is_some_and(|m| !m.imported() || m.origin.is_none()) {
            continue;
        }
        if all.iter().any(|s| s.secret_refs().iter().any(|(e, n)| *n == c && !(s.name == service && e == key))) {
            continue;
        }
        return Some(c);
    }
    None
}

// ------------------------------------------------------------------ plans

#[derive(Clone)]
struct PlanItem {
    key: Option<String>,
    name: String,
    reference: String,
    template: Option<String>,
    origin: Option<String>,
    source: &'static str,
    /// Already stored for this exact reference: granting it doesn't need 1Password.
    cached: bool,
    status: &'static str,
    warnings: Vec<String>,
    /// For Environment imports the daemon holds the values the client read (zeroized on drop).
    value: Option<String>,
}

impl Drop for PlanItem {
    fn drop(&mut self) {
        if let Some(v) = self.value.as_mut() {
            v.zeroize();
        }
    }
}

struct Plan {
    kind: &'static str,
    service: Option<String>,
    created: i64,
    items: Vec<PlanItem>,
    removals: Vec<Removed>,
}

static PLANS: Mutex<Option<HashMap<String, Plan>>> = Mutex::new(None);

fn plans<T>(f: impl FnOnce(&mut HashMap<String, Plan>) -> T) -> T {
    let mut g = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    let now = now_ms();
    m.retain(|_, p| now - p.created < PLAN_TTL_MS);
    f(m)
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn store_plan(p: Plan) -> Value {
    let metas = secrets::all_meta();
    // A cached value from a paired Mac is shown as such (not as one read from 1Password here).
    let from = |i: &PlanItem| -> Option<String> {
        let m = metas.get(&i.name).filter(|m| i.cached && m.from_peer())?;
        m.source.as_deref()?.strip_prefix("peer:").map(String::from)
    };
    let out = json!({
        "id": "",
        "kind": p.kind,
        "service": p.service,
        "expiresAt": p.created + PLAN_TTL_MS,
        "items": p.items.iter().map(|i| {
            let (vault, item, field) = dotenv::ref_parts(&i.reference).unwrap_or_default();
            json!({
                "key": i.key, "name": i.name, "ref": i.reference, "template": i.template, "source": i.source,
                "cached": i.cached, "status": i.status, "warnings": i.warnings, "from": from(i),
                "fetchedAt": metas.get(&i.name).filter(|_| i.cached).and_then(|m| m.fetched_at),
                "vault": vault, "item": item, "field": field,
                // What the client has to read from 1Password (cached/Environment items need nothing).
                "needsRead": i.source == "op" && i.value.is_none() && !i.cached,
            })
        }).collect::<Vec<_>>(),
        "removals": p.removals,
    });
    let id = new_id();
    plans(|m| {
        if m.len() >= MAX_PLANS {
            if let Some(oldest) = m.iter().min_by_key(|(_, p)| p.created).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
            }
        }
        m.insert(id.clone(), p);
    });
    let mut out = out;
    out["id"] = json!(id);
    out
}

pub fn cancel_plan(id: &str) -> bool {
    plans(|m| m.remove(id).is_some())
}

fn effective(sup: &Sup, service: &str) -> Res<ServiceConfig> {
    sup.effective_configs().into_iter().find(|c| c.name == service).ok_or((404, format!("no such service \"{service}\"")))
}

fn ready() -> Res<()> {
    if secrets::is_locked() {
        return Err((423, "the secret store is locked (lstack secret unlock)".into()));
    }
    Ok(())
}

/// Review for one service (no plan, nothing to approve).
pub fn review_service(sup: &Sup, service: &str) -> Res<Value> {
    let cfg = effective(sup, service)?;
    let r = review(&cfg, &secrets::all_meta(), &super::terminal::load().onepassword);
    let mut v = serde_json::to_value(&r).unwrap_or_default();
    // Paired Macs offering a reference (approve it with an import; no 1Password read needed).
    for (i, x) in r.refs.iter().enumerate() {
        let by = super::secretshare::offered_by(&x.reference);
        if !by.is_empty() {
            v["refs"][i]["offeredBy"] = json!(by);
        }
    }
    Ok(v)
}

/// Plan to import a service's new/changed/missing references (`keys` limits it to some).
pub fn plan_env(sup: &Sup, service: &str, keys: Option<&[String]>) -> Res<Value> {
    ready()?;
    let cfg = effective(sup, service)?;
    if cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
        return bad(format!("{service} has no envFiles"));
    }
    let metas = secrets::all_meta();
    let op = super::terminal::load().onepassword;
    let r = review(&cfg, &metas, &op);
    let all = sup.effective_configs();
    let mut taken = HashMap::new();
    let mut items = vec![];
    for x in r.refs.iter().filter(|x| matches!(x.status, "new" | "changed" | "missing")) {
        if keys.is_some_and(|k| !k.iter().any(|k| *k == x.key)) {
            continue;
        }
        let Some(name) = allocate(service, &x.key, &x.reference, &metas, &all, &taken) else {
            return bad(format!("no free secret name for {}", x.key));
        };
        taken.insert(name.clone(), x.reference.clone());
        let cached = metas.get(&name).is_some_and(|m| m.imported() && m.reference.as_deref() == Some(x.reference.as_str()));
        items.push(PlanItem {
            key: Some(x.key.clone()),
            name,
            reference: x.reference.clone(),
            template: x.template.clone(),
            origin: Some(x.file.clone()),
            source: "op",
            cached,
            status: x.status,
            warnings: x.warnings.clone(),
            value: None,
        });
    }
    let removals = r.removed.into_iter().filter(|x| keys.is_none_or(|k| k.contains(&x.key))).collect::<Vec<_>>();
    if items.is_empty() && removals.is_empty() {
        return bad(format!("nothing to import for {service}"));
    }
    Ok(store_plan(Plan { kind: "env", service: Some(service.into()), created: now_ms(), items, removals }))
}

/// Plan to re-read stored 1Password values: by name, every one a service uses, or all of them.
pub fn plan_refresh(sup: &Sup, names: &[String], service: Option<&str>) -> Res<Value> {
    ready()?;
    let metas = secrets::all_meta();
    let mut wanted: BTreeSet<String> = names.iter().cloned().collect();
    if let Some(s) = service {
        wanted.extend(effective(sup, s)?.secret_refs().into_iter().map(|(_, n)| n));
    }
    let all = names.is_empty() && service.is_none();
    let mut items = vec![];
    for (n, m) in &metas {
        let explicit = names.contains(n);
        if !(all || wanted.contains(n)) {
            continue;
        }
        if m.source.as_deref() != Some("op") || m.reference.is_none() {
            if explicit {
                return bad(format!("{n} wasn't imported from a 1Password reference"));
            }
            continue;
        }
        items.push(PlanItem {
            key: None,
            name: n.clone(),
            reference: m.reference.clone().unwrap_or_default(),
            template: m.template.clone(),
            origin: m.origin.clone(),
            source: "op",
            cached: false,
            status: "refresh",
            warnings: vec![],
            value: None,
        });
    }
    if let Some(n) = names.iter().find(|n| !metas.contains_key(*n)) {
        return Err((404, format!("no secret named {n}")));
    }
    if items.is_empty() {
        return bad("no 1Password-sourced secrets to refresh");
    }
    Ok(store_plan(Plan { kind: "refresh", service: service.map(String::from), created: now_ms(), items, removals: vec![] }))
}

/// Plan to store one reference under `name` (and optionally pass it to `service` as `key`).
pub fn plan_link(sup: &Sup, name: &str, r: &str, service: Option<&str>, key: Option<&str>) -> Res<Value> {
    ready()?;
    let op = super::terminal::load().onepassword;
    if !dotenv::is_op_ref(r) {
        return bad("expected an op://vault/item/field reference");
    }
    if let Some(why) = block_reason(name, r, None, &op) {
        return bad(why);
    }
    let metas = secrets::all_meta();
    if let Some(m) = metas.get(name).filter(|m| !m.from_1password()) {
        return Err((409, format!("{name} is already set ({}); remove it first", m.source.as_deref().unwrap_or("local"))));
    }
    let key = match (service, key) {
        (Some(s), k) => {
            effective(sup, s)?;
            let k = k.unwrap_or(name);
            if !secrets::valid_name(k) || k.starts_with(INTERNAL_PREFIX) {
                return bad(format!("invalid key {k}"));
            }
            Some(k.to_string())
        }
        (None, Some(_)) => return bad("key needs a service"),
        (None, None) => None,
    };
    let (vault, _, _) = dotenv::ref_parts(r).unwrap_or_default();
    let item = PlanItem {
        key,
        name: name.into(),
        reference: r.into(),
        template: None,
        origin: None,
        source: "op",
        cached: false,
        status: if metas.contains_key(name) { "changed" } else { "new" },
        warnings: ref_warnings(&vault, &known_vaults(&metas)),
        value: None,
    };
    Ok(store_plan(Plan { kind: "link", service: service.map(String::from), created: now_ms(), items: vec![item], removals: vec![] }))
}

/// Plan to import a 1Password Environment the client read (with approval) from its mount at `path`.
/// The daemon keeps the values in memory until the plan is confirmed or expires.
pub fn plan_environment(sup: &Sup, service: Option<&str>, path: &str, mut content: String) -> Res<Value> {
    ready()?;
    let parsed = dotenv::parse(&content, &|_| None);
    content.zeroize();
    let vars = parsed.map_err(|e| (400, format!("environment: {e}")))?;
    let metas = secrets::all_meta();
    let all = sup.effective_configs();
    if let Some(s) = service {
        effective(sup, s)?;
    }
    let svc = service.unwrap_or("env");
    let mut taken = HashMap::new();
    let mut items = vec![];
    for mut v in vars {
        let r = format!("{path}#{}", v.key);
        if v.key.starts_with(INTERNAL_PREFIX) || !secrets::valid_name(&v.key) {
            v.value.zeroize();
            continue;
        }
        let existing = metas.iter().find(|(_, m)| m.source.as_deref() == Some("op-env") && m.reference.as_deref() == Some(r.as_str())).map(|(n, _)| n.clone());
        let name = match existing.or_else(|| allocate(svc, &v.key, &r, &metas, &all, &taken)) {
            Some(n) => n,
            None => return bad(format!("no free secret name for {}", v.key)),
        };
        taken.insert(name.clone(), r.clone());
        items.push(PlanItem {
            key: service.map(|_| v.key.clone()),
            status: if metas.contains_key(&name) { "refresh" } else { "new" },
            name,
            reference: r,
            template: None,
            origin: Some(path.into()),
            source: "op-env",
            cached: false,
            warnings: vec![],
            value: Some(std::mem::take(&mut v.value)),
        });
    }
    if items.is_empty() {
        return bad("the environment has no variables");
    }
    Ok(store_plan(Plan { kind: "environment", service: service.map(String::from), created: now_ms(), items, removals: vec![] }))
}

pub struct ImportReq {
    pub plan: String,
    /// name → value read from 1Password by the client.
    pub values: BTreeMap<String, String>,
    /// Limit to these keys / names (default: every item in the plan).
    pub keys: Option<Vec<String>>,
    pub apply_removals: Option<bool>,
    pub account: Option<String>,
    /// `cli` / `app`, for the audit log.
    pub actor: String,
}

impl Drop for ImportReq {
    fn drop(&mut self) {
        for v in self.values.values_mut() {
            v.zeroize();
        }
    }
}

/// Store the values for a plan, update the service's `secrets:` map, drop removed ones and clean up
/// stored values nothing uses any more.
pub fn import(sup: &Arc<Sup>, mut req: ImportReq) -> Res<Value> {
    ready()?;
    let plan = plans(|m| -> Res<Plan> {
        let p = m.get(&req.plan).ok_or((404, "plan expired or unknown; review again".to_string()))?;
        for (n, v) in &req.values {
            if !p.items.iter().any(|i| i.name == *n && i.source == "op") {
                return bad(format!("{n} isn't in this plan"));
            }
            if v.is_empty() || v.len() > MAX_VALUE || v.contains('\0') {
                return bad(format!("{n}: empty, too large or binary value"));
            }
        }
        Ok(m.remove(&req.plan).unwrap())
    })?;
    let selected = |i: &PlanItem| req.keys.as_ref().is_none_or(|k| k.iter().any(|k| Some(k) == i.key.as_ref() || *k == i.name));
    let now = now_ms();
    let metas = secrets::all_meta();
    let account = req.account.clone().filter(|a| !a.is_empty());
    let mut to_set = vec![];
    let mut grant: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped = vec![];
    for i in plan.items.iter().filter(|i| selected(i)) {
        let value = req.values.get_mut(&i.name).map(std::mem::take).or_else(|| i.value.clone());
        match value {
            Some(v) => {
                let mut meta = if plan.kind == "refresh" { metas.get(&i.name).cloned().unwrap_or_default() } else { Meta::default() };
                meta.source = Some(i.source.into());
                meta.reference = Some(i.reference.clone());
                meta.template = i.template.clone().or(meta.template);
                meta.origin = i.origin.clone().or(meta.origin);
                meta.account = account.clone().or(meta.account);
                meta.fetched_at = Some(now);
                to_set.push((i.name.clone(), v, meta));
                if let Some(k) = &i.key {
                    grant.insert(k.clone(), i.name.clone());
                }
            }
            None if i.cached && metas.get(&i.name).is_some_and(|m| m.reference.as_deref() == Some(i.reference.as_str())) => {
                if let Some(k) = &i.key {
                    grant.insert(k.clone(), i.name.clone());
                }
            }
            None => skipped.push(i.name.clone()),
        }
    }
    let stored: Vec<String> = to_set.iter().map(|(n, _, _)| n.clone()).collect();
    if !to_set.is_empty() {
        secrets::set_many(to_set).map_err(|e| (500, e))?;
    }
    let removals: Vec<Removed> = if plan.kind == "env" && req.apply_removals.unwrap_or(true) {
        plan.removals.iter().filter(|r| req.keys.as_ref().is_none_or(|k| k.contains(&r.key))).cloned().collect()
    } else {
        vec![]
    };
    let mut dropped = vec![];
    if let Some(service) = &plan.service {
        if !grant.is_empty() || !removals.is_empty() {
            dropped = update_mapping(sup, service, &grant, &removals, false)?;
        }
    }
    let gc = collect_garbage(sup, &dropped);
    if !stored.is_empty() {
        sup.rescrub_logs();
    }
    audit(
        &req.actor,
        plan.kind,
        json!({
            "service": plan.service,
            "stored": plan.items.iter().filter(|i| stored.contains(&i.name)).map(|i| json!({ "name": i.name, "ref": i.reference })).collect::<Vec<_>>(),
            "granted": grant, "removed": removals.iter().map(|r| &r.key).collect::<Vec<_>>(), "deleted": gc, "account": account,
        }),
    );
    if let Some(s) = &plan.service {
        refresh_pending(sup, s, false);
    }
    let restarting = if super::terminal::load().onepassword.restart_on_change {
        restart_users(sup, &stored, plan.service.as_deref().filter(|_| !grant.is_empty() || !removals.is_empty()))
    } else {
        vec![]
    };
    Ok(json!({ "stored": stored, "granted": grant, "removed": removals.iter().map(|r| &r.key).collect::<Vec<_>>(), "deleted": gc, "skipped": skipped, "restarting": restarting }))
}

/// Add `grant` (key → name) and drop `removals` from the service's `secrets:`. Returns names no longer
/// used by this service.
/// `stored`: the grants come from `lstack://` lines (else 1Password references).
fn update_mapping(sup: &Arc<Sup>, service: &str, grant: &BTreeMap<String, String>, removals: &[Removed], stored: bool) -> Res<Vec<String>> {
    let cfg = effective(sup, service)?;
    let mut map: BTreeMap<String, String> = cfg.secret_refs().into_iter().collect();
    let mut files: BTreeSet<String> = cfg.file_secrets.iter().flatten().cloned().collect();
    let before: BTreeSet<String> = map.values().cloned().collect();
    for r in removals {
        if map.get(&r.key) == Some(&r.secret) {
            map.remove(&r.key);
            files.remove(&r.key);
        }
    }
    for (k, n) in grant {
        map.insert(k.clone(), n.clone());
        if stored {
            files.insert(k.clone());
        } else {
            files.remove(k);
        }
    }
    files.retain(|k| map.contains_key(k));
    let after: BTreeSet<String> = map.values().cloned().collect();
    let mut patch = if map.is_empty() { json!({ "secrets": null }) } else { json!({ "secrets": map }) };
    patch["fileSecrets"] = if files.is_empty() { Value::Null } else { json!(files) };
    sup.update(service, &patch).map_err(|e| (e.status, e.msg))?;
    Ok(before.difference(&after).cloned().collect())
}

/// Names dropped from a config that a running service still uses (its change is queued until the
/// restart). They stay stored until then so its logs keep being scrubbed.
static GC_LATER: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Delete stored values that came from an env file import and no service uses any more.
fn collect_garbage(sup: &Sup, candidates: &[String]) -> Vec<String> {
    if candidates.is_empty() {
        return vec![];
    }
    let metas = secrets::all_meta();
    let names = |cs: Vec<ServiceConfig>| -> BTreeSet<String> { cs.iter().flat_map(|c| c.secret_refs().into_iter().map(|(_, n)| n)).collect() };
    let (wanted, running) = (names(sup.effective_configs()), names(sup.configs()));
    let mut later = GC_LATER.lock().unwrap_or_else(|e| e.into_inner());
    let mut gone = vec![];
    for n in candidates {
        later.remove(n);
        if wanted.contains(n) || !metas.get(n).is_some_and(|m| m.imported() && m.origin.is_some()) {
            continue;
        }
        if running.contains(n) {
            later.insert(n.clone());
        } else {
            gone.push(n.clone());
        }
    }
    drop(later);
    if !gone.is_empty() {
        let _ = secrets::remove_many(&gone);
    }
    gone
}

fn collect_deferred(sup: &Sup) {
    let names: Vec<String> = GC_LATER.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
    let gone = collect_garbage(sup, &names);
    if !gone.is_empty() {
        audit("watcher", "forget", json!({ "deleted": gone }));
    }
}

/// Restart running services that use any of `names` (or `service` itself when its mapping changed).
pub(super) fn restart_users(sup: &Arc<Sup>, names: &[String], service: Option<&str>) -> Vec<String> {
    let running: BTreeSet<String> = sup.list().into_iter().filter(|s| s.status == "running").map(|s| s.name).collect();
    let mut out: Vec<String> = sup
        .effective_configs()
        .into_iter()
        .filter(|c| running.contains(&c.name))
        .filter(|c| Some(c.name.as_str()) == service || c.secret_refs().iter().any(|(_, n)| names.contains(n)))
        .map(|c| c.name)
        .collect();
    out.dedup();
    for n in &out {
        let (sup, n) = (sup.clone(), n.clone());
        tokio::spawn(async move {
            sup.with(&n, None, |m| m.system("[lstack] restarting: 1Password values changed"));
            let _ = sup.restart(&n).await;
        });
    }
    out
}


// ------------------------------------------------------------------ editing env files

/// key → hash of value: what a start applied, compared later without keeping the values.
pub fn hashed(vars: &[(String, String)]) -> BTreeMap<String, u64> {
    use std::hash::{Hash, Hasher};
    vars.iter()
        .map(|(k, v)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            v.hash(&mut h);
            (k.clone(), h.finish())
        })
        .collect()
}

/// Every variable in `cfg`'s env files, file by file (later files win; earlier ones are `shadowed`),
/// with plain values as written and the review status of references. Local API only.
pub fn preview_cfg(cfg: &ServiceConfig) -> Value {
    let base = base_dir(cfg);
    let names: Vec<String> = cfg.env_files.iter().flatten().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
    let metas = secrets::all_meta();
    let op = super::terminal::load().onepassword;
    let rv = review(cfg, &metas, &op);
    let ops: HashMap<&str, &RefItem> = rv.refs.iter().map(|r| (r.key.as_str(), r)).collect();
    let stored: HashMap<&str, &StoredItem> = rv.stored.iter().map(|r| (r.key.as_str(), r)).collect();
    let parsed = parse_files(cfg);
    let winner: HashMap<&str, &str> = parsed.vars.iter().map(|(v, f)| (v.key.as_str(), f.as_str())).collect();
    let mut files = vec![];
    let mut vars = vec![];
    let mut earlier: HashMap<String, String> = HashMap::new();
    for (name, info) in names.iter().zip(parsed.files.iter()) {
        let path = base.join(expand_home(name));
        let path_s = path.to_string_lossy().into_owned();
        files.push(json!({ "name": name, "path": info.path, "state": info.state, "error": info.error }));
        let FileRead::Text(mut t) = read_safe(&path) else { continue };
        let lookup = |n: &str| earlier.get(n).cloned().or_else(|| cfg.env.as_ref().and_then(|e| e.get(n).cloned())).or_else(|| host_var(n));
        let parsed_file = dotenv::parse(&t, &lookup);
        t.zeroize();
        let Ok(list) = parsed_file else { continue };
        for v in list {
            let shadowed = winner.get(v.key.as_str()).is_some_and(|f| *f != path_s);
            let mut e = json!({ "key": v.key, "file": name, "line": v.line });
            if shadowed {
                e["shadowed"] = json!(true);
            }
            if let Some(n) = v.lstack_ref() {
                e["kind"] = json!("lstack");
                e["name"] = json!(n);
                if let Some(x) = stored.get(v.key.as_str()).filter(|_| !shadowed) {
                    e["status"] = json!(x.status);
                    e["secret"] = json!(x.secret);
                }
            } else if let Some(rf) = v.op_ref() {
                e["kind"] = json!("op");
                e["ref"] = json!(rf);
                if let Some(x) = ops.get(v.key.as_str()).filter(|_| !shadowed) {
                    e["status"] = json!(x.status);
                    e["secret"] = json!(x.secret);
                    if let Some(b) = &x.blocked {
                        e["blocked"] = json!(b);
                    }
                }
            } else {
                e["kind"] = json!("plain");
                e["value"] = json!(v.value);
                if v.template != v.value {
                    e["template"] = json!(v.template);
                }
            }
            earlier.insert(v.key.clone(), v.value.clone());
            vars.push(e);
        }
    }
    let file_keys: BTreeSet<String> = cfg.file_secrets.iter().flatten().cloned().collect();
    let overrides = json!({
        "env": cfg.env.iter().flatten().map(|(k, _)| k).collect::<Vec<_>>(),
        "secrets": cfg.secret_refs().into_iter().filter(|(k, _)| !file_keys.contains(k) && !ops.contains_key(k.as_str()))
            .map(|(k, n)| json!({ "key": k, "name": n })).collect::<Vec<_>>(),
    });
    json!({
        "service": cfg.name, "dir": base.to_string_lossy(), "files": files, "vars": vars, "overrides": overrides,
        "removed": rv.removed, "warnings": rv.warnings, "locked": rv.locked,
    })
}

/// Preview for an existing service (optionally with other `files`), or for a folder (add service).
pub fn preview(sup: &Sup, service: Option<&str>, dir: Option<&str>, files: Option<Vec<String>>) -> Res<Value> {
    let mut cfg = match service {
        Some(s) => effective(sup, s)?,
        None => ServiceConfig { name: String::new(), ..Default::default() },
    };
    if let Some(d) = dir.filter(|d| !d.trim().is_empty()) {
        cfg.cwd = Some(d.to_string());
    }
    if let Some(f) = files {
        cfg.env_files = Some(f);
    }
    if service.is_none() && cfg.cwd.is_none() {
        return bad("dir required");
    }
    Ok(preview_cfg(&cfg))
}

#[derive(serde::Deserialize, Debug)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum EnvOp {
    /// Set a plain value.
    Set { file: Option<String>, key: String, value: String },
    /// Remove the key (from `file`, else from every env file).
    Remove { file: Option<String>, key: String },
    /// Store `value` (or the line's current plain value) in LStack and write `lstack://NAME`.
    Secret { file: Option<String>, key: String, value: Option<String>, name: Option<String> },
    /// Point the key at a secret already stored in LStack.
    Link { file: Option<String>, key: String, name: String },
}

impl EnvOp {
    fn key(&self) -> &str {
        match self {
            EnvOp::Set { key, .. } | EnvOp::Remove { key, .. } | EnvOp::Secret { key, .. } | EnvOp::Link { key, .. } => key,
        }
    }
    fn file(&self) -> Option<&str> {
        match self {
            EnvOp::Set { file, .. } | EnvOp::Remove { file, .. } | EnvOp::Secret { file, .. } | EnvOp::Link { file, .. } => file.as_deref(),
        }
    }
}

impl Drop for EnvOp {
    fn drop(&mut self) {
        match self {
            EnvOp::Set { value, .. } => value.zeroize(),
            EnvOp::Secret { value: Some(v), .. } => v.zeroize(),
            _ => {}
        }
    }
}

/// A fresh stored name for converting `key`: the key, else `SERVICE_KEY`, `SERVICE_KEY_2`, …
fn fresh_name(service: &str, key: &str, metas: &BTreeMap<String, Meta>, taken: &BTreeSet<String>) -> Option<String> {
    let pre = service_prefix(service);
    std::iter::once(key.to_string())
        .chain(std::iter::once(format!("{pre}_{key}")))
        .chain((2..100).map(|i| format!("{pre}_{key}_{i}")))
        .find(|c| secrets::valid_name(c) && !c.starts_with(INTERNAL_PREFIX) && !metas.contains_key(c) && !taken.contains(c))
}

/// Write `text` over `path` atomically (temp file + rename), keeping its permissions (new files 0600).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&target).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let dir = target.parent().ok_or("no parent folder")?;
    let tmp = dir.join(format!(".{}.lstack-{}", target.file_name().and_then(|n| n.to_str()).unwrap_or("env"), new_id()));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(|e| e.to_string())
}

/// Apply edits to a service's env files. Secrets it stores or links are approved for this service
/// right away (a person asked for them here); everything else follows the usual review.
pub fn apply_env(sup: &Arc<Sup>, service: &str, ops: Vec<EnvOp>, actor: &str) -> Res<Value> {
    if ops.is_empty() {
        return bad("no changes");
    }
    if ops.len() > 500 {
        return bad("too many changes");
    }
    let cfg = effective(sup, service)?;
    let base = base_dir(&cfg);
    let names: Vec<String> = cfg.env_files.iter().flatten().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
    if names.is_empty() {
        return bad(format!("{service} has no env files; add one first"));
    }
    if ops.iter().any(|o| matches!(o, EnvOp::Secret { .. } | EnvOp::Link { .. })) {
        ready()?;
    }
    // Load every env file once.
    let mut texts: Vec<(String, PathBuf, Option<String>, bool)> = vec![]; // name, path, text (None = absent), changed
    for n in &names {
        let path = base.join(expand_home(n));
        let text = match read_safe(&path) {
            FileRead::Text(t) => Some(t),
            FileRead::Missing => None,
            FileRead::Fifo => {
                if ops.iter().any(|o| o.file() == Some(n.as_str())) {
                    return bad(format!("{n} is a 1Password Environment mount; edit it in 1Password"));
                }
                continue;
            }
            FileRead::Error(e) => return bad(format!("{n}: {e}")),
        };
        texts.push((n.clone(), path, text, false));
    }
    let index_of = |texts: &[(String, PathBuf, Option<String>, bool)], f: &str| texts.iter().position(|t| t.0 == f);
    let metas = secrets::all_meta();
    let mut grant: BTreeMap<String, String> = BTreeMap::new();
    let mut to_store: Vec<(String, String)> = vec![];
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut summary: Vec<Value> = vec![];
    for op in &ops {
        let key = op.key().to_string();
        if !secrets::valid_name(&key) {
            return bad(format!("{key:?}: keys are letters, digits and _, not starting with a digit"));
        }
        // Which file: as given, else where the key lives now (the last one wins), else the first.
        let holder = texts.iter().rposition(|t| t.2.as_deref().is_some_and(|s| dotenv::parse(s, &|_| None).is_ok_and(|v| v.iter().any(|v| v.key == key))));
        let target = match op.file() {
            Some(f) => Some(index_of(&texts, f).ok_or_else(|| (400, format!("{f} isn't one of {service}'s env files")))?),
            None => holder.or(if matches!(op, EnvOp::Remove { .. }) { None } else { Some(0) }),
        };
        let line_value = match op {
            EnvOp::Set { value, .. } => Some(value.clone()),
            EnvOp::Remove { .. } => None,
            EnvOp::Secret { value, name, file: _, key: _ } => {
                let i = target.ok_or((400, "no env file".to_string()))?;
                let mut v = match value {
                    Some(v) => v.clone(),
                    None => {
                        let cur = texts[i].2.as_deref().and_then(|s| dotenv::parse(s, &host_var).ok()).and_then(|vs| vs.into_iter().find(|v| v.key == key));
                        match cur {
                            Some(v) if v.is_plain() => v.value.clone(),
                            Some(_) => return bad(format!("{key} is already a reference")),
                            None => return bad(format!("{key} isn't in {}", texts[i].0)),
                        }
                    }
                };
                if v.len() > MAX_VALUE {
                    v.zeroize();
                    return bad(format!("{key}: value larger than 1 MB"));
                }
                let n = match name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(n) => {
                        if !secrets::valid_name(n) || n.starts_with(INTERNAL_PREFIX) {
                            return bad(format!("{n:?} can't be used as a secret name"));
                        }
                        if metas.get(n).is_some_and(|m| m.imported()) {
                            return bad(format!("{n} holds a value from 1Password; pick another name"));
                        }
                        n.to_string()
                    }
                    None => fresh_name(service, &key, &metas, &taken).ok_or((400, format!("no free secret name for {key}")))?,
                };
                taken.insert(n.clone());
                to_store.push((n.clone(), v));
                grant.insert(key.clone(), n.clone());
                Some(format!("{}{n}", dotenv::LSTACK_SCHEME))
            }
            EnvOp::Link { name, .. } => {
                if !metas.contains_key(name) && !taken.contains(name) {
                    return bad(format!("no stored secret named {name}"));
                }
                if name.starts_with(INTERNAL_PREFIX) {
                    return bad(format!("{INTERNAL_PREFIX} names are reserved for LStack"));
                }
                grant.insert(key.clone(), name.clone());
                Some(format!("{}{name}", dotenv::LSTACK_SCHEME))
            }
        };
        let kind = match op {
            EnvOp::Set { .. } => "set",
            EnvOp::Remove { .. } => "remove",
            EnvOp::Secret { .. } => "secret",
            EnvOp::Link { .. } => "link",
        };
        let idxs: Vec<usize> = match (op, target) {
            (EnvOp::Remove { file: None, .. }, _) => (0..texts.len()).collect(),
            (_, Some(i)) => vec![i],
            (_, None) => vec![],
        };
        for i in idxs {
            let cur = texts[i].2.clone().unwrap_or_default();
            let mut next = dotenv::edit(&cur, &key, line_value.as_deref()).map_err(|e| (400, format!("{}: {e}", texts[i].0)))?;
            if texts[i].2.as_deref() != Some(next.as_str()) && !(texts[i].2.is_none() && next.is_empty()) {
                texts[i].2 = Some(std::mem::take(&mut next));
                texts[i].3 = true;
            }
            next.zeroize();
        }
        if !matches!(op, EnvOp::Set { .. } | EnvOp::Remove { .. }) {
            grant.entry(key.clone()).or_default();
        } else {
            grant.remove(&key);
        }
        summary.push(json!({ "op": kind, "key": key, "file": target.map(|i| texts[i].0.clone()) }));
    }
    // Store first: a file pointing at a secret that failed to store would just say "not stored".
    let stored_names: Vec<String> = to_store.iter().map(|(n, _)| n.clone()).collect();
    for (n, v) in to_store.iter_mut() {
        let res = secrets::set(n, v);
        v.zeroize();
        res.map_err(|e| (400, format!("storing {n}: {e}")))?;
    }
    let mut written = vec![];
    for (name, path, text, changed) in texts.iter_mut() {
        if *changed {
            write_atomic(path, text.as_deref().unwrap_or_default()).map_err(|e| (500, format!("writing {name}: {e}")))?;
            written.push(name.clone());
        }
        if let Some(t) = text.as_mut() {
            t.zeroize();
        }
    }
    if !stored_names.is_empty() {
        sup.rescrub_logs();
    }
    // Approve what was just linked/stored here, and stop passing removed `lstack://` lines.
    grant.retain(|_, n| !n.is_empty());
    let cfg = effective(sup, service)?;
    let rv = review(&cfg, &secrets::all_meta(), &super::terminal::load().onepassword);
    let grant: BTreeMap<String, String> =
        grant.into_iter().filter(|(k, n)| rv.stored.iter().any(|s| s.key == *k && s.name == *n && s.status != "blocked" && s.status != "notStored")).collect();
    let removals: Vec<Removed> = rv.removed.iter().filter(|r| r.stored).cloned().collect();
    if !grant.is_empty() || !removals.is_empty() {
        update_mapping(sup, service, &grant, &removals, true)?;
    }
    audit(actor, "env-edit", json!({ "service": service, "changes": summary, "stored": stored_names, "granted": grant, "files": written }));
    refresh_pending(sup, service, true);
    check_plain(sup, service);
    let cfg = effective(sup, service)?;
    Ok(preview_cfg(&cfg))
}

/// Approve `lstack://` lines waiting for review (`keys`: some of them; else all).
pub fn approve_stored(sup: &Arc<Sup>, service: &str, keys: Option<&[String]>, actor: &str) -> Res<Value> {
    ready()?;
    let cfg = effective(sup, service)?;
    let rv = review(&cfg, &secrets::all_meta(), &super::terminal::load().onepassword);
    let grant: BTreeMap<String, String> = rv
        .stored
        .iter()
        .filter(|s| matches!(s.status, "new" | "changed") && keys.is_none_or(|k| k.contains(&s.key)))
        .map(|s| (s.key.clone(), s.name.clone()))
        .collect();
    if grant.is_empty() {
        return bad("nothing to approve");
    }
    update_mapping(sup, service, &grant, &[], true)?;
    audit(actor, "env-approve", json!({ "service": service, "granted": grant }));
    sup.with(service, None, |m| m.system(format!("[lstack] approved stored secrets for {}", grant.keys().cloned().collect::<Vec<_>>().join(", "))));
    refresh_pending(sup, service, true);
    let cfg = effective(sup, service)?;
    Ok(preview_cfg(&cfg))
}

/// Compare the plain env-file values with what the running process got; flag a restart (and say so
/// once per change) when they differ. Also keeps the key list the menu bar shows.
pub fn check_plain(sup: &Arc<Sup>, service: &str) {
    let Ok(cfg) = effective(sup, service) else { return };
    let has_files = cfg.env_files.as_ref().is_some_and(|f| !f.is_empty());
    let parsed = has_files.then(|| parse_files(&cfg));
    let keys: Vec<String> = parsed.as_ref().map(|p| p.vars.iter().map(|(v, _)| v.key.clone()).collect()).unwrap_or_default();
    let current = if has_files { plain_env(&cfg).ok().map(|(v, _)| hashed(&v)) } else { Some(BTreeMap::new()) };
    let mut notify: Option<(Vec<String>, Vec<String>)> = None;
    sup.with(service, None, |m| {
        let mut dirty = false;
        if m.env_file_keys != keys {
            m.env_file_keys = keys.clone();
            dirty = true;
        }
        let stale = match (&m.env_applied, &current) {
            (Some(a), Some(c)) if m.rt.status == "running" => {
                let mut ks: Vec<String> = a.keys().chain(c.keys()).filter(|k| a.get(*k) != c.get(*k)).cloned().collect();
                ks.dedup();
                (!ks.is_empty()).then_some(ks)
            }
            _ => None,
        };
        if stale != m.env_stale {
            let fresh: Vec<String> = stale.iter().flatten().filter(|k| !m.env_stale.iter().flatten().any(|x| x == *k)).cloned().collect();
            if !fresh.is_empty() {
                let files = parsed.as_ref().map(|p| p.files.iter().map(|f| Path::new(&f.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect()).unwrap_or_default();
                notify = Some((fresh, files));
            }
            m.env_stale = stale;
            dirty = true;
        }
        if dirty {
            sup.changed(m);
        }
    });
    if let Some((keys, files)) = notify {
        let msg = format!("{service}: {} changed in {} — restart to apply", keys.join(", "), files.join(", "));
        sup.with(service, None, |m| m.system(format!("[lstack] env file changed: {} (restart to apply)", keys.join(", "))));
        sup.notice("Environment file changed", &msg);
    }
}

// ------------------------------------------------------------------ audit log

fn audit_path() -> PathBuf {
    paths::home().join("secrets-audit.log")
}

/// Append one line (never values) to `~/.localstack/secrets-audit.log`.
pub fn audit(actor: &str, action: &str, detail: Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let line = json!({ "ts": paths::iso(now_ms()), "actor": actor, "action": action, "detail": detail });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(audit_path()) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn audit_tail(n: usize) -> Vec<Value> {
    let s = std::fs::read_to_string(audit_path()).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

// ------------------------------------------------------------------ watcher

/// Recompute a service's pending 1Password changes; notify when new ones appear. With
/// `auto_removals`, references removed from the env files stop being passed right away.
pub fn refresh_pending(sup: &Arc<Sup>, service: &str, auto_removals: bool) {
    let Ok(cfg) = effective(sup, service) else { return };
    let op = super::terminal::load().onepassword;
    if cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
        set_pending(sup, service, None);
        return;
    }
    let metas = secrets::all_meta();
    let mut r = review(&cfg, &metas, &op);
    let auto: Vec<Removed> = r.removed.iter().filter(|x| x.stored || op.auto_apply_removals).cloned().collect();
    if auto_removals && !auto.is_empty() && !r.locked {
        let keys: Vec<String> = auto.iter().map(|x| x.key.clone()).collect();
        if let Ok(dropped) = update_mapping(sup, service, &BTreeMap::new(), &auto, false) {
            let gc = collect_garbage(sup, &dropped);
            audit("watcher", "remove", json!({ "service": service, "removed": keys, "deleted": gc }));
            sup.with(service, None, |m| m.system(format!("[lstack] stopped passing secret {} (removed from the env file)", keys.join(", "))));
            if let Ok(c) = effective(sup, service) {
                r = review(&c, &secrets::all_meta(), &op);
            }
        }
    }
    let summary = pending_summary(&r, &op);
    let prev = sup.with(service, None, |m| m.secrets_pending.clone()).flatten();
    if summary == prev {
        return;
    }
    let keys_of = |v: &Option<Value>| -> BTreeSet<String> {
        let Some(v) = v else { return BTreeSet::new() };
        ["new", "changed", "missing", "blocked", "removed", "link", "notStored"]
            .iter()
            .flat_map(|k| v[*k].as_array().cloned().unwrap_or_default())
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    };
    let fresh: Vec<String> = keys_of(&summary).difference(&keys_of(&prev)).cloned().collect();
    if !fresh.is_empty() {
        let msg = format!("{}: review secret changes for {}", service, fresh.join(", "));
        sup.with(service, None, |m| m.system(format!("[lstack] env file secrets need review: {} (Edit Service › Environment, or lstack env {service})", fresh.join(", "))));
        sup.notice("Environment changes to review", &msg);
    }
    set_pending(sup, service, summary);
}

fn set_pending(sup: &Sup, service: &str, v: Option<Value>) {
    sup.with(service, None, |m| {
        if m.secrets_pending != v {
            m.secrets_pending = v;
            sup.changed(m);
        }
    });
}

fn fingerprint(cfg: &ServiceConfig, gen: u64, op: &OnePassword) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::MetadataExt;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(cfg).unwrap_or_default().hash(&mut h);
    serde_json::to_string(op).unwrap_or_default().hash(&mut h);
    gen.hash(&mut h);
    for p in file_paths(cfg) {
        for md in [std::fs::symlink_metadata(&p), std::fs::metadata(&p)] {
            match md {
                Ok(m) => (m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ino(), m.dev()).hash(&mut h),
                Err(_) => 0u8.hash(&mut h),
            }
        }
    }
    h.finish()
}

/// Poll imported env files every 2 s (metadata only until something changes).
pub fn spawn_watcher(sup: Arc<Sup>) {
    tokio::spawn(async move {
        let mut seen: HashMap<String, u64> = HashMap::new();
        // Plain values: file fingerprint plus the start they're compared against.
        let mut plain_seen: HashMap<String, (u64, Option<i64>)> = HashMap::new();
        let mut first = true;
        loop {
            let op = super::terminal::load().onepassword;
            let gen = secrets::generation();
            let cfgs = sup.effective_configs();
            seen.retain(|n, _| cfgs.iter().any(|c| c.name == *n));
            plain_seen.retain(|n, _| cfgs.iter().any(|c| c.name == *n));
            let started: HashMap<String, Option<i64>> = sup.list().into_iter().map(|s| (s.name, s.started_at)).collect();
            for cfg in cfgs {
                if cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
                    if seen.remove(&cfg.name).is_some() {
                        set_pending(&sup, &cfg.name, None);
                    }
                    if plain_seen.remove(&cfg.name).is_some() {
                        check_plain(&sup, &cfg.name);
                    }
                    continue;
                }
                let fp = fingerprint(&cfg, gen, &op);
                if (op.watch || first) && seen.get(&cfg.name) != Some(&fp) {
                    refresh_pending(&sup, &cfg.name, true);
                    // The removal may have changed the config; take its fingerprint afresh.
                    let fp = effective(&sup, &cfg.name).map(|c| fingerprint(&c, secrets::generation(), &op)).unwrap_or(fp);
                    seen.insert(cfg.name.clone(), fp);
                }
                let key = (fp, started.get(&cfg.name).copied().flatten());
                if plain_seen.get(&cfg.name) != Some(&key) {
                    check_plain(&sup, &cfg.name);
                    plain_seen.insert(cfg.name.clone(), key);
                }
            }
            first = false;
            collect_deferred(&sup);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("lstack-envimport-{}", new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn svc(dir: &Path, files: &[&str], secrets: &[(&str, &str)]) -> ServiceConfig {
        ServiceConfig {
            name: "api".into(),
            cwd: Some(dir.to_string_lossy().into()),
            env_files: Some(files.iter().map(|s| s.to_string()).collect()),
            env: Some([("APP_ENV".to_string(), "dev".to_string()), ("SHARED".to_string(), "from-env".to_string())].into()),
            secrets: (!secrets.is_empty()).then(|| crate::types::SecretRefs::Map(secrets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect())),
            ..Default::default()
        }
    }

    fn op_meta(r: &str, origin: bool) -> Meta {
        Meta { source: Some("op".into()), reference: Some(r.into()), origin: origin.then(|| "/x/.env".into()), ..Default::default() }
    }

    #[test]
    fn plain_env_follows_op_run_precedence() {
        let d = tmpdir();
        std::fs::write(d.join(".env"), "A=1\nSHARED=file\nDB=op://Dev/db/password\nU=http://$APP_ENV.local\n").unwrap();
        std::fs::write(d.join(".env.local"), "A=2\n").unwrap();
        let cfg = svc(&d, &[".env", ".env.local"], &[]);
        let (vars, warn) = plain_env(&cfg).unwrap();
        let m: BTreeMap<_, _> = vars.into_iter().collect();
        assert_eq!(m.get("A").map(String::as_str), Some("2"), "later file wins");
        assert_eq!(m.get("SHARED").map(String::as_str), Some("file"), "file overrides env: (applied after it)");
        assert_eq!(m.get("U").map(String::as_str), Some("http://dev.local"), "$VAR from the service env");
        assert!(!m.contains_key("DB"), "references never come from the file at runtime");
        assert!(warn.is_empty());
        let missing = svc(&d, &[".env", "nope.env"], &[]);
        assert!(plain_env(&missing).unwrap_err().contains("nope.env"));
    }

    #[test]
    fn code_loading_vars_dropped_when_service_has_secrets() {
        let d = tmpdir();
        std::fs::write(d.join(".env"), "NODE_OPTIONS=--require ./evil.js\nDYLD_INSERT_LIBRARIES=/x.dylib\nPORT=3000\n").unwrap();
        let (vars, warn) = plain_env(&svc(&d, &[".env"], &[])).unwrap();
        assert_eq!(vars.len(), 3, "no secrets: nothing to protect, passed as written");
        assert!(warn.is_empty());
        let (vars, warn) = plain_env(&svc(&d, &[".env"], &[("DB", "API_DB")])).unwrap();
        assert_eq!(vars, vec![("PORT".to_string(), "3000".to_string())]);
        assert_eq!(warn.len(), 2);
        assert!(loads_code("LD_PRELOAD") && loads_code("PATH") && !loads_code("NODE_ENV"));
    }

    #[test]
    fn fifo_is_never_opened() {
        let d = tmpdir();
        let p = d.join("mount.env");
        let c = std::ffi::CString::new(p.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Opening a FIFO for reading with no writer would block; read_safe must return at once.
        assert!(matches!(read_safe(&p), FileRead::Fifo));
        let cfg = svc(&d, &["mount.env"], &[]);
        let (vars, warn) = plain_env(&cfg).unwrap();
        assert!(vars.is_empty());
        assert_eq!(warn.len(), 1);
    }

    #[test]
    fn review_statuses_and_removals() {
        let d = tmpdir();
        std::fs::write(
            d.join(".env"),
            "OK=op://Dev/a/pw\nCHANGED=op://Dev/b/new\nNEW=op://Dev/c/pw\nMISS=op://Dev/d/pw\nOTP=op://Dev/e/one?attribute=otp\nVARS=op://$APP_ENV/f/pw\nHALF=op://$NOPE_UNSET/f/pw\nLSTACK_X=op://Dev/g/pw\n",
        )
        .unwrap();
        let cfg = svc(&d, &[".env"], &[("OK", "OK"), ("CHANGED", "CHANGED"), ("MISS", "MISS"), ("GONE", "GONE"), ("MANUAL", "MANUAL")]);
        let metas: BTreeMap<String, Meta> = [
            ("OK".to_string(), op_meta("op://Dev/a/pw", true)),
            ("CHANGED".to_string(), op_meta("op://Dev/b/old", true)),
            ("GONE".to_string(), op_meta("op://Dev/z/pw", true)),
            ("MANUAL".to_string(), Meta::default()),
        ]
        .into();
        let r = review(&cfg, &metas, &OnePassword::default());
        let st: BTreeMap<&str, &str> = r.refs.iter().map(|x| (x.key.as_str(), x.status)).collect();
        assert_eq!(st["OK"], "ok");
        assert_eq!(st["CHANGED"], "changed");
        assert_eq!(st["NEW"], "new");
        assert_eq!(st["MISS"], "missing");
        assert_eq!(st["OTP"], "blocked");
        assert_eq!(st["VARS"], "new");
        assert_eq!(st["HALF"], "blocked");
        assert_eq!(st["LSTACK_X"], "blocked");
        let vars = r.refs.iter().find(|x| x.key == "VARS").unwrap();
        assert_eq!(vars.reference, "op://dev/f/pw");
        assert_eq!(vars.template.as_deref(), Some("op://$APP_ENV/f/pw"));
        // Only imported entries are removed; a hand-typed secret stays.
        assert_eq!(r.removed.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["GONE"]);
        let allowed = OnePassword { allowed_vaults: vec!["prod".into()], ..Default::default() };
        let r2 = review(&cfg, &metas, &allowed);
        assert_eq!(r2.refs.iter().find(|x| x.key == "NEW").unwrap().status, "blocked");
        let p = pending_summary(&r, &OnePassword::default()).unwrap();
        assert_eq!(p["new"], json!(["NEW", "VARS"]));
        assert_eq!(p["removed"], json!([]), "auto-applied, so not pending");
    }

    #[test]
    fn lstack_references_need_approval() {
        let d = tmpdir();
        std::fs::write(d.join(".env.lstack"), "PORT=3000\nTOKEN=lstack://GH_TOKEN\nNEWONE=lstack://GH_TOKEN\nMOVED=lstack://OTHER\nNOPE=lstack://NOT_THERE\nRES=lstack://LSTACK_KEY\n").unwrap();
        let mut cfg = svc(&d, &[".env.lstack"], &[("TOKEN", "GH_TOKEN"), ("MOVED", "GH_TOKEN"), ("DROPPED", "GH_TOKEN"), ("MANUAL", "GH_TOKEN")]);
        cfg.file_secrets = Some(vec!["TOKEN".into(), "MOVED".into(), "DROPPED".into()]);
        let metas: BTreeMap<String, Meta> =
            [("GH_TOKEN".to_string(), Meta::default()), ("OTHER".to_string(), Meta::default()), ("LSTACK_KEY".to_string(), Meta::default())].into();
        let r = review(&cfg, &metas, &OnePassword::default());
        let st: BTreeMap<&str, &str> = r.stored.iter().map(|x| (x.key.as_str(), x.status)).collect();
        assert_eq!(st, [("TOKEN", "ok"), ("NEWONE", "new"), ("MOVED", "changed"), ("NOPE", "notStored"), ("RES", "blocked")].into());
        assert_eq!(r.plain, vec!["PORT"]);
        // A line removed from the file stops the mapping; a hand-written `secrets:` entry stays.
        assert_eq!(r.removed.iter().map(|x| (x.key.as_str(), x.stored)).collect::<Vec<_>>(), vec![("DROPPED", true)]);
        let p = pending_summary(&r, &OnePassword { auto_apply_removals: false, ..Default::default() }).unwrap();
        assert_eq!(p["link"], json!(["NEWONE", "MOVED"]));
        assert_eq!(p["notStored"], json!(["NOPE"]));
        assert_eq!(p["removed"], json!([]), "stored removals always apply right away");
        assert_eq!(p["blocked"], json!(["RES"]));
        // References are never passed as text.
        let (vars, _) = plain_env(&cfg).unwrap();
        assert_eq!(vars, vec![("PORT".to_string(), "3000".to_string())]);
    }

    #[test]
    fn fresh_names_skip_existing() {
        let metas: BTreeMap<String, Meta> = [("TOKEN".to_string(), Meta::default())].into();
        assert_eq!(fresh_name("my-api", "TOKEN", &metas, &BTreeSet::new()).as_deref(), Some("MY_API_TOKEN"));
        let taken: BTreeSet<String> = ["MY_API_TOKEN".to_string()].into();
        assert_eq!(fresh_name("my-api", "TOKEN", &metas, &taken).as_deref(), Some("MY_API_TOKEN_2"));
        assert_eq!(fresh_name("x", "PORT", &metas, &BTreeSet::new()).as_deref(), Some("PORT"));
    }

    #[test]
    fn atomic_write_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir();
        let p = d.join(".env");
        std::fs::write(&p, "A=1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&p, "A=2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A=2\n");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        let n = d.join(".env.lstack");
        write_atomic(&n, "B=1\n").unwrap();
        assert_eq!(std::fs::metadata(&n).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "no temp files left");
    }

    #[test]
    fn allocation_never_takes_foreign_names() {
        let metas: BTreeMap<String, Meta> = [
            ("TOKEN".to_string(), Meta::default()),
            ("SHARED".to_string(), op_meta("op://Dev/s/pw", true)),
            ("OLD".to_string(), op_meta("op://Dev/o/old", true)),
        ]
        .into();
        let other = ServiceConfig { name: "web".into(), secrets: Some(crate::types::SecretRefs::List(vec!["DB".into()])), ..Default::default() };
        let me = ServiceConfig { name: "api".into(), secrets: Some(crate::types::SecretRefs::List(vec!["OLD".into()])), ..Default::default() };
        let all = vec![other, me];
        let t = HashMap::new();
        assert_eq!(allocate("api", "TOKEN", "op://Dev/t/pw", &metas, &all, &t).as_deref(), Some("API_TOKEN"), "hand-typed name");
        assert_eq!(allocate("api", "DB", "op://Dev/db/pw", &metas, &all, &t).as_deref(), Some("API_DB"), "used by another service");
        assert_eq!(allocate("api", "X", "op://Dev/s/pw", &metas, &all, &t).as_deref(), Some("SHARED"), "same reference is shared");
        assert_eq!(allocate("api", "OLD", "op://Dev/o/new", &metas, &all, &t).as_deref(), Some("OLD"), "own imported entry is updated");
        assert_eq!(allocate("my-api", "LSTACK_K", "op://Dev/k/pw", &metas, &all, &t), None);
        let mut t2 = HashMap::new();
        t2.insert("NEW".to_string(), "op://Dev/1/pw".to_string());
        assert_eq!(allocate("2fa", "NEW", "op://Dev/2/pw", &metas, &all, &t2).as_deref(), Some("S2FA_NEW"));
    }
}

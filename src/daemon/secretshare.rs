//! Sharing 1Password-sourced values with paired Macs, from the encrypted store (never from 1Password).
//!
//! Security model:
//! - Sender: a paired Mac receives a value only for an exact reference granted to it here
//!   (`Grants.secrets`, Secrets › Share). Only values read from 1Password on this Mac (`op` / `op-env`)
//!   are sent: never ones typed in, never ones received from another Mac (no re-sharing), never `LSTACK_*`.
//!   Serving them never reads 1Password, so a peer can't cause an approval prompt here.
//! - Receiver: asks only for references its own services' env files wait for, or for newer values of ones
//!   it already holds from that Mac. It never overwrites a stored value that holds a different reference, so
//!   a new value doesn't reach a service until someone approves it (`lstack secret import`, which then
//!   needs no 1Password read; the import shows which Mac it came from). Not even a missing value for an
//!   approved reference is filled in directly. Values received are tied to the sender's pinned link key;
//!   only that key can update them later. What we wait for is told to paired Macs only when someone asks
//!   to import (or `pullFromPeers` is on).
//! - Both sides audit transfers (`secrets-audit.log`; the sender also in `lstack remote activity`).

use super::envimport::{self, Res};
use super::peers::Peers;
use super::remote::{Remote, SharedPeer};
use super::secrets::{self, Meta, INTERNAL_PREFIX};
use super::supervisor::Sup;
use crate::paths::now_ms;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use zeroize::Zeroize;

const MAX_REFS: usize = 500;
const MAX_REF_LEN: usize = 1024;
const MAX_VALUE: usize = 1 << 20;
const SYNC_SECS: u64 = 15;

/// References from another Mac are shown in terminals and the app: printable, bounded text only.
fn clean(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_REF_LEN && !crate::dotenv::has_hidden(s)
}

fn strings(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).filter(|s| clean(s)).map(String::from).take(MAX_REFS).collect())
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// The entry holding `r` that was read from 1Password on this Mac (the only kind ever sent).
fn shareable<'a>(metas: &'a BTreeMap<String, Meta>, r: &str) -> Option<(&'a String, &'a Meta)> {
    metas.iter().find(|(n, m)| m.is_from_1password() && m.reference.as_deref() == Some(r) && !n.starts_with(INTERNAL_PREFIX))
}

// ------------------------------------------------------------------ sender

/// What each paired Mac last said it waits for (peer id → (when, refs)). References only.
static WANTS: Mutex<BTreeMap<String, (i64, Vec<String>)>> = Mutex::new(BTreeMap::new());

/// `{want: [ref], have: {ref: fetchedAt}}` from a paired Mac → `{offer: [{ref, fetchedAt}]}`: granted
/// references it asked for that this Mac has, and newer values of ones it holds (when `pushToPeers`).
pub fn offer(p: &SharedPeer, v: &Value) -> Value {
    let want = strings(&v["want"]);
    let have: BTreeMap<String, i64> = v["have"]
        .as_object()
        .map(|o| o.iter().take(MAX_REFS).map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).filter(|(k, _)| clean(k)).collect())
        .unwrap_or_default();
    WANTS.lock().unwrap_or_else(|e| e.into_inner()).insert(p.id.clone(), (now_ms(), want.clone()));
    if secrets::is_locked() {
        return json!({ "offer": [] });
    }
    let push = super::terminal::load().onepassword.push_to_peers;
    let metas = secrets::all_meta();
    let refs: BTreeSet<&String> = want.iter().chain(have.keys()).collect();
    let offer: Vec<Value> = refs
        .into_iter()
        .filter(|r| p.grants.secrets.contains(r))
        .filter_map(|r| {
            let (_, m) = shareable(&metas, r)?;
            let at = m.fetched_at.unwrap_or(0);
            let send = match have.get(r) {
                None => true,
                Some(h) => push && at > *h,
            };
            send.then(|| json!({ "ref": r, "fetchedAt": at }))
        })
        .collect();
    json!({ "offer": offer })
}

/// `{refs: [..]}` → `{values: [{ref, value, fetchedAt}]}` for references granted to this Mac.
pub fn serve(remote: &Remote, p: &SharedPeer, v: &Value) -> Res<Value> {
    if secrets::is_locked() {
        return Err((423, "the secret store is locked".into()));
    }
    let refs = strings(&v["refs"]);
    if let Some(r) = refs.iter().find(|r| !p.grants.secrets.contains(r)) {
        return Err((403, format!("{r} isn't shared with this Mac")));
    }
    let metas = secrets::all_meta();
    let mut out = vec![];
    let mut sent = vec![];
    for r in &refs {
        let Some((n, m)) = shareable(&metas, r) else { continue };
        let Some(mut val) = secrets::internal_get(n) else { continue };
        out.push(json!({ "ref": r, "value": val, "fetchedAt": m.fetched_at.unwrap_or(0) }));
        val.zeroize();
        sent.push(r.clone());
    }
    if !sent.is_empty() {
        remote.audit(json!({ "event": "secrets-sent", "peer": p.name, "refs": sent }));
        envimport::audit(&format!("peer:{}", p.name), "peer-sent", json!({ "peer": p.name, "refs": sent }));
    }
    Ok(json!({ "values": out }))
}

/// For Secrets › Share: per paired Mac, what it's granted and what it waits for (and whether this Mac has it).
pub fn shares(remote: &Remote) -> Value {
    let metas = secrets::all_meta();
    let wants = WANTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let item = |r: &String| {
        let mine = shareable(&metas, r);
        json!({ "ref": r, "name": mine.map(|(n, _)| n), "have": mine.is_some(), "fetchedAt": mine.and_then(|(_, m)| m.fetched_at) })
    };
    let peers: Vec<Value> = remote
        .config()
        .peers
        .iter()
        .map(|p| {
            let (at, want) = wants.get(&p.id).cloned().unwrap_or_default();
            json!({
                "id": p.id,
                "name": p.name,
                "granted": p.grants.secrets.iter().map(item).collect::<Vec<_>>(),
                "wants": want.iter().map(|r| { let mut v = item(r); v["granted"] = json!(p.grants.secrets.contains(r)); v }).collect::<Vec<_>>(),
                "wantsAt": (at > 0).then_some(at),
            })
        })
        .collect();
    let received: Vec<Value> = metas
        .iter()
        .filter(|(_, m)| m.is_from_peer())
        .map(|(n, m)| json!({ "name": n, "ref": m.reference, "from": m.source.as_deref().and_then(|s| s.strip_prefix("peer:")), "fetchedAt": m.fetched_at }))
        .collect();
    let available: Vec<Value> = OFFERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flat_map(|(m, refs)| refs.iter().map(move |r| json!({ "machine": m, "ref": r })))
        .collect();
    json!({ "peers": peers, "received": received, "available": available })
}

/// Share (or stop sharing) stored 1Password values with a paired Mac. `items` are stored names or references.
pub fn set_share(remote: &Remote, peer: &str, items: &[String], allow: bool, actor: &str) -> Res<Value> {
    let p = remote.peer(peer).ok_or((404, format!("no paired machine {peer:?} (lstack remote peers)")))?;
    let metas = secrets::all_meta();
    let mut refs = vec![];
    for i in items {
        let r = match metas.get(i) {
            Some(m) => match (m.is_from_1password(), m.reference.clone()) {
                (true, Some(r)) => r,
                _ if m.is_from_peer() => return Err((400, format!("{i} came from another Mac; only values read from 1Password here can be shared"))),
                _ => return Err((400, format!("{i} wasn't read from 1Password; only 1Password-sourced values can be shared"))),
            },
            None => i.clone(),
        };
        if allow && shareable(&metas, &r).is_none() && !p.grants.secrets.contains(&r) {
            return Err((404, format!("{i}: no value read from 1Password on this Mac holds that reference")));
        }
        if r.starts_with(INTERNAL_PREFIX) {
            return Err((400, format!("{r} is reserved for LStack")));
        }
        refs.push(r);
    }
    if refs.is_empty() {
        return Err((400, "nothing to share".into()));
    }
    let (name, _) = remote.set_secret_grants(&p.id, &refs, allow).map_err(|e| (e.0, e.1))?;
    envimport::audit(actor, if allow { "share" } else { "unshare" }, json!({ "peer": name, "refs": refs }));
    Ok(shares(remote))
}

// ------------------------------------------------------------------ receiver

/// References paired Macs offered that we haven't taken (machine → refs), for the review screens.
static OFFERS: Mutex<BTreeMap<String, BTreeSet<String>>> = Mutex::new(BTreeMap::new());

/// Machines offering `r` right now.
pub fn offered_by(r: &str) -> Vec<String> {
    OFFERS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, s)| s.contains(r)).map(|(m, _)| m.clone()).collect()
}

struct Want {
    service: String,
    key: String,
    reference: String,
    file: String,
}

/// References our services' env files wait for that aren't stored yet.
fn wants(sup: &Sup, metas: &BTreeMap<String, Meta>, only: Option<&str>) -> Vec<Want> {
    let op = super::terminal::load().onepassword;
    let mut out = vec![];
    for cfg in sup.effective_configs() {
        if only.is_some_and(|s| s != cfg.name) || cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
            continue;
        }
        for x in envimport::review(&cfg, metas, &op).refs {
            if !matches!(x.status, "new" | "changed" | "missing") || metas.values().any(|m| m.imported() && m.reference.as_deref() == Some(&x.reference)) {
                continue;
            }
            out.push(Want { service: cfg.name.clone(), key: x.key, reference: x.reference, file: x.file });
        }
    }
    out
}

/// A fresh name for a received value: never one a service is already granted (not even when its value is
/// missing), so nothing from a paired Mac reaches a service until someone approves it in an import.
fn receive_name(w: &Want, metas: &BTreeMap<String, Meta>, all: &[crate::types::ServiceConfig], taken: &mut HashMap<String, String>) -> Option<String> {
    let granted = |n: &str| all.iter().any(|s| s.secret_refs().iter().any(|(_, x)| x == n));
    let mut avoid = taken.clone();
    for _ in 0..100 {
        let c = envimport::allocate(&w.service, &w.key, &w.reference, metas, all, &avoid)?;
        if !metas.contains_key(&c) && !granted(&c) {
            return Some(c);
        }
        avoid.insert(c, String::new());
    }
    None
}

/// Ask online paired Macs for granted values: newer values of ones we hold from them always, and (with
/// `fetch_new`) references our services wait for. `only` limits the latter to one service. Blocking.
pub fn sync(sup: &Arc<Sup>, peers: &Peers, only: Option<&str>, fetch_new: bool) -> Value {
    if secrets::is_locked() {
        return json!({ "received": [], "errors": [] });
    }
    let mut received = vec![];
    let mut errors = vec![];
    let online = peers.online();
    OFFERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|m, _| online.contains(m));
    for machine in online {
        // Received values are tied to the Mac's pinned link key, not its name: a different Mac that later
        // takes the name (or re-pairs with a new key) can't update them.
        let Some(key) = peers.public_key(&machine) else { continue };
        let metas = secrets::all_meta();
        let src = format!("peer:{machine}");
        let from_this = |m: &Meta| m.is_from_peer() && m.peer_key.as_deref() == Some(key.as_str());
        let wants = wants(sup, &metas, None);
        let have: BTreeMap<String, i64> = metas.values().filter(|m| from_this(m)).filter_map(|m| Some((m.reference.clone()?, m.fetched_at.unwrap_or(0)))).collect();
        // What we wait for (vault/item/field names) is only told to paired Macs when someone asked to
        // import (or turned on pullFromPeers); the background sync otherwise only refreshes held values.
        let want_refs: BTreeSet<String> = if fetch_new { wants.iter().filter(|w| only.is_none_or(|s| s == w.service)).map(|w| w.reference.clone()).collect() } else { BTreeSet::new() };
        if want_refs.is_empty() && have.is_empty() {
            OFFERS.lock().unwrap_or_else(|e| e.into_inner()).remove(&machine);
            continue;
        }
        let offer = match peers.call_json(&machine, "POST", "/secrets/offer", &json!({ "want": want_refs, "have": have })) {
            Ok(v) => v,
            Err(e) if e.0 == 404 => continue, // an older LStack
            Err(e) => {
                errors.push(e.1);
                continue;
            }
        };
        let offered: Vec<(String, i64)> =
            offer["offer"].as_array().into_iter().flatten().filter_map(|o| Some((o["ref"].as_str()?.to_string(), o["fetchedAt"].as_i64().unwrap_or(0)))).collect();
        let fetch_for = |r: &str| fetch_new && wants.iter().any(|w| w.reference == r && only.is_none_or(|s| s == w.service));
        let pull: Vec<String> = offered
            .iter()
            .filter(|(r, at)| have.get(r).is_some_and(|h| at > h) || (!have.contains_key(r) && fetch_for(r)))
            .map(|(r, _)| r.clone())
            .collect();
        let waiting: BTreeSet<String> = offered.iter().map(|(r, _)| r.clone()).filter(|r| want_refs.contains(r) && !pull.contains(r)).collect();
        OFFERS.lock().unwrap_or_else(|e| e.into_inner()).insert(machine.clone(), waiting);
        if pull.is_empty() {
            continue;
        }
        let mut resp = match peers.call_json(&machine, "POST", "/secrets/pull", &json!({ "refs": pull })) {
            Ok(v) => v,
            Err(e) => {
                errors.push(e.1);
                continue;
            }
        };
        let all = sup.effective_configs();
        let mut taken: HashMap<String, String> = HashMap::new();
        let mut to_set = vec![];
        let mut updated = vec![];
        for item in resp["values"].as_array_mut().into_iter().flatten() {
            let r = item["ref"].as_str().unwrap_or_default().to_string();
            let at = item["fetchedAt"].as_i64().unwrap_or(0);
            let mut value = match item.get_mut("value").map(Value::take) {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            if !pull.contains(&r) || value.is_empty() || value.len() > MAX_VALUE || value.contains('\0') {
                value.zeroize();
                continue;
            }
            if have.contains_key(&r) {
                for (n, m) in metas.iter().filter(|(_, m)| from_this(m) && m.reference.as_deref() == Some(r.as_str())) {
                    let mut meta = m.clone();
                    meta.fetched_at = Some(at);
                    to_set.push((n.clone(), value.clone(), meta));
                    updated.push(n.clone());
                }
            } else if let Some(w) = wants.iter().find(|w| w.reference == r) {
                if let Some(n) = receive_name(w, &metas, &all, &mut taken) {
                    taken.insert(n.clone(), r.clone());
                    let meta = Meta {
                        source: Some(src.clone()),
                        reference: Some(r.clone()),
                        origin: Some(w.file.clone()),
                        fetched_at: Some(at),
                        peer_key: Some(key.clone()),
                        ..Default::default()
                    };
                    to_set.push((n, value.clone(), meta));
                }
            }
            value.zeroize();
        }
        if to_set.is_empty() {
            continue;
        }
        let names: Vec<String> = to_set.iter().map(|(n, _, _)| n.clone()).collect();
        let refs: Vec<String> = to_set.iter().filter_map(|(_, _, m)| m.reference.clone()).collect();
        if let Err(e) = secrets::set_many(to_set) {
            errors.push(e);
            continue;
        }
        sup.rescrub_logs();
        envimport::audit(&src, "peer-received", json!({ "peer": machine, "refs": refs, "stored": names }));
        let fresh: Vec<&String> = names.iter().filter(|n| !updated.contains(n)).collect();
        if !fresh.is_empty() {
            let users: BTreeSet<&str> = wants.iter().filter(|w| refs.contains(&w.reference)).map(|w| w.service.as_str()).collect();
            let users: Vec<&str> = users.into_iter().collect();
            sup.notice(
                &format!("Received 1Password values from {machine}"),
                &format!("{} value(s) for {}. Approve them with lstack secret import (no 1Password prompt needed).", fresh.len(), users.join(", ")),
            );
        }
        if !updated.is_empty() && super::terminal::load().onepassword.restart_on_change {
            envimport::restart_users(sup, &updated, None);
        }
        received.push(json!({ "machine": machine, "stored": names, "updated": updated }));
    }
    json!({ "received": received, "errors": errors })
}

/// Every 15 s: newer values of ones held from paired Macs, plus (with `pullFromPeers`) references our
/// services wait for. Cache to cache only — 1Password is never involved on either side.
pub fn spawn_sync(sup: Arc<Sup>, peers: Arc<Peers>) {
    tokio::spawn(async move {
        let mut last = Value::Null;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SYNC_SECS)).await;
            let (s, p) = (sup.clone(), peers.clone());
            let fetch_new = super::terminal::load().onepassword.pull_from_peers;
            let Ok(v) = tokio::task::spawn_blocking(move || sync(&s, &p, None, fetch_new)).await else { continue };
            // Log a failure once, not every 15 s.
            if v["errors"] != last {
                for e in v["errors"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    println!("{} secrets: {}", crate::paths::iso(now_ms()), crate::scrub::scrub_string(e.to_string()));
                }
                last = v["errors"].clone();
            }
        }
    });
}

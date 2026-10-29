//! Sharing 1Password-sourced values with paired Macs, from the encrypted store (never from 1Password).
//!
//! One ask, one approval — from either side:
//! - Request: the Mac that needs a value asks (`pullFromPeers` on: automatically every 15 s, so it reaches
//!   the other Mac whenever that one is online; or Pull Shared Values / `lstack secret sync`). The Mac
//!   holding it is notified with the service that wants it and approves (Share). The value then arrives
//!   and goes straight to the services that asked — no second confirmation.
//! - Push: the Mac holding values offers them (Machines › Push Secrets / `lstack secret push`). The other
//!   Mac is notified and accepts (or declines); accepted values go to its services that reference them.
//!
//! Security model:
//! - Sender: a paired Mac receives a value only for an exact reference granted to it here
//!   (`Grants.secrets`: an approved request, a push, or Share). Only values read from 1Password on this Mac
//!   (`op` / `op-env`) are sent: never ones typed in, never ones received from another Mac (no re-sharing),
//!   never `LSTACK_*`. Serving them never reads 1Password, so a peer can't cause an approval prompt here.
//! - Receiver: takes a new value only for a reference it asked for (its own env files reference it) or
//!   a push someone accepted here. It never overwrites a stored value that holds a different reference.
//!   Values received are tied to the sender's pinned link key; only that key can update them later. What
//!   we wait for (references and service names) is told to paired Macs only when someone asked (Pull
//!   Shared Values, or `pullFromPeers` on).
//! - Both sides audit transfers (`secrets-audit.log`; the sender also in `lstack remote activity`).

use super::envimport::{self, Res};
use super::peers::{PeerCfg, Peers};
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
/// Matched by reference, never by name: names differ between Macs (and may collide). Several entries may
/// hold the same reference; the most recently read one is used.
fn shareable<'a>(metas: &'a BTreeMap<String, Meta>, r: &str) -> Option<(&'a String, &'a Meta)> {
    metas.iter().filter(|(n, m)| m.is_from_1password() && m.reference.as_deref() == Some(r) && !n.starts_with(INTERNAL_PREFIX)).max_by_key(|(_, m)| m.fetched_at.unwrap_or(0))
}

// ------------------------------------------------------------------ sender

/// What a paired Mac last asked for: when, references, and which of its services want each.
#[derive(Clone, Default)]
struct Asked {
    at: i64,
    refs: Vec<String>,
    services: BTreeMap<String, Vec<String>>,
    /// The env key it uses for each (a naming hint only; values are matched by reference).
    keys: BTreeMap<String, String>,
}

/// Per paired Mac id. References and service names only.
static WANTS: Mutex<BTreeMap<String, Asked>> = Mutex::new(BTreeMap::new());
/// (peer id, ref) requests already announced here, and ones someone declined (hidden until restart).
static ANNOUNCED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());
static IGNORED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A paired Mac values can go to: one that watches this Mac (`SharedPeer`: it calls here for them) or one
/// this Mac watches (`PeerCfg`: we deliver them over our link). Paired both ways, the first is used.
#[derive(Clone)]
struct Party {
    /// `SharedPeer` id, or `machine:<name>` for a watched machine.
    id: String,
    name: String,
    granted: Vec<String>,
    pushing: Vec<String>,
    watched: bool,
}

const MACHINE: &str = "machine:";

impl Party {
    fn shared(p: &SharedPeer) -> Self {
        Party { id: p.id.clone(), name: p.name.clone(), granted: p.grants.secrets.clone(), pushing: p.grants.push.clone(), watched: false }
    }
    fn machine(c: &PeerCfg) -> Self {
        Party { id: format!("{MACHINE}{}", c.name), name: c.name.clone(), granted: c.secrets.clone(), pushing: c.push.clone(), watched: true }
    }
}

/// Watched machines values go to through our link: not ones that also watch this Mac (they ask here).
fn watched_parties(remote: &Remote, peers: &Peers) -> Vec<PeerCfg> {
    let keys: BTreeSet<String> = remote.config().peers.iter().map(|p| p.public_key.clone()).collect();
    peers.machines().into_iter().filter(|c| !c.legacy() && !keys.contains(&c.public_key)).collect()
}

/// Share (`allow`) / unshare references with a paired Mac.
fn set_grants(remote: &Remote, peers: &Peers, p: &Party, refs: &[String], allow: bool) -> Res<()> {
    if !p.watched {
        return remote.set_secret_grants(&p.id, refs, allow).map(|_| ()).map_err(|e| (e.0, e.1));
    }
    peers
        .set_secret_lists(&p.name, |c| {
            for r in refs {
                c.secrets.retain(|x| x != r);
                if allow {
                    c.secrets.push(r.clone());
                }
            }
        })
        .map_err(|e| (e.0, e.1))?;
    remote.audit(json!({ "event": if allow { "secrets-shared" } else { "secrets-unshared" }, "peer": p.name, "refs": refs }));
    Ok(())
}

/// Offer (`add`) shared references to a paired Mac for it to accept, or forget the offer.
fn set_push(remote: &Remote, peers: &Peers, p: &Party, refs: &[String], add: bool) -> Res<()> {
    if !p.watched {
        return remote.set_secret_push(&p.id, refs, add).map(|_| ()).map_err(|e| (e.0, e.1));
    }
    let before = p.pushing.clone();
    let c = peers
        .set_secret_lists(&p.name, |c| {
            for r in refs {
                c.push.retain(|x| x != r);
                if add {
                    c.push.push(r.clone());
                }
            }
        })
        .map_err(|e| (e.0, e.1))?;
    if add && c.push != before {
        remote.audit(json!({ "event": "secrets-pushed", "peer": p.name, "refs": refs }));
    }
    Ok(())
}

/// `{want: [ref], wantFor: {ref: [service]}, have: {ref: fetchedAt}, declined: [ref]}` from a paired Mac →
/// `{offer: [{ref, fetchedAt}], push: [{ref, name, fetchedAt}]}`: granted references it asked for, newer
/// values of ones it holds (when `pushToPeers`), and values pushed to it that it hasn't answered.
/// References only, never values. New requests for values held here are announced once.
pub fn offer(sup: &Sup, remote: &Remote, peers: &Peers, p: &SharedPeer, v: &Value) -> Value {
    plan(sup, remote, peers, &Party::shared(p), v)
}

fn plan(sup: &Sup, remote: &Remote, peers: &Peers, p: &Party, v: &Value) -> Value {
    let want = strings(&v["want"]);
    let services: BTreeMap<String, Vec<String>> =
        v["wantFor"].as_object().map(|o| o.iter().filter(|(r, _)| want.contains(r)).map(|(r, s)| (r.clone(), strings(s).into_iter().take(20).collect())).collect()).unwrap_or_default();
    let have: BTreeMap<String, i64> = v["have"].as_object().map(|o| o.iter().take(MAX_REFS).map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).filter(|(k, _)| clean(k)).collect()).unwrap_or_default();
    let keys: BTreeMap<String, String> = v["wantKey"]
        .as_object()
        .map(|o| o.iter().filter(|(r, _)| want.contains(r)).filter_map(|(r, k)| Some((r.clone(), k.as_str().filter(|k| secrets::valid_name(k))?.to_string()))).collect())
        .unwrap_or_default();
    let declined = strings(&v["declined"]);
    lock(&WANTS).insert(p.id.clone(), Asked { at: now_ms(), refs: want.clone(), services: services.clone(), keys: keys.clone() });
    // A push it declined, or already holds, is no longer waiting there.
    let done: Vec<String> = p.pushing.iter().filter(|r| declined.contains(r) || have.contains_key(*r)).cloned().collect();
    let mut pushing = p.pushing.clone();
    if !done.is_empty() {
        pushing.retain(|r| !done.contains(r));
        let _ = set_push(remote, peers, p, &done, false);
        if !declined.is_empty() {
            envimport::audit(&format!("peer:{}", p.name), "push-declined", json!({ "peer": p.name, "refs": declined }));
        }
    }
    if secrets::is_locked() {
        return json!({ "offer": [], "push": [] });
    }
    let op = super::terminal::load().onepassword;
    let metas = secrets::all_meta();
    announce(sup, p, &want, &services, &keys, &metas);
    let refs: BTreeSet<&String> = want.iter().chain(have.keys()).collect();
    let offer: Vec<Value> = refs
        .into_iter()
        .filter(|r| p.granted.contains(r))
        .filter_map(|r| {
            let (_, m) = shareable(&metas, r)?;
            let at = m.fetched_at.unwrap_or(0);
            let send = match have.get(r) {
                None => true,
                Some(h) => op.push_to_peers && at > *h,
            };
            send.then(|| json!({ "ref": r, "fetchedAt": at }))
        })
        .collect();
    json!({ "offer": offer, "push": push_list(&metas, p, &pushing) })
}

/// Pending pushes, as told to the other Mac: references, names and dates — never values.
fn push_list(metas: &BTreeMap<String, Meta>, p: &Party, pushing: &[String]) -> Vec<Value> {
    pushing
        .iter()
        .filter(|r| p.granted.contains(r))
        .filter_map(|r| {
            let (n, m) = shareable(metas, r)?;
            Some(json!({ "ref": r, "name": n, "fetchedAt": m.fetched_at.unwrap_or(0) }))
        })
        .collect()
}

/// Notify once per new request this Mac hasn't approved: for values held here, and (approving reads
/// it from 1Password here) for ones that aren't. The notice carries the request so it can be approved
/// from the notification.
fn announce(sup: &Sup, p: &Party, want: &[String], services: &BTreeMap<String, Vec<String>>, keys: &BTreeMap<String, String>, metas: &BTreeMap<String, Meta>) {
    let ignored = lock(&IGNORED).clone();
    let mut seen = lock(&ANNOUNCED);
    let mut fresh = vec![];
    let mut items = vec![];
    for r in want {
        let key = (p.id.clone(), r.clone());
        if p.granted.contains(r) || ignored.contains(&key) || seen.contains(&key) || r.starts_with(INTERNAL_PREFIX) {
            continue;
        }
        seen.insert(key);
        let svcs = services.get(r).map(|s| s.join(", ")).filter(|s| !s.is_empty()).unwrap_or_else(|| "a service".into());
        let mine = shareable(metas, r).map(|(n, _)| n.clone());
        fresh.push(match &mine {
            Some(n) => format!("{n} ({r}) for {svcs}"),
            None => format!("{r} for {svcs} — not on this Mac; approving reads it from 1Password"),
        });
        items.push(json!({ "ref": r, "name": mine, "have": mine.is_some(), "key": keys.get(r), "services": services.get(r) }));
    }
    drop(seen);
    if !fresh.is_empty() {
        sup.notice_with(
            &format!("{} asks for 1Password values", p.name),
            &fresh.join("; "),
            json!({ "request": { "peer": p.id, "machine": p.name, "items": items } }),
        );
    }
}

/// Cached values of `refs` (only ones read from 1Password here) and the references they hold.
fn values_for(refs: &[String]) -> (Vec<Value>, Vec<String>) {
    let metas = secrets::all_meta();
    let mut out = vec![];
    let mut sent = vec![];
    for r in refs {
        let Some((n, m)) = shareable(&metas, r) else { continue };
        let Some(mut val) = secrets::internal_get(n) else { continue };
        out.push(json!({ "ref": r, "value": val, "fetchedAt": m.fetched_at.unwrap_or(0) }));
        val.zeroize();
        sent.push(r.clone());
    }
    (out, sent)
}

fn sent(remote: &Remote, peers: &Peers, p: &Party, refs: &[String]) {
    if refs.is_empty() {
        return;
    }
    let delivered: Vec<String> = refs.iter().filter(|r| p.pushing.contains(r)).cloned().collect();
    if !delivered.is_empty() {
        let _ = set_push(remote, peers, p, &delivered, false);
    }
    remote.audit(json!({ "event": "secrets-sent", "peer": p.name, "refs": refs }));
    envimport::audit(&format!("peer:{}", p.name), "peer-sent", json!({ "peer": p.name, "refs": refs }));
}

/// `{refs: [..]}` → `{values: [{ref, value, fetchedAt}]}` for references granted to this Mac.
pub fn serve(remote: &Remote, peers: &Peers, p: &SharedPeer, v: &Value) -> Res<Value> {
    if secrets::is_locked() {
        return Err((423, "the secret store is locked".into()));
    }
    let refs = strings(&v["refs"]);
    if let Some(r) = refs.iter().find(|r| !p.grants.secrets.contains(r)) {
        return Err((403, format!("{r} isn't shared with this Mac")));
    }
    let (out, refs) = values_for(&refs);
    sent(remote, peers, &Party::shared(p), &refs);
    Ok(json!({ "values": out }))
}

/// For a Mac this one watches (it can't call here): tell it what we push, learn what it asks for and
/// accepted, and deliver what's approved over our link. Blocking.
fn serve_watched(sup: &Sup, remote: &Remote, peers: &Peers, c: &PeerCfg) -> Result<Vec<String>, String> {
    let p = Party::machine(c);
    let metas = secrets::all_meta();
    let push = if secrets::is_locked() { vec![] } else { push_list(&metas, &p, &p.pushing) };
    let needs = match peers.call_json(&p.name, "POST", "/secrets/needs", &json!({ "push": push })) {
        Ok(v) => v,
        Err(e) if e.0 == 404 => return Ok(vec![]), // an older LStack
        Err(e) => return Err(e.1),
    };
    let o = plan(sup, remote, peers, &p, &needs);
    let p = peers.machine(&p.name).map(|c| Party::machine(&c)).unwrap_or(p);
    let mut refs: Vec<String> = o["offer"].as_array().into_iter().flatten().filter_map(|x| x["ref"].as_str().map(String::from)).collect();
    refs.extend(strings(&needs["accepted"]).into_iter().filter(|r| p.pushing.contains(r) && p.granted.contains(r)));
    refs.sort();
    refs.dedup();
    if refs.is_empty() || secrets::is_locked() {
        return Ok(vec![]);
    }
    let (values, refs) = values_for(&refs);
    if refs.is_empty() {
        return Ok(vec![]);
    }
    peers.call_json(&p.name, "POST", "/secrets/deliver", &json!({ "values": values })).map_err(|e| e.1)?;
    sent(remote, peers, &p, &refs);
    Ok(refs)
}

/// Deliver to every online machine this Mac watches what it asked for (and we approved) or accepted.
pub fn serve_watchers(sup: &Sup, remote: &Remote, peers: &Peers, only: Option<&str>) -> Vec<String> {
    let online = peers.online();
    let mut errors = vec![];
    for c in watched_parties(remote, peers).into_iter().filter(|c| online.contains(&c.name) && only.is_none_or(|n| n == c.name)) {
        if let Err(e) = serve_watched(sup, remote, peers, &c) {
            errors.push(e);
        }
    }
    errors
}

/// For Secrets › Shared with Macs and Machines: per paired Mac, what it's granted, what's pushed and not
/// answered yet, and what it asks for (with its services, and whether this Mac has it). Plus, as the
/// receiving side, values received here and pushes waiting for approval here.
pub fn shares(remote: &Remote, peers: &Peers) -> Value {
    let metas = secrets::all_meta();
    let wants = lock(&WANTS).clone();
    let ignored = lock(&IGNORED).clone();
    let item = |r: &String| {
        let mine = shareable(&metas, r);
        json!({ "ref": r, "name": mine.map(|(n, _)| n), "have": mine.is_some(), "fetchedAt": mine.and_then(|(_, m)| m.fetched_at) })
    };
    let parties: Vec<Party> = remote.config().peers.iter().map(Party::shared).chain(watched_parties(remote, peers).iter().map(Party::machine)).collect();
    let list: Vec<Value> = parties
        .iter()
        .map(|p| {
            let asked = wants.get(&p.id).cloned().unwrap_or_default();
            json!({
                "id": p.id,
                "name": p.name,
                "watched": p.watched,
                "granted": p.granted.iter().map(item).collect::<Vec<_>>(),
                "pushing": p.pushing.iter().map(item).collect::<Vec<_>>(),
                "wants": asked.refs.iter().map(|r| {
                    let mut v = item(r);
                    v["granted"] = json!(p.granted.contains(r));
                    v["services"] = json!(asked.services.get(r).cloned().unwrap_or_default());
                    v["declined"] = json!(ignored.contains(&(p.id.clone(), r.clone())));
                    v["key"] = json!(asked.keys.get(r));
                    v
                }).collect::<Vec<_>>(),
                "wantsAt": (asked.at > 0).then_some(asked.at),
            })
        })
        .collect();
    let received: Vec<Value> = metas
        .iter()
        .filter(|(_, m)| m.is_from_peer())
        .map(|(n, m)| json!({ "name": n, "ref": m.reference, "from": m.source.as_deref().and_then(|s| s.strip_prefix("peer:")), "fetchedAt": m.fetched_at }))
        .collect();
    let incoming: Vec<Value> = live_inboxes()
        .into_iter()
        .flat_map(|b| b.items.into_iter().map(move |i| json!({ "machine": b.machine, "ref": i.reference, "name": i.name, "fetchedAt": i.at, "services": i.services })))
        .collect();
    json!({ "peers": list, "received": received, "incoming": incoming })
}

/// Stored names or references → shareable references for a paired Mac.
fn resolve(p: &Party, items: &[String], allow: bool) -> Res<Vec<String>> {
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
        if allow && shareable(&metas, &r).is_none() && !p.granted.contains(&r) {
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
    Ok(refs)
}

/// A paired Mac by `SharedPeer` id or name, or a watched machine (`machine:<name>` or its name).
fn find_party(remote: &Remote, peers: &Peers, peer: &str) -> Res<Party> {
    if let Some(p) = remote.peer(peer) {
        return Ok(Party::shared(&p));
    }
    let name = peer.strip_prefix(MACHINE).unwrap_or(peer);
    let c = peers.machine(name).ok_or((404, format!("no paired machine {peer:?} (lstack remote peers, lstack machines)")))?;
    match remote.config().peers.iter().find(|p| p.public_key == c.public_key) {
        Some(p) => Ok(Party::shared(p)),
        None if c.legacy() => Err((409, format!("{name} needs pairing again"))),
        None => Ok(Party::machine(&c)),
    }
}

/// Deliver right away to a watched machine (instead of on the next 15 s round). Blocking.
pub fn nudge(sup: &Sup, remote: &Remote, peers: &Peers, peer: &str) {
    if let Ok(p) = find_party(remote, peers, peer) {
        if p.watched {
            serve_watchers(sup, remote, peers, Some(&p.name));
        }
    }
}

/// Share (or stop sharing) stored 1Password values with a paired Mac. `items` are stored names or
/// references. Approving a request: the value goes to the asking services on its next sync.
pub fn set_share(remote: &Remote, peers: &Peers, peer: &str, items: &[String], allow: bool, actor: &str) -> Res<Value> {
    let p = find_party(remote, peers, peer)?;
    let refs = resolve(&p, items, allow)?;
    set_grants(remote, peers, &p, &refs, allow)?;
    let mut ignored = lock(&IGNORED);
    for r in &refs {
        ignored.remove(&(p.id.clone(), r.clone()));
    }
    drop(ignored);
    envimport::audit(actor, if allow { "share" } else { "unshare" }, json!({ "peer": p.name, "refs": refs }));
    Ok(shares(remote, peers))
}

/// Push stored 1Password values to a paired Mac: share them and ask it to accept them. It sees the offer
/// within 15 s of being online and must accept before anything is sent.
pub fn push(remote: &Remote, peers: &Peers, peer: &str, items: &[String], actor: &str) -> Res<Value> {
    let p = find_party(remote, peers, peer)?;
    let refs = resolve(&p, items, true)?;
    set_grants(remote, peers, &p, &refs, true)?;
    set_push(remote, peers, &p, &refs, true)?;
    envimport::audit(actor, "push", json!({ "peer": p.name, "refs": refs }));
    Ok(shares(remote, peers))
}

/// Decline (hide) requests from a paired Mac. They show again after a restart if it still asks.
pub fn ignore(remote: &Remote, peers: &Peers, peer: &str, refs: &[String], actor: &str) -> Res<Value> {
    let p = find_party(remote, peers, peer)?;
    let mut ignored = lock(&IGNORED);
    for r in refs {
        ignored.insert((p.id.clone(), r.clone()));
    }
    drop(ignored);
    envimport::audit(actor, "request-declined", json!({ "peer": p.name, "refs": refs }));
    Ok(shares(remote, peers))
}

/// Plans that read a requested value from 1Password here, to share with the Mac that asked once imported:
/// plan id → (party id, reference).
static FULFILL: Mutex<BTreeMap<String, (String, String)>> = Mutex::new(BTreeMap::new());

/// A store name for a value read to answer a request: `hint` (the other Mac's env key) or one derived from
/// the reference, never one already used here — a clash must not replace a different value.
pub fn free_name(sup: &Sup, r: &str, hint: Option<&str>) -> Option<String> {
    let metas = secrets::all_meta();
    let used: BTreeSet<String> = sup.effective_configs().iter().flat_map(|c| c.secret_refs().into_iter().map(|(_, n)| n)).collect();
    let derived = || {
        let (_, item, field) = crate::dotenv::ref_parts(r).unwrap_or_default();
        let raw = format!("{item}_{field}").to_uppercase();
        let s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let s = s.trim_matches('_').to_string();
        if s.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("S_{s}") } else { s }
    };
    let base = hint.filter(|h| secrets::valid_name(h) && !h.starts_with(INTERNAL_PREFIX)).map(String::from).unwrap_or_else(derived);
    let base = if secrets::valid_name(&base) && !base.starts_with(INTERNAL_PREFIX) { base } else { "SHARED_SECRET".into() };
    std::iter::once(base.clone()).chain((2..100).map(|k| format!("{base}_{k}"))).find(|n| !metas.contains_key(n) && !used.contains(n))
}

/// Read `r` from 1Password here to answer `peer`'s request: the plan is shared with it once imported.
pub fn fulfill_later(remote: &Remote, peers: &Peers, plan_id: &str, peer: &str, r: &str) -> Res<()> {
    let p = find_party(remote, peers, peer)?;
    lock(&FULFILL).insert(plan_id.to_string(), (p.id, r.to_string()));
    Ok(())
}

/// After an import: share values read for a paired Mac's request with it (and deliver now if we watch
/// it). Returns that Mac's name.
pub fn fulfilled(sup: &Sup, remote: &Remote, peers: &Peers, plan_id: &str, actor: &str) -> Option<String> {
    let (peer, r) = lock(&FULFILL).remove(plan_id)?;
    let p = find_party(remote, peers, &peer).ok()?;
    shareable(&secrets::all_meta(), &r)?;
    set_share(remote, peers, &peer, std::slice::from_ref(&r), true, actor).ok()?;
    nudge(sup, remote, peers, &peer);
    Some(p.name)
}

// ------------------------------------------------------------------ receiver

#[derive(Clone)]
struct Incoming {
    reference: String,
    name: String,
    at: i64,
    /// Our services whose env files reference it (accepting passes it to them).
    services: Vec<String>,
}

/// Values one paired Mac pushed that wait for approval here.
#[derive(Clone)]
struct Inbox {
    /// Its name here.
    machine: String,
    /// `SharedPeer` id when it watches this Mac (it delivers on its next round once accepted); `None` when
    /// this Mac watches it (we pull on accept).
    via: Option<String>,
    at: i64,
    items: Vec<Incoming>,
}

/// Keyed by machine name (Macs we watch) or `w:<id>` (Macs that watch us).
static INCOMING: Mutex<BTreeMap<String, Inbox>> = Mutex::new(BTreeMap::new());
/// Pushes declined here, to tell that Mac on the next sync (inbox key → refs).
static DECLINED: Mutex<BTreeMap<String, BTreeSet<String>>> = Mutex::new(BTreeMap::new());
/// (inbox key, ref) pushes already announced here.
static PUSH_SEEN: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());
/// Pushes accepted here from Macs that watch us, until they deliver them (`SharedPeer` id → offers).
static ACCEPTED: Mutex<BTreeMap<String, Vec<Incoming>>> = Mutex::new(BTreeMap::new());
/// Until when Macs that watch us are told what we wait for without `pullFromPeers` (after Pull Shared Values).
static ASK_UNTIL: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// Macs that watch us and asked what we need: id → (name, when).
static WATCHERS: Mutex<BTreeMap<String, (String, i64)>> = Mutex::new(BTreeMap::new());
/// A Mac that watches us counts as online (for its pushes and our requests) this long after its last round.
const WATCHER_TTL: i64 = 3 * SYNC_SECS as i64 * 1000;

fn live_inboxes() -> Vec<Inbox> {
    let now = now_ms();
    lock(&INCOMING).values().filter(|b| !b.items.is_empty() && (b.via.is_none() || now - b.at < WATCHER_TTL)).cloned().collect()
}

/// Machines that pushed `r` and wait for us to accept it.
pub fn offered_by(r: &str) -> Vec<String> {
    live_inboxes().into_iter().filter(|b| b.items.iter().any(|i| i.reference == r)).map(|b| b.machine).collect()
}

fn asking() -> bool {
    super::terminal::load().onepassword.pull_from_peers || now_ms() < ASK_UNTIL.load(std::sync::atomic::Ordering::Relaxed)
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

/// A fresh name for a received value: never one a service already uses for something else.
fn receive_name(service: &str, key: &str, r: &str, metas: &BTreeMap<String, Meta>, all: &[crate::types::ServiceConfig], taken: &mut HashMap<String, String>) -> Option<String> {
    let granted = |n: &str| all.iter().any(|s| s.secret_refs().iter().any(|(_, x)| x == n));
    let mut avoid = taken.clone();
    for _ in 0..100 {
        let c = envimport::allocate(service, key, r, metas, all, &avoid)?;
        if !metas.contains_key(&c) && !granted(&c) {
            return Some(c);
        }
        avoid.insert(c, String::new());
    }
    None
}

/// What a sync (or accept) took from one Mac.
#[derive(Default)]
struct Taken {
    stored: Vec<String>,
    updated: Vec<String>,
    services: Vec<String>,
}

/// Pull `refs` from `machine` and store them. Ones we hold from it are updated in place; new ones go to
/// the services in `wants` that reference them (the ask here + the approval there is the one approval).
/// With `pushed` (accepted pushes), new ones no service references are kept under the sender's name.
fn take(sup: &Arc<Sup>, peers: &Peers, machine: &str, key: &str, refs: &[String], wants: &[Want], pushed: &[Incoming]) -> Result<Taken, String> {
    let resp = peers.call_json(machine, "POST", "/secrets/pull", &json!({ "refs": refs })).map_err(|e| e.1)?;
    store(sup, machine, key, resp, refs, wants, pushed)
}

/// Store `{values: [{ref, value, fetchedAt}]}` from `machine` (link key `key`); only `refs` are taken.
fn store(sup: &Arc<Sup>, machine: &str, key: &str, mut resp: Value, refs: &[String], wants: &[Want], pushed: &[Incoming]) -> Result<Taken, String> {
    let metas = secrets::all_meta();
    let from_this = |m: &Meta| m.is_from_peer() && m.peer_key.as_deref() == Some(key);
    let all = sup.effective_configs();
    let src = format!("peer:{machine}");
    let mut taken: HashMap<String, String> = HashMap::new();
    let mut to_set = vec![];
    let mut out = Taken::default();
    let mut grants: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for item in resp["values"].as_array_mut().into_iter().flatten() {
        let r = item["ref"].as_str().unwrap_or_default().to_string();
        let at = item["fetchedAt"].as_i64().unwrap_or(0);
        let mut value = match item.get_mut("value").map(Value::take) {
            Some(Value::String(s)) => s,
            _ => continue,
        };
        if !refs.contains(&r) || value.len() > MAX_VALUE || value.contains('\0') {
            value.zeroize();
            continue;
        }
        let held: Vec<(&String, &Meta)> = metas.iter().filter(|(_, m)| from_this(m) && m.reference.as_deref() == Some(r.as_str())).collect();
        if !held.is_empty() {
            for (n, m) in held {
                let mut meta = m.clone();
                meta.fetched_at = Some(at);
                to_set.push((n.clone(), value.clone(), meta));
                out.updated.push(n.clone());
            }
        } else {
            let users: Vec<&Want> = wants.iter().filter(|w| w.reference == r).collect();
            let p = pushed.iter().find(|i| i.reference == r);
            let name = match (users.first(), p) {
                (Some(w), _) => receive_name(&w.service, &w.key, &r, &metas, &all, &mut taken),
                (None, Some(i)) => {
                    let granted = |n: &str| all.iter().any(|s| s.secret_refs().iter().any(|(_, x)| x == n));
                    let ok = |n: &str| !metas.contains_key(n) && !granted(n) && !taken.contains_key(n) && !n.starts_with(INTERNAL_PREFIX);
                    let base = if secrets::valid_name(&i.name) { i.name.clone() } else { "SHARED_SECRET".into() };
                    std::iter::once(base.clone()).chain((2..100).map(|k| format!("{base}_{k}"))).find(|n| ok(n))
                }
                _ => None,
            };
            let Some(n) = name else {
                value.zeroize();
                continue;
            };
            taken.insert(n.clone(), r.clone());
            for w in users {
                grants.entry(w.service.clone()).or_default().insert(w.key.clone(), n.clone());
            }
            let meta = Meta {
                source: Some(src.clone()),
                reference: Some(r.clone()),
                origin: wants.iter().find(|w| w.reference == r).map(|w| w.file.clone()),
                fetched_at: Some(at),
                peer_key: Some(key.to_string()),
                ..Default::default()
            };
            to_set.push((n.clone(), value.clone(), meta));
            out.stored.push(n);
        }
        value.zeroize();
    }
    if to_set.is_empty() {
        return Ok(out);
    }
    let refs: Vec<String> = to_set.iter().filter_map(|(_, _, m)| m.reference.clone()).collect();
    let names: Vec<String> = to_set.iter().map(|(n, _, _)| n.clone()).collect();
    secrets::set_many(to_set)?;
    sup.rescrub_logs();
    envimport::audit(&src, "peer-received", json!({ "peer": machine, "refs": refs, "stored": names }));
    out.services = envimport::grant_received(sup, &grants, machine);
    let restart = super::terminal::load().onepassword.restart_on_change;
    if restart && !out.updated.is_empty() {
        envimport::restart_users(sup, &out.updated, None);
    }
    if !out.stored.is_empty() {
        let used = if out.services.is_empty() { String::new() } else { format!(" — now passed to {}", out.services.join(", ")) };
        let tail = if out.services.is_empty() || restart { "" } else { " (restart running ones to use them)" };
        sup.notice(&format!("Received 1Password values from {machine}"), &format!("{}{used}{tail}", out.stored.join(", ")));
        if restart {
            for s in &out.services {
                envimport::restart_users(sup, &[], Some(s));
            }
        }
    }
    Ok(out)
}

/// A value received from a paired Mac that a service still lists as new (a receive that stored it but
/// never attached it): attach it, so the service stops waiting and nothing asks for it again.
fn heal(sup: &Arc<Sup>) -> Vec<String> {
    let metas = secrets::all_meta();
    let op = super::terminal::load().onepassword;
    let mut by_machine: BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>> = BTreeMap::new();
    for cfg in sup.effective_configs() {
        if cfg.env_files.as_ref().is_none_or(|f| f.is_empty()) {
            continue;
        }
        for x in envimport::review(&cfg, &metas, &op).refs.into_iter().filter(|x| matches!(x.status, "new" | "missing")) {
            let held = metas.iter().find(|(n, m)| m.is_from_peer() && m.reference.as_deref() == Some(x.reference.as_str()) && !n.starts_with(INTERNAL_PREFIX));
            let Some((name, m)) = held else { continue };
            let Some(machine) = m.source.as_deref().and_then(|s| s.strip_prefix("peer:")) else { continue };
            by_machine.entry(machine.to_string()).or_default().entry(cfg.name.clone()).or_default().insert(x.key, name.clone());
        }
    }
    let mut attached = vec![];
    for (machine, grants) in by_machine {
        attached.extend(envimport::grant_received(sup, &grants, &machine));
    }
    attached.sort();
    attached.dedup();
    attached
}

/// Sync with online paired Macs: always newer values of ones we hold from them and their pending pushes;
/// with `ask` (pullFromPeers, or Pull Shared Values), also request what our services wait for (`only`:
/// one service) and take what they approved. Blocking.
pub fn sync(sup: &Arc<Sup>, peers: &Peers, only: Option<&str>, ask: bool) -> Value {
    if secrets::is_locked() {
        return json!({ "received": [], "asked": [], "errors": [] });
    }
    let attached = heal(sup);
    let mut received = vec![];
    let mut asked = vec![];
    let mut errors = vec![];
    let online = peers.online();
    lock(&INCOMING).retain(|m, b| b.via.is_some() || online.contains(m));
    if ask {
        ASK_UNTIL.fetch_max(now_ms() + 10 * 60_000, std::sync::atomic::Ordering::Relaxed);
    }
    for machine in online {
        // Received values are tied to the Mac's pinned link key, not its name: a different Mac that later
        // takes the name (or re-pairs with a new key) can't update them.
        let Some(key) = peers.public_key(&machine) else { continue };
        let metas = secrets::all_meta();
        let from_this = |m: &Meta| m.is_from_peer() && m.peer_key.as_deref() == Some(key.as_str());
        let all_wants = wants(sup, &metas, None);
        let have: BTreeMap<String, i64> = metas.values().filter(|m| from_this(m)).filter_map(|m| Some((m.reference.clone()?, m.fetched_at.unwrap_or(0)))).collect();
        let mine: Vec<&Want> = if ask { all_wants.iter().filter(|w| only.is_none_or(|s| s == w.service)).collect() } else { vec![] };
        let mut want_for: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for w in &mine {
            want_for.entry(w.reference.as_str()).or_default().insert(w.service.as_str());
        }
        let want_refs: Vec<&str> = want_for.keys().copied().collect();
        let declined: Vec<String> = lock(&DECLINED).get(&machine).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        let want_key: BTreeMap<&str, &str> = mine.iter().map(|w| (w.reference.as_str(), w.key.as_str())).collect();
        let body = json!({ "want": want_refs, "wantFor": want_for, "wantKey": want_key, "have": have, "declined": declined });
        let offer = match peers.call_json(&machine, "POST", "/secrets/offer", &body) {
            Ok(v) => v,
            Err(e) if e.0 == 404 => continue, // an older LStack
            Err(e) => {
                errors.push(e.1);
                continue;
            }
        };
        if !declined.is_empty() {
            if let Some(s) = lock(&DECLINED).get_mut(&machine) {
                s.retain(|r| !declined.contains(r));
            }
        }
        let offered: Vec<(String, i64)> = offer["offer"].as_array().into_iter().flatten().filter_map(|o| Some((o["ref"].as_str()?.to_string(), o["fetchedAt"].as_i64().unwrap_or(0)))).collect();
        let pull: Vec<String> = offered.iter().filter(|(r, at)| have.get(r).is_some_and(|h| at > h) || (!have.contains_key(r) && want_for.contains_key(r.as_str()))).map(|(r, _)| r.clone()).collect();
        let waiting: Vec<&str> = want_refs.iter().copied().filter(|r| !offered.iter().any(|(o, _)| o == r)).collect();
        if !waiting.is_empty() {
            asked.push(json!({ "machine": machine, "refs": waiting }));
        }
        // Pushes: ones we asked for are taken now; the rest wait for someone to accept them here.
        let declined_now = lock(&DECLINED).get(&machine).cloned().unwrap_or_default();
        let pushed: Vec<Incoming> = offer["push"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| {
                let r = o["ref"].as_str().filter(|r| clean(r))?.to_string();
                let name = o["name"].as_str().filter(|n| clean(n)).unwrap_or("SHARED_SECRET").to_string();
                let services: BTreeSet<String> = all_wants.iter().filter(|w| w.reference == r).map(|w| w.service.clone()).collect();
                Some(Incoming { reference: r, name, at: o["fetchedAt"].as_i64().unwrap_or(0), services: services.into_iter().collect() })
            })
            .filter(|i| !have.contains_key(&i.reference) && !pull.contains(&i.reference) && !declined_now.contains(&i.reference))
            .take(MAX_REFS)
            .collect();
        let fresh: Vec<String> = {
            let mut seen = lock(&PUSH_SEEN);
            pushed.iter().filter(|i| seen.insert((machine.clone(), i.reference.clone()))).map(|i| i.name.clone()).collect()
        };
        if !fresh.is_empty() {
            let refs: Vec<&String> = pushed.iter().map(|i| &i.reference).collect();
            sup.notice_with(
                &format!("{machine} sent you 1Password values"),
                &format!("{}. Accept in Settings › Machines, or: lstack secret accept {machine}", fresh.join(", ")),
                json!({ "pushed": { "machine": machine, "refs": refs } }),
            );
        }
        if pushed.is_empty() {
            lock(&INCOMING).remove(&machine);
        } else {
            lock(&INCOMING).insert(machine.clone(), Inbox { machine: machine.clone(), via: None, at: now_ms(), items: pushed });
        }
        if pull.is_empty() {
            continue;
        }
        let ws: Vec<Want> = mine.iter().map(|w| Want { service: w.service.clone(), key: w.key.clone(), reference: w.reference.clone(), file: w.file.clone() }).collect();
        match take(sup, peers, &machine, &key, &pull, &ws, &[]) {
            Ok(t) if !t.stored.is_empty() || !t.updated.is_empty() => {
                received.push(json!({ "machine": machine, "stored": t.stored, "updated": t.updated, "services": t.services }));
            }
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    // Macs that watch us learn what we wait for on their next round (within 15 s) and deliver what they approve.
    if ask {
        let metas = secrets::all_meta();
        let refs: BTreeSet<String> = wants(sup, &metas, only).into_iter().map(|w| w.reference).collect();
        let now = now_ms();
        let watchers: Vec<String> = lock(&WATCHERS).values().filter(|(_, at)| now - at < WATCHER_TTL).map(|(n, _)| n.clone()).collect();
        if !refs.is_empty() {
            for n in watchers {
                asked.push(json!({ "machine": n, "refs": refs }));
            }
        }
    }
    json!({ "received": received, "asked": asked, "errors": errors, "attached": attached })
}

/// The inbox holding `machine`'s pushes: a Mac we watch first, else one that watches us.
fn inbox_key(machine: &str) -> Option<(String, Inbox)> {
    let g = lock(&INCOMING);
    g.get_key_value(machine).or_else(|| g.iter().find(|(_, b)| b.via.is_some() && b.machine == machine)).map(|(k, b)| (k.clone(), b.clone()))
}

/// Accept values `machine` pushed (`refs`: some of them; else all). They go to our services whose env
/// files reference them, or are kept under the sender's name. From a Mac we watch they're pulled now;
/// one that watches us delivers them on its next round (within 15 s). Blocking.
pub fn accept(sup: &Arc<Sup>, peers: &Peers, machine: &str, refs: Option<&[String]>) -> Res<Value> {
    if secrets::is_locked() {
        return Err((423, "the secret store is locked".into()));
    }
    let none = || (404, format!("{machine} hasn't sent anything waiting for approval"));
    let (key, inbox) = inbox_key(machine).ok_or_else(none)?;
    let list: Vec<Incoming> = inbox.items.iter().filter(|i| refs.is_none_or(|r| r.contains(&i.reference))).cloned().collect();
    if list.is_empty() {
        return Err(none());
    }
    let want: Vec<String> = list.iter().map(|i| i.reference.clone()).collect();
    let forget = || {
        if let Some(b) = lock(&INCOMING).get_mut(&key) {
            b.items.retain(|i| !want.contains(&i.reference));
        }
    };
    if let Some(id) = &inbox.via {
        let mut acc = lock(&ACCEPTED);
        let l = acc.entry(id.clone()).or_default();
        l.retain(|i| !want.contains(&i.reference));
        l.extend(list);
        drop(acc);
        forget();
        envimport::audit("app", "push-accepted", json!({ "peer": machine, "refs": want }));
        return Ok(json!({ "machine": machine, "accepted": want, "stored": [], "updated": [], "services": [], "pending": true }));
    }
    let key_hex = peers.public_key(machine).ok_or((409, format!("{machine} isn't connected")))?;
    let ws = wants(sup, &secrets::all_meta(), None);
    let t = take(sup, peers, machine, &key_hex, &want, &ws, &list).map_err(|e| (502, e))?;
    forget();
    envimport::audit("app", "push-accepted", json!({ "peer": machine, "refs": want }));
    Ok(json!({ "machine": machine, "accepted": want, "stored": t.stored, "updated": t.updated, "services": t.services }))
}

/// Decline values `machine` pushed (`refs`: some; else all). That Mac is told on the next sync.
pub fn decline(machine: &str, refs: Option<&[String]>) -> Res<Value> {
    let (key, _) = inbox_key(machine).ok_or((404, format!("{machine} hasn't sent anything waiting for approval")))?;
    let mut incoming = lock(&INCOMING);
    let Some(b) = incoming.get_mut(&key) else { return Err((404, format!("{machine} hasn't sent anything waiting for approval"))) };
    let gone: Vec<String> = b.items.iter().filter(|i| refs.is_none_or(|r| r.contains(&i.reference))).map(|i| i.reference.clone()).collect();
    b.items.retain(|i| !gone.contains(&i.reference));
    drop(incoming);
    lock(&PUSH_SEEN).retain(|(m, r)| *m != key || !gone.contains(r));
    lock(&DECLINED).entry(key).or_default().extend(gone.iter().cloned());
    envimport::audit("app", "push-declined", json!({ "peer": machine, "refs": gone }));
    Ok(json!({ "machine": machine, "declined": gone }))
}

/// A Mac that watches this one (it can't be called from here) asks, every round: `{push: [{ref, name,
/// fetchedAt}]}` (what it pushes us) → `{want, wantFor, have, declined, accepted}`. What we wait for is
/// told only while we ask (`pullFromPeers`, or Pull Shared Values in the last 10 minutes). Blocking.
pub fn needs(sup: &Sup, p: &SharedPeer, v: &Value) -> Value {
    let now = now_ms();
    lock(&WATCHERS).insert(p.id.clone(), (p.name.clone(), now));
    let key = format!("w:{}", p.id);
    if secrets::is_locked() {
        return json!({ "want": [], "wantFor": {}, "have": {}, "declined": [], "accepted": [] });
    }
    let metas = secrets::all_meta();
    let have: BTreeMap<String, i64> =
        metas.values().filter(|m| m.is_from_peer() && m.peer_key.as_deref() == Some(p.public_key.as_str())).filter_map(|m| Some((m.reference.clone()?, m.fetched_at.unwrap_or(0)))).collect();
    let all_wants = wants(sup, &metas, None);
    let mut want_for: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    if asking() {
        for w in all_wants.iter().filter(|w| !have.contains_key(&w.reference)) {
            want_for.entry(w.reference.as_str()).or_default().insert(w.service.as_str());
        }
    }
    let declined: Vec<String> = lock(&DECLINED).remove(&key).map(|s| s.into_iter().collect()).unwrap_or_default();
    let accepted: Vec<String> = lock(&ACCEPTED).get(&p.id).map(|l| l.iter().map(|i| i.reference.clone()).collect()).unwrap_or_default();
    let pushed: Vec<Incoming> = v["push"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let r = o["ref"].as_str().filter(|r| clean(r))?.to_string();
            let name = o["name"].as_str().filter(|n| clean(n)).unwrap_or("SHARED_SECRET").to_string();
            let services: BTreeSet<String> = all_wants.iter().filter(|w| w.reference == r).map(|w| w.service.clone()).collect();
            Some(Incoming { reference: r, name, at: o["fetchedAt"].as_i64().unwrap_or(0), services: services.into_iter().collect() })
        })
        // Asked for anyway (it arrives as an approved request), held, just declined, or already accepted.
        .filter(|i| !have.contains_key(&i.reference) && !want_for.contains_key(i.reference.as_str()) && !declined.contains(&i.reference) && !accepted.contains(&i.reference))
        .take(MAX_REFS)
        .collect();
    let fresh: Vec<String> = {
        let mut seen = lock(&PUSH_SEEN);
        pushed.iter().filter(|i| seen.insert((key.clone(), i.reference.clone()))).map(|i| i.name.clone()).collect()
    };
    if !fresh.is_empty() {
        let refs: Vec<&String> = pushed.iter().map(|i| &i.reference).collect();
        sup.notice_with(
            &format!("{} sent you 1Password values", p.name),
            &format!("{}. Accept in Settings › Machines, or: lstack secret accept {}", fresh.join(", "), p.name),
            json!({ "pushed": { "machine": p.name, "refs": refs } }),
        );
    }
    lock(&INCOMING).insert(key, Inbox { machine: p.name.clone(), via: Some(p.id.clone()), at: now, items: pushed });
    let want: Vec<&str> = want_for.keys().copied().collect();
    let want_key: BTreeMap<&str, &str> = all_wants.iter().filter(|w| want_for.contains_key(w.reference.as_str())).map(|w| (w.reference.as_str(), w.key.as_str())).collect();
    json!({ "want": want, "wantFor": want_for, "wantKey": want_key, "have": have, "declined": declined, "accepted": accepted })
}

/// `{values: [{ref, value, fetchedAt}]}` from a Mac that watches this one. Taken only for what we ask for
/// (while asking), accepted, or hold from it already (a newer value); anything else is dropped. Blocking.
pub fn deliver(sup: &Arc<Sup>, p: &SharedPeer, v: Value) -> Res<Value> {
    if secrets::is_locked() {
        return Err((423, "the secret store is locked".into()));
    }
    let metas = secrets::all_meta();
    let have: BTreeSet<String> = metas.values().filter(|m| m.is_from_peer() && m.peer_key.as_deref() == Some(p.public_key.as_str())).filter_map(|m| m.reference.clone()).collect();
    let all_wants = wants(sup, &metas, None);
    let accepted: Vec<Incoming> = lock(&ACCEPTED).get(&p.id).cloned().unwrap_or_default();
    let ask = asking();
    let mut refs: Vec<String> = have.into_iter().chain(accepted.iter().map(|i| i.reference.clone())).collect();
    if ask {
        refs.extend(all_wants.iter().map(|w| w.reference.clone()));
    }
    refs.sort();
    refs.dedup();
    let ws: Vec<Want> = all_wants.into_iter().filter(|w| ask || accepted.iter().any(|i| i.reference == w.reference)).collect();
    let t = store(sup, &p.name, &p.public_key, v, &refs, &ws, &accepted).map_err(|e| (500, e))?;
    let got: BTreeSet<&String> = t.stored.iter().chain(&t.updated).collect();
    if !got.is_empty() {
        let metas = secrets::all_meta();
        let done: BTreeSet<String> = got.iter().filter_map(|n| metas.get(*n)?.reference.clone()).collect();
        if let Some(l) = lock(&ACCEPTED).get_mut(&p.id) {
            l.retain(|i| !done.contains(&i.reference));
        }
    }
    Ok(json!({ "stored": t.stored, "updated": t.updated, "services": t.services }))
}

/// Every 15 s: newer values of ones held from paired Macs and their pushes, plus (with `pullFromPeers`)
/// requests for references our services wait for. Cache to cache only — 1Password is never involved.
pub fn spawn_sync(sup: Arc<Sup>, peers: Arc<Peers>, remote: Arc<Remote>) {
    tokio::spawn(async move {
        let mut last = Value::Null;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SYNC_SECS)).await;
            let (s, p, r) = (sup.clone(), peers.clone(), remote.clone());
            let ask = super::terminal::load().onepassword.pull_from_peers;
            let Ok(v) = tokio::task::spawn_blocking(move || {
                let mut v = sync(&s, &p, None, ask);
                let errs = serve_watchers(&s, &r, &p, None);
                if let Some(a) = v["errors"].as_array_mut() {
                    a.extend(errs.into_iter().map(Value::String));
                }
                v
            })
            .await
            else {
                continue;
            };
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

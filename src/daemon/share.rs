//! The shared Mac's side of the machine link: accepts pairing requests and link connections on the link
//! port, then serves each paired Mac's streams according to its grants.
//!
//! Stream kinds (the first frame of every stream names one):
//! - `http`   — the peer API (`remote::peer_router`): status, events, logs, metrics, control (control grant);
//! - `tunnel` — `{service, port}`: a raw byte relay to a port the service is listening on right now
//!   (tunnel grant), dialed on loopback only.
//!
//! Unknown kinds are refused and audited, so new kinds (e.g. screen sharing) can be added with their own grants.
use super::link::{self, Incoming, Mux, PairPolicy};
use super::remote::{LiveLink, Remote};
use super::server::AppState;
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// Concurrent streams one paired Mac may hold open.
const MAX_STREAMS: usize = 160;

pub async fn serve(listener: TcpListener, app: AppState, remote: Arc<Remote>) {
    loop {
        let Ok((tcp, addr)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        let (app, remote) = (app.clone(), remote.clone());
        tokio::spawn(async move { connection(tcp, addr, app, remote).await });
    }
}

async fn connection(mut tcp: TcpStream, addr: SocketAddr, app: AppState, remote: Arc<Remote>) {
    let ip = addr.ip();
    if remote.locked_out(ip) {
        return;
    }
    let mut magic = [0u8; 6];
    if tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut magic)).await.map_or(true, |r| r.is_err()) {
        return;
    }
    if &magic == link::MAGIC_PAIR {
        let _ = link::pair_server(tcp, &Pairing { remote: remote.clone(), ip }).await;
        return;
    }
    if &magic != link::MAGIC_LINK {
        remote.failed(ip);
        return;
    }
    let Ok(id) = link::identity() else { return };
    let cfg = remote.config();
    let mut who = None;
    let accepted = link::accept(tcp, &id, |key, _hello| match cfg.peer_by_key(key) {
        Some(p) => {
            who = Some((p.id.clone(), p.name.clone()));
            Ok(json!({ "hostname": super::remote::hostname(), "version": crate::paths::VERSION }))
        }
        None => Err(format!("{} doesn't know this Mac (it was unpaired or its key changed) — pair again", super::remote::hostname())),
    })
    .await;
    let mut acc = match accepted {
        Ok(a) => a,
        Err(e) => {
            // A known-but-unpaired key retrying isn't guessing anything (keys can't be brute-forced), so only
            // malformed handshakes count toward the per-address lockout that also guards pairing.
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                remote.audit(json!({ "event": "link-refused", "address": ip.to_string(), "reason": "unknown key" }));
            } else {
                remote.failed(ip);
            }
            return;
        }
    };
    let Some((peer_id, peer_name)) = who else { return };
    remote.succeeded(ip);
    remote.touch_peer(&peer_id);
    remote.links.lock().unwrap().push(LiveLink { peer_id: peer_id.clone(), mux: acc.mux.clone(), addr });
    remote.audit(json!({ "event": "link-up", "peer": peer_name, "address": ip.to_string(), "version": acc.hello["version"] }));
    let router = super::remote::peer_router(app.clone(), remote.clone(), peer_id.clone(), acc.mux.clone());
    let ctx = Arc::new(Ctx { app, remote: remote.clone(), peer_id: peer_id.clone(), peer_name: peer_name.clone(), mux: acc.mux.clone() });
    while let Some(inc) = acc.incoming.recv().await {
        if acc.mux.stream_count() > MAX_STREAMS {
            ctx.remote.audit(json!({ "event": "refused", "peer": peer_name, "reason": "too many streams" }));
            continue;
        }
        match inc.header["kind"].as_str() {
            Some("http") => {
                let router = router.clone();
                tokio::spawn(serve_http(router, inc));
            }
            Some("tunnel") => {
                tokio::spawn(tunnel(ctx.clone(), inc));
            }
            other => {
                ctx.remote.audit(json!({ "event": "refused", "peer": peer_name, "kind": other, "reason": "unknown stream kind" }));
            }
        }
    }
    acc.mux.shutdown();
    remote.links.lock().unwrap().retain(|l| !Arc::ptr_eq(&l.mux, &acc.mux));
    remote.touch_peer(&peer_id);
    remote.audit(json!({ "event": "link-down", "peer": peer_name }));
}

/// Serve one HTTP/1.1 exchange of the peer API on a stream.
async fn serve_http(router: axum::Router, inc: Incoming) {
    use tower::ServiceExt;
    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        async move { router.oneshot(req.map(axum::body::Body::new)).await }
    });
    let io = hyper_util::rt::TokioIo::new(inc.stream);
    let _ = hyper::server::conn::http1::Builder::new().keep_alive(false).serve_connection(io, svc).await;
}

struct Ctx {
    app: AppState,
    remote: Arc<Remote>,
    peer_id: String,
    peer_name: String,
    mux: Arc<Mux>,
}

/// Why a tunnel to `service:port` may not be opened for this peer right now (None = allowed).
fn tunnel_denied(ctx: &Ctx, service: &str, port: u16) -> Option<&'static str> {
    let cfg = ctx.remote.config();
    if !cfg.sharing() || ctx.mux.is_closed() {
        return Some("sharing is off");
    }
    let Some(p) = cfg.peers.iter().find(|p| p.id == ctx.peer_id) else { return Some("not paired") };
    if !p.grants.tunnel_allows(service) {
        return Some("no tunnel permission for this service");
    }
    let Ok(st) = ctx.app.sup.get_state(service) else { return Some("no such service") };
    if st.status != "running" {
        return Some("service isn't running");
    }
    // Only a port the service's own processes are listening on right now (not whatever else holds it).
    if !st.urls.iter().any(|u| u.listening && u.port == Some(port)) {
        return Some("the service isn't listening on that port");
    }
    None
}

async fn tunnel(ctx: Arc<Ctx>, inc: Incoming) {
    let service = inc.header["service"].as_str().unwrap_or("").to_string();
    let port = inc.header["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
    if let Some(why) = tunnel_denied(&ctx, &service, port) {
        ctx.remote.audit(json!({ "event": "tunnel-refused", "peer": ctx.peer_name, "service": service, "port": port, "reason": why }));
        return;
    }
    // Loopback only: the tunnel never reaches other hosts on this Mac's network.
    let mut target = None;
    for ip in [IpAddr::from([127, 0, 0, 1]), IpAddr::from(std::net::Ipv6Addr::LOCALHOST)] {
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(SocketAddr::new(ip, port))).await {
            target = Some(s);
            break;
        }
    }
    let Some(mut target) = target else {
        ctx.remote.audit(json!({ "event": "tunnel-failed", "peer": ctx.peer_name, "service": service, "port": port, "reason": "nothing answered on loopback" }));
        return;
    };
    let _ = target.set_nodelay(true);
    let started = Instant::now();
    let mut stream = inc.stream;
    let (up, down) = tokio::io::copy_bidirectional(&mut stream, &mut target).await.unwrap_or((0, 0));
    let ms = started.elapsed().as_millis() as u64;
    // Short-lived connections (every browser request) would flood the log; keep the long or big ones.
    if ms > 2000 || up + down > 1_000_000 {
        ctx.remote.audit(json!({ "event": "tunnel", "peer": ctx.peer_name, "service": service, "port": port, "bytesIn": up, "bytesOut": down, "ms": ms }));
    }
    ctx.remote.count_tunnel(&ctx.peer_name, &service, port, up + down);
}

struct Pairing {
    remote: Arc<Remote>,
    ip: IpAddr,
}

impl PairPolicy for Pairing {
    fn code(&self) -> Option<String> {
        self.remote.current_pair_code()
    }

    fn wrong_code(&self) {
        self.remote.pair_code_failed();
        self.remote.failed(self.ip);
        self.remote.audit(json!({ "event": "pair-failed", "address": self.ip.to_string(), "reason": "wrong code" }));
    }

    async fn admit(&self, req: &Value, public: [u8; 32], phrase: &str) -> Result<Value, String> {
        let s = |k: &str| req[k].as_str().unwrap_or("").to_string();
        let totp = req["totp"].as_str().filter(|c| !c.is_empty());
        if !self.remote.check_login(&s("username"), &s("password"), totp).await {
            self.remote.pair_code_failed();
            self.remote.failed(self.ip);
            self.remote.audit(json!({ "event": "pair-failed", "address": self.ip.to_string(), "reason": "wrong login" }));
            let msg = if self.remote.config().totp_secret.is_empty() { "invalid username or password" } else { "invalid username, password or 2FA code" };
            return Err(msg.into());
        }
        let name = match s("hostName") {
            n if n.trim().is_empty() => self.ip.to_string(),
            n => n,
        };
        let peer = self.remote.add_peer(&name, &public, phrase).map_err(|e| e.1)?;
        self.remote.pair_code_used();
        self.remote.succeeded(self.ip);
        self.remote.audit(json!({ "event": "paired", "peer": peer.name, "address": self.ip.to_string(), "phrase": phrase, "fingerprint": link::fingerprint(&public) }));
        Ok(peer.grants.json())
    }
}

//! The encrypted link between two paired LStack installs.
//!
//! - **Identity**: every install has an X25519 keypair, kept in the secret store (never on disk in plaintext,
//!   never sent anywhere). Possession of the private key is the only credential between machines.
//! - **Pairing** (`MAGIC_PAIR`): SPAKE2 keyed by the one-time code shown on the shared Mac, so the code never
//!   crosses the wire and can't be guessed offline. The SPAKE2 key then encrypts the host's dashboard login
//!   (+2FA) and the exchange of public keys, which both sides pin.
//! - **Link** (`MAGIC_LINK`): Noise `IK` (both static keys authenticated, forward secrecy) over TCP, then a
//!   small multiplexer: many independent byte streams with per-stream flow control over one connection.
//!   Each stream starts with a JSON header naming its `kind` (`http`, `tunnel`, …); the shared Mac checks
//!   the peer's grants for that kind before serving it.
use crate::paths::now_ms;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Semaphore};
use zeroize::Zeroizing;

pub const DEFAULT_PORT: u16 = 7381;
pub const MAGIC_LINK: &[u8; 6] = b"LSTKL1";
pub const MAGIC_PAIR: &[u8; 6] = b"LSTKP1";
const NOISE: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"lstack-link-v1";
const MAX_MSG: usize = 65535;
const CHUNK: usize = 16 * 1024;
const WINDOW: u32 = 256 * 1024;
const PING_EVERY: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(45);
const IDENTITY_SECRET: &str = "LSTACK_IDENTITY";

// ------------------------------------------------------------------ identity

pub struct Identity {
    private: Zeroizing<Vec<u8>>,
    pub public: [u8; 32],
}

static IDENTITY: Mutex<Option<Arc<Identity>>> = Mutex::new(None);

fn hexs(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn key_hex(k: &[u8; 32]) -> String {
    hexs(k)
}

fn generate() -> (Zeroizing<Vec<u8>>, [u8; 32]) {
    let kp = snow::Builder::new(NOISE.parse().unwrap()).generate_keypair().expect("keypair");
    let mut public = [0u8; 32];
    public.copy_from_slice(&kp.public);
    (Zeroizing::new(kp.private), public)
}

/// This install's identity, created on first use (needs the secret store to be unlocked).
pub fn identity() -> Result<Arc<Identity>, String> {
    let mut g = IDENTITY.lock().unwrap();
    if let Some(id) = &*g {
        return Ok(id.clone());
    }
    let id = match super::secrets::internal_get(IDENTITY_SECRET) {
        Some(stored) => {
            let stored = Zeroizing::new(stored);
            let (prv, pubk) = stored.split_once(':').ok_or("stored identity is malformed")?;
            let private = Zeroizing::new(unhex32(prv).ok_or("stored identity is malformed")?.to_vec());
            Identity { private, public: unhex32(pubk).ok_or("stored identity is malformed")? }
        }
        None => {
            let (private, public) = generate();
            let stored = Zeroizing::new(format!("{}:{}", hexs(&private), hexs(&public)));
            super::secrets::set(IDENTITY_SECRET, &stored).map_err(|e| format!("cannot store this Mac's identity key: {e}"))?;
            Identity { private, public }
        }
    };
    let id = Arc::new(id);
    *g = Some(id.clone());
    Ok(id)
}

/// Replace this install's key. Every pairing must be redone afterwards.
pub fn rekey() -> Result<Arc<Identity>, String> {
    let (private, public) = generate();
    let stored = Zeroizing::new(format!("{}:{}", hexs(&private), hexs(&public)));
    super::secrets::set(IDENTITY_SECRET, &stored)?;
    let id = Arc::new(Identity { private, public });
    *IDENTITY.lock().unwrap() = Some(id.clone());
    Ok(id)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Short, human-comparable form of a public key, e.g. `7QF3-K2MX-9D4A-PL2C`.
pub fn fingerprint(k: &[u8; 32]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let h = sha256(&[b"lstack-fp", k]);
    let chars: Vec<char> = h[..16].iter().map(|b| ALPHABET[(*b as usize) % 32] as char).collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

const WORDS: [&str; 256] = [
    "acorn", "adobe", "agate", "alder", "alpine", "amber", "anchor", "anvil", "apple", "apron", "arbor", "arch", "arrow", "aspen", "atlas", "aurora",
    "badge", "bagel", "bamboo", "banjo", "barley", "basil", "beacon", "beaver", "berry", "birch", "bison", "blaze", "bloom", "bluff", "bongo", "bramble",
    "brick", "brook", "bucket", "budget", "bumper", "butter", "cabin", "cactus", "camel", "canoe", "canyon", "carbon", "cargo", "cedar", "cello", "chalk",
    "cherry", "chess", "cider", "cinder", "citrus", "clover", "cobalt", "comet", "copper", "coral", "cotton", "cougar", "crane", "cricket", "crystal", "cumin",
    "dahlia", "daisy", "delta", "denim", "desert", "dingo", "dolphin", "domino", "dragon", "drift", "dune", "eagle", "easel", "echo", "eclipse", "ember",
    "emerald", "falcon", "fennel", "fern", "fiddle", "fig", "fjord", "flint", "forest", "fossil", "fox", "garnet", "gazelle", "geyser", "ginger", "glacier",
    "goblet", "gopher", "granite", "grape", "gravel", "harbor", "hazel", "heron", "hickory", "hollow", "honey", "hornet", "husky", "igloo", "indigo", "iris",
    "island", "ivory", "jackal", "jade", "jasmine", "jelly", "jigsaw", "juniper", "kayak", "kelp", "kettle", "kiwi", "koala", "lagoon", "lantern", "larch",
    "lava", "lemon", "lilac", "linen", "lizard", "llama", "lobster", "locket", "lotus", "lunar", "magnet", "mango", "maple", "marble", "meadow", "melon",
    "mesa", "mint", "mocha", "monsoon", "moose", "mosaic", "moss", "mural", "nectar", "nickel", "nimbus", "nutmeg", "oasis", "ocean", "olive", "onyx",
    "orbit", "orchid", "osprey", "otter", "oyster", "paddle", "panda", "papaya", "parrot", "pebble", "pecan", "pepper", "piano", "pigeon", "pine", "pixel",
    "plum", "polar", "poppy", "prairie", "prism", "pumpkin", "quail", "quartz", "quill", "quokka", "radar", "radish", "raven", "reef", "ribbon", "ridge",
    "river", "robin", "rocket", "rose", "ruby", "saddle", "saffron", "salmon", "sandal", "sapphire", "satin", "scarlet", "sequoia", "shadow", "sierra", "silk",
    "slate", "snow", "sonar", "spruce", "squid", "summit", "sunset", "swan", "tango", "tapir", "thistle", "thunder", "tiger", "timber", "topaz", "tulip",
    "tundra", "turtle", "umber", "valley", "velvet", "violet", "walnut", "walrus", "willow", "wombat", "yarrow", "yeti", "zebra", "zephyr", "zinc", "zircon",
    "badger", "cobra", "dove", "elm", "finch", "gull", "hawk", "ibis", "jay", "kite", "lark", "mole", "newt", "owl", "puma", "wren",
];

/// Four words both sides of a pairing derive from the shared key; matching words = no one in the middle.
fn phrase(k: &[u8]) -> String {
    let h = sha256(&[b"lstack-phrase", k]);
    h[..4].iter().map(|b| WORDS[*b as usize]).collect::<Vec<_>>().join(" ")
}

// ------------------------------------------------------------------ framing

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_MSG {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut buf = Vec::with_capacity(2 + data.len());
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

fn ioerr(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

async fn timed<T>(d: Duration, f: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(d, f).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))?
}

// ------------------------------------------------------------------ handshake

/// Dial a paired machine's link port. Fails unless it holds the private key matching `their`
/// and has pinned our key.
pub async fn connect(addr: &str, id: &Identity, their: &[u8; 32], hello: Value) -> io::Result<(Arc<Mux>, Value)> {
    let mut tcp = timed(Duration::from_secs(8), TcpStream::connect(addr)).await?;
    let _ = tcp.set_nodelay(true);
    let mut hs = snow::Builder::new(NOISE.parse().unwrap())
        .local_private_key(&id.private)
        .remote_public_key(their)
        .prologue(PROLOGUE)
        .build_initiator()
        .map_err(ioerr)?;
    let mut buf = vec![0u8; MAX_MSG];
    let n = hs.write_message(hello.to_string().as_bytes(), &mut buf).map_err(ioerr)?;
    tcp.write_all(MAGIC_LINK).await?;
    write_frame(&mut tcp, &buf[..n]).await?;
    let reply = timed(Duration::from_secs(10), read_frame(&mut tcp)).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(io::ErrorKind::PermissionDenied, "the other Mac closed the connection")
        } else {
            e
        }
    })?;
    match reply.split_first() {
        Some((1, msg)) => {
            let n = hs.read_message(msg, &mut buf).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "the other Mac's key doesn't match the one pinned at pairing"))?;
            let server_hello: Value = serde_json::from_slice(&buf[..n]).unwrap_or(Value::Null);
            let ts = hs.into_transport_mode().map_err(ioerr)?;
            Ok((Mux::start(tcp, ts, true, None), server_hello))
        }
        Some((0, msg)) => Err(io::Error::new(io::ErrorKind::PermissionDenied, String::from_utf8_lossy(msg).into_owned())),
        _ => Err(ioerr("unexpected reply")),
    }
}

pub struct Accepted {
    pub mux: Arc<Mux>,
    pub hello: Value,
    pub incoming: mpsc::Receiver<Incoming>,
}

/// Server half of the link handshake (after `MAGIC_LINK` was read). `authorize` maps the caller's static
/// key to a reply payload, or refuses with a message.
pub async fn accept(mut tcp: TcpStream, id: &Identity, authorize: impl FnOnce(&[u8; 32], &Value) -> Result<Value, String>) -> io::Result<Accepted> {
    let _ = tcp.set_nodelay(true);
    let mut hs = snow::Builder::new(NOISE.parse().unwrap()).local_private_key(&id.private).prologue(PROLOGUE).build_responder().map_err(ioerr)?;
    let msg = timed(Duration::from_secs(10), read_frame(&mut tcp)).await?;
    let mut buf = vec![0u8; MAX_MSG];
    let n = hs.read_message(&msg, &mut buf).map_err(|_| ioerr("handshake failed"))?;
    let hello: Value = serde_json::from_slice(&buf[..n]).unwrap_or(Value::Null);
    let mut peer = [0u8; 32];
    peer.copy_from_slice(hs.get_remote_static().ok_or_else(|| ioerr("no static key"))?);
    let reply = match authorize(&peer, &hello) {
        Ok(v) => v,
        Err(why) => {
            let mut m = vec![0u8];
            m.extend_from_slice(why.as_bytes());
            let _ = write_frame(&mut tcp, &m).await;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
        }
    };
    let n = hs.write_message(reply.to_string().as_bytes(), &mut buf).map_err(ioerr)?;
    let mut m = vec![1u8];
    m.extend_from_slice(&buf[..n]);
    write_frame(&mut tcp, &m).await?;
    let ts = hs.into_transport_mode().map_err(ioerr)?;
    let (tx, rx) = mpsc::channel(64);
    Ok(Accepted { mux: Mux::start(tcp, ts, false, Some(tx)), hello, incoming: rx })
}

// ------------------------------------------------------------------ multiplexer

const F_OPEN: u8 = 1;
const F_DATA: u8 = 2;
const F_FIN: u8 = 3;
const F_RESET: u8 = 4;
const F_WINDOW: u8 = 5;
const F_PING: u8 = 6;
const F_PONG: u8 = 7;

/// A stream opened by the other side.
pub struct Incoming {
    pub header: Value,
    pub stream: DuplexStream,
}

enum In {
    Data(Vec<u8>),
    Fin,
    Reset,
}

struct Slot {
    inbound: mpsc::UnboundedSender<In>,
    credit: Arc<Semaphore>,
}

pub struct Mux {
    out: mpsc::Sender<Vec<u8>>,
    streams: Mutex<HashMap<u32, Slot>>,
    next: AtomicU32,
    closed: watch::Sender<bool>,
    dead: AtomicBool,
    incoming: Mutex<Option<mpsc::Sender<Incoming>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub opened_at: i64,
}

fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.push(kind);
    f.extend_from_slice(&id.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

impl Mux {
    fn start(tcp: TcpStream, ts: snow::TransportState, client: bool, incoming: Option<mpsc::Sender<Incoming>>) -> Arc<Mux> {
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
        let mux = Arc::new(Mux {
            out: out_tx,
            streams: Mutex::new(HashMap::new()),
            next: AtomicU32::new(if client { 1 } else { 2 }),
            closed: watch::channel(false).0,
            dead: AtomicBool::new(false),
            incoming: Mutex::new(incoming),
            tasks: Mutex::new(vec![]),
            opened_at: now_ms(),
        });
        let ts = Arc::new(Mutex::new(ts));
        let (mut rd, mut wr) = tcp.into_split();
        let wts = ts.clone();
        let wm = mux.clone();
        let writer = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_MSG];
            let mut ping = tokio::time::interval(PING_EVERY);
            ping.tick().await;
            loop {
                let plain = tokio::select! {
                    f = out_rx.recv() => match f { Some(f) => f, None => break },
                    _ = ping.tick() => frame(F_PING, 0, &[]),
                };
                let n = match wts.lock().unwrap().write_message(&plain, &mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if write_frame(&mut wr, &buf[..n]).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
            wm.shutdown();
        });
        let rm = mux.clone();
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_MSG];
            loop {
                let Ok(Ok(ct)) = tokio::time::timeout(DEAD_AFTER, read_frame(&mut rd)).await else { break };
                let n = match ts.lock().unwrap().read_message(&ct, &mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n < 5 || !rm.dispatch(buf[0], u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]), &buf[5..n]).await {
                    break;
                }
            }
            rm.shutdown();
        });
        mux.tasks.lock().unwrap().extend([writer, reader]);
        mux
    }

    /// Handle one frame from the other side; false = protocol violation, drop the link.
    async fn dispatch(self: &Arc<Self>, kind: u8, id: u32, payload: &[u8]) -> bool {
        match kind {
            F_OPEN => {
                let tx = self.incoming.lock().unwrap().clone();
                let header: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
                let fresh = !self.streams.lock().unwrap().contains_key(&id);
                match (tx, fresh, id % 2 == self.next.load(Ordering::Relaxed) % 2) {
                    (Some(tx), true, false) => {
                        let stream = self.attach(id);
                        if tx.send(Incoming { header, stream }).await.is_err() {
                            self.send(F_RESET, id, &[]).await;
                        }
                    }
                    _ => {
                        self.send(F_RESET, id, &[]).await;
                    }
                }
                true
            }
            F_DATA | F_FIN | F_RESET => {
                let g = self.streams.lock().unwrap();
                if let Some(s) = g.get(&id) {
                    let m = match kind {
                        F_DATA => In::Data(payload.to_vec()),
                        F_FIN => In::Fin,
                        _ => {
                            s.credit.close();
                            In::Reset
                        }
                    };
                    let _ = s.inbound.send(m);
                }
                true
            }
            F_WINDOW => {
                if payload.len() == 4 {
                    let n = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if let Some(s) = self.streams.lock().unwrap().get(&id) {
                        if s.credit.available_permits() + (n as usize) <= Semaphore::MAX_PERMITS {
                            s.credit.add_permits(n as usize);
                        }
                    }
                }
                true
            }
            F_PING => {
                self.send(F_PONG, 0, &[]).await;
                true
            }
            F_PONG => true,
            _ => false,
        }
    }

    async fn send(&self, kind: u8, id: u32, payload: &[u8]) -> bool {
        self.out.send(frame(kind, id, payload)).await.is_ok()
    }

    /// Register stream `id` and start pumping it; returns the application's end.
    fn attach(self: &Arc<Self>, id: u32) -> DuplexStream {
        let (app, mine) = tokio::io::duplex(CHUNK * 4);
        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<In>();
        let credit = Arc::new(Semaphore::new(WINDOW as usize));
        self.streams.lock().unwrap().insert(id, Slot { inbound: in_tx, credit: credit.clone() });
        let (mut r, mut w) = tokio::io::split(mine);
        let done = Arc::new(AtomicU8::new(0));
        let finish = {
            let mux = self.clone();
            let done = done.clone();
            move || {
                if done.fetch_add(1, Ordering::SeqCst) == 1 {
                    mux.streams.lock().unwrap().remove(&id);
                }
            }
        };
        let f2 = finish.clone();
        let mux = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match r.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        mux.send(F_FIN, id, &[]).await;
                        break;
                    }
                    Ok(n) => n,
                };
                match credit.acquire_many(n as u32).await {
                    Ok(p) => p.forget(),
                    Err(_) => break,
                }
                if !mux.send(F_DATA, id, &buf[..n]).await {
                    break;
                }
            }
            finish();
        });
        let mux = self.clone();
        tokio::spawn(async move {
            while let Some(m) = in_rx.recv().await {
                match m {
                    In::Data(b) => {
                        if w.write_all(&b).await.is_err() {
                            mux.send(F_RESET, id, &[]).await;
                            break;
                        }
                        mux.send(F_WINDOW, id, &(b.len() as u32).to_be_bytes()).await;
                    }
                    // The other half of the duplex stays with the outbound pump, so end the app's reads
                    // explicitly; otherwise a reader would block after the link went away.
                    In::Fin | In::Reset => {
                        let _ = w.shutdown().await;
                        break;
                    }
                }
            }
            f2();
        });
        app
    }

    /// Open a stream to the other side. The header's `kind` decides what serves it.
    pub async fn open(self: &Arc<Self>, header: Value) -> io::Result<DuplexStream> {
        if self.is_closed() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "link is down"));
        }
        let id = self.next.fetch_add(2, Ordering::Relaxed);
        let stream = self.attach(id);
        if !self.send(F_OPEN, id, header.to_string().as_bytes()).await {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "link is down"));
        }
        Ok(stream)
    }

    /// Resolves when the link goes down.
    #[cfg(test)]
    pub async fn closed(&self) {
        let mut rx = self.closed.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    pub fn is_closed(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.send_replace(true);
        *self.incoming.lock().unwrap() = None;
        for (_, s) in self.streams.lock().unwrap().drain() {
            s.credit.close();
            let _ = s.inbound.send(In::Reset);
        }
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }

    pub fn stream_count(&self) -> usize {
        self.streams.lock().unwrap().len()
    }
}

// ------------------------------------------------------------------ blocking HTTP over a stream

/// Minimal HTTP/1.1 client for the peer API over a link stream (used from the peers' blocking threads).
pub fn http(mux: &Arc<Mux>, rt: &tokio::runtime::Handle, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Box<dyn BufRead + Send>), String> {
    let stream = rt.block_on(mux.open(json!({ "kind": "http" }))).map_err(|e| e.to_string())?;
    let mut io = tokio_util::io::SyncIoBridge::new_with_handle(stream, rt.clone());
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: lstack\r\nConnection: close\r\nX-LStack: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    io.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    io.flush().map_err(|e| e.to_string())?;
    let mut rd = io::BufReader::new(io);
    let mut line = String::new();
    rd.read_line(&mut line).map_err(|e| e.to_string())?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| "link closed before a response".to_string())?;
    let (mut chunked, mut len) = (false, None);
    loop {
        line.clear();
        if rd.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if k == "content-length" {
                len = v.parse::<u64>().ok();
            }
        }
    }
    let body: Box<dyn BufRead + Send> = if chunked {
        Box::new(io::BufReader::new(Chunked { inner: rd, left: 0, done: false }))
    } else if let Some(n) = len {
        Box::new(rd.take(n))
    } else {
        Box::new(rd)
    };
    Ok((status, body))
}

struct Chunked<R: BufRead> {
    inner: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            if line.trim().is_empty() {
                line.clear();
                self.inner.read_line(&mut line)?;
            }
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = usize::from_str_radix(size, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = out.len().min(self.left);
        let n = self.inner.read(&mut out[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n;
        Ok(n)
    }
}

// ------------------------------------------------------------------ pairing

/// Encrypts the pairing exchange under the SPAKE2 key; separate keys per direction.
struct PairBox {
    send: chacha20poly1305::ChaCha20Poly1305,
    recv: chacha20poly1305::ChaCha20Poly1305,
    n_send: u64,
    n_recv: u64,
}

impl PairBox {
    fn new(k: &[u8], initiator: bool) -> PairBox {
        use chacha20poly1305::KeyInit;
        let c2s = sha256(&[b"lstack-pair c2s", k]);
        let s2c = sha256(&[b"lstack-pair s2c", k]);
        let (s, r) = if initiator { (c2s, s2c) } else { (s2c, c2s) };
        PairBox {
            send: chacha20poly1305::ChaCha20Poly1305::new(&s.into()),
            recv: chacha20poly1305::ChaCha20Poly1305::new(&r.into()),
            n_send: 0,
            n_recv: 0,
        }
    }
    fn nonce(n: u64) -> chacha20poly1305::Nonce {
        let mut b = [0u8; 12];
        b[4..].copy_from_slice(&n.to_be_bytes());
        b.into()
    }
    fn seal(&mut self, v: &Value) -> Vec<u8> {
        use chacha20poly1305::aead::Aead;
        let ct = self.send.encrypt(&Self::nonce(self.n_send), v.to_string().as_bytes()).expect("encrypt");
        self.n_send += 1;
        let mut m = vec![1u8];
        m.extend(ct);
        m
    }
    fn open(&mut self, m: &[u8]) -> Option<Value> {
        use chacha20poly1305::aead::Aead;
        let (1, ct) = m.split_first()? else { return None };
        let pt = Zeroizing::new(self.recv.decrypt(&Self::nonce(self.n_recv), ct).ok()?);
        self.n_recv += 1;
        serde_json::from_slice(&pt).ok()
    }
}

fn plain(v: &Value) -> Vec<u8> {
    let mut m = vec![0u8];
    m.extend(v.to_string().as_bytes());
    m
}

fn as_plain(m: &[u8]) -> Option<Value> {
    let (0, b) = m.split_first()? else { return None };
    serde_json::from_slice(b).ok()
}

fn spake_ids() -> (spake2::Identity, spake2::Identity) {
    (spake2::Identity::new(b"lstack-host"), spake2::Identity::new(b"lstack-share"))
}

fn normalize_code(c: &str) -> Vec<u8> {
    c.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase().into_bytes()
}

/// What the host learns from a successful pairing.
pub struct Paired {
    pub public: [u8; 32],
    pub hostname: String,
    pub version: String,
    pub phrase: String,
    pub grants: Value,
}

/// Host side: pair with the machine at `addr` using its one-time code and its dashboard login.
pub async fn pair_client(addr: &str, code: &str, login: Value, host_name: &str) -> Result<Paired, String> {
    use spake2::{Ed25519Group, Password, Spake2};
    let id = identity()?;
    let mut tcp = timed(Duration::from_secs(8), TcpStream::connect(addr)).await.map_err(|e| format!("can't reach {addr}: {e} (is “Share this Mac” on there?)"))?;
    let (a, b) = spake_ids();
    let (st, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(normalize_code(code)), &a, &b);
    let io = async {
        tcp.write_all(MAGIC_PAIR).await?;
        write_frame(&mut tcp, &plain(&json!({ "v": 1, "spake": hexs(&msg) }))).await?;
        timed(Duration::from_secs(10), read_frame(&mut tcp)).await
    };
    let reply = io.await.map_err(|e| format!("pairing with {addr} failed: {e}"))?;
    let reply = as_plain(&reply).ok_or("unexpected reply")?;
    if let Some(e) = reply["error"].as_str() {
        return Err(e.to_string());
    }
    let theirs = reply["spake"].as_str().and_then(|s| (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect::<Option<Vec<u8>>>()).ok_or("unexpected reply")?;
    let key = Zeroizing::new(st.finish(&theirs).map_err(|_| "pairing failed (bad key exchange)")?);
    let mut bx = PairBox::new(&key, true);
    let mut req = login;
    req["hostName"] = json!(host_name);
    req["publicKey"] = json!(hexs(&id.public));
    req["version"] = json!(crate::paths::VERSION);
    write_frame(&mut tcp, &bx.seal(&req)).await.map_err(|e| e.to_string())?;
    let m = timed(Duration::from_secs(20), read_frame(&mut tcp)).await.map_err(|e| e.to_string())?;
    let v = match bx.open(&m) {
        Some(v) => v,
        None => return Err(as_plain(&m).and_then(|v| v["error"].as_str().map(String::from)).unwrap_or_else(|| "pairing failed".into())),
    };
    if let Some(e) = v["error"].as_str() {
        return Err(e.to_string());
    }
    Ok(Paired {
        public: v["publicKey"].as_str().and_then(unhex32).ok_or("the other Mac sent no key")?,
        hostname: v["hostname"].as_str().unwrap_or("").to_string(),
        version: v["version"].as_str().unwrap_or("").to_string(),
        phrase: phrase(&key),
        grants: v["grants"].clone(),
    })
}

/// What the shared Mac must decide while serving a pairing request.
pub trait PairPolicy: Send + Sync {
    /// The current one-time code, if one is showing.
    fn code(&self) -> Option<String>;
    /// The code was wrong (it is burned after a few attempts).
    fn wrong_code(&self);
    /// Check the dashboard login (+2FA) inside the encrypted exchange; on success pin the host's key and
    /// return the grants it gets. Errors are shown to the host.
    fn admit(&self, req: &Value, public: [u8; 32], phrase: &str) -> impl std::future::Future<Output = Result<Value, String>> + Send;
}

/// Shared-Mac side of pairing (after `MAGIC_PAIR` was read).
pub async fn pair_server<P: PairPolicy>(mut tcp: TcpStream, policy: &P) -> io::Result<()> {
    use spake2::{Ed25519Group, Password, Spake2};
    let first = timed(Duration::from_secs(10), read_frame(&mut tcp)).await?;
    let Some(first) = as_plain(&first) else { return Ok(()) };
    let Some(code) = policy.code() else {
        let msg = format!("no pairing code is showing on {} — open Settings → Machines → Share this Mac → Show Pairing Code there", super::remote::hostname());
        return write_frame(&mut tcp, &plain(&json!({ "error": msg }))).await;
    };
    let theirs = first["spake"].as_str().and_then(|s| (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect::<Option<Vec<u8>>>());
    let Some(theirs) = theirs else { return Ok(()) };
    let (a, b) = spake_ids();
    let (st, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(normalize_code(&code)), &a, &b);
    write_frame(&mut tcp, &plain(&json!({ "spake": hexs(&msg) }))).await?;
    let Ok(key) = st.finish(&theirs) else { return Ok(()) };
    let key = Zeroizing::new(key);
    let mut bx = PairBox::new(&key, false);
    let m = timed(Duration::from_secs(60), read_frame(&mut tcp)).await?;
    let Some(req) = bx.open(&m) else {
        policy.wrong_code();
        tokio::time::sleep(Duration::from_millis(400)).await;
        return write_frame(&mut tcp, &plain(&json!({ "error": "wrong or expired pairing code" }))).await;
    };
    let Some(public) = req["publicKey"].as_str().and_then(unhex32) else { return Ok(()) };
    let ph = phrase(&key);
    let reply = match policy.admit(&req, public, &ph).await {
        Ok(grants) => {
            let id = identity().map_err(ioerr)?;
            json!({ "publicKey": hexs(&id.public), "hostname": super::remote::hostname(), "version": crate::paths::VERSION, "grants": grants })
        }
        Err(e) => {
            tokio::time::sleep(Duration::from_millis(400)).await;
            json!({ "error": e })
        }
    };
    write_frame(&mut tcp, &bx.seal(&reply)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_id() -> Identity {
        let (private, public) = generate();
        Identity { private, public }
    }

    async fn pair_of_links(server_trusts_client: bool) -> io::Result<(Arc<Mux>, Accepted)> {
        let (a, b) = (test_id(), test_id());
        let b_pub = b.public;
        let a_pub = a.public;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?.to_string();
        let srv = tokio::spawn(async move {
            let (mut s, _) = l.accept().await?;
            let mut magic = [0u8; 6];
            s.read_exact(&mut magic).await?;
            accept(s, &b, |k, _| if server_trusts_client && *k == a_pub { Ok(json!({ "name": "b" })) } else { Err("not paired".into()) }).await
        });
        let c = connect(&addr, &a, &b_pub, json!({ "name": "a" })).await;
        let s = srv.await.unwrap();
        Ok((c?.0, s?))
    }

    #[tokio::test]
    async fn link_streams_both_ways() {
        let (client, mut server) = pair_of_links(true).await.unwrap();
        let mut s1 = client.open(json!({ "kind": "echo" })).await.unwrap();
        let inc = server.incoming.recv().await.unwrap();
        assert_eq!(inc.header["kind"], "echo");
        let mut st = inc.stream;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = st.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                st.write_all(&buf[..n]).await.unwrap();
            }
            st.shutdown().await.unwrap();
        });
        // Larger than the flow-control window, to exercise credit updates.
        let data: Vec<u8> = (0..(WINDOW as usize * 3)).map(|i| (i % 251) as u8).collect();
        let (mut r, mut w) = tokio::io::split(&mut s1);
        let d2 = data.clone();
        let (got, _) = tokio::join!(
            async {
                let mut got = vec![];
                r.read_to_end(&mut got).await.unwrap();
                got
            },
            async move {
                w.write_all(&d2).await.unwrap();
                w.shutdown().await.unwrap();
            }
        );
        assert_eq!(got.len(), data.len());
        assert!(got == data);
        client.shutdown();
        tokio::time::timeout(Duration::from_secs(2), server.mux.closed()).await.unwrap();
    }

    #[tokio::test]
    async fn open_streams_end_when_the_link_drops() {
        let (client, mut server) = pair_of_links(true).await.unwrap();
        let mut s = client.open(json!({ "kind": "x" })).await.unwrap();
        let _held = server.incoming.recv().await.unwrap();
        server.mux.shutdown();
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.expect("reader must not hang");
        assert_eq!(n.unwrap_or(0), 0);
        assert!(client.is_closed());
    }

    #[tokio::test]
    async fn unknown_key_is_refused() {
        let err = pair_of_links(false).await.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("not paired"));
    }

    #[tokio::test]
    async fn wrong_server_key_fails() {
        let (a, b, impostor) = (test_id(), test_id(), test_id());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut magic = [0u8; 6];
            s.read_exact(&mut magic).await.unwrap();
            let _ = accept(s, &impostor, |_, _| Ok(json!({}))).await;
        });
        assert!(connect(&addr, &a, &b.public, json!({})).await.is_err());
    }

    struct Policy {
        code: String,
        wrong: AtomicU32,
    }
    impl PairPolicy for Policy {
        fn code(&self) -> Option<String> {
            Some(self.code.clone())
        }
        fn wrong_code(&self) {
            self.wrong.fetch_add(1, Ordering::SeqCst);
        }
        async fn admit(&self, req: &Value, _public: [u8; 32], _phrase: &str) -> Result<Value, String> {
            if req["password"] == "pw" { Ok(json!({ "tunnel": "all" })) } else { Err("invalid username or password".into()) }
        }
    }

    async fn try_pair(server_code: &str, code: &str, password: &str) -> (Result<Paired, String>, u32) {
        let pol = Arc::new(Policy { code: server_code.into(), wrong: AtomicU32::new(0) });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let p2 = pol.clone();
        let srv = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut magic = [0u8; 6];
            s.read_exact(&mut magic).await.unwrap();
            let _ = pair_server(s, &*p2).await;
        });
        let r = pair_client(&addr, code, json!({ "username": "me", "password": password }), "host").await;
        srv.await.unwrap();
        (r, pol.wrong.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn pairing_needs_code_and_password() {
        IDENTITY.lock().unwrap().get_or_insert_with(|| Arc::new(test_id()));
        let (ok, wrong) = try_pair("ABCD2345", "abcd-2345", "pw").await;
        let ok = ok.unwrap();
        assert_eq!(wrong, 0);
        assert_eq!(ok.grants["tunnel"], "all");
        assert_eq!(ok.phrase.split(' ').count(), 4);
        let (bad, wrong) = try_pair("ABCD2345", "ABCD2346", "pw").await;
        assert!(bad.err().unwrap().contains("wrong or expired"));
        assert_eq!(wrong, 1);
        let (bad, wrong) = try_pair("ABCD2345", "ABCD2345", "nope").await;
        assert!(bad.err().unwrap().contains("invalid username or password"));
        assert_eq!(wrong, 0);
    }

    #[test]
    fn fingerprints_and_phrases() {
        let k = [7u8; 32];
        assert_eq!(fingerprint(&k).len(), 19);
        assert_eq!(fingerprint(&k), fingerprint(&k));
        assert_ne!(fingerprint(&k), fingerprint(&[8u8; 32]));
        assert_eq!(unhex32(&key_hex(&k)), Some(k));
        let mut seen = std::collections::HashSet::new();
        assert!(WORDS.iter().all(|w| seen.insert(*w)));
    }
}

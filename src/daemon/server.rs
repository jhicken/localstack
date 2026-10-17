//! Local HTTP API (127.0.0.1 only, bearer-token auth). Shapes are shared with the CLI and the menu bar app.
use super::supervisor::{Event, Sup, SupErr};
use crate::config;
use crate::paths::{self, now_ms, VERSION};
use crate::types::{ServiceConfig, ServiceState};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    pub sup: Arc<Sup>,
    pub token: Arc<String>,
    pub shutdown: Arc<Notify>,
    pub remote: Arc<super::remote::Remote>,
    pub peers: Arc<super::peers::Peers>,
}

pub struct ApiErr(pub u16, pub String);
impl ApiErr {
    pub fn new(code: u16, msg: impl Into<String>) -> Self {
        ApiErr(code, msg.into())
    }
}
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "error": crate::scrub::scrub(&self.1) }))).into_response()
    }
}
impl From<SupErr> for ApiErr {
    fn from(e: SupErr) -> Self {
        ApiErr(e.status, e.msg)
    }
}
type Res = Result<Json<Value>, ApiErr>;
type Q = Query<HashMap<String, String>>;

fn ok() -> Res {
    Ok(Json(json!({ "ok": true })))
}
fn to_json<T: serde::Serialize>(v: T) -> Res {
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}
fn parse_body(b: &Bytes) -> Result<Value, ApiErr> {
    if b.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(b).map_err(|_| ApiErr(400, "invalid JSON body".into()))
}

pub fn parse_duration(s: &str) -> i64 {
    let s = s.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = s[digits.len()..].trim();
    let Ok(n) = digits.parse::<i64>() else { return 3_600_000 };
    match unit {
        "" | "s" => n * 1000,
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        _ => 3_600_000,
    }
}

async fn auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let header_tok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    // Only the header: a token in a URL ends up in shell history, logs and process lists.
    let tok_ok = header_tok.is_some_and(|t| ct_eq(t.as_bytes(), st.token.as_bytes()));
    if !tok_ok {
        return ApiErr(401, "unauthorized".into()).into_response();
    }
    // The CLI and the app never send Origin; a browser always does on cross-site requests. Host must be a
    // loopback name, so a DNS-rebound page can't talk to the API under its own domain.
    if req.headers().contains_key(header::ORIGIN) || !loopback_host(req.headers().get(header::HOST).and_then(|h| h.to_str().ok())) {
        return ApiErr(403, "forbidden".into()).into_response();
    }
    next.run(req).await
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn loopback_host(h: Option<&str>) -> bool {
    let Some(h) = h else { return true }; // HTTP/1.0 clients may omit it
    let host = if let Some(rest) = h.strip_prefix('[') { rest.split(']').next().unwrap_or("") } else { h.split(':').next().unwrap_or("") };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub fn router(st: AppState) -> Router {
    Router::new()
        .route("/api/health", get(|| async { Json(json!({ "ok": true, "version": VERSION, "pid": std::process::id() })) }))
        .route("/api/services", get(list).post(create))
        .route("/api/services/:name/exec", get(super::exec::handler))
        .route("/api/services/:name", get(get_one).patch(patch).delete(destroy))
        .route("/api/services/:name/start", post(start))
        .route("/api/services/:name/stop", post(stop))
        .route("/api/services/:name/restart", post(restart))
        .route("/api/services/:name/stdin", post(stdin))
        .route("/api/services/:name/logs", get(logs).delete(clear_logs))
        .route("/api/services/:name/logs/stream", get(log_stream))
        .route("/api/services/:name/metrics", get(metrics))
        .route("/api/system", get(system))
        .route("/api/processes", get(processes))
        .route("/api/services/:name/open-terminal", post(open_term))
        .route("/api/events", get(events))
        .route("/api/remote", get(remote_status).put(remote_update))
        .route("/api/remote/totp", post(|State(st): State<AppState>| async move { to_json(st.remote.totp_begin()) }).delete(|State(st): State<AppState>| async move { Ok::<_, ApiErr>(Json(st.remote.totp_disable()?)) }))
        .route("/api/remote/totp/confirm", post(totp_confirm))
        // Security settings (2FA for pairing new Macs and dashboard sign-in); /api/remote/totp is the old name.
        .route("/api/security", get(|State(st): State<AppState>| async move { Json(json!({ "totp": st.remote.totp_enabled() })) }))
        .route("/api/security/totp", post(|State(st): State<AppState>| async move { to_json(st.remote.totp_begin()) }).delete(|State(st): State<AppState>| async move { Ok::<_, ApiErr>(Json(st.remote.totp_disable()?)) }))
        .route("/api/security/totp/confirm", post(totp_confirm))
        .route("/api/remote/pair-code", post(|State(st): State<AppState>| async move { Ok::<_, ApiErr>(Json(st.remote.new_pair_code()?)) }))
        .route("/api/remote/peers", get(|State(st): State<AppState>| async move { Json(st.remote.shared_peers()) }))
        .route(
            "/api/remote/peers/:id",
            axum::routing::patch(|State(st): State<AppState>, Path(id): Path<String>, body: Bytes| async move { Ok::<_, ApiErr>(Json(st.remote.set_grants(&id, &parse_body(&body)?)?)) })
                .delete(|State(st): State<AppState>, Path(id): Path<String>| async move {
                    st.remote.revoke_peer(&id)?;
                    ok()
                }),
        )
        .route(
            "/api/remote/addresses",
            get(|State(st): State<AppState>| async move {
                let sel = st.remote.config().share_address;
                let found = tokio::task::spawn_blocking(move || super::remote::discover_addresses(sel.as_deref())).await.unwrap_or_default();
                Json(found)
            }),
        )
        .route("/api/network", get(|| async { Json(json!({ "vpn": tokio::task::spawn_blocking(super::remote::vpn_status).await.unwrap_or_default() })) }))
        .route("/api/remote/activity", get(|State(st): State<AppState>| async move { Json(st.remote.activity()) }))
        .route("/api/identity", get(identity))
        .route("/api/identity/rekey", post(identity_rekey))
        .route("/api/forwards", get(|State(st): State<AppState>| async move { Json(st.peers.forwards.list()) }))
        .route("/api/forwards/pause", post(|st: State<AppState>, body: Bytes| async move { forwards_pause(st, body, true) }))
        .route("/api/forwards/resume", post(|st: State<AppState>, body: Bytes| async move { forwards_pause(st, body, false) }))
        .route("/api/settings", get(|| async { to_json(super::terminal::status()) }).put(settings_update))
        .route("/api/split-command", post(split_command))
        .route("/api/secrets", get(secrets_list))
        .route("/api/approvals", get(approvals_list))
        .route("/api/approvals/key", post(approval_key))
        .route("/api/approvals/:id", get(approval_get).post(approval_sign))
        .route("/api/approvals/:id/deny", post(approval_deny))
        .route("/api/secrets/unlock", post(secrets_unlock))
        .route("/api/secrets/rotate", post(secrets_rotate))
        .route("/api/secrets/:name", axum::routing::put(secret_set).delete(secret_delete))
        // 1Password: the daemon never runs `op`; clients read values after a person approves.
        .route("/api/env/preview", post(env_preview))
        .route("/api/services/:name/env", post(env_apply))
        .route("/api/services/:name/env/approve", post(env_approve))
        .route("/api/onepassword/review/:service", get(op_review))
        .route("/api/onepassword/detect", post(op_detect))
        .route("/api/onepassword/plan", post(op_plan))
        .route("/api/onepassword/plan/:id", axum::routing::delete(op_plan_cancel))
        .route("/api/onepassword/import", post(op_import))
        .route("/api/onepassword/audit", get(op_audit))
        .route("/api/onepassword/shares", get(op_shares).post(op_share))
        .route("/api/onepassword/shares/decline", post(op_share_decline))
        .route("/api/onepassword/push", post(op_push))
        .route("/api/onepassword/peers/sync", post(op_peer_sync))
        .route("/api/onepassword/peers/accept", post(op_peer_accept))
        .route("/api/onepassword/peers/decline", post(op_peer_decline))
        .route("/api/reload", post(|State(st): State<AppState>| async move { to_json(st.sup.reload()) }))
        .route("/api/shutdown", post(shutdown))
        .route("/api/open-dashboard", post(|q: Q| async move { open_terminal_with(&["top"], q.get("terminal").cloned()).await }))
        .route("/api/heartbeat/test", post(heartbeat_test))
        .route("/api/machines", get(machines_list).post(machine_add))
        .route("/api/machines/:m", get(machine_get).patch(machine_patch).delete(machine_remove))
        .route("/api/machines/reconnect", post(|State(st): State<AppState>| async move { Json(json!({ "reconnecting": st.peers.reconnect_down() })) }))
        .route("/api/machines/:m/services/:name", get(m_service))
        .route("/api/machines/:m/services/:name/logs", get(m_logs))
        .route("/api/machines/:m/services/:name/logs/stream", get(m_log_stream))
        .route("/api/machines/:m/services/:name/metrics", get(m_metrics))
        .route("/api/machines/:m/services/:name/:action", post(|st: State<AppState>, p: Path<(String, String, String)>| async move { m_action(st, p, true).await }))
        .route("/api/terminal/cmux", get(|| async { to_json(tokio::task::spawn_blocking(super::terminal::cmux_status).await.unwrap_or_default()) }))
        .route("/api/terminal/cmux/enable-password", post(cmux_enable))
        .fallback(|m: Method, uri: axum::http::Uri| async move { ApiErr(404, format!("no route for {m} {}", uri.path())) })
        .layer(middleware::from_fn_with_state(st.clone(), auth))
        .with_state(st)
}

async fn list(State(st): State<AppState>) -> Res {
    to_json(json!({ "services": st.sup.list(), "configErrors": st.sup.config_errors() }))
}

async fn create(State(st): State<AppState>, q: Q, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let cfg: ServiceConfig = serde_json::from_value(v).map_err(|e| ApiErr(400, e.to_string()))?;
    let cfg = config::validate(cfg).map_err(|e| ApiErr(400, e))?;
    to_json(st.sup.create(cfg, q.get("start").map(String::as_str) == Some("1")).await?)
}

/// Suggest cwd / env / source split out of a pasted command (used by the app's Add Service form).
async fn split_command(body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let command = v.get("command").and_then(Value::as_str).unwrap_or_default();
    let home = paths::user_home().to_string_lossy().into_owned();
    let base = v.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()).map(paths::expand_home).unwrap_or(home);
    let shell = crate::cmdline::user_shell();
    let sp = crate::cmdline::split(command, &base);
    to_json(json!({
        "changed": sp.changed(),
        "split": sp,
        "shell": shell,
        "defaultSource": crate::cmdline::default_source(&shell),
    }))
}

async fn get_one(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    to_json(st.sup.get_state(&name)?)
}

async fn patch(State(st): State<AppState>, Path(name): Path<String>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    to_json(st.sup.update(&name, &v)?)
}

async fn destroy(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    st.sup.destroy(&name).await?;
    ok()
}

async fn start(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    to_json(st.sup.start(&name).await?)
}
async fn stop(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    to_json(st.sup.stop(&name).await?)
}
async fn restart(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    to_json(st.sup.restart(&name).await?)
}

async fn stdin(State(st): State<AppState>, Path(name): Path<String>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    console_gate(&st, &name)?;
    let data = match &v["data"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if !st.sup.write(&name, &data)? {
        return Err(ApiErr(409, "service does not accept input (not running, or not a process)".into()));
    }
    ok()
}

fn tail_param(q: &Q) -> usize {
    q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(200)
}

/// `?tail=N` = the newest N lines; add `&before=<pos>` to page to the N lines before a line's `pos`.
pub(super) async fn logs(State(st): State<AppState>, Path(name): Path<String>, q: Q) -> Res {
    let store = st.sup.logs(&name)?;
    let n = tail_param(&q);
    let lines = match q.get("before").cloned() {
        Some(before) => blocking(move || Ok::<_, ApiErr>(store.older(&before, n))).await?,
        None => store.tail(n),
    };
    to_json(json!({ "lines": lines }))
}

async fn clear_logs(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    st.sup.logs(&name)?.clear();
    ok()
}

async fn system(State(st): State<AppState>) -> Res {
    Ok(Json(st.sup.system()))
}

pub(super) async fn metrics(State(st): State<AppState>, Path(name): Path<String>, q: Q) -> Res {
    st.sup.get_state(&name)?;
    let now = now_ms();
    let since = now - parse_duration(q.get("range").map(String::as_str).unwrap_or("1h"));
    let points = q.get("points").and_then(|p| p.parse().ok()).unwrap_or(120);
    to_json(json!({ "samples": st.sup.store.query(&name, since, now, points) }))
}

fn sse_msg(event: &str, data: &impl serde::Serialize) -> String {
    format!("event: {event}\ndata: {}\n\n", serde_json::to_string(data).unwrap_or_default())
}

fn sse_response(rx: mpsc::Receiver<String>) -> Response {
    let stream = ReceiverStream::new(rx).map(|s| Ok::<_, std::convert::Infallible>(Bytes::from(s)));
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(super) async fn log_stream(State(st): State<AppState>, Path(name): Path<String>, q: Q) -> Result<Response, ApiErr> {
    let store = st.sup.logs(&name)?;
    let mut sub = store.subscribe();
    let (tx, rx) = mpsc::channel::<String>(512);
    let mut first = String::from(": connected\n\n");
    for l in store.tail(tail_param(&q)) {
        first.push_str(&sse_msg("log", &l));
    }
    let _ = tx.try_send(first);
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.tick().await;
        loop {
            let msg = tokio::select! {
                r = sub.recv() => match r {
                    Ok(l) => sse_msg("log", &l),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = ping.tick() => ": ping\n\n".to_string(),
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    Ok(sse_response(rx))
}

async fn events(State(st): State<AppState>) -> Response {
    events_stream(st, false, true, None).await
}

/// Re-checked every 15 s; the stream ends once it returns false (e.g. a revoked peer token).
pub(super) type Alive = Box<dyn Fn() -> bool + Send + Sync>;

/// Service events as SSE. `redact` hides env values and ping URLs (used by the remote dashboard); `with_peers`
/// also forwards other machines' events (never on the peer API itself, so paired machines can't loop).
pub(super) async fn events_stream(st: AppState, redact: bool, with_peers: bool, alive: Option<Alive>) -> Response {
    let clean = move |s: ServiceState| if redact { super::remote::redact(s) } else { s };
    let mut sub = st.sup.events.subscribe();
    let (tx, rx) = mpsc::channel::<String>(512);
    let peers = st.peers.clone();
    let snapshot = move |sup: &Sup| {
        let services: Vec<ServiceState> = sup.list().into_iter().map(clean).collect();
        let mut m = sse_msg("snapshot", &json!({ "services": services, "configErrors": sup.config_errors(), "machine": super::peers::local_name() }));
        if with_peers {
            m.push_str(&sse_msg("machines", &json!({ "local": super::peers::local_name(), "machines": peers.list() })));
        }
        m
    };
    let _ = tx.try_send(format!(": connected\n\n{}", snapshot(&st.sup)));
    let sup = st.sup.clone();
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.tick().await;
        loop {
            let msg = tokio::select! {
                r = sub.recv() => match r {
                    Ok(Event::Service(s)) => sse_msg("service", &clean(*s)),
                    Ok(Event::Removed(name)) => sse_msg("removed", &json!({ "name": name })),
                    Ok(Event::Metrics(name, m)) => sse_msg("metrics", &json!({ "name": name, "metrics": m })),
                    Ok(Event::System(s)) => sse_msg("system", &s),
                    Ok(Event::Processes(p)) => sse_msg("processes", &json!({ "processes": *p })),
                    Ok(Event::Peer(p)) if with_peers => sse_msg("peer", &json!({ "machine": p.machine, "event": p.event, "data": p.data })),
                    Ok(Event::Peer(_)) => continue,
                    // Approval requests quote config changes and are answered only on this Mac.
                    Ok(Event::Notice(n)) if redact && n.get("approval").is_some() => continue,
                    Ok(Event::Notice(n)) if with_peers => sse_msg("notice", &*n),
                    Ok(Event::Notice(_)) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => snapshot(&sup),
                    Err(_) => break,
                },
                _ = ping.tick() => {
                    if alive.as_ref().is_some_and(|f| !f()) {
                        break;
                    }
                    ": ping\n\n".to_string()
                }
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    sse_response(rx)
}

async fn remote_status(State(st): State<AppState>) -> Res {
    Ok(Json(st.remote.status()))
}

async fn remote_update(State(st): State<AppState>, body: Bytes) -> Res {
    let patch: super::remote::RemotePatch =
        serde_json::from_slice(if body.is_empty() { b"{}" } else { &body }).map_err(|e| ApiErr(400, e.to_string()))?;
    let remote = st.remote.clone();
    Ok(Json(remote.update(&st, patch).await?))
}

// Secrets are write-only: there is deliberately no route that returns a value.
async fn secrets_list(State(st): State<AppState>) -> Res {
    Ok(Json(super::secrets::status(&st.sup.configs())))
}

/// Typing into (or running commands inside) a service that holds secrets needs a Touch ID approval.
pub(super) fn console_gate(st: &AppState, name: &str) -> Result<(), ApiErr> {
    let cfg = st.sup.get_state(name)?.config;
    super::approvals::console(&cfg).map_err(|e| ApiErr(428, e))
}

async fn approvals_list() -> Res {
    Ok(Json(tokio::task::spawn_blocking(super::approvals::list).await.map_err(|e| ApiErr(500, e.to_string()))?))
}

async fn approval_get(Path(id): Path<String>) -> Res {
    super::approvals::get(&id).map(Json).ok_or_else(|| ApiErr(404, "no such approval".into()))
}

async fn approval_sign(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let sig = v["signature"].as_str().ok_or_else(|| ApiErr(400, "body must be {\"signature\": \"<hex DER>\"}".into()))?.to_string();
    let id2 = id.clone();
    let start = tokio::task::spawn_blocking(move || super::approvals::approve(&id2, &sig))
        .await
        .map_err(|e| ApiErr(500, e.to_string()))?
        .map_err(|(c, e)| ApiErr(c, e))?;
    if let Some(name) = start {
        // It was held back at start: start it now if it's meant to be running.
        if st.sup.wants_running(&name) {
            let sup = st.sup.clone();
            tokio::spawn(async move {
                let _ = sup.start(&name).await;
            });
        } else {
            st.sup.touch(&name);
        }
    }
    approval_get(Path(id)).await
}

async fn approval_deny(State(st): State<AppState>, Path(id): Path<String>) -> Res {
    super::approvals::deny(&id).map_err(|(c, e)| ApiErr(c, e))?;
    if let Some(name) = super::approvals::get(&id).and_then(|p| p["service"].as_str().map(String::from)) {
        st.sup.touch(&name);
    }
    approval_get(Path(id)).await
}

async fn approval_key(State(st): State<AppState>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let pk = v["publicKey"].as_str().ok_or_else(|| ApiErr(400, "body must be {\"publicKey\": \"<hex>\"}".into()))?.to_string();
    let sig = v["signature"].as_str().map(String::from);
    let configs = st.sup.configs();
    tokio::task::spawn_blocking(move || super::approvals::register(&pk, sig.as_deref(), &configs))
        .await
        .map_err(|e| ApiErr(500, e.to_string()))?
        .map(Json)
        .map_err(|(c, e)| ApiErr(c, e))
}

fn not_internal(name: &str) -> Result<(), ApiErr> {
    if name.starts_with(super::secrets::INTERNAL_PREFIX) {
        return Err(ApiErr(403, format!("{} names are managed by LStack itself", super::secrets::INTERNAL_PREFIX)));
    }
    Ok(())
}

async fn secret_set(State(st): State<AppState>, Path(name): Path<String>, body: Bytes) -> Res {
    not_internal(&name)?;
    let v = parse_body(&body)?;
    let value = v.get("value").and_then(Value::as_str).ok_or_else(|| ApiErr(400, "body must be {\"value\": \"…\"}".into()))?.to_string();
    tokio::task::spawn_blocking(move || super::secrets::set(&name, &value))
        .await
        .map_err(|e| ApiErr(500, e.to_string()))?
        .map_err(|e| ApiErr(400, e))?;
    let sup = st.sup.clone();
    tokio::task::spawn_blocking(move || sup.rescrub_logs());
    secrets_list(State(st)).await
}

async fn secret_delete(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    not_internal(&name)?;
    if !super::secrets::remove(&name).map_err(|e| ApiErr(400, e))? {
        return Err(ApiErr(404, format!("no secret named {name}")));
    }
    secrets_list(State(st)).await
}

fn op_err((code, msg): (u16, String)) -> ApiErr {
    ApiErr(code, msg)
}

async fn op_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiErr> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiErr(500, e.to_string()))
}

async fn op_review(State(st): State<AppState>, Path(service): Path<String>) -> Res {
    let sup = st.sup.clone();
    Ok(Json(op_blocking(move || super::envimport::review_service(&sup, &service)).await?.map_err(op_err)?))
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct EnvPreviewReq {
    service: Option<String>,
    dir: Option<String>,
    files: Option<Vec<String>>,
}

/// Every variable in a service's (or a folder's) env files, plain values included. Local only.
async fn env_preview(State(st): State<AppState>, Json(r): Json<EnvPreviewReq>) -> Res {
    let sup = st.sup.clone();
    Ok(Json(op_blocking(move || super::envimport::preview(&sup, r.service.as_deref(), r.dir.as_deref(), r.files)).await?.map_err(op_err)?))
}

#[derive(serde::Deserialize)]
struct EnvApplyReq {
    ops: Vec<super::envimport::EnvOp>,
    #[serde(default)]
    actor: Option<String>,
}

async fn env_apply(State(st): State<AppState>, Path(name): Path<String>, body: Bytes) -> Res {
    let r: EnvApplyReq = serde_json::from_slice(&body).map_err(|e| ApiErr(400, format!("bad request: {e}")))?;
    let sup = st.sup.clone();
    let actor = r.actor.clone().unwrap_or_else(|| "app".into());
    Ok(Json(op_blocking(move || super::envimport::apply_env(&sup, &name, r.ops, &actor)).await?.map_err(op_err)?))
}

async fn env_approve(State(st): State<AppState>, Path(name): Path<String>, body: Bytes) -> Res {
    let v = if body.is_empty() { Value::Null } else { parse_body(&body)? };
    let keys: Option<Vec<String>> = v.get("keys").and_then(|k| serde_json::from_value(k.clone()).ok());
    let actor = v.get("actor").and_then(Value::as_str).unwrap_or("app").to_string();
    let sup = st.sup.clone();
    Ok(Json(op_blocking(move || super::envimport::approve_stored(&sup, &name, keys.as_deref(), &actor)).await?.map_err(op_err)?))
}

async fn op_detect(Json(v): Json<Value>) -> Res {
    let dir = v.get("cwd").and_then(Value::as_str).ok_or_else(|| ApiErr(400, "cwd required".into()))?.to_string();
    Ok(Json(op_blocking(move || super::envimport::detect(&dir)).await?))
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PlanReq {
    kind: String,
    service: Option<String>,
    keys: Option<Vec<String>>,
    names: Vec<String>,
    name: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    key: Option<String>,
    path: Option<String>,
    content: Option<String>,
    /// `link` answering a paired Mac's request (its id): shared with it once imported. Without `name`,
    /// a free one is picked (from `hint`, its env key) so a clash never replaces another value.
    #[serde(rename = "sharePeer")]
    share_peer: Option<String>,
    hint: Option<String>,
}

/// Start a review: what would be read from 1Password (or, for an Environment, what was read) and
/// where it would go. Returns a plan id the client confirms with `/api/onepassword/import`.
async fn op_plan(State(st): State<AppState>, Json(mut r): Json<PlanReq>) -> Res {
    let (sup, remote, peers) = (st.sup.clone(), st.remote.clone(), st.peers.clone());
    let v = op_blocking(move || {
        use super::envimport as e;
        if let (Some(peer), "link") = (r.share_peer.clone(), r.kind.as_str()) {
            let reference = r.reference.clone().ok_or((400, "ref required".to_string()))?;
            let name = match r.name.clone() {
                Some(n) => n,
                None => super::secretshare::free_name(&sup, &reference, r.hint.as_deref()).ok_or((409, "no free secret name".to_string()))?,
            };
            let v = e::plan_link(&sup, &name, &reference, None, None)?;
            if let Some(id) = v["id"].as_str() {
                super::secretshare::fulfill_later(&remote, &peers, id, &peer, &reference)?;
            }
            return Ok(v);
        }
        match r.kind.as_str() {
            "env" => e::plan_env(&sup, r.service.as_deref().ok_or((400, "service required".to_string()))?, r.keys.as_deref()),
            "refresh" => e::plan_refresh(&sup, &r.names, r.service.as_deref()),
            "link" => e::plan_link(
                &sup,
                r.name.as_deref().ok_or((400, "name required".to_string()))?,
                r.reference.as_deref().ok_or((400, "ref required".to_string()))?,
                r.service.as_deref(),
                r.key.as_deref(),
            ),
            "environment" => {
                let path = r.path.clone().ok_or((400, "path required".to_string()))?;
                let content = r.content.take().ok_or((400, "content required".to_string()))?;
                e::plan_environment(&sup, r.service.as_deref(), &path, content)
            }
            k => Err((400, format!("unknown plan kind {k:?} (env, refresh, link, environment)"))),
        }
    })
    .await?
    .map_err(op_err)?;
    Ok(Json(v))
}

async fn op_plan_cancel(Path(id): Path<String>) -> Res {
    Ok(Json(json!({ "cancelled": super::envimport::cancel_plan(&id) })))
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ImportBody {
    plan: String,
    values: std::collections::BTreeMap<String, String>,
    keys: Option<Vec<String>>,
    apply_removals: Option<bool>,
    account: Option<String>,
    actor: Option<String>,
}

async fn op_import(State(st): State<AppState>, Json(mut b): Json<ImportBody>) -> Res {
    let req = super::envimport::ImportReq {
        plan: std::mem::take(&mut b.plan),
        values: std::mem::take(&mut b.values),
        keys: b.keys.take(),
        apply_removals: b.apply_removals,
        account: b.account.take(),
        actor: b.actor.take().filter(|a| matches!(a.as_str(), "cli" | "app")).unwrap_or_else(|| "api".into()),
    };
    let (sup, remote, peers) = (st.sup.clone(), st.remote.clone(), st.peers.clone());
    let r = op_blocking(move || {
        let (id, actor) = (req.plan.clone(), req.actor.clone());
        let mut v = super::envimport::import(&sup, req)?;
        // Read to answer a paired Mac's request: share it with that Mac now.
        if let Some(m) = super::secretshare::fulfilled(&sup, &remote, &peers, &id, &actor) {
            v["sharedWith"] = json!(m);
        }
        Ok(v)
    });
    Ok(Json(r.await?.map_err(op_err)?))
}

async fn op_shares(State(st): State<AppState>) -> Res {
    Ok(Json(super::secretshare::shares(&st.remote, &st.peers)))
}

fn items_of(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

fn actor_of(v: &Value) -> String {
    v["actor"].as_str().filter(|a| matches!(*a, "cli" | "app")).unwrap_or("api").to_string()
}

fn peer_of(v: &Value, key: &str) -> Result<String, ApiErr> {
    v[key].as_str().map(String::from).ok_or_else(|| ApiErr(400, format!("{key} required")))
}

/// `{peer, items: [name | ref], allow: bool, actor}`: share stored 1Password values with a paired Mac (or
/// stop). Approves its requests for them.
async fn op_share(State(st): State<AppState>, Json(v): Json<Value>) -> Res {
    let (peer, items, actor) = (peer_of(&v, "peer")?, items_of(&v, "items"), actor_of(&v));
    let allow = v["allow"].as_bool().unwrap_or(true);
    let (sup, remote, peers) = (st.sup.clone(), st.remote.clone(), st.peers.clone());
    let r = op_blocking(move || {
        super::secretshare::set_share(&remote, &peers, &peer, &items, allow, &actor)?;
        // A Mac this one watches gets approved values now rather than on the next round.
        super::secretshare::nudge(&sup, &remote, &peers, &peer);
        Ok(super::secretshare::shares(&remote, &peers))
    });
    Ok(Json(r.await?.map_err(op_err)?))
}

/// `{peer, items: [ref], actor}`: decline (hide) a paired Mac's requests.
async fn op_share_decline(State(st): State<AppState>, Json(v): Json<Value>) -> Res {
    let (peer, items, actor) = (peer_of(&v, "peer")?, items_of(&v, "items"), actor_of(&v));
    let (remote, peers) = (st.remote.clone(), st.peers.clone());
    Ok(Json(op_blocking(move || super::secretshare::ignore(&remote, &peers, &peer, &items, &actor)).await?.map_err(op_err)?))
}

/// `{peer, items: [name | ref], actor}`: push stored 1Password values to a paired Mac; it must accept them.
async fn op_push(State(st): State<AppState>, Json(v): Json<Value>) -> Res {
    let (peer, items, actor) = (peer_of(&v, "peer")?, items_of(&v, "items"), actor_of(&v));
    let (sup, remote, peers) = (st.sup.clone(), st.remote.clone(), st.peers.clone());
    let r = op_blocking(move || {
        super::secretshare::push(&remote, &peers, &peer, &items, &actor)?;
        super::secretshare::nudge(&sup, &remote, &peers, &peer);
        Ok(super::secretshare::shares(&remote, &peers))
    });
    Ok(Json(r.await?.map_err(op_err)?))
}

/// `{service?}`: sync with paired Macs now — ask for what our services wait for (or `service`'s), take
/// what they approved, and newer values of ones held.
async fn op_peer_sync(State(st): State<AppState>, body: axum::body::Bytes) -> Res {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let service = v["service"].as_str().map(String::from);
    let (sup, peers, remote) = (st.sup.clone(), st.peers.clone(), st.remote.clone());
    let r = op_blocking(move || {
        let mut v = super::secretshare::sync(&sup, &peers, service.as_deref(), true);
        let errs = super::secretshare::serve_watchers(&sup, &remote, &peers, None);
        if let Some(a) = v["errors"].as_array_mut() {
            a.extend(errs.into_iter().map(Value::String));
        }
        v
    });
    Ok(Json(r.await?))
}

/// `{machine, refs?}`: accept values a paired Mac pushed (all of them without `refs`).
async fn op_peer_accept(State(st): State<AppState>, Json(v): Json<Value>) -> Res {
    let machine = peer_of(&v, "machine")?;
    let refs = v.get("refs").map(|_| items_of(&v, "refs"));
    let (sup, peers) = (st.sup.clone(), st.peers.clone());
    Ok(Json(op_blocking(move || super::secretshare::accept(&sup, &peers, &machine, refs.as_deref())).await?.map_err(op_err)?))
}

/// `{machine, refs?}`: decline values a paired Mac pushed.
async fn op_peer_decline(Json(v): Json<Value>) -> Res {
    let machine = peer_of(&v, "machine")?;
    let refs = v.get("refs").map(|_| items_of(&v, "refs"));
    Ok(Json(super::secretshare::decline(&machine, refs.as_deref()).map_err(op_err)?))
}

async fn op_audit(Query(q): Query<HashMap<String, String>>) -> Res {
    let n = q.get("n").and_then(|n| n.parse().ok()).unwrap_or(100usize).min(1000);
    Ok(Json(json!({ "entries": super::envimport::audit_tail(n) })))
}

async fn secrets_unlock(State(st): State<AppState>) -> Res {
    tokio::task::spawn_blocking(super::secrets::unlock).await.map_err(|e| ApiErr(500, e.to_string()))?.map_err(|e| ApiErr(409, e))?;
    secrets_list(State(st)).await
}

async fn secrets_rotate(State(st): State<AppState>) -> Res {
    tokio::task::spawn_blocking(super::secrets::rotate).await.map_err(|e| ApiErr(500, e.to_string()))?.map_err(|e| ApiErr(500, e))?;
    secrets_list(State(st)).await
}

pub(super) async fn processes(State(st): State<AppState>) -> Res {
    to_json(json!({ "processes": *st.sup.processes(), "thresholds": super::terminal::load().processes }))
}

async fn shutdown(State(st): State<AppState>) -> Res {
    let n = st.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        n.notify_one();
    });
    ok()
}

async fn open_term(State(st): State<AppState>, Path(name): Path<String>, q: Q) -> Res {
    let s = st.sup.get_state(&name)?;
    open_terminal_with(&[if s.kind == "process" { "attach" } else { "exec" }, &s.name], q.get("terminal").cloned()).await
}

async fn cmux_enable() -> Res {
    let v = tokio::task::spawn_blocking(super::terminal::cmux_enable_password).await.map_err(|e| ApiErr(500, e.to_string()))?.map_err(|e| ApiErr(400, e))?;
    to_json(v)
}

async fn totp_confirm(State(st): State<AppState>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let code = v.get("code").and_then(Value::as_str).unwrap_or_default();
    st.remote.totp_confirm(code)?;
    // Restart the listener so open remote streams from the old sessions are cut off.
    let remote = st.remote.clone();
    remote.apply(&st).await;
    Ok(Json(st.remote.status()))
}

async fn settings_update(body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let mut s = super::terminal::load();
    if let Some(h) = v.get("healthchecks") {
        let mut cur = serde_json::to_value(&s.healthchecks).unwrap_or_default();
        merge(&mut cur, h);
        s.healthchecks = serde_json::from_value(cur).map_err(|e| ApiErr(400, format!("healthchecks: {e}")))?;
        if let Some(m) = &s.healthchecks.machine {
            if let Some(u) = m.url.as_deref().filter(|u| !u.is_empty()) {
                if !u.starts_with("http://") && !u.starts_with("https://") {
                    return Err(ApiErr(400, "healthchecks.machine.url must be http(s)".into()));
                }
            }
        }
        if !s.healthchecks.base_url.is_empty() && !s.healthchecks.base_url.starts_with("http") {
            return Err(ApiErr(400, "healthchecks.baseUrl must be http(s)".into()));
        }
        // Write-only: the ping key goes to the secret store, never settings.json.
        match h.get("pingKey") {
            Some(Value::String(k)) if !k.is_empty() => {
                let k = k.clone();
                tokio::task::spawn_blocking(move || super::secrets::set(super::heartbeat::PING_KEY_SECRET, &k))
                    .await
                    .map_err(|e| ApiErr(500, e.to_string()))?
                    .map_err(|e| ApiErr(400, e))?;
            }
            Some(Value::Null) | Some(Value::String(_)) => {
                let _ = super::secrets::remove(super::heartbeat::PING_KEY_SECRET);
            }
            _ => {}
        }
    }
    if let Some(p) = v.get("processes") {
        let mut cur = serde_json::to_value(&s.processes).unwrap_or_default();
        merge(&mut cur, p);
        s.processes = serde_json::from_value(cur).map_err(|e| ApiErr(400, format!("processes: {e}")))?;
    }
    if let Some(p) = v.get("onepassword") {
        let mut cur = serde_json::to_value(&s.onepassword).unwrap_or_default();
        merge(&mut cur, p);
        s.onepassword = serde_json::from_value(cur).map_err(|e| ApiErr(400, format!("onepassword: {e}")))?;
    }
    if let Some(t) = v.get("terminal").and_then(Value::as_str) {
        if !t.is_empty() && !super::terminal::is_known(t) {
            return Err(ApiErr(400, format!("unknown terminal {t:?} (terminal, iterm, cmux, ghostty)")));
        }
        s.terminal = t.to_string();
    }
    super::terminal::save(&s).map_err(|e| ApiErr(500, format!("cannot save settings.json: {e}")))?;
    to_json(super::terminal::status())
}

/// Shallow-ish JSON merge for settings patches (`null` resets a key to its default).
fn merge(dst: &mut Value, patch: &Value) {
    let (Some(d), Some(p)) = (dst.as_object_mut(), patch.as_object()) else { return };
    for (k, v) in p {
        if k == "pingKey" {
            continue;
        }
        if v.is_null() {
            d.remove(k);
        } else if v.is_object() && d.get(k).is_some_and(Value::is_object) {
            merge(d.get_mut(k).unwrap(), v);
        } else {
            d.insert(k.clone(), v.clone());
        }
    }
}

async fn heartbeat_test(State(st): State<AppState>, body: Bytes) -> Res {
    let v = if body.is_empty() { json!({}) } else { parse_body(&body)? };
    let name = v.get("name").and_then(Value::as_str).map(String::from);
    let sup = st.sup.clone();
    let r = tokio::task::spawn_blocking(move || super::heartbeat::test(&sup, name.as_deref())).await.map_err(|e| ApiErr(500, e.to_string()))?;
    to_json(r.map_err(|e| ApiErr(502, e))?)
}

/// Open the preferred terminal app (or `terminal`, if given) running `lstack <args>`; runs
/// osascript/open, so off the async runtime. When cmux refuses the connection, answers
/// `{ok: false, cmuxDenied: true, error}` so the UI can offer the password fix or a fallback.
async fn open_terminal_with(args: &[&str], terminal: Option<String>) -> Res {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let r = tokio::task::spawn_blocking(move || super::terminal::open_with(&args.iter().map(String::as_str).collect::<Vec<_>>(), terminal.as_deref()))
        .await
        .map_err(|e| ApiErr(500, e.to_string()))?;
    match r {
        Ok(()) => ok(),
        Err(e) if e.starts_with(super::terminal::CMUX_DENIED) => {
            let msg = e.trim_start_matches(super::terminal::CMUX_DENIED).trim_start_matches(':').trim().to_string();
            to_json(json!({ "ok": false, "cmuxDenied": true, "error": msg }))
        }
        Err(e) => Err(ApiErr(500, e)),
    }
}


// ------------------------------------------------------------------ machines (peers)

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiErr> + Send + 'static) -> Result<T, ApiErr> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiErr(500, e.to_string()))?
}

/// This machine plus every paired machine.
pub(super) async fn machines_list(State(st): State<AppState>) -> Res {
    let local = json!({
        "name": super::peers::local_name(),
        "local": true,
        "status": "online",
        "hostname": super::remote::hostname(),
        "version": VERSION,
        "processes": *st.sup.processes(),
    });
    Ok(Json(json!({ "local": local, "machines": st.peers.list() })))
}

pub(super) async fn machine_get(State(st): State<AppState>, Path(m): Path<String>) -> Res {
    Ok(Json(st.peers.get(&m)?))
}

async fn machine_add(State(st): State<AppState>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let mut addr = v["address"].as_str().or(v["url"].as_str()).unwrap_or("").to_string();
    let name = v["name"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let mut code = v["code"].as_str().unwrap_or("").trim().to_string();
    // A Quick Connect link carries both.
    if let Some((a, c)) = super::peers::parse_quick_link(&addr) {
        addr = a;
        if code.is_empty() {
            code = c;
        }
    }
    if code.is_empty() {
        return Err(ApiErr(400, "pairing needs the other Mac's one-time code (Share this Mac → Show pairing code there), plus its 2FA code if it uses two-factor authentication".into()));
    }
    let mut auth = json!({});
    if let Some(t) = v["totp"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        auth["totp"] = json!(t);
    }
    Ok(Json(st.peers.pair(&addr, &code, auth, name.as_deref()).await?))
}

async fn identity() -> Res {
    let id = blocking(|| super::link::identity().map_err(|e| ApiErr(500, e))).await?;
    Ok(Json(json!({
        "name": super::peers::local_name(),
        "fingerprint": super::link::fingerprint(&id.public),
        "publicKey": super::link::key_hex(&id.public),
    })))
}

/// New install key: every pairing (both directions) must be made again.
async fn identity_rekey(State(st): State<AppState>) -> Res {
    let id = blocking(|| super::link::rekey().map_err(|e| ApiErr(500, e))).await?;
    st.remote.forget_peers();
    st.peers.reconnect_all();
    Ok(Json(json!({ "name": super::peers::local_name(), "fingerprint": super::link::fingerprint(&id.public), "publicKey": super::link::key_hex(&id.public) })))
}

fn forwards_pause(State(st): State<AppState>, body: Bytes, paused: bool) -> Res {
    let v = parse_body(&body)?;
    let (Some(m), Some(svc)) = (v["machine"].as_str(), v["service"].as_str()) else {
        return Err(ApiErr(400, "machine and service required".into()));
    };
    st.peers.set_forward_paused(m, svc, paused)?;
    Ok(Json(st.peers.forwards.list()))
}

async fn machine_patch(State(st): State<AppState>, Path(m): Path<String>, body: Bytes) -> Res {
    let v = parse_body(&body)?;
    let to = v["name"].as_str().ok_or_else(|| ApiErr(400, "name required".into()))?.to_string();
    let peers = st.peers.clone();
    Ok(Json(blocking(move || peers.rename(&m, &to)).await?))
}

async fn machine_remove(State(st): State<AppState>, Path(m): Path<String>) -> Res {
    st.peers.unpair(&m)?;
    ok()
}

pub(super) fn is_local(m: &str) -> bool {
    m == super::peers::local_name()
}

pub(super) async fn m_service(State(st): State<AppState>, Path((m, name)): Path<(String, String)>) -> Res {
    if is_local(&m) {
        return to_json(st.sup.get_state(&name)?);
    }
    let peers = st.peers.clone();
    Ok(Json(blocking(move || peers.call(&m, "GET", &format!("/services/{}", enc(&name)))).await?))
}

pub(super) async fn m_logs(State(st): State<AppState>, Path((m, name)): Path<(String, String)>, q: Q) -> Res {
    if is_local(&m) {
        return logs(State(st), Path(name), q).await;
    }
    let peers = st.peers.clone();
    let tail = tail_param(&q);
    let before = q.get("before").map(|b| format!("&before={}", enc(b))).unwrap_or_default();
    Ok(Json(blocking(move || peers.call(&m, "GET", &format!("/services/{}/logs?tail={tail}{before}", enc(&name)))).await?))
}

pub(super) async fn m_metrics(State(st): State<AppState>, Path((m, name)): Path<(String, String)>, q: Q) -> Res {
    if is_local(&m) {
        return metrics(State(st), Path(name), q).await;
    }
    let peers = st.peers.clone();
    let range = enc(q.get("range").map(String::as_str).unwrap_or("1h"));
    let points = q.get("points").and_then(|p| p.parse::<usize>().ok()).unwrap_or(120);
    Ok(Json(blocking(move || peers.call(&m, "GET", &format!("/services/{}/metrics?range={range}&points={points}", enc(&name)))).await?))
}

/// `allowed`: whether *this* daemon lets the caller control services (the peer also checks its own setting).
pub(super) async fn m_action(State(st): State<AppState>, Path((m, name, action)): Path<(String, String, String)>, allowed: bool) -> Res {
    if !allowed {
        return Err(ApiErr(403, "remote control is disabled on this machine".into()));
    }
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return Err(ApiErr(404, "unknown action".into()));
    }
    if is_local(&m) {
        let st = match action.as_str() {
            "start" => st.sup.start(&name).await?,
            "stop" => st.sup.stop(&name).await?,
            _ => st.sup.restart(&name).await?,
        };
        return to_json(st);
    }
    let peers = st.peers.clone();
    Ok(Json(blocking(move || peers.call(&m, "POST", &format!("/services/{}/{action}", enc(&name)))).await?))
}

/// Relay a peer's log stream, scrubbing each line with this machine's secrets too.
pub(super) async fn m_log_stream(State(st): State<AppState>, Path((m, name)): Path<(String, String)>, q: Q) -> Result<Response, ApiErr> {
    if is_local(&m) {
        return log_stream(State(st), Path(name), q).await;
    }
    let peers = st.peers.clone();
    let path = format!("/services/{}/logs/stream?tail={}", enc(&name), tail_param(&q));
    let reader = blocking(move || peers.stream(&m, &path)).await?;
    let (tx, rx) = mpsc::channel::<String>(256);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut rd = std::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match rd.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if tx.blocking_send(crate::scrub::scrub(&line).into_owned()).is_err() {
                break;
            }
        }
    });
    Ok(sse_response(rx))
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations() {
        use super::parse_duration as p;
        assert_eq!(p("30s"), 30_000);
        assert_eq!(p("5m"), 300_000);
        assert_eq!(p("2h"), 7_200_000);
        assert_eq!(p("1d"), 86_400_000);
        assert_eq!(p("90"), 90_000);
        assert_eq!(p("junk"), 3_600_000);
    }
}

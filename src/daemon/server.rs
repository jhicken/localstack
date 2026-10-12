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
}

pub struct ApiErr(u16, String);
impl ApiErr {
    pub fn new(code: u16, msg: impl Into<String>) -> Self {
        ApiErr(code, msg.into())
    }
}
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "error": self.1 }))).into_response()
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
    let query_tok = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("token=").map(String::from))
    });
    if header_tok.or(query_tok).as_deref() != Some(st.token.as_str()) {
        return ApiErr(401, "unauthorized".into()).into_response();
    }
    next.run(req).await
}

pub fn router(st: AppState) -> Router {
    Router::new()
        .route("/api/health", get(|| async { Json(json!({ "ok": true, "version": VERSION, "pid": std::process::id() })) }))
        .route("/api/services", get(list).post(create))
        .route("/api/services/:name", get(get_one).patch(patch).delete(destroy))
        .route("/api/services/:name/start", post(start))
        .route("/api/services/:name/stop", post(stop))
        .route("/api/services/:name/restart", post(restart))
        .route("/api/services/:name/stdin", post(stdin))
        .route("/api/services/:name/logs", get(logs).delete(clear_logs))
        .route("/api/services/:name/logs/stream", get(log_stream))
        .route("/api/services/:name/metrics", get(metrics))
        .route("/api/services/:name/open-terminal", post(open_term))
        .route("/api/events", get(events))
        .route("/api/remote", get(remote_status).put(remote_update))
        .route("/api/remote/totp", post(|State(st): State<AppState>| async move { to_json(st.remote.totp_begin()) }).delete(|State(st): State<AppState>| async move { Ok::<_, ApiErr>(Json(st.remote.totp_disable()?)) }))
        .route("/api/remote/totp/confirm", post(totp_confirm))
        .route("/api/settings", get(|| async { to_json(super::terminal::status()) }).put(settings_update))
        .route("/api/split-command", post(split_command))
        .route("/api/reload", post(|State(st): State<AppState>| async move { to_json(st.sup.reload()) }))
        .route("/api/shutdown", post(shutdown))
        .route("/api/open-dashboard", post(|| async { open_terminal(&["top"]).await }))
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

pub(super) async fn logs(State(st): State<AppState>, Path(name): Path<String>, q: Q) -> Res {
    to_json(json!({ "lines": st.sup.logs(&name)?.tail(tail_param(&q)) }))
}

async fn clear_logs(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    st.sup.logs(&name)?.clear();
    ok()
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
    events_stream(st, false).await
}

/// Service events as SSE. `redact` hides env values (used by the remote dashboard).
pub(super) async fn events_stream(st: AppState, redact: bool) -> Response {
    let clean = move |mut s: ServiceState| {
        if redact {
            if let Some(env) = s.config.env.as_mut() {
                env.values_mut().for_each(|v| *v = "••••".into());
            }
        }
        s
    };
    let mut sub = st.sup.events.subscribe();
    let (tx, rx) = mpsc::channel::<String>(512);
    let snapshot = move |sup: &Sup| {
        let services: Vec<ServiceState> = sup.list().into_iter().map(clean).collect();
        sse_msg("snapshot", &json!({ "services": services, "configErrors": sup.config_errors() }))
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
                    Err(broadcast::error::RecvError::Lagged(_)) => snapshot(&sup),
                    Err(_) => break,
                },
                _ = ping.tick() => ": ping\n\n".to_string(),
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

async fn shutdown(State(st): State<AppState>) -> Res {
    let n = st.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        n.notify_one();
    });
    ok()
}

async fn open_term(State(st): State<AppState>, Path(name): Path<String>) -> Res {
    let s = st.sup.get_state(&name)?;
    open_terminal(&[if s.kind == "process" { "attach" } else { "exec" }, &s.name]).await
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
    if let Some(t) = v.get("terminal").and_then(Value::as_str) {
        if !t.is_empty() && !super::terminal::is_known(t) {
            return Err(ApiErr(400, format!("unknown terminal {t:?} (terminal, iterm, cmux, ghostty)")));
        }
        s.terminal = t.to_string();
    }
    super::terminal::save(&s).map_err(|e| ApiErr(500, format!("cannot save settings.json: {e}")))?;
    to_json(super::terminal::status())
}

/// Open the preferred terminal app running `lstack <args>` (runs osascript/open, so off the async runtime).
async fn open_terminal(args: &[&str]) -> Res {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || super::terminal::open(&args.iter().map(String::as_str).collect::<Vec<_>>()))
        .await
        .map_err(|e| ApiErr(500, e.to_string()))?
        .map_err(|e| ApiErr(500, e))?;
    ok()
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

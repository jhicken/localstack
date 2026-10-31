use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

fn scalar_string(v: serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Accept numbers/bools where strings are expected (`PORT: 3000`, `ports: [8080:80]`).
fn lenient_map<'de, D: Deserializer<'de>>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error> {
    let raw: Option<BTreeMap<String, serde_yaml::Value>> = Option::deserialize(d)?;
    Ok(raw.map(|m| m.into_iter().filter_map(|(k, v)| scalar_string(v).map(|s| (k, s))).collect()))
}
fn lenient_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    let raw: Option<Vec<serde_yaml::Value>> = Option::deserialize(d)?;
    Ok(raw.map(|v| v.into_iter().filter_map(scalar_string).collect()))
}

/// A list, or a single string as shorthand for a one-item list.
fn string_or_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(match Option::<serde_yaml::Value>::deserialize(d)? {
        None | Some(serde_yaml::Value::Null) => None,
        Some(serde_yaml::Value::Sequence(v)) => Some(v.into_iter().filter_map(scalar_string).collect()),
        Some(v) => Some(scalar_string(v).into_iter().collect()),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// `secrets: [NAME, …]` (env var = secret name) or `secrets: { ENV_VAR: SECRET_NAME }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretRefs {
    List(Vec<String>),
    Map(BTreeMap<String, String>),
}

/// A web address a service serves: a plain string or `{ url, label }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UrlSpec {
    Url(String),
    Labeled {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        label: Option<String>,
    },
}

impl UrlSpec {
    pub fn url(&self) -> &str {
        match self {
            UrlSpec::Url(u) | UrlSpec::Labeled { url: u, .. } => u,
        }
    }
    pub fn label(&self) -> Option<&str> {
        match self {
            UrlSpec::Url(_) => None,
            UrlSpec::Labeled { label, .. } => label.as_deref(),
        }
    }
}

/// healthchecks.io-compatible pings for one service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    /// Full ping URL (e.g. https://hc-ping.com/<uuid>). Otherwise built from the ping key + slug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Slug used with the ping key (default: the service name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// Seconds between success pings (default 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<f64>,
    /// Send `/start` when the service starts (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<bool>,
    /// Send `/fail` on crash / unhealthy (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail: Option<bool>,
    /// Attach the last log lines (scrubbed) to `/fail` pings (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<bool>,
    /// Set to false to keep the config but stop pinging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// On-disk service definition (`~/.localstack/services/<name>.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfig {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type", default = "default_type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
    /// Registered from the Docker engine (`lstack docker import|sync`); sync removes it again once
    /// its container / compose project no longer exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_vec")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthCheck>,
    /// Secrets from the LStack secret store, passed as environment variables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretRefs>,
    /// Web addresses to show for this service, in addition to detected listening ports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<UrlSpec>>,
    /// `false` = only show `urls`, don't add auto-detected ports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detect_urls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<Heartbeat>,
    // process
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_map")]
    pub env: Option<BTreeMap<String, String>>,
    /// `.env` files (relative to `cwd`) read the way `op run --env-file` reads them: plain values are
    /// set at each start (later files win, and they override `env:`); `op://` references are imported
    /// into the secret store and injected through `secrets:`. Applies to every service type.
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "string_or_vec")]
    pub env_files: Option<Vec<String>>,
    /// Keys of `secrets:` that were approved from an `lstack://NAME` line in the env files; removing
    /// the line stops passing the secret.
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "string_or_vec")]
    pub file_secrets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Files sourced before the command (default: the shell's rc file, e.g. ~/.zshrc). `[]` = none.
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "string_or_vec")]
    pub source: Option<Vec<String>>,
    // docker
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_vec")]
    pub ports: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_vec")]
    pub volumes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_vec")]
    pub args: Option<Vec<String>>,
    // compose
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    // k8s
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

fn default_type() -> String {
    "process".into()
}

impl ServiceConfig {
    pub fn container_name(&self) -> String {
        self.container.clone().unwrap_or_else(|| format!("lstack-{}", self.name))
    }
    pub fn autostart(&self) -> bool {
        self.autostart.unwrap_or(false)
    }
    /// (env var, secret name) pairs.
    pub fn secret_refs(&self) -> Vec<(String, String)> {
        match &self.secrets {
            None => vec![],
            Some(SecretRefs::List(v)) => v.iter().map(|s| (s.clone(), s.clone())).collect(),
            Some(SecretRefs::Map(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }
}

/// A web address shown for a running service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub port: Option<u16>,
    /// "detected" (listening socket / docker port) or "config" (`urls:` in YAML).
    pub source: String,
    /// Bound to loopback only: reachable from this machine but not from other machines.
    #[serde(default)]
    pub local_only: bool,
    /// A socket of the service's own processes is listening on this port right now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listening: bool,
    /// Reached through the encrypted link to this machine (a local forward); the URL is on this Mac.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub via: Option<String>,
    /// Why it can't be opened from here as is: "off" (forwarding is off for the service; turn it on) or
    /// "untunneled" (it's on another Mac that doesn't let this one tunnel to it; services on other Macs
    /// are only reached through the encrypted link, never directly over the network).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub blocked: Option<String>,
}

/// Outcome of the latest healthchecks.io ping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatStatus {
    pub last_ping: i64,
    /// "success" | "start" | "fail"
    pub kind: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// A process on the machine that crosses a usage threshold (or belongs to a service).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProcInfo {
    pub pid: i32,
    pub name: String,
    /// % of one core.
    pub cpu: f64,
    /// Resident bytes.
    pub mem: f64,
    /// Network bytes/s (rx + tx), when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub net: Option<f64>,
    /// The LStack service this process belongs to, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub service: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MetricSample {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub net_rx: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub net_tx: Option<f64>,
}

/// Whole-machine load at one point in time, with LStack's own share of it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemSample {
    pub ts: i64,
    /// Busy CPU, % of all cores (0–100).
    pub cpu: f64,
    /// Memory in use (bytes).
    pub mem: f64,
    /// CPU used by LStack services, % of all cores.
    pub ls_cpu: f64,
    /// Memory used by LStack services (bytes).
    pub ls_mem: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceState {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub autostart: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    pub status: String,
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub restarts: u32,
    /// Set while a crash-loop restart is scheduled: when it will fire (ms since epoch).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub restart_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metrics: Option<MetricSample>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub urls: Vec<ServiceUrl>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub heartbeat: Option<HeartbeatStatus>,
    /// Which machine runs it: None = this machine, else the paired machine's name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub machine: Option<String>,
    /// 1Password references in `envFiles` that changed since they were last approved (names and refs only).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub secrets_pending: Option<serde_json::Value>,
    /// Plain env-file keys edited since the service started (a restart applies them).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub env_stale: Option<Vec<String>>,
    /// Keys defined in the service's env files (names only).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub env_file_keys: Vec<String>,
    /// A Touch ID approval this service is waiting on (its secrets are withheld until then).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub approval: Option<serde_json::Value>,
    pub config: ServiceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: i64,
    pub stream: String,
    pub line: String,
    /// Opaque cursor of this line in the log file; pass it as `before=` to page to older lines.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pos: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: i32,
    pub port: u16,
    pub token: String,
    pub version: String,
    /// API capability level (`paths::PROTOCOL`); 0 for daemons that predate it.
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub started_at: i64,
    /// Absolute path of the lstack binary running the daemon.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub running: Option<bool>,
}

impl ServiceState {
    /// Status for display: "restarting 12s" while a crash-loop restart is pending.
    pub fn status_label(&self) -> String {
        match self.restart_at {
            Some(t) => format!("restarting {}s", ((t - crate::paths::now_ms()).max(0) + 999) / 1000),
            None => self.status.clone(),
        }
    }
}

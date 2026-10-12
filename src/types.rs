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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_vec")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthCheck>,
    // process
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "lenient_map")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metrics: Option<MetricSample>,
    pub config: ServiceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: i64,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: i32,
    pub port: u16,
    pub token: String,
    pub version: String,
    #[serde(default)]
    pub started_at: i64,
    /// Absolute path of the lstack binary running the daemon.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub running: Option<bool>,
}

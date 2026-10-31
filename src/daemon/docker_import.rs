//! Discover containers that already exist in the Docker engine (Docker Desktop) and adopt them as
//! LStack services: plain containers become `docker` services (`container: <name>`), containers of a
//! compose project become one `compose` service per project.
use super::docker_api;
use super::supervisor::Sup;
use crate::config;
use crate::types::ServiceConfig;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// One importable unit: a container or a whole compose project.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub kind: &'static str, // docker | compose
    pub name: String,       // container name or compose project
    pub image: String,
    pub state: String,
    pub status: String,
    pub ports: Vec<String>,
    pub containers: usize,
    pub compose_file: Option<String>,
    pub managed: bool,
}

impl Candidate {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind, "name": self.name, "image": self.image, "state": self.state,
            "status": self.status, "ports": self.ports, "containers": self.containers,
            "composeFile": self.compose_file, "managed": self.managed,
        })
    }
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

fn container_name(c: &Value) -> Option<String> {
    Some(c["Names"].as_array()?.first()?.as_str()?.trim_start_matches('/').to_string())
}

fn port_strings(c: &Value) -> Vec<String> {
    let mut out: Vec<String> = c["Ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let public = p["PublicPort"].as_u64()?;
            Some(format!("{public}:{}", p["PrivatePort"].as_u64()?))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Existing (not yet managed) workloads from a `containers/json?all=1` listing.
pub fn candidates(all: &[Value], services: &[ServiceConfig]) -> Vec<Candidate> {
    let managed_containers: HashSet<String> = services.iter().filter(|s| s.kind == "docker").map(|s| s.container_name()).collect();
    let managed_projects: HashSet<&str> = services.iter().filter(|s| s.kind == "compose").filter_map(|s| s.project.as_deref()).collect();
    let managed_files: HashSet<String> = services.iter().filter(|s| s.kind == "compose").filter_map(|s| s.compose_file.clone()).collect();
    let mut singles = vec![];
    let mut projects: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for c in all {
        match c.pointer("/Labels/com.docker.compose.project").and_then(Value::as_str) {
            Some(p) => projects.entry(p.to_string()).or_default().push(c),
            None => {
                let Some(name) = container_name(c) else { continue };
                singles.push(Candidate {
                    kind: "docker",
                    managed: managed_containers.contains(&name),
                    name,
                    image: c["Image"].as_str().unwrap_or_default().to_string(),
                    state: c["State"].as_str().unwrap_or_default().to_string(),
                    status: c["Status"].as_str().unwrap_or_default().to_string(),
                    ports: port_strings(c),
                    containers: 1,
                    compose_file: None,
                });
            }
        }
    }
    for (project, cs) in projects {
        let file = cs
            .iter()
            .filter_map(|c| c.pointer("/Labels/com.docker.compose.project.config_files").and_then(Value::as_str))
            .flat_map(|f| f.split(','))
            .map(str::trim)
            .find(|f| !f.is_empty())
            .map(String::from);
        let running = cs.iter().filter(|c| c["State"].as_str() == Some("running")).count();
        let mut ports: Vec<String> = cs.iter().flat_map(|c| port_strings(c)).collect();
        ports.sort();
        ports.dedup();
        singles.push(Candidate {
            kind: "compose",
            managed: managed_projects.contains(project.as_str()) || file.as_ref().is_some_and(|f| managed_files.contains(f)),
            image: format!("{} service(s)", cs.len()),
            state: if running > 0 { "running".into() } else { "exited".into() },
            status: format!("{running}/{} running", cs.len()),
            ports,
            containers: cs.len(),
            compose_file: file,
            name: project,
        });
    }
    singles.sort_by(|a, b| a.name.cmp(&b.name));
    singles
}

/// Service name for a container/project that is valid and not already taken.
pub fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    let mut n: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' }).collect();
    n = n.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take(55).collect();
    if n.is_empty() {
        n = "container".into();
    }
    if !taken.contains(&n) {
        return n;
    }
    let mut i = 2;
    loop {
        let cand = format!("{n}-{i}");
        if !taken.contains(&cand) {
            return cand;
        }
        i += 1;
    }
}

pub fn config_for(c: &Candidate, name: String) -> ServiceConfig {
    let mut cfg = ServiceConfig { name, kind: c.kind.into(), autostart: Some(false), discovered: Some(true), ..Default::default() };
    if c.kind == "docker" {
        cfg.container = Some(c.name.clone());
    } else {
        cfg.project = Some(c.name.clone());
        cfg.compose_file = c.compose_file.clone();
    }
    cfg
}

pub async fn list(sup: &Arc<Sup>) -> Result<Vec<Candidate>, String> {
    if !docker_api::ping().await {
        return Err("Docker engine is not running (start Docker Desktop)".into());
    }
    let all = match docker_api::call("GET", "/containers/json?all=1", None, 15).await? {
        (200, Value::Array(a)) => a,
        (s, v) => return Err(docker_api::api_error(s, &v)),
    };
    Ok(candidates(&all, &sup.configs()))
}

/// Register unmanaged candidates: those named in `names`, or every one when `names` is None.
/// Returns (imported service names, skipped reasons).
async fn register(sup: &Arc<Sup>, cands: &[Candidate], names: Option<&[String]>) -> (Vec<String>, Vec<String>) {
    let mut taken: HashSet<String> = sup.configs().into_iter().map(|c| c.name).collect();
    let (mut done, mut skipped) = (vec![], vec![]);
    for n in names.into_iter().flatten() {
        if !cands.iter().any(|c| &c.name == n) {
            skipped.push(format!("{n}: no such container or compose project"));
        }
    }
    for c in cands.iter().filter(|c| names.is_none_or(|n| n.contains(&c.name))) {
        if c.managed {
            if names.is_some() {
                skipped.push(format!("{}: already managed", c.name));
            }
            continue;
        }
        if c.kind == "compose" && c.compose_file.is_none() {
            skipped.push(format!("{}: compose project has no config file label (started by another tool?)", c.name));
            continue;
        }
        let cfg = match config::validate(config_for(c, unique_name(&c.name, &taken))) {
            Ok(cfg) => cfg,
            Err(e) => {
                skipped.push(e);
                continue;
            }
        };
        let svc = cfg.name.clone();
        match sup.create(cfg, false).await {
            Ok(_) => {
                taken.insert(svc.clone());
                if c.running() {
                    sup.attach(&svc).await;
                }
                done.push(svc);
            }
            Err(e) => skipped.push(format!("{}: {}", c.name, e.msg)),
        }
    }
    (done, skipped)
}

/// Import the named containers / compose projects.
pub async fn import(sup: &Arc<Sup>, names: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let cands = list(sup).await?;
    Ok(register(sup, &cands, Some(names)).await)
}

/// Discovered services whose container (or compose project) is gone from `docker ps -a`.
pub fn gone(cands: &[Candidate], services: &[ServiceConfig]) -> Vec<String> {
    services
        .iter()
        .filter(|s| s.discovered == Some(true))
        .filter(|s| match s.kind.as_str() {
            "docker" => !cands.iter().any(|c| c.kind == "docker" && c.name == s.container_name()),
            "compose" => !s.project.as_deref().is_some_and(|p| cands.iter().any(|c| c.kind == "compose" && c.name == p)),
            _ => false,
        })
        .map(|s| s.name.clone())
        .collect()
}

pub struct SyncResult {
    pub imported: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
}

/// Make LStack match Docker: register new containers and drop discovered services whose container is
/// gone. A service must be missing in more than `grace` consecutive passes before it is dropped
/// (`missing` carries the counts), so a container that is being recreated isn't dropped and re-added.
pub async fn reconcile(sup: &Arc<Sup>, missing: &mut HashMap<String, u32>, grace: u32) -> Result<SyncResult, String> {
    let cands = list(sup).await?;
    let gone_now = gone(&cands, &sup.configs());
    missing.retain(|n, _| gone_now.contains(n));
    let mut removed = vec![];
    for n in gone_now {
        let seen = missing.entry(n.clone()).or_insert(0);
        *seen += 1;
        if *seen > grace && sup.destroy(&n, true).await.is_ok() {
            missing.remove(&n);
            removed.push(n);
        }
    }
    let cands = candidates_after(sup, cands);
    let (imported, skipped) = register(sup, &cands, None).await;
    Ok(SyncResult { imported, removed, skipped })
}

/// Re-flag `managed` after services were removed, so a just-dropped name can be registered again.
fn candidates_after(sup: &Arc<Sup>, mut cands: Vec<Candidate>) -> Vec<Candidate> {
    let services = sup.configs();
    for c in cands.iter_mut() {
        c.managed = services.iter().any(|s| match (c.kind, s.kind.as_str()) {
            ("docker", "docker") => s.container_name() == c.name,
            ("compose", "compose") => s.project.as_deref() == Some(c.name.as_str()),
            _ => false,
        });
    }
    cands
}

/// Background loop: while `dockerSync` is on in settings.json, reconcile every few seconds.
pub fn spawn_sync(sup: Arc<Sup>) {
    tokio::spawn(async move {
        let mut missing = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if !super::terminal::load().docker_sync {
                missing.clear();
                continue;
            }
            match reconcile(&sup, &mut missing, 1).await {
                Ok(r) => {
                    for n in &r.imported {
                        super::log(&format!("docker sync: registered {n}"));
                    }
                    for n in &r.removed {
                        super::log(&format!("docker sync: removed {n} (container is gone)"));
                    }
                }
                Err(_) => missing.clear(), // engine unreachable: never drop anything
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, state: &str, labels: Value) -> Value {
        json!({"Names": [format!("/{name}")], "Image": "img:1", "State": state, "Status": "Up", "Labels": labels,
               "Ports": [{"PublicPort": 8080, "PrivatePort": 80, "Type": "tcp"}]})
    }

    #[test]
    fn groups_compose_and_flags_managed() {
        let rows = vec![
            row("db", "running", json!({})),
            row("old", "exited", json!({})),
            row("app-web-1", "running", json!({"com.docker.compose.project": "app", "com.docker.compose.project.config_files": "/a/dc.yml"})),
            row("app-db-1", "exited", json!({"com.docker.compose.project": "app", "com.docker.compose.project.config_files": "/a/dc.yml"})),
        ];
        let managed = ServiceConfig { name: "x".into(), kind: "docker".into(), container: Some("old".into()), ..Default::default() };
        let c = candidates(&rows, &[managed]);
        assert_eq!(c.iter().map(|c| (c.name.as_str(), c.kind, c.managed)).collect::<Vec<_>>(), vec![("app", "compose", false), ("db", "docker", false), ("old", "docker", true)]);
        assert_eq!(c[0].containers, 2);
        assert!(c[0].running());
        assert_eq!(c[0].compose_file.as_deref(), Some("/a/dc.yml"));
        assert_eq!(c[1].ports, vec!["8080:80"]);
    }

    #[test]
    fn finds_gone_discovered_services() {
        let rows = vec![row("db", "running", json!({})), row("w", "running", json!({"com.docker.compose.project": "app"}))];
        let c = candidates(&rows, &[]);
        let mk = |name: &str, kind: &str, container: Option<&str>, project: Option<&str>, disc: bool| ServiceConfig {
            name: name.into(),
            kind: kind.into(),
            container: container.map(Into::into),
            project: project.map(Into::into),
            discovered: disc.then_some(true),
            ..Default::default()
        };
        let svcs = vec![
            mk("db", "docker", Some("db"), None, true),
            mk("old", "docker", Some("old"), None, true),
            mk("hand", "docker", Some("hand"), None, false),
            mk("app", "compose", None, Some("app"), true),
            mk("dead", "compose", None, Some("dead"), true),
        ];
        assert_eq!(gone(&c, &svcs), vec!["old", "dead"]);
    }

    #[test]
    fn names_are_valid_and_unique() {
        let taken: HashSet<String> = ["db".to_string()].into();
        assert_eq!(unique_name("db", &taken), "db-2");
        assert_eq!(unique_name("my app/x", &taken), "my-app-x");
        assert!(config::valid_name(&unique_name("_weird", &taken)));
    }
}

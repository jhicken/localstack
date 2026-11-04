//! OrbStack's Linux machines, listed as child services of the OrbStack service (next to its containers)
//! and kept current with `orbctl list`. A machine is a persistent VM, so stopped ones stay listed; they
//! go away when the machine is deleted.
use super::supervisor::Sup;
use crate::types::ServiceConfig;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub name: String,
    /// "ubuntu noble" and the like.
    pub image: String,
    pub running: bool,
}

pub struct Child {
    pub name: String,
    pub running: bool,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Add(Machine),
    Refresh(String, bool),
    Drop(String),
}

/// Where the OrbStack CLI is (the daemon's PATH is bare under launchd).
fn orbctl_bin() -> String {
    if let Ok(p) = std::env::var("LSTACK_ORBCTL") {
        return p;
    }
    let home = crate::paths::user_home();
    [home.join(".orbstack/bin/orbctl"), "/usr/local/bin/orbctl".into(), "/opt/homebrew/bin/orbctl".into()]
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "orbctl".into())
}

async fn orb(args: &[&str], secs: u64) -> Result<String, String> {
    let bin = orbctl_bin();
    let run = tokio::process::Command::new(&bin).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(secs), run).await.map_err(|_| "orbctl timed out".to_string())?.map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() { format!("orbctl exited with {}", out.status.code().unwrap_or(1)) } else { err })
}

/// `orbctl list -f json`: each entry is `{"record": {"name", "state", "image": {"distro", "version"}}, …}`.
pub fn parse(text: &str) -> Vec<Machine> {
    let Ok(Value::Array(all)) = serde_json::from_str::<Value>(text) else { return vec![] };
    all.iter()
        .filter_map(|m| {
            let r = m.get("record").unwrap_or(m);
            let image = ["distro", "version"].iter().filter_map(|k| r.pointer(&format!("/image/{k}")).and_then(Value::as_str)).collect::<Vec<_>>().join(" ");
            Some(Machine { name: r["name"].as_str()?.to_string(), image, running: r["state"].as_str() == Some("running") })
        })
        .collect()
}

pub fn child_name(parent: &str, machine: &str) -> String {
    let mut n = format!("{parent}.vm.{machine}");
    n.truncate(63);
    n
}

/// The machine a child service stands for.
pub fn machine_of(parent: &str, child: &str) -> Option<String> {
    child.strip_prefix(&format!("{parent}.vm.")).map(String::from)
}

pub fn plan(parent: &str, machines: &[Machine], children: &[Child], taken: &HashSet<String>) -> Vec<Action> {
    let mut actions = vec![];
    let mut keep = HashSet::new();
    for m in machines {
        let name = child_name(parent, &m.name);
        if !crate::config::valid_name(&name) || taken.contains(&name) {
            continue;
        }
        keep.insert(name.clone());
        match children.iter().find(|c| c.name == name) {
            Some(c) if c.running != m.running => actions.push(Action::Refresh(name, m.running)),
            Some(_) => {}
            None => actions.push(Action::Add(m.clone())),
        }
    }
    for c in children {
        if !keep.contains(&c.name) {
            actions.push(Action::Drop(c.name.clone()));
        }
    }
    actions
}

/// `Some(machines)` while OrbStack is running, None while it isn't (its machines are all off then).
async fn list() -> Option<Vec<Machine>> {
    // `orbctl list` could start OrbStack itself; only ask once it says it's running.
    if orb(&["status"], 10).await.ok()?.trim() != "Running" {
        return None;
    }
    Some(parse(&orb(&["list", "-f", "json"], 15).await.ok()?))
}

impl Sup {
    pub(crate) async fn sync_machines(self: &Arc<Self>) {
        let Some(snap) = self.machine_snapshot() else { return };
        let Some(parent) = snap.parent else {
            for c in &snap.children {
                self.drop_child(&c.name);
            }
            return;
        };
        let Some(machines) = list().await else {
            for c in snap.children.iter().filter(|c| c.running) {
                self.machine_apply(&c.name, false);
            }
            return;
        };
        for action in plan(&parent, &machines, &snap.children, &snap.taken) {
            match action {
                Action::Add(m) => {
                    let cfg = ServiceConfig {
                        name: child_name(&parent, &m.name),
                        kind: "machine".into(),
                        description: (!m.image.is_empty()).then(|| m.image.clone()),
                        restart: Some("never".into()),
                        ..Default::default()
                    };
                    let name = cfg.name.clone();
                    if self.add_child(cfg, &parent).is_some() {
                        self.with(&name, None, |m2| m2.system(format!("[lstack] picked up OrbStack machine {}", m.name)));
                        self.machine_apply(&name, m.running);
                    }
                }
                Action::Refresh(name, running) => self.machine_apply(&name, running),
                Action::Drop(name) => {
                    self.drop_child(&name);
                }
            }
        }
    }

    /// Record the state OrbStack reports, unless a start or stop of our own is under way.
    fn machine_apply(&self, name: &str, running: bool) {
        self.with(name, None, |m| {
            if matches!(m.rt.status, "starting" | "stopping") {
                return;
            }
            if running {
                m.rt.started_at.get_or_insert_with(crate::paths::now_ms);
                m.want_running = true;
            } else {
                m.rt.started_at = None;
            }
            self.set_status(m, if running { "running" } else { "stopped" });
        });
    }

    pub(crate) async fn machine_start(self: &Arc<Self>, name: &str, gen: u64) -> Result<(), String> {
        let Some(machine) = self.machine_target(name, Some(gen), true) else { return Err("service was replaced".into()) };
        self.system_log_pub(name, &format!("$ orbctl start {machine}"));
        match orb(&["start", &machine], 180).await {
            Ok(_) => {
                self.with(name, Some(gen), |m| {
                    m.rt.started_at = Some(crate::paths::now_ms());
                    self.set_status(m, "running");
                });
                Ok(())
            }
            Err(e) => self.fail(name, gen, &e),
        }
    }

    pub(crate) async fn machine_stop(self: &Arc<Self>, name: &str) {
        let Some(machine) = self.machine_target(name, None, false) else { return };
        self.system_log_pub(name, &format!("$ orbctl stop {machine}"));
        let res = orb(&["stop", &machine], 120).await;
        self.with(name, None, |m| {
            if let Err(e) = &res {
                m.rt.last_error = Some(e.clone());
                m.system(e.clone());
            }
            m.rt.started_at = None;
            self.set_status(m, "stopped");
        });
    }

    /// The machine behind a child service; marks it as starting (or stopping).
    fn machine_target(&self, name: &str, gen: Option<u64>, starting: bool) -> Option<String> {
        self.with(name, gen, |m| {
            let machine = machine_of(m.parent.as_deref()?, &m.cfg.name)?;
            m.rt.last_error = None;
            self.set_status(m, if starting { "starting" } else { "stopping" });
            Some(machine)
        })
        .flatten()
    }

    fn system_log_pub(&self, name: &str, msg: &str) {
        self.with(name, None, |m| m.system(msg));
    }

    fn machine_snapshot(&self) -> Option<Snapshot> {
        let on = super::terminal::load().docker_containers;
        let g = self.lock();
        let parent = g.services.values().find(|m| on && m.parent.is_none() && is_orbstack(&m.cfg)).map(|m| m.cfg.name.clone());
        let mut snap = Snapshot { parent, children: vec![], taken: HashSet::new() };
        for m in g.services.values() {
            if m.parent.is_none() {
                snap.taken.insert(m.cfg.name.clone());
            } else if m.cfg.kind == "machine" {
                snap.children.push(Child { name: m.cfg.name.clone(), running: m.rt.status == "running" });
            }
        }
        Some(snap)
    }
}

struct Snapshot {
    parent: Option<String>,
    children: Vec<Child>,
    taken: HashSet<String>,
}

/// The OrbStack app itself, added as a script.
pub fn is_orbstack(c: &ServiceConfig) -> bool {
    c.kind == "process" && (c.name.to_lowercase().contains("orbstack") || c.command.as_deref().unwrap_or("").to_lowercase().contains("orbstack"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, running: bool) -> Machine {
        Machine { name: name.into(), image: "ubuntu noble".into(), running }
    }

    #[test]
    fn parses_orbctl_json() {
        let text = r#"[{"record":{"id":"01H","name":"ubuntu","image":{"distro":"ubuntu","version":"noble","arch":"arm64"},"state":"running"},"ip4":"198.19.249.2"},
                      {"record":{"name":"dev","image":{"distro":"debian"},"state":"stopped"}}, {"name":"flat","state":"running"}]"#;
        assert_eq!(parse(text), vec![machine("ubuntu", true), Machine { name: "dev".into(), image: "debian".into(), running: false }, Machine { name: "flat".into(), image: String::new(), running: true }]);
        assert!(parse("not json").is_empty());
    }

    #[test]
    fn follows_orbstack() {
        let children = vec![Child { name: "orb.vm.a".into(), running: true }, Child { name: "orb.vm.gone".into(), running: false }, Child { name: "orb.vm.b".into(), running: false }];
        let machines = vec![machine("a", true), machine("b", true), machine("new", false)];
        assert_eq!(
            plan("orb", &machines, &children, &HashSet::new()),
            vec![Action::Refresh("orb.vm.b".into(), true), Action::Add(machine("new", false)), Action::Drop("orb.vm.gone".into())]
        );
        assert_eq!(machine_of("orb", "orb.vm.a.b").as_deref(), Some("a.b"));
        assert_eq!(machine_of("orb", "orb.web"), None);
    }
}

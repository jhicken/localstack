//! Lists the Docker engine's containers as child services of the service that runs the engine
//! (Docker Desktop, OrbStack…), and keeps that list current as containers start, stop and are removed.
use super::docker_api;
use super::supervisor::Sup;
use crate::types::ServiceConfig;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task::JoinSet;

#[derive(Default)]
pub struct Snapshot {
    /// The service the containers are listed under; None = nothing asked for them.
    pub parent: Option<String>,
    /// Containers already discovered.
    pub children: Vec<Child>,
    /// Names of the services that aren't discovered containers.
    pub taken: HashSet<String>,
    /// Containers other services already manage.
    pub owned_names: HashSet<String>,
    pub owned_ids: HashSet<String>,
    /// Compose projects (by name, or by compose file) that compose services already manage.
    pub owned_projects: HashSet<String>,
    pub owned_files: HashSet<String>,
}

pub struct Child {
    pub name: String,
    pub running: bool,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub container: String,
    pub image: String,
    pub running: bool,
    pub project: Option<String>,
    pub config_files: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Add(Row),
    /// Re-read the container's state (it started, stopped or was recreated).
    Refresh(String, bool),
    Drop(String),
}

pub fn rows(all: &[Value]) -> Vec<Row> {
    all.iter()
        .filter_map(|c| {
            // `Names` also lists link aliases ("/web/db"); the container's own name has a single slash.
            let container = c["Names"].as_array()?.iter().filter_map(|n| n.as_str()).find(|n| n.matches('/').count() == 1)?.trim_start_matches('/').to_string();
            Some(Row {
                id: c["Id"].as_str()?.to_string(),
                container,
                image: c["Image"].as_str().unwrap_or_default().to_string(),
                running: matches!(c["State"].as_str(), Some("running" | "restarting")),
                project: c.pointer("/Labels/com.docker.compose.project").and_then(Value::as_str).map(String::from),
                config_files: c
                    .pointer("/Labels/com.docker.compose.project.config_files")
                    .and_then(Value::as_str)
                    .map(|f| f.split(',').map(|x| x.trim().to_string()).collect())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub fn child_name(parent: &str, container: &str) -> String {
    let mut n = format!("{parent}.{container}");
    n.truncate(63);
    n
}

/// What to do so the discovered services match the engine. Only running containers are picked up;
/// ones that stop stay listed (stopped) until they are removed.
pub fn plan(parent: &str, rows: &[Row], snap: &Snapshot) -> Vec<Action> {
    let mut actions = vec![];
    let mut keep = HashSet::new();
    for r in rows {
        let name = child_name(parent, &r.container);
        let compose_owned = r.project.as_ref().is_some_and(|p| snap.owned_projects.contains(p)) || r.config_files.iter().any(|f| snap.owned_files.contains(f));
        if compose_owned || snap.owned_names.contains(&r.container) || snap.owned_ids.contains(&r.id) || snap.taken.contains(&name) || !crate::config::valid_name(&name) {
            continue;
        }
        match snap.children.iter().find(|c| c.name == name) {
            Some(c) => {
                keep.insert(name.clone());
                if r.running != c.running || (r.running && c.ids != [r.id.clone()]) {
                    actions.push(Action::Refresh(name, r.running));
                }
            }
            None if r.running => {
                keep.insert(name);
                actions.push(Action::Add(r.clone()));
            }
            None => {}
        }
    }
    for c in &snap.children {
        if !keep.contains(&c.name) {
            actions.push(Action::Drop(c.name.clone()));
        }
    }
    actions
}

impl Sup {
    pub(crate) async fn sync_containers(self: &Arc<Self>) {
        let Some(snap) = self.container_snapshot() else { return };
        let Some(parent) = snap.parent.clone() else {
            for c in &snap.children {
                self.drop_child(&c.name);
            }
            return;
        };
        // Engine down: the children's own refresh marks them stopped.
        if docker_api::socket_path().is_none() {
            return;
        }
        let Ok(all) = docker_api::all_containers().await else { return };
        let rows = rows(&all);
        let mut set = JoinSet::new();
        for action in plan(&parent, &rows, &snap) {
            let sup = self.clone();
            let parent = parent.clone();
            set.spawn(async move {
                match action {
                    Action::Add(r) => {
                        let cfg = ServiceConfig {
                            name: child_name(&parent, &r.container),
                            kind: "docker".into(),
                            description: (!r.image.is_empty()).then(|| r.image.clone()),
                            restart: Some("never".into()),
                            container: Some(r.container.clone()),
                            ..Default::default()
                        };
                        let name = cfg.name.clone();
                        if let Some(gen) = sup.add_child(cfg, &parent) {
                            if sup.docker_adopt(&name, gen).await {
                                sup.mark_wanted(&name, gen);
                                sup.with(&name, Some(gen), |m| m.system(format!("[lstack] picked up running container {} ({})", r.container, r.image)));
                            }
                        }
                    }
                    Action::Refresh(name, running) => {
                        if let Some(gen) = sup.with(&name, None, |m| m.gen) {
                            if running && sup.docker_adopt(&name, gen).await {
                                sup.mark_wanted(&name, gen);
                            } else if !running {
                                sup.docker_refresh(&name, gen).await;
                            }
                        }
                    }
                    Action::Drop(name) => {
                        sup.drop_child(&name);
                    }
                }
            });
        }
        while set.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, running: bool) -> Row {
        Row { id: id.into(), container: name.into(), image: "img".into(), running, project: None, config_files: vec![] }
    }
    fn child(name: &str, running: bool, id: &str) -> Child {
        Child { name: name.into(), running, ids: if running { vec![id.into()] } else { vec![] } }
    }

    #[test]
    fn parses_rows() {
        let all = vec![
            serde_json::json!({"Id": "a1", "Names": ["/web/db", "/web"], "Image": "nginx", "State": "running"}),
            serde_json::json!({"Id": "b2", "Names": ["/job"], "Image": "alpine", "State": "exited"}),
        ];
        let r = rows(&all);
        assert_eq!((r[0].container.as_str(), r[0].running), ("web", true));
        assert_eq!((r[1].container.as_str(), r[1].running), ("job", false));
    }

    #[test]
    fn follows_the_engine() {
        let snap = Snapshot {
            children: vec![child("docker.up", true, "1"), child("docker.gone", true, "2"), child("docker.was", true, "3"), child("docker.idle", false, "")],
            ..Default::default()
        };
        let rows = vec![
            row("1", "up", true),
            row("3", "was", false),
            row("4", "idle", true),
            row("5", "fresh", true),
            row("6", "never-started", false),
        ];
        assert_eq!(
            plan("docker", &rows, &snap),
            vec![Action::Refresh("docker.was".into(), false), Action::Refresh("docker.idle".into(), true), Action::Add(rows[3].clone()), Action::Drop("docker.gone".into())]
        );
    }

    #[test]
    fn recreated_container_is_refreshed() {
        let snap = Snapshot { children: vec![child("docker.web", true, "old")], ..Default::default() };
        assert_eq!(plan("docker", &[row("new", "web", true)], &snap), vec![Action::Refresh("docker.web".into(), true)]);
    }

    #[test]
    fn skips_containers_of_managed_compose_projects() {
        let mut snap = Snapshot::default();
        snap.owned_projects.insert("app".into());
        snap.owned_files.insert("/b/compose.yml".into());
        let in_project = Row { project: Some("app".into()), ..row("1", "app-web-1", true) };
        let by_file = Row { config_files: vec!["/x/o.yml".into(), "/b/compose.yml".into()], ..row("2", "b-db-1", true) };
        let other = Row { project: Some("other".into()), ..row("3", "other-web-1", true) };
        assert_eq!(plan("docker", &[in_project, by_file, other.clone()], &snap), vec![Action::Add(other)]);
    }

    #[test]
    fn skips_containers_other_services_manage() {
        let mut snap = Snapshot { children: vec![child("docker.db", true, "1")], ..Default::default() };
        snap.owned_names.insert("db".into());
        snap.owned_ids.insert("2".into());
        snap.taken.insert("docker.cache".into());
        let rows = vec![row("1", "db", true), row("2", "stack-web-1", true), row("3", "cache", true)];
        assert_eq!(plan("docker", &rows, &snap), vec![Action::Drop("docker.db".into())]);
    }
}

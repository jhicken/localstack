use crate::paths;
use crate::types::ServiceConfig;
use std::path::PathBuf;

const TYPES: [&str; 4] = ["process", "docker", "compose", "k8s"];

pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

pub fn validate(mut c: ServiceConfig) -> Result<ServiceConfig, String> {
    if !valid_name(&c.name) {
        return Err(format!("invalid service name \"{}\" (letters, digits, _ . -)", c.name));
    }
    if c.kind.is_empty() {
        c.kind = "process".into();
    }
    if !TYPES.contains(&c.kind.as_str()) {
        return Err(format!("{}: unknown type \"{}\"", c.name, c.kind));
    }
    if let Some(r) = &c.restart {
        if !["always", "on-failure", "never"].contains(&r.as_str()) {
            return Err(format!("{}: restart must be always | on-failure | never", c.name));
        }
    }
    let ok = match c.kind.as_str() {
        "process" => c.command.as_deref().is_some_and(|s| !s.is_empty()),
        "docker" => c.container.is_some() || c.image.is_some(),
        "compose" => c.compose_file.as_deref().is_some_and(|s| !s.is_empty()),
        _ => c.manifest.is_some() || c.selector.is_some(),
    };
    if !ok {
        let need = match c.kind.as_str() {
            "process" => "process services require \"command\"",
            "docker" => "docker services require \"container\" or \"image\"",
            "compose" => "compose services require \"composeFile\"",
            _ => "k8s services require \"manifest\" or \"selector\"",
        };
        return Err(format!("{}: {need}", c.name));
    }
    if let Some(h) = &c.healthcheck {
        match h.kind.as_str() {
            "http" if h.url.is_none() => return Err(format!("{}: http healthcheck requires url", c.name)),
            "tcp" if h.port.is_none() => return Err(format!("{}: tcp healthcheck requires port", c.name)),
            "http" | "tcp" => {}
            _ => return Err(format!("{}: healthcheck type must be http | tcp", c.name)),
        }
    }
    for (env, secret) in c.secret_refs() {
        if !crate::daemon::secrets::valid_name(&env) || !crate::daemon::secrets::valid_name(&secret) {
            return Err(format!("{}: invalid secret reference {env:?} → {secret:?} (letters, digits and _)", c.name));
        }
    }
    for u in c.urls.iter().flatten() {
        if !(u.url().starts_with("http://") || u.url().starts_with("https://")) {
            return Err(format!("{}: url {:?} must start with http:// or https://", c.name, u.url()));
        }
    }
    if let Some(h) = &c.heartbeat {
        if let Some(u) = &h.url {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err(format!("{}: heartbeat url must start with http:// or https://", c.name));
            }
        }
        if h.interval.is_some_and(|i| !(5.0..=86_400.0).contains(&i)) {
            return Err(format!("{}: heartbeat interval must be between 5 and 86400 seconds", c.name));
        }
    }
    Ok(c)
}

pub fn parse_yaml(text: &str) -> Result<ServiceConfig, String> {
    let c: ServiceConfig = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    validate(c)
}

pub fn service_file(name: &str) -> PathBuf {
    paths::services_dir().join(format!("{name}.yaml"))
}

pub fn load_services() -> (Vec<ServiceConfig>, Vec<String>) {
    paths::ensure_dirs();
    let mut files: Vec<String> = std::fs::read_dir(paths::services_dir())
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    files.sort();
    let mut services: Vec<ServiceConfig> = Vec::new();
    let mut errors = Vec::new();
    for f in files {
        if !(f.ends_with(".yaml") || f.ends_with(".yml")) {
            continue;
        }
        let res = std::fs::read_to_string(paths::services_dir().join(&f))
            .map_err(|e| e.to_string())
            .and_then(|t| parse_yaml(&t))
            .and_then(|s| {
                if services.iter().any(|x| x.name == s.name) {
                    Err(format!("duplicate service name \"{}\"", s.name))
                } else {
                    Ok(s)
                }
            });
        match res {
            Ok(s) => services.push(s),
            Err(e) => errors.push(format!("{f}: {e}")),
        }
    }
    (services, errors)
}

pub fn to_yaml(cfg: &ServiceConfig) -> String {
    serde_yaml::to_string(cfg).unwrap_or_default()
}

pub fn save_service(cfg: &ServiceConfig) -> Result<(), String> {
    paths::ensure_dirs();
    let cfg = validate(cfg.clone())?;
    // Keep a hand-written .yml file in place rather than creating a duplicate .yaml.
    let yml = paths::services_dir().join(format!("{}.yml", cfg.name));
    let file = if yml.exists() { yml } else { service_file(&cfg.name) };
    std::fs::write(file, to_yaml(&cfg)).map_err(|e| e.to_string())
}

pub fn delete_service(name: &str) -> bool {
    for ext in ["yaml", "yml"] {
        let f = paths::services_dir().join(format!("{name}.{ext}"));
        if f.exists() && std::fs::remove_file(f).is_ok() {
            return true;
        }
    }
    false
}

/// Shallow JSON merge-patch (`null` removes a key), then validate.
pub fn merge_patch(cfg: &ServiceConfig, patch: &serde_json::Value) -> Result<ServiceConfig, String> {
    let mut base = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    let obj = base.as_object_mut().unwrap();
    let p = patch.as_object().ok_or("patch must be a JSON object")?;
    for (k, v) in p {
        if v.is_null() {
            obj.remove(k);
        } else {
            obj.insert(k.clone(), v.clone());
        }
    }
    obj.insert("name".into(), serde_json::Value::String(cfg.name.clone()));
    let c: ServiceConfig = serde_json::from_value(base).map_err(|e| e.to_string())?;
    validate(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("api"));
        assert!(valid_name("my-svc.v2_x"));
        assert!(!valid_name("-bad"));
        assert!(!valid_name("has space"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(64)));
    }

    #[test]
    fn validation() {
        assert!(parse_yaml("name: a\ncommand: echo hi\n").is_ok());
        assert_eq!(parse_yaml("name: a\n").unwrap_err(), "a: process services require \"command\"");
        assert!(parse_yaml("name: a\ntype: docker\n").unwrap_err().contains("container"));
        assert!(parse_yaml("name: a\ntype: compose\n").unwrap_err().contains("composeFile"));
        assert!(parse_yaml("name: a\ntype: nope\ncommand: x\n").unwrap_err().contains("unknown type"));
        assert!(parse_yaml("name: a\ncommand: x\nrestart: sometimes\n").is_err());
        assert!(parse_yaml("name: a\ncommand: x\nhealthcheck: {type: tcp}\n").is_err());
    }

    #[test]
    fn new_fields() {
        let c = parse_yaml("name: a\ncommand: x\nsecrets: [GH_TOKEN]\nurls: [http://localhost:3000, {url: 'http://localhost:9229', label: debugger}]\nheartbeat: {slug: a-svc, interval: 30}\n").unwrap();
        assert_eq!(c.secret_refs(), vec![("GH_TOKEN".to_string(), "GH_TOKEN".to_string())]);
        assert_eq!(c.urls.as_ref().unwrap()[1].label(), Some("debugger"));
        assert_eq!(parse_yaml(&to_yaml(&c)).unwrap(), c);
        let m = parse_yaml("name: a\ncommand: x\nsecrets: {DB_PASSWORD: PROD_DB}\n").unwrap();
        assert_eq!(m.secret_refs(), vec![("DB_PASSWORD".to_string(), "PROD_DB".to_string())]);
        assert!(parse_yaml("name: a\ncommand: x\nsecrets: [bad-name]\n").is_err());
        assert!(parse_yaml("name: a\ncommand: x\nurls: [localhost:3000]\n").is_err());
        assert!(parse_yaml("name: a\ncommand: x\nheartbeat: {interval: 1}\n").is_err());
    }

    #[test]
    fn lenient_scalars() {
        let c = parse_yaml("name: a\ncommand: x\nenv: {PORT: 3000, DEBUG: true}\nports: [8080:80]\n").unwrap();
        assert_eq!(c.env.unwrap()["PORT"], "3000");
        assert_eq!(c.ports.unwrap()[0], "8080:80");
    }

    #[test]
    fn roundtrip_and_patch() {
        let c = parse_yaml("name: web\ncommand: npm start\nautostart: true\nmaxRestarts: 3\n").unwrap();
        let again = parse_yaml(&to_yaml(&c)).unwrap();
        assert_eq!(c, again);
        let p = merge_patch(&c, &serde_json::json!({"autostart": null, "cwd": "~/x", "name": "evil"})).unwrap();
        assert_eq!(p.name, "web");
        assert_eq!(p.autostart, None);
        assert_eq!(p.cwd.as_deref(), Some("~/x"));
    }
}

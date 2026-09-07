//! Shell integration: completion is read-only and never takes administrative locks.

use std::{fs::File, io::Read, path::Path};
use serde::Deserialize;
use crate::{registry, service};

pub const ZSH: &str = include_str!("../completions/zsh.zsh");
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct Goals {
    goals: Vec<registry::Goal>,
}

#[derive(Deserialize)]
struct Services {
    services: Vec<service::ServiceRecord>,
}

fn read_snapshot<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    // Missing, malformed, or oversized state must not break interactive completion.
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn describe_text(text: &str) -> String {
    // zsh _describe uses ':' as a separator. Never emit terminal controls or
    // extra candidates from state-file content; nothing here is shell-evaluated.
    let mut result = String::new();
    for ch in text.chars().take(512) {
        if ch.is_control() {
            result.push(' ');
        } else {
            if ch == ':' || ch == '\\' {
                result.push('\\');
            }
            result.push(ch);
        }
    }
    result
}

fn candidates(root: &Path) -> Vec<String> {
    let Some(mut goals) = read_snapshot::<Goals>(&root.join("goals.json")) else {
        return Vec::new();
    };
    let services = read_snapshot::<Services>(&root.join("services.json"));
    goals.goals.sort_by(|a, b| a.id.cmp(&b.id));
    goals.goals.dedup_by(|a, b| a.id == b.id);
    goals.goals.into_iter().filter_map(|goal| {
        if registry::validate_id(&goal.id).is_err() || !goal.config_path.is_absolute() {
            return None;
        }
        let enabled = if goal.enabled { "enabled" } else { "disabled" };
        let status = match &services {
            Some(services) if services.services.iter().any(|record| {
                record.config_path == goal.config_path
                    && service::process_is_alive(record.pid)
                    && read_snapshot::<service::ServiceRecord>(
                        &record.project_dir.join(".goal/service.json")
                    ).is_some_and(|local| local == *record)
            }) => "running (snapshot)",
            Some(_) => "stopped",
            None => "state unknown",
        };
        Some(format!("{}:{} / {} — {}", goal.id, enabled, status,
                     describe_text(&goal.config_path.to_string_lossy())))
    }).collect()
}

pub fn goal_ids() -> Vec<String> {
    service::registry_root().map(|root| candidates(&root)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn completion_does_not_create_missing_state() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(candidates(&missing).is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn snapshot_candidates_are_sorted_safe_and_include_disabled_goals() {
        let dir = tempfile::tempdir().unwrap();
        let data = serde_json::json!({"goals": [
            {"id":"z", "enabled":false, "config_path":"/tmp/path:with\\slash\n/goal.toml"},
            {"id":"a", "enabled":true, "config_path":"/tmp/a/goal.toml"},
            {"id":"bad:injection", "enabled":true, "config_path":"/tmp/b/goal.toml"}
        ]});
        fs::write(dir.path().join("goals.json"), data.to_string()).unwrap();
        fs::write(dir.path().join("services.json"), r#"{"services":[]}"#).unwrap();
        let result = candidates(dir.path());
        assert_eq!(result.len(), 2);
        assert!(result[0].starts_with("a:enabled / stopped"));
        assert!(result[1].starts_with("z:disabled / stopped"));
        assert!(result[1].contains(r"path\:with\\slash "));
        assert!(!result[1].contains('\n'));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn malformed_state_is_not_reported_as_healthy() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goals.json"), "{").unwrap();
        assert!(candidates(dir.path()).is_empty());
        fs::write(dir.path().join("goals.json"),
                  r#"{"goals":[{"id":"a","enabled":true,"config_path":"/tmp/a/goal.toml"}]}"#).unwrap();
        assert!(candidates(dir.path())[0].contains("state unknown"));
    }

    #[test]
    fn oversized_snapshot_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goals.json"), vec![b' '; MAX_SNAPSHOT_BYTES as usize + 1]).unwrap();
        assert!(candidates(dir.path()).is_empty());
    }
}

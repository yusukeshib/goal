use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::WorkerCompletion;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersistentState {
    pub latest_worker_completion: Option<WorkerCompletion>,
    pub latest_cycle_id: Option<String>,
    pub latest_cycle_timestamp: Option<u64>,
}

#[derive(Debug)]
pub struct ControllerLock {
    _file: fs::File,
}

impl ControllerLock {
    pub fn acquire(config_path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(config_path)
            .with_context(|| format!("open config lock {}", config_path.display()))?;
        if let Err(error) = file.try_lock_exclusive() {
            bail!(
                "another goal controller is already running for {}: {error}",
                config_path.display()
            );
        }
        Ok(Self { _file: file })
    }
}

pub struct StateStore {
    root: PathBuf,
    state_path: PathBuf,
    events_path: PathBuf,
}

impl StateStore {
    pub fn new(project_dir: &Path) -> Result<Self> {
        let root = project_dir.join(".goal");
        fs::create_dir_all(&root).context("create .goal directory")?;
        Ok(Self {
            state_path: root.join("state.json"),
            events_path: root.join("events.jsonl"),
            root,
        })
    }

    pub fn load(&self) -> Result<PersistentState> {
        match fs::read(&self.state_path) {
            Ok(bytes) => {
                let mut value: Value =
                    serde_json::from_slice(&bytes).context("parse .goal/state.json")?;
                if let Some(object) = value.as_object_mut() {
                    object.remove("pending_human_question");
                    object.remove("latest_human_answer");
                    let legacy_completion = object
                        .get("latest_worker_completion")
                        .and_then(Value::as_object)
                        .and_then(|completion| completion.get("type"))
                        .and_then(Value::as_str)
                        .is_some_and(|kind| matches!(kind, "needs_input" | "blocked"));
                    if legacy_completion {
                        object.insert("latest_worker_completion".to_owned(), Value::Null);
                    }
                }
                serde_json::from_value(value).context("parse .goal/state.json")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(PersistentState::default())
            }
            Err(error) => Err(error).context("read .goal/state.json"),
        }
    }

    pub fn save(&self, state: &PersistentState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        let temporary = self
            .root
            .join(format!("state.json.tmp-{}", std::process::id()));
        {
            let mut file = fs::File::create(&temporary).context("create temporary state")?;
            file.write_all(&bytes).context("write temporary state")?;
            file.sync_all().context("sync temporary state")?;
        }
        fs::rename(&temporary, &self.state_path).context("publish state atomically")
    }

    pub fn event(&self, kind: &str, details: Value) -> Result<()> {
        let event = serde_json::json!({
            "timestamp": unix_timestamp(),
            "type": kind,
            "details": details,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)
            .context("open events log")?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("goal.toml");
        fs::write(&config, "goal_file = 'GOAL.md'").unwrap();

        let first = ControllerLock::acquire(&config).unwrap();
        let error = ControllerLock::acquire(&config).unwrap_err();
        assert!(error.to_string().contains("already running"));
        drop(first);
        ControllerLock::acquire(&config).unwrap();
    }

    #[test]
    fn atomic_state_round_trip_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path()).unwrap();
        let state = PersistentState {
            latest_worker_completion: Some(WorkerCompletion::Done {
                summary: "verified".into(),
            }),
            latest_cycle_id: Some("cycle-1".into()),
            ..PersistentState::default()
        };
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        store
            .event("test", serde_json::json!({"ok": true}))
            .unwrap();
        let line = fs::read_to_string(dir.path().join(".goal/events.jsonl")).unwrap();
        assert_eq!(line.lines().count(), 1);
        serde_json::from_str::<Value>(line.trim()).unwrap();
    }

    #[test]
    fn load_discards_legacy_human_state_and_completion() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join(".goal")).unwrap();
        fs::write(
            dir.path().join(".goal/state.json"),
            r#"{
                "latest_worker_completion": {
                    "type":"needs_input",
                    "question":"Continue?",
                    "context":"Legacy context",
                    "resume_hint":null
                },
                "pending_human_question": {"question":"Continue?","context":null},
                "latest_human_answer": {"question":"Old?","context":null,"answer":"yes"},
                "latest_cycle_id": "cycle-old",
                "latest_cycle_timestamp": 1
            }"#,
        )
        .unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.latest_cycle_id.as_deref(), Some("cycle-old"));
        assert!(state.latest_worker_completion.is_none());
        store.save(&state).unwrap();
        let saved = fs::read_to_string(dir.path().join(".goal/state.json")).unwrap();
        assert!(!saved.contains("human"));
        assert!(!saved.contains("question"));
    }
}

use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{WorkerBatchCompletion, WorkerCompletion};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersistentState {
    pub latest_worker_completion: Option<WorkerCompletion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_worker_batch: Option<WorkerBatchCompletion>,
    pub latest_cycle_id: Option<String>,
    pub latest_cycle_timestamp: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ControllerOwner {
    pid: u32,
    config_path: PathBuf,
}

#[derive(Debug)]
pub struct ControllerLock {
    _file: fs::File,
}

impl ControllerLock {
    pub fn acquire(project_dir: &Path, config_path: &Path) -> Result<Self> {
        let state_dir = project_dir.join(".goal");
        fs::create_dir_all(&state_dir).context("create .goal directory for controller lock")?;
        let lock_path = state_dir.join("controller.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open controller lock {}", lock_path.display()))?;
        if let Err(error) = file.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                bail!(
                    "another goal controller is already running for {}: {error}",
                    project_dir.display()
                );
            }
            return Err(error)
                .with_context(|| format!("lock controller file {}", lock_path.display()));
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        serde_json::to_writer(
            &mut file,
            &ControllerOwner {
                pid: std::process::id(),
                config_path: config_path.to_owned(),
            },
        )?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(Self { _file: file })
    }

    pub fn owner_matches(project_dir: &Path, config_path: &Path, pid: u32) -> bool {
        let lock_path = project_dir.join(".goal/controller.lock");
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            return false;
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let mut bytes = Vec::new();
                file.seek(SeekFrom::Start(0)).is_ok()
                    && file.read_to_end(&mut bytes).is_ok()
                    && serde_json::from_slice::<ControllerOwner>(&bytes)
                        .is_ok_and(|owner| owner.pid == pid && owner.config_path == config_path)
            }
            Err(_) => false,
        }
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
        let first = ControllerLock::acquire(dir.path(), &config).unwrap();
        assert!(ControllerLock::owner_matches(
            dir.path(),
            &config,
            std::process::id()
        ));
        let error = ControllerLock::acquire(dir.path(), &config).unwrap_err();
        assert!(error.to_string().contains("already running"));
        drop(first);
        assert!(!ControllerLock::owner_matches(
            dir.path(),
            &config,
            std::process::id()
        ));
        ControllerLock::acquire(dir.path(), &config).unwrap();
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
    fn batch_state_round_trip_and_old_state_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path()).unwrap();
        let state = PersistentState {
            latest_worker_batch: Some(WorkerBatchCompletion {
                batch_id: "batch-1".into(),
                task_count: 2,
                results: vec![crate::model::WorkerTaskResult {
                    task_index: 1,
                    task: "second task".into(),
                    run_id: Some("run-2".into()),
                    completion: WorkerCompletion::Failure { reason: "unavailable".into() },
                }],
            }),
            ..PersistentState::default()
        };
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        let old: PersistentState = serde_json::from_str(
            r#"{"latest_worker_completion":{"type":"done","summary":"old"}}"#,
        ).unwrap();
        assert!(old.latest_worker_batch.is_none());
        assert!(!serde_json::to_string(&old).unwrap().contains("latest_worker_batch"));
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

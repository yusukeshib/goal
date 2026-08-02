use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::WorkerCompletion;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersistentState {
    pub latest_worker_completion: Option<WorkerCompletion>,
    pub pending_human_question: Option<HumanQuestion>,
    pub latest_human_answer: Option<HumanAnswer>,
    pub latest_cycle_id: Option<String>,
    pub latest_cycle_timestamp: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanQuestion {
    pub question: String,
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanAnswer {
    pub question: String,
    pub context: Option<String>,
    pub answer: String,
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
            Ok(bytes) => serde_json::from_slice(&bytes).context("parse .goal/state.json"),
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
    fn atomic_state_round_trip_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path()).unwrap();
        let state = PersistentState {
            pending_human_question: Some(HumanQuestion {
                question: "Deploy?".into(),
                context: Some("CI passed".into()),
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
}

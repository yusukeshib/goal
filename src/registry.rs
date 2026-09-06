use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{config::LoadedConfig, service};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub config_path: PathBuf,
    pub enabled: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    goals: Vec<Goal>,
}

/// Administrative operations hold this lock through runtime startup/shutdown.
/// Controllers only write services.json, never this durable registration file.
pub struct Registry {
    _lock: File,
    path: PathBuf,
    data: Data,
}

impl Registry {
    pub fn open() -> Result<Self> {
        let root = service::registry_root()?;
        fs::create_dir_all(&root)
            .with_context(|| format!("create goal state directory {}", root.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(root.join("goals.lock"))
            .context("open goal registration lock")?;
        lock.lock_exclusive().context("lock goal registrations")?;
        let path = root.join("goals.json");
        let data: Data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse goal registrations {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(error) => return Err(error).context("read goal registrations"),
        };
        for (index, goal) in data.goals.iter().enumerate() {
            validate_id(&goal.id)?;
            if !goal.config_path.is_absolute() || goal.config_path.parent().is_none() {
                bail!("invalid registered config path for {}", goal.id);
            }
            if data.goals[..index].iter().any(|other| {
                other.id == goal.id || other.config_path.parent() == goal.config_path.parent()
            }) {
                bail!("duplicate goal registration for {}", goal.id);
            }
        }
        Ok(Self {
            _lock: lock,
            path,
            data,
        })
    }

    pub fn goals(&self) -> Vec<Goal> {
        let mut goals = self.data.goals.clone();
        goals.sort_by(|left, right| left.id.cmp(&right.id));
        goals
    }

    pub fn get(&self, id: &str) -> Result<Goal> {
        self.data
            .goals
            .iter()
            .find(|goal| goal.id == id)
            .cloned()
            .with_context(|| format!("unknown goal ID {id:?}; use goal list or goal add <path>"))
    }

    pub fn add(&mut self, path: &Path, id: Option<&str>) -> Result<Goal> {
        let path = if path.is_dir() {
            path.join("goal.toml")
        } else {
            path.to_owned()
        };
        let loaded = LoadedConfig::load(&path)?;
        let derived = loaded.project_dir.file_name().and_then(|name| name.to_str());
        let id = id
            .or(derived)
            .context("cannot derive a goal ID from the directory; pass --id <id>")?;
        validate_id(id)?;
        if self.data.goals.iter().any(|goal| goal.id == id) {
            bail!("goal ID {id:?} is already registered; choose another --id");
        }
        if let Some(existing) = self
            .data
            .goals
            .iter()
            .find(|goal| goal.config_path.parent() == Some(loaded.project_dir.as_path()))
        {
            bail!("goal project is already registered as {}", existing.id);
        }
        let goal = Goal {
            id: id.to_owned(),
            config_path: loaded.config_path,
            enabled: true,
        };
        self.data.goals.push(goal.clone());
        self.save()?;
        Ok(goal)
    }

    pub fn remove(&mut self, id: &str) -> Result<Goal> {
        let goal = self.get(id)?;
        if service::service_for_config(&goal.config_path)?.is_some() {
            bail!("goal {id} is running; run goal down {id} before removing it");
        }
        self.data.goals.retain(|goal| goal.id != id);
        self.save()?;
        Ok(goal)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<Goal> {
        self.get(id)?;
        let goal = self
            .data
            .goals
            .iter_mut()
            .find(|goal| goal.id == id)
            .unwrap();
        goal.enabled = enabled;
        let goal = goal.clone();
        self.save()?;
        Ok(goal)
    }

    fn save(&self) -> Result<()> {
        service::write_json_atomic(&self.path, &self.data).context("save goal registrations")
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        bail!("invalid goal ID {id:?}; use letters, digits, '.', '_' or '-', starting with a letter or digit (override with --id)");
    }
    Ok(())
}

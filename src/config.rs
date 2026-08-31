use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub goal_file: PathBuf,
    pub interval_seconds: u64,
    #[serde(default = "default_max_wait")]
    pub max_wait_seconds: u64,
    #[serde(default)]
    pub worker_observation: WorkerObservation,
    #[serde(default)]
    pub max_completed_runs: Option<usize>,
    pub sensor: CommandConfig,
    pub decider: CommandConfig,
    pub worker: CommandConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerObservation {
    #[default]
    Full,
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    pub command: Vec<String>,
    pub timeout_seconds: u64,
}

fn default_max_wait() -> u64 {
    3600
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub project_dir: PathBuf,
    pub goal_path: PathBuf,
}

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let requested_path = if path.is_dir() {
            path.join("goal.toml")
        } else {
            path.to_owned()
        };
        let config_path = fs::canonicalize(&requested_path)
            .with_context(|| format!("resolve config {}", requested_path.display()))?;
        let text = fs::read_to_string(&config_path)
            .with_context(|| format!("read config {}", config_path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        config.validate()?;

        let parent = requested_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let project_dir = fs::canonicalize(parent)
            .with_context(|| format!("resolve project directory {}", parent.display()))?;
        let goal_path = if config.goal_file.is_absolute() {
            config.goal_file.clone()
        } else {
            project_dir.join(&config.goal_file)
        };
        let loaded = Self {
            config,
            project_dir,
            goal_path,
        };
        loaded.read_goal()?;
        Ok(loaded)
    }

    pub fn read_goal(&self) -> Result<String> {
        let goal = fs::read_to_string(&self.goal_path)
            .with_context(|| format!("read goal file {}", self.goal_path.display()))?;
        if goal.trim().is_empty() {
            bail!("goal file {} must not be empty", self.goal_path.display());
        }
        Ok(goal)
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.goal_file.as_os_str().is_empty() {
            bail!("goal_file must not be empty");
        }
        if self.max_wait_seconds == 0 {
            bail!("max_wait_seconds must be greater than zero");
        }
        if self.max_completed_runs == Some(0) {
            bail!("max_completed_runs must be greater than zero when set");
        }
        validate_command("sensor", &self.sensor)?;
        validate_command("decider", &self.decider)?;
        validate_command("worker", &self.worker)?;
        Ok(())
    }
}

fn validate_command(name: &str, command: &CommandConfig) -> Result<()> {
    if command.command.is_empty() || command.command.iter().any(|part| part.is_empty()) {
        bail!("{name}.command must contain non-empty argv elements");
    }
    if command.timeout_seconds == 0 {
        bail!("{name}.timeout_seconds must be greater than zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> &'static str {
        r#"goal_file = "GOAL.md"
interval_seconds = 0
max_wait_seconds = 10
[sensor]
command = ["sensor"]
timeout_seconds = 1
[decider]
command = ["decider", "{prompt}"]
timeout_seconds = 1
[worker]
command = ["worker"]
timeout_seconds = 1
"#
    }

    #[test]
    fn loads_file_or_directory_with_relative_goal_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goal.toml"), valid()).unwrap();
        fs::write(dir.path().join("GOAL.md"), "Ship the feature").unwrap();
        for path in [dir.path(), &dir.path().join("goal.toml")] {
            let loaded = LoadedConfig::load(path).unwrap();
            assert_eq!(loaded.read_goal().unwrap(), "Ship the feature");
            assert_eq!(loaded.project_dir, fs::canonicalize(dir.path()).unwrap());
            assert_eq!(loaded.config.worker_observation, WorkerObservation::Full);
            assert_eq!(loaded.config.max_completed_runs, None);
        }
    }

    #[test]
    fn loads_worker_observation_and_retention_options() {
        let dir = tempfile::tempdir().unwrap();
        let text = valid().replace(
            "max_wait_seconds = 10",
            "max_wait_seconds = 10\nworker_observation = \"none\"\nmax_completed_runs = 25",
        );
        fs::write(dir.path().join("goal.toml"), text).unwrap();
        fs::write(dir.path().join("GOAL.md"), "Ship the feature").unwrap();
        let loaded = LoadedConfig::load(dir.path()).unwrap();
        assert_eq!(loaded.config.worker_observation, WorkerObservation::None);
        assert_eq!(loaded.config.max_completed_runs, Some(25));
    }

    #[test]
    fn directory_without_goal_toml_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = LoadedConfig::load(dir.path()).unwrap_err();
        assert!(error.to_string().contains("goal.toml"));
    }

    #[test]
    fn rejects_bad_configuration_and_goal() {
        let invalid = [
            valid().replace("command = [\"sensor\"]", "command = []"),
            valid().replace("timeout_seconds = 1", "timeout_seconds = 0"),
            valid().replace("max_wait_seconds = 10", "max_wait_seconds = 0"),
            valid().replace("max_wait_seconds = 10", "max_wait_seconds = 10\nmax_completed_runs = 0"),
            valid().replace("max_wait_seconds = 10", "max_wait_seconds = 10\nworker_observation = \"selected\""),
        ];
        for (index, text) in invalid.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("goal.toml"), text).unwrap();
            fs::write(dir.path().join("GOAL.md"), "goal").unwrap();
            assert!(
                LoadedConfig::load(&dir.path().join("goal.toml")).is_err(),
                "case {index}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goal.toml"), valid()).unwrap();
        fs::write(dir.path().join("GOAL.md"), "  ").unwrap();
        assert!(LoadedConfig::load(&dir.path().join("goal.toml")).is_err());
    }
}

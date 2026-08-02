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
    pub retry_seconds: u64,
    #[serde(default = "default_max_wait")]
    pub max_wait_seconds: u64,
    pub sensor: CommandConfig,
    pub decider: CommandConfig,
    pub worker: CommandConfig,
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
    pub goal: String,
}

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        config.validate()?;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let project_dir = fs::canonicalize(parent)
            .with_context(|| format!("resolve project directory {}", parent.display()))?;
        let goal_path = if config.goal_file.is_absolute() {
            config.goal_file.clone()
        } else {
            project_dir.join(&config.goal_file)
        };
        let goal = fs::read_to_string(&goal_path)
            .with_context(|| format!("read goal file {}", goal_path.display()))?;
        if goal.trim().is_empty() {
            bail!("goal file {} must not be empty", goal_path.display());
        }
        Ok(Self {
            config,
            project_dir,
            goal,
        })
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
retry_seconds = 0
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
    fn loads_relative_goal_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goal.toml"), valid()).unwrap();
        fs::write(dir.path().join("GOAL.md"), "Ship the feature").unwrap();
        let loaded = LoadedConfig::load(&dir.path().join("goal.toml")).unwrap();
        assert_eq!(loaded.goal, "Ship the feature");
        assert_eq!(loaded.project_dir, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn rejects_bad_configuration_and_goal() {
        let invalid = [
            valid().replace("command = [\"sensor\"]", "command = []"),
            valid().replace("timeout_seconds = 1", "timeout_seconds = 0"),
            valid().replace("max_wait_seconds = 10", "max_wait_seconds = 0"),
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

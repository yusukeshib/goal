use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use wait_timeout::ChildExt;

use crate::{
    analytics::{METADATA_FILE, RunMetadata, RunOutcome},
    config::CommandConfig,
    output::Output,
};

#[derive(Debug)]
pub struct RunArtifacts {
    pub id: String,
    pub dir: PathBuf,
    pub prompt_path: PathBuf,
    pub result_path: PathBuf,
    role: String,
    started_at_ms: u64,
    metadata_path: PathBuf,
}

impl RunArtifacts {
    pub fn finish(
        &self,
        outcome: RunOutcome,
        failure_kind: Option<&str>,
        result_type: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        RunMetadata::finished(
            &self.id,
            &self.role,
            self.started_at_ms,
            outcome,
            failure_kind,
            result_type,
            reason,
        )
        .save(&self.metadata_path)
    }
}

#[derive(Debug)]
pub enum RunError {
    Infrastructure(anyhow::Error),
    NonZero(ExitStatus),
    Timeout,
    Cancelled,
    Protocol(anyhow::Error),
}

pub type RunResult<T> =
    std::result::Result<(T, RunArtifacts), (RunError, Option<Box<RunArtifacts>>)>;

impl RunError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Infrastructure(_) => "infrastructure",
            Self::NonZero(_) => "process",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Protocol(_) => "protocol",
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Infrastructure(error) => write!(f, "infrastructure failure: {error:#}"),
            Self::NonZero(status) => write!(f, "process exited unsuccessfully: {status}"),
            Self::Timeout => write!(f, "process timed out"),
            Self::Cancelled => write!(f, "process cancelled"),
            Self::Protocol(error) => write!(f, "protocol failure: {error:#}"),
        }
    }
}

pub struct Runner {
    project_dir: PathBuf,
    runs_dir: PathBuf,
    cancelled: Arc<AtomicBool>,
    output: Output,
}

impl Runner {
    pub fn new(project_dir: &Path, cancelled: Arc<AtomicBool>, output: Output) -> Result<Self> {
        let runs_dir = project_dir.join(".goal/runs");
        fs::create_dir_all(&runs_dir).context("create runs directory")?;
        Ok(Self {
            project_dir: project_dir.to_owned(),
            runs_dir,
            cancelled,
            output,
        })
    }

    pub fn run_json<T>(&self, role: &str, config: &CommandConfig, prompt: &str) -> RunResult<T>
    where
        T: DeserializeOwned,
    {
        let artifacts = match self.create_artifacts(role, prompt) {
            Ok(value) => value,
            Err(error) => return Err((RunError::Infrastructure(error), None)),
        };
        match self.run_child(role, config, &artifacts, Some(prompt)) {
            Ok(_) => {}
            Err(error) => return Err((error, Some(Box::new(artifacts)))),
        }
        let bytes = match fs::read(&artifacts.result_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err((
                    RunError::Protocol(
                        anyhow!(error).context(format!("read {}", artifacts.result_path.display())),
                    ),
                    Some(Box::new(artifacts)),
                ));
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok((value, artifacts)),
            Err(error) => Err((
                RunError::Protocol(anyhow!(error).context(format!(
                    "parse {} as one JSON object",
                    artifacts.result_path.display()
                ))),
                Some(Box::new(artifacts)),
            )),
        }
    }

    fn create_artifacts(&self, role: &str, prompt: &str) -> Result<RunArtifacts> {
        let started = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let nonce = started.as_nanos();
        let started_at_ms = started.as_millis() as u64;
        let id = format!("{nonce}-{role}");
        let dir = self.runs_dir.join(&id);
        let temporary_dir = self
            .runs_dir
            .join(format!(".{id}.tmp-{}", std::process::id()));
        fs::create_dir(&temporary_dir)
            .with_context(|| format!("create run directory {}", temporary_dir.display()))?;
        let setup = (|| -> Result<()> {
            fs::write(temporary_dir.join("prompt.md"), prompt).context("write prompt")?;
            File::create(temporary_dir.join("stdout.log")).context("create stdout log")?;
            File::create(temporary_dir.join("stderr.log")).context("create stderr log")?;
            RunMetadata::running(&id, role, started_at_ms)
                .save(&temporary_dir.join(METADATA_FILE))?;
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = fs::remove_dir_all(&temporary_dir);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary_dir, &dir) {
            let _ = fs::remove_dir_all(&temporary_dir);
            return Err(error).with_context(|| format!("publish run directory {}", dir.display()));
        }
        Ok(RunArtifacts {
            prompt_path: dir.join("prompt.md"),
            result_path: dir.join("result.json"),
            metadata_path: dir.join(METADATA_FILE),
            id,
            dir,
            role: role.to_owned(),
            started_at_ms,
        })
    }

    pub fn run_sensor(&self, config: &CommandConfig) -> RunResult<serde_json::Value> {
        let artifacts = match self.create_artifacts(
            "sensor",
            "Sensor invocation: stdout is the JSON observation.\n",
        ) {
            Ok(value) => value,
            Err(error) => return Err((RunError::Infrastructure(error), None)),
        };
        let (stdout, _) = match self.run_child("sensor", config, &artifacts, None) {
            Ok(output) => output,
            Err(error) => return Err((error, Some(Box::new(artifacts)))),
        };
        let value: serde_json::Value = match serde_json::from_slice(&stdout) {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    RunError::Protocol(
                        anyhow!(error).context("parse sensor stdout as exactly one JSON value"),
                    ),
                    Some(Box::new(artifacts)),
                ));
            }
        };
        if let Err(error) = atomic_write(&artifacts.result_path, &stdout) {
            return Err((RunError::Infrastructure(error), Some(Box::new(artifacts))));
        }
        Ok((value, artifacts))
    }

    fn run_child(
        &self,
        role: &str,
        config: &CommandConfig,
        artifacts: &RunArtifacts,
        prompt: Option<&str>,
    ) -> std::result::Result<(Vec<u8>, Vec<u8>), RunError> {
        let prompt_path = artifacts.prompt_path.to_string_lossy();
        let uses_placeholder =
            prompt.is_some() && config.command.iter().any(|arg| arg.contains("{prompt}"));
        let argv: Vec<String> = config
            .command
            .iter()
            .map(|arg| arg.replace("{prompt}", &prompt_path))
            .collect();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&self.project_dir)
            .env("GOAL_RUN_ID", &artifacts.id)
            .env("GOAL_PROMPT_PATH", &artifacts.prompt_path)
            .env("GOAL_RESULT_PATH", &artifacts.result_path)
            .env("GOAL_PROJECT_DIR", &self.project_dir)
            .stdin(if prompt.is_some() && !uses_placeholder {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            RunError::Infrastructure(anyhow!(error).context(format!("spawn {}", argv[0])))
        })?;

        if let Some(prompt) = prompt.filter(|_| !uses_placeholder) {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| RunError::Infrastructure(anyhow!("child stdin unavailable")))?;
            if let Err(error) = stdin.write_all(prompt.as_bytes()) {
                terminate(&mut child);
                return Err(RunError::Infrastructure(
                    anyhow!(error).context("write prompt to child stdin"),
                ));
            }
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunError::Infrastructure(anyhow!("child stdout unavailable")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RunError::Infrastructure(anyhow!("child stderr unavailable")))?;
        let stdout_log = artifacts.dir.join("stdout.log");
        let stderr_log = artifacts.dir.join("stderr.log");
        let stdout_output = self.output.clone();
        let stderr_output = self.output.clone();
        let stdout_role = role.to_owned();
        let stderr_role = role.to_owned();
        let stdout_run_id = artifacts.id.clone();
        let stderr_run_id = artifacts.id.clone();
        let out_thread = thread::spawn(move || {
            tee(
                stdout,
                stdout_log,
                stdout_output,
                stdout_role,
                "stdout",
                stdout_run_id,
            )
        });
        let err_thread = thread::spawn(move || {
            tee(
                stderr,
                stderr_log,
                stderr_output,
                stderr_role,
                "stderr",
                stderr_run_id,
            )
        });

        let result = wait_for_child(
            &mut child,
            Duration::from_secs(config.timeout_seconds),
            &self.cancelled,
        );
        if result.is_err() {
            terminate(&mut child);
        }
        let stream_result = join_streams(out_thread, err_thread);
        result?;
        stream_result.map_err(RunError::Infrastructure)
    }
}

fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> std::result::Result<(), RunError> {
    let start = Instant::now();
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err(RunError::Cancelled);
        }
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return Err(RunError::Timeout);
        };
        let slice = remaining.min(Duration::from_millis(100));
        match child
            .wait_timeout(slice)
            .map_err(|error| RunError::Infrastructure(error.into()))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(RunError::NonZero(status)),
            None if start.elapsed() >= timeout => return Err(RunError::Timeout),
            None => {}
        }
    }
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: the child was placed in its own process group at spawn time. A
    // negative PID targets that group and does not alias the controller.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

fn tee(
    reader: impl Read,
    path: PathBuf,
    output: Output,
    role: String,
    stream: &'static str,
    run_id: String,
) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut log = File::create(&path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        log.write_all(&line)?;
        captured.extend_from_slice(&line);
        log.flush()?;
        output.child_line(&role, stream, &run_id, &line)?;
    }
    Ok(captured)
}

fn join_streams(
    stdout: thread::JoinHandle<Result<Vec<u8>>>,
    stderr: thread::JoinHandle<Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let stdout = stdout
        .join()
        .map_err(|_| anyhow!("stdout streaming thread panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow!("stderr streaming thread panicked"))??;
    Ok((stdout, stderr))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(dir: &Path) -> Runner {
        Runner::new(
            dir,
            Arc::new(AtomicBool::new(false)),
            Output::new(crate::output::OutputMode::Plain),
        )
        .unwrap()
    }

    fn command(script: &str) -> CommandConfig {
        CommandConfig {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            timeout_seconds: 1,
        }
    }

    #[test]
    fn substitutes_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let config = command("test -f \"$1\" && printf '{\"ok\":true}' > \"$GOAL_RESULT_PATH\"");
        let config = CommandConfig {
            command: config
                .command
                .into_iter()
                .chain(["sh".into(), "{prompt}".into()])
                .collect(),
            ..config
        };
        let (value, artifacts) = runner(dir.path())
            .run_json::<serde_json::Value>("test", &config, "hello")
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        assert_eq!(fs::read_to_string(artifacts.prompt_path).unwrap(), "hello");
    }

    #[test]
    fn pipes_prompt_and_saves_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let config = command(
            "cat > received; echo out; echo err >&2; printf '{\"ok\":true}' > \"$GOAL_RESULT_PATH\"",
        );
        let (_, artifacts) = runner(dir.path())
            .run_json::<serde_json::Value>("test", &config, "from stdin")
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("received")).unwrap(),
            "from stdin"
        );
        assert_eq!(
            fs::read_to_string(artifacts.dir.join("stdout.log")).unwrap(),
            "out\n"
        );
        assert_eq!(
            fs::read_to_string(artifacts.dir.join("stderr.log")).unwrap(),
            "err\n"
        );
    }

    #[test]
    fn distinguishes_process_timeout_and_protocol_failures() {
        let dir = tempfile::tempdir().unwrap();
        let failed = runner(dir.path())
            .run_json::<serde_json::Value>("failed", &command("exit 7"), "")
            .unwrap_err()
            .0;
        assert!(matches!(failed, RunError::NonZero(_)));

        let missing = runner(dir.path())
            .run_json::<serde_json::Value>("missing", &command("true"), "")
            .unwrap_err()
            .0;
        assert!(matches!(missing, RunError::Protocol(_)));

        let malformed = runner(dir.path())
            .run_json::<serde_json::Value>(
                "malformed",
                &command("printf nope > \"$GOAL_RESULT_PATH\""),
                "",
            )
            .unwrap_err()
            .0;
        assert!(matches!(malformed, RunError::Protocol(_)));

        let timeout = runner(dir.path())
            .run_json::<serde_json::Value>("timeout", &command("sleep 2"), "")
            .unwrap_err()
            .0;
        assert!(matches!(timeout, RunError::Timeout));
    }
}

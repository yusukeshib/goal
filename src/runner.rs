use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
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
    tui::ArtifactRange,
};

const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHILD_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

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
        let uses_placeholder =
            prompt.is_some() && config.command.iter().any(|arg| arg.contains("{prompt}"));
        let prompt_path = if uses_placeholder {
            artifacts.prompt_path.to_str().ok_or_else(|| {
                RunError::Infrastructure(anyhow!(
                    "prompt path is not valid UTF-8: {}",
                    artifacts.prompt_path.display()
                ))
            })?
        } else {
            ""
        };
        let argv: Vec<String> = config
            .command
            .iter()
            .map(|arg| arg.replace("{prompt}", prompt_path))
            .collect();
        let Some(program) = argv.first() else {
            return Err(RunError::Infrastructure(anyhow!(
                "child command must not be empty"
            )));
        };
        let mut command = Command::new(program);
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
            RunError::Infrastructure(anyhow!(error).context(format!("spawn {program}")))
        })?;

        let input = if prompt.is_some() && !uses_placeholder {
            match child.stdin.take() {
                Some(stdin) => Some(stdin),
                None => {
                    terminate(&mut child);
                    return Err(RunError::Infrastructure(anyhow!(
                        "child stdin unavailable"
                    )));
                }
            }
        } else {
            None
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate(&mut child);
                return Err(RunError::Infrastructure(anyhow!(
                    "child stdout unavailable"
                )));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate(&mut child);
                return Err(RunError::Infrastructure(anyhow!(
                    "child stderr unavailable"
                )));
            }
        };
        if let Err(error) = set_nonblocking(&stdout)
            .and_then(|()| set_nonblocking(&stderr))
            .and_then(|()| input.as_ref().map_or(Ok(()), set_nonblocking))
        {
            terminate(&mut child);
            return Err(RunError::Infrastructure(error));
        }
        let stdout_log = artifacts.dir.join("stdout.log");
        let stderr_log = artifacts.dir.join("stderr.log");
        let stdout_output = self.output.clone();
        let stderr_output = self.output.clone();
        let stdout_role = role.to_owned();
        let stderr_role = role.to_owned();
        let stdout_run_id = artifacts.id.clone();
        let stderr_run_id = artifacts.id.clone();
        let capture_stdout = role == "sensor";
        let io_stopped = Arc::new(AtomicBool::new(false));
        let stream_failed = Arc::new(AtomicBool::new(false));
        let stdout_stopped = Arc::clone(&io_stopped);
        let stderr_stopped = Arc::clone(&io_stopped);
        let stdout_failed = Arc::clone(&stream_failed);
        let stderr_failed = Arc::clone(&stream_failed);
        let out_thread = thread::spawn(move || {
            let result = tee(
                stdout,
                stdout_log,
                stdout_output,
                stdout_role,
                "stdout",
                stdout_run_id,
                capture_stdout,
                stdout_stopped,
            );
            if result.is_err() {
                stdout_failed.store(true, Ordering::SeqCst);
            }
            result
        });
        let err_thread = thread::spawn(move || {
            let result = tee(
                stderr,
                stderr_log,
                stderr_output,
                stderr_role,
                "stderr",
                stderr_run_id,
                false,
                stderr_stopped,
            );
            if result.is_err() {
                stderr_failed.store(true, Ordering::SeqCst);
            }
            result
        });
        // Write stdin concurrently with reading diagnostics and enforcing the
        // timeout. Writing first can deadlock when a child fills stdout before
        // it starts reading a large prompt.
        let input_path = artifacts.prompt_path.clone();
        let input_stopped = Arc::clone(&io_stopped);
        let mut in_thread = input.map(|stdin| {
            thread::spawn(move || write_prompt(stdin, &input_path, &input_stopped))
        });

        let result = wait_for_child(
            &mut child,
            Duration::from_secs(config.timeout_seconds),
            &self.cancelled,
            &mut in_thread,
            &stream_failed,
        );
        let stream_failure_before_cleanup = stream_failed.load(Ordering::SeqCst);
        // Always terminate the process group, even after the direct child exits.
        // A stray descendant may otherwise keep stdin/stdout/stderr open and
        // make the joins below hang forever outside the configured timeout.
        terminate(&mut child);
        io_stopped.store(true, Ordering::SeqCst);
        let input_result = join_input(in_thread);
        let stream_result = join_streams(out_thread, err_thread);
        if let Err(error) = result {
            if stream_failure_before_cleanup
                && let Err(stream_error) = stream_result
            {
                return Err(RunError::Infrastructure(stream_error));
            }
            return Err(error);
        }
        input_result.map_err(RunError::Infrastructure)?;
        stream_result.map_err(RunError::Infrastructure)
    }
}

fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    cancelled: &AtomicBool,
    input: &mut Option<thread::JoinHandle<Result<()>>>,
    stream_failed: &AtomicBool,
) -> std::result::Result<(), RunError> {
    let start = Instant::now();
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err(RunError::Cancelled);
        }
        if input.as_ref().is_some_and(|thread| thread.is_finished()) {
            join_input(input.take()).map_err(RunError::Infrastructure)?;
        }
        if stream_failed.load(Ordering::SeqCst) {
            return Err(RunError::Infrastructure(anyhow!(
                "child output streaming failed"
            )));
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

fn write_prompt(
    mut stdin: ChildStdin,
    path: &Path,
    stopped: &AtomicBool,
) -> Result<()> {
    let mut prompt = File::open(path).with_context(|| format!("open prompt {}", path.display()))?;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let read = prompt.read(&mut buffer).context("read prompt")?;
        if read == 0 {
            return Ok(());
        }
        let mut written = 0;
        while written < read {
            if stopped.load(Ordering::SeqCst) {
                return Ok(());
            }
            match stdin.write(&buffer[written..read]) {
                Ok(0) => {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
                        .context("write prompt to child stdin");
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("write prompt to child stdin"),
            }
        }
    }
}

fn join_input(input: Option<thread::JoinHandle<Result<()>>>) -> Result<()> {
    match input {
        Some(input) => input
            .join()
            .map_err(|_| anyhow!("stdin writer thread panicked"))?,
        None => Ok(()),
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
    mut reader: impl Read,
    path: PathBuf,
    output: Output,
    role: String,
    stream: &'static str,
    run_id: String,
    capture: bool,
    stopped: Arc<AtomicBool>,
) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut log = File::create(&path).with_context(|| format!("open {}", path.display()))?;
    let mut buffer = [0_u8; 16 * 1024];
    let mut line = Vec::new();
    let mut offset = 0_u64;
    let mut total = 0_usize;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let bytes = &buffer[..read];
                let remaining = MAX_STREAM_BYTES.saturating_sub(total);
                if read > remaining {
                    log.write_all(&bytes[..remaining])?;
                    anyhow::bail!("{stream} exceeded {MAX_STREAM_BYTES} bytes");
                }
                log.write_all(bytes)?;
                total += read;
                if capture {
                    if captured.len().saturating_add(read) > MAX_CAPTURE_BYTES {
                        anyhow::bail!("captured {stream} exceeded {MAX_CAPTURE_BYTES} bytes");
                    }
                    captured.extend_from_slice(bytes);
                }
                for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
                    if line.len().saturating_add(segment.len()) > MAX_CHILD_LINE_BYTES {
                        anyhow::bail!("{stream} line exceeded {MAX_CHILD_LINE_BYTES} bytes");
                    }
                    line.extend_from_slice(segment);
                    if line.ends_with(b"\n") {
                        emit_line(&mut log, &output, &role, stream, &run_id, &path, offset, &line)?;
                        offset += line.len() as u64;
                        line.clear();
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).with_context(|| format!("read {stream}")),
        }
    }
    if !line.is_empty() {
        emit_line(&mut log, &output, &role, stream, &run_id, &path, offset, &line)?;
    }
    log.flush()?;
    Ok(captured)
}

#[allow(clippy::too_many_arguments)]
fn emit_line(
    log: &mut File,
    output: &Output,
    role: &str,
    stream: &str,
    run_id: &str,
    path: &Path,
    offset: u64,
    line: &[u8],
) -> Result<()> {
    log.flush()?;
    output.child_line(
        role,
        stream,
        run_id,
        ArtifactRange {
            path: path.to_owned(),
            offset,
            length: line.len() as u64,
        },
        line,
    )
}

#[cfg(unix)]
fn set_nonblocking<T: std::os::fd::AsRawFd>(stream: &T) -> Result<()> {
    let descriptor = stream.as_raw_fd();
    // SAFETY: fcntl only reads and updates the flags of this owned descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read child pipe flags");
    }
    // SAFETY: the descriptor remains valid for this call and O_NONBLOCK is a
    // supported status flag for Unix pipes.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("set child pipe nonblocking");
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking<T>(_stream: &T) -> Result<()> {
    Ok(())
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
    fn reads_diagnostics_while_writing_a_large_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = command(
            "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat > received; printf '{\"ok\":true}' > \"$GOAL_RESULT_PATH\"",
        );
        config.timeout_seconds = 3;
        let prompt = "x".repeat(256 * 1024);
        let quiet_runner = Runner::new(
            dir.path(),
            Arc::new(AtomicBool::new(false)),
            Output::new(crate::output::OutputMode::Json),
        )
        .unwrap();
        let (_, artifacts) = quiet_runner
            .run_json::<serde_json::Value>("test", &config, &prompt)
            .unwrap();
        assert_eq!(fs::metadata(dir.path().join("received")).unwrap().len(), prompt.len() as u64);
        assert_eq!(
            fs::metadata(artifacts.dir.join("stdout.log")).unwrap().len(),
            256 * 1024
        );
    }

    #[test]
    fn tui_notifications_reference_exact_logged_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let input = b"first\n{\"second\":true}\n";
        let (sender, receiver) = std::sync::mpsc::sync_channel(4);
        let captured = tee(
            std::io::Cursor::new(input),
            path.clone(),
            Output::tui(sender),
            "decider".to_owned(),
            "stdout",
            "run".to_owned(),
            true,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(captured, input);
        assert_eq!(fs::read(&path).unwrap(), input);

        let first = receiver.recv().unwrap();
        let second = receiver.recv().unwrap();
        assert!(matches!(
            first,
            crate::tui::Activity::Child { artifact, .. }
                if artifact.offset == 0 && artifact.length == 6 && artifact.path == path
        ));
        assert!(matches!(
            second,
            crate::tui::Activity::Child { artifact, .. }
                if artifact.offset == 6 && artifact.length == 16 && artifact.path == path
        ));
    }

    #[test]
    fn cleans_up_descendants_that_outlive_a_successful_child() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        runner(dir.path())
            .run_json::<serde_json::Value>(
                "test",
                &command("sleep 10 & printf '{\"ok\":true}' > \"$GOAL_RESULT_PATH\""),
                "",
            )
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn distinguishes_process_timeout_and_protocol_failures() {
        let dir = tempfile::tempdir().unwrap();
        let empty = CommandConfig {
            command: Vec::new(),
            timeout_seconds: 1,
        };
        let empty = runner(dir.path())
            .run_json::<serde_json::Value>("empty", &empty, "")
            .unwrap_err()
            .0;
        assert!(matches!(empty, RunError::Infrastructure(_)));

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

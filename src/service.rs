use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::Shutdown,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::state::ControllerLock;

const REGISTRY_FILE: &str = "services.json";
const REGISTRY_LOCK: &str = "services.lock";
const LOCAL_SERVICE_FILE: &str = "service.json";
const SERVICE_LOG_FILE: &str = "service.log";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceRecord {
    pub config_path: PathBuf,
    pub project_dir: PathBuf,
    pub pid: u32,
    pub started_at: u64,
    pub foreground: bool,
    pub log_path: Option<PathBuf>,
    instance_id: String,
    control_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ServiceInfo<'a> {
    pub config_path: &'a Path,
    pub project_dir: &'a Path,
    pub pid: u32,
    pub started_at: u64,
    pub foreground: bool,
    pub log_path: Option<&'a Path>,
}

impl ServiceRecord {
    pub fn info(&self) -> ServiceInfo<'_> {
        ServiceInfo {
            config_path: &self.config_path,
            project_dir: &self.project_dir,
            pid: self.pid,
            started_at: self.started_at,
            foreground: self.foreground,
            log_path: self.log_path.as_deref(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Registry {
    services: Vec<ServiceRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StartResult {
    ok: bool,
    service: Option<ServiceRecord>,
    error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ControlRequest {
    command: String,
    instance_id: String,
}

pub struct Registration {
    record: ServiceRecord,
    control_stop: Arc<AtomicBool>,
    control_thread: Option<thread::JoinHandle<()>>,
}

impl Registration {
    pub fn create(
        config_path: &Path,
        project_dir: &Path,
        foreground: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let record = ServiceRecord {
            config_path: config_path.to_owned(),
            project_dir: project_dir.to_owned(),
            pid: std::process::id(),
            started_at: unix_timestamp(),
            foreground,
            log_path: (!foreground).then(|| project_dir.join(".goal").join(SERVICE_LOG_FILE)),
            instance_id: format!("{}-{nonce}", std::process::id()),
            control_path: registry_root()?
                .join(format!("control-{}-{nonce}.sock", std::process::id())),
        };
        let (control_stop, control_thread) = start_control_server(&record, cancelled)?;

        if let Err(error) = write_json_atomic(&local_service_path(project_dir), &record)
            .context("write local service record")
        {
            control_stop.store(true, Ordering::SeqCst);
            let _ = control_thread.join();
            let _ = fs::remove_file(&record.control_path);
            return Err(error);
        }
        if let Err(error) = update_registry(|registry| {
            registry
                .services
                .retain(|existing| existing.config_path != record.config_path);
            registry.services.push(record.clone());
            Ok(())
        }) {
            let _ = fs::remove_file(local_service_path(project_dir));
            control_stop.store(true, Ordering::SeqCst);
            let _ = control_thread.join();
            let _ = fs::remove_file(&record.control_path);
            return Err(error);
        }

        Ok(Self {
            record,
            control_stop,
            control_thread: Some(control_thread),
        })
    }

    pub fn record(&self) -> &ServiceRecord {
        &self.record
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.control_stop.store(true, Ordering::SeqCst);
        if let Some(control_thread) = self.control_thread.take() {
            let _ = control_thread.join();
        }
        let _ = fs::remove_file(&self.record.control_path);
        let _ = remove_registry_record(&self.record.config_path, self.record.pid);
        let local_path = local_service_path(&self.record.project_dir);
        if read_json::<ServiceRecord>(&local_path).is_ok_and(|record| {
            record.pid == self.record.pid && record.instance_id == self.record.instance_id
        }) {
            let _ = fs::remove_file(local_path);
        }
    }
}

#[cfg(unix)]
fn start_control_server(
    record: &ServiceRecord,
    cancelled: Arc<AtomicBool>,
) -> Result<(Arc<AtomicBool>, thread::JoinHandle<()>)> {
    let path = record.control_path.clone();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create goal state directory {}", parent.display()))?;
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove stale {}", path.display()));
        }
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind goal control socket {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect goal control socket {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("make goal control socket nonblocking")?;

    let instance_id = record.instance_id.clone();
    let stopping = Arc::new(AtomicBool::new(false));
    let thread_stopping = Arc::clone(&stopping);
    let worker = thread::Builder::new()
        .name("goal-control".to_owned())
        .spawn(move || {
            while !thread_stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut payload = Vec::new();
                        let read = (&mut stream).take(4_097).read_to_end(&mut payload);
                        let request = read
                            .ok()
                            .filter(|_| payload.len() <= 4_096)
                            .and_then(|_| serde_json::from_slice::<ControlRequest>(&payload).ok());
                        let accepted = request.is_some_and(|request| {
                            request.command == "stop" && request.instance_id == instance_id
                        });
                        if accepted {
                            cancelled.store(true, Ordering::SeqCst);
                        }
                        let response = if accepted {
                            b"{\"ok\":true}\n".as_slice()
                        } else {
                            b"{\"ok\":false}\n".as_slice()
                        };
                        let _ = stream.write_all(response);
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        })
        .context("start goal control thread")?;
    Ok((stopping, worker))
}

#[cfg(not(unix))]
fn start_control_server(
    _record: &ServiceRecord,
    _cancelled: Arc<AtomicBool>,
) -> Result<(Arc<AtomicBool>, thread::JoinHandle<()>)> {
    bail!("goal services are currently supported only on Unix")
}

pub fn start_background(config_path: &Path, project_dir: &Path) -> Result<ServiceRecord> {
    #[cfg(not(unix))]
    {
        let _ = (config_path, project_dir);
        bail!("background services are currently supported only on Unix");
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        fs::create_dir_all(project_dir.join(".goal")).context("create .goal directory")?;
        if let Some(record) = service_for_config(config_path)? {
            bail!(
                "goal service is already running for {} with pid {}",
                config_path.display(),
                record.pid
            );
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let ready_path = project_dir
            .join(".goal")
            .join(format!("start-{}-{nonce}.json", std::process::id()));
        let log_path = project_dir.join(".goal").join(SERVICE_LOG_FILE);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open service log {}", log_path.display()))?;
        let stderr = log.try_clone().context("clone service log handle")?;

        let executable = env::current_exe().context("resolve goal executable")?;
        let mut command = Command::new(executable);
        command
            .arg("--output")
            .arg("plain")
            .arg("__service")
            .arg(config_path)
            .arg("--ready")
            .arg(&ready_path)
            .current_dir(project_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        // SAFETY: this closure invokes only the async-signal-safe setsid syscall
        // between fork and exec. The child must outlive the invoking shell.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().context("start background goal service")?;
        let deadline = std::time::Instant::now() + START_TIMEOUT;

        loop {
            if let Ok(started) = read_json::<StartResult>(&ready_path) {
                let _ = fs::remove_file(&ready_path);
                if started.ok {
                    return started
                        .service
                        .ok_or_else(|| anyhow!("service started without a service record"));
                }
                let _ = child.wait();
                bail!(
                    "background goal service failed to start: {}",
                    started.error.unwrap_or_else(|| "unknown error".to_owned())
                );
            }
            if let Some(status) = child.try_wait().context("check background goal service")? {
                // Give an exiting child a moment to publish its structured startup error.
                thread::sleep(Duration::from_millis(25));
                if let Ok(started) = read_json::<StartResult>(&ready_path) {
                    let _ = fs::remove_file(&ready_path);
                    bail!(
                        "background goal service failed to start: {}",
                        started
                            .error
                            .unwrap_or_else(|| format!("exited with {status}"))
                    );
                }
                let _ = fs::remove_file(&ready_path);
                bail!(
                    "background goal service exited before startup completed ({status}); inspect {}",
                    log_path.display()
                );
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&ready_path);
                bail!(
                    "timed out waiting for background goal service to start; inspect {}",
                    log_path.display()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

pub fn write_start_success(path: &Path, record: &ServiceRecord) -> Result<()> {
    write_json_atomic(
        path,
        &StartResult {
            ok: true,
            service: Some(record.clone()),
            error: None,
        },
    )
}

pub fn write_start_error(path: &Path, error: &anyhow::Error) -> Result<()> {
    write_json_atomic(
        path,
        &StartResult {
            ok: false,
            service: None,
            error: Some(format!("{error:#}")),
        },
    )
}

pub fn list() -> Result<Vec<ServiceRecord>> {
    let mut running = Vec::new();
    update_registry(|registry| {
        registry.services.retain(|record| {
            let active = record_is_current(record);
            if active {
                running.push(record.clone());
            }
            active
        });
        Ok(())
    })?;
    running.sort_by(|left, right| left.config_path.cmp(&right.config_path));
    Ok(running)
}

pub fn service_for_config(config_path: &Path) -> Result<Option<ServiceRecord>> {
    let mut found = None;
    update_registry(|registry| {
        registry.services.retain(|record| record_is_current(record));
        found = registry
            .services
            .iter()
            .find(|record| record.config_path == config_path)
            .cloned();
        Ok(())
    })?;
    Ok(found)
}

pub fn stop(config_path: &Path) -> Result<ServiceRecord> {
    let Some(record) = service_for_config(config_path)? else {
        bail!("no running goal service for {}", config_path.display());
    };
    if !record_is_current(&record) {
        let _ = remove_registry_record(config_path, record.pid);
        bail!("goal service record is stale for {}", config_path.display());
    }

    request_stop(&record)?;
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    while record_is_current(&record) {
        if std::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for goal service pid {} to stop",
                record.pid
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = remove_registry_record(config_path, record.pid);
    Ok(record)
}

pub fn tail(config_path: &Path, lines: usize, follow: bool) -> Result<()> {
    let project_dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", config_path.display()))?;
    let log_path = project_dir.join(".goal").join(SERVICE_LOG_FILE);
    let mut file = File::open(&log_path)
        .with_context(|| format!("open service log {}", log_path.display()))?;
    let start = tail_start(&mut file, lines)?;
    file.seek(SeekFrom::Start(start))?;
    let mut stdout = std::io::stdout().lock();
    copy_available(&mut file, &mut stdout)?;
    stdout.flush()?;
    if !follow {
        return Ok(());
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::SeqCst))?;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        let copied = copy_available(&mut file, &mut stdout)?;
        if copied > 0 {
            stdout.flush()?;
            continue;
        }
        if service_for_config(config_path)?.is_none() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn tail_start(file: &mut File, lines: usize) -> Result<u64> {
    let length = file.metadata()?.len();
    if lines == 0 {
        return Ok(length);
    }
    let mut position = length;
    let mut newlines = 0_usize;
    let mut buffer = [0_u8; 8 * 1024];
    let ends_with_newline = if length == 0 {
        false
    } else {
        file.seek(SeekFrom::Start(length - 1))?;
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)?;
        byte[0] == b'\n'
    };
    let wanted = lines.saturating_add(usize::from(ends_with_newline));

    while position > 0 {
        let read = usize::try_from(position.min(buffer.len() as u64)).unwrap_or(buffer.len());
        position -= read as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut buffer[..read])?;
        for index in (0..read).rev() {
            if buffer[index] == b'\n' {
                newlines += 1;
                if newlines == wanted {
                    return Ok(position + index as u64 + 1);
                }
            }
        }
    }
    Ok(0)
}

fn copy_available(file: &mut File, writer: &mut impl Write) -> Result<u64> {
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
}

fn record_is_current(record: &ServiceRecord) -> bool {
    if !process_is_alive(record.pid) {
        return false;
    }
    read_json::<ServiceRecord>(&local_service_path(&record.project_dir)).is_ok_and(|local| {
        local.pid == record.pid
            && local.config_path == record.config_path
            && local.instance_id == record.instance_id
            && local.control_path == record.control_path
            && ControllerLock::owner_matches(&record.project_dir, &record.config_path, record.pid)
    })
}

fn local_service_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".goal").join(LOCAL_SERVICE_FILE)
}

fn remove_registry_record(config_path: &Path, pid: u32) -> Result<()> {
    update_registry(|registry| {
        registry
            .services
            .retain(|record| record.config_path != config_path || record.pid != pid);
        Ok(())
    })
}

fn update_registry(mut update: impl FnMut(&mut Registry) -> Result<()>) -> Result<()> {
    let root = registry_root()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("create goal state directory {}", root.display()))?;
    let lock_path = root.join(REGISTRY_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open service registry lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock service registry {}", lock_path.display()))?;

    let registry_path = root.join(REGISTRY_FILE);
    let mut registry = match fs::read(&registry_path) {
        Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse service registry {}", registry_path.display()))?,
        Ok(_) => Registry::default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read service registry {}", registry_path.display()));
        }
    };
    update(&mut registry)?;
    write_json_atomic(&registry_path, &registry)
}

fn registry_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GOAL_STATE_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("goal"));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("HOME is not set; set XDG_STATE_HOME for the goal service registry")
        })?;
    Ok(PathBuf::from(home).join(".local/state/goal"))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".goal-tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.flush()?;
        fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs existence/permission checking only.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn request_stop(record: &ServiceRecord) -> Result<()> {
    let mut stream = UnixStream::connect(&record.control_path).with_context(|| {
        format!(
            "connect to goal control socket {}",
            record.control_path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("set goal control response timeout")?;
    serde_json::to_writer(
        &mut stream,
        &ControlRequest {
            command: "stop".to_owned(),
            instance_id: record.instance_id.clone(),
        },
    )?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let response: serde_json::Value = serde_json::from_str(&response)?;
    if response["ok"] != true {
        bail!("goal service rejected the stop request");
    }
    Ok(())
}

#[cfg(not(unix))]
fn request_stop(_record: &ServiceRecord) -> Result<()> {
    bail!("stopping goal services is currently supported only on Unix")
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_start_returns_the_requested_suffix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        let mut file = File::open(path).unwrap();
        assert_eq!(tail_start(&mut file, 2).unwrap(), 4);
    }

    #[test]
    fn tail_start_handles_a_final_line_without_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        fs::write(&path, b"one\ntwo\nthree").unwrap();
        let mut file = File::open(path).unwrap();
        assert_eq!(tail_start(&mut file, 2).unwrap(), 4);
    }
}

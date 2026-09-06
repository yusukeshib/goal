#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const TEST_GOAL_ID: &str = "test-goal";

fn test_tempdir() -> tempfile::TempDir {
    // macOS's default /var/folders/... temp root can exceed sockaddr_un's
    // path limit once the registry and control socket filename are appended.
    let root = fs::canonicalize("/tmp").unwrap();
    tempfile::Builder::new().prefix("goal-").tempdir_in(root).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
    registry: PathBuf,
}

impl Fixture {
    fn new(sensor: &str, decider: &str, worker: &str) -> Self {
        let dir = test_tempdir();
        fs::write(dir.path().join("GOAL.md"), "Reach the fake goal safely.\n").unwrap();
        let sensor = script(dir.path(), "sensor.sh", sensor);
        let decider = script(dir.path(), "decider.sh", decider);
        let worker = script(dir.path(), "worker.sh", worker);
        let config = dir.path().join("goal.toml");
        fs::write(
            &config,
            format!(
                r#"goal_file = "GOAL.md"
interval_seconds = 0
max_wait_seconds = 1
[sensor]
command = ["{}"]
timeout_seconds = 5
[decider]
command = ["{}", "{{prompt}}"]
timeout_seconds = 5
[worker]
command = ["{}"]
timeout_seconds = 5
"#,
                sensor.display(),
                decider.display(),
                worker.display()
            ),
        )
        .unwrap();
        let registry = dir.path().join("registry");
        let added = goal_command(&registry)
            .arg("add")
            .arg(&config)
            .args(["--id", TEST_GOAL_ID])
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "failed to register fixture: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        Self {
            dir,
            config,
            registry,
        }
    }

    fn run(&self) -> Output {
        command(&self.config).output().unwrap()
    }

    fn set_interval(&self, seconds: u64) {
        let config = fs::read_to_string(&self.config).unwrap();
        fs::write(
            &self.config,
            config.replace(
                "interval_seconds = 0",
                &format!("interval_seconds = {seconds}"),
            ),
        )
        .unwrap();
    }

    fn set_max_concurrency(&self, concurrency: usize) {
        let config = fs::read_to_string(&self.config).unwrap();
        fs::write(
            &self.config,
            config.replace(
                "max_wait_seconds = 1",
                &format!("max_wait_seconds = 1\nmax_concurrency = {concurrency}"),
            ),
        )
        .unwrap();
    }

    fn set_timeout(&self, section: &str, seconds: u64) {
        let mut config = fs::read_to_string(&self.config).unwrap();
        let section_offset = config.find(&format!("[{section}]")).unwrap();
        let value_offset = section_offset
            + config[section_offset..].find("timeout_seconds = ").unwrap()
            + "timeout_seconds = ".len();
        let value_end = value_offset + config[value_offset..].find('\n').unwrap();
        config.replace_range(value_offset..value_end, &seconds.to_string());
        fs::write(&self.config, config).unwrap();
    }

    fn count(&self, name: &str) -> u64 {
        fs::read_to_string(self.dir.path().join(name))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }
}

fn goal_command(state_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_goal"));
    command
        .env("GOAL_STATE_DIR", state_dir)
        .env_remove("GOAL_DIR");
    command
}

fn command(config: &Path) -> Command {
    let mut command = goal_command(&config.parent().unwrap().join("registry"));
    command
        .args(["up", TEST_GOAL_ID, "--foreground"]);
    command
}

fn write_project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    fs::create_dir(&project).unwrap();
    fs::write(project.join("GOAL.md"), "Exercise the registry.\n").unwrap();
    let config = project.join("goal.toml");
    fs::write(
        &config,
        r#"goal_file = "GOAL.md"
interval_seconds = 0
max_wait_seconds = 1
[sensor]
command = ["/usr/bin/true"]
timeout_seconds = 1
[decider]
command = ["/usr/bin/true"]
timeout_seconds = 1
[worker]
command = ["/usr/bin/true"]
timeout_seconds = 1
"#,
    )
    .unwrap();
    config
}

fn listed_goals(registry: &Path) -> Vec<serde_json::Value> {
    let output = goal_command(registry)
        .args(["list", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to list goals: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn events(fixture: &Fixture) -> Vec<serde_json::Value> {
    fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn state(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(fixture.dir.path().join(".goal/state.json")).unwrap(),
    )
    .unwrap()
}

fn wait_for(description: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(condition(), "timed out waiting for {description}");
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct ServicesGuard {
    registry: PathBuf,
    armed: bool,
}

impl ServicesGuard {
    fn new(registry: &Path) -> Self {
        Self {
            registry: registry.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ServicesGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = goal_command(&self.registry).arg("down").output();
        }
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn interrupt(&self) {
        let child = self.0.as_ref().unwrap();
        let status = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn wait(&mut self) -> ExitStatus {
        use wait_timeout::ChildExt;
        let status = self.0.as_mut().unwrap()
            .wait_timeout(Duration::from_secs(10)).unwrap()
            .expect("controller did not stop within 10 seconds");
        self.0.take();
        status
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let _ = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

const COUNT_SENSOR: &str = r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; printf '{"sense":%s}\n' "$n""#;

#[test]
fn help_fully_describes_configuration_and_process_contracts() {
    let state = test_tempdir();
    let root = goal_command(state.path()).arg("--help").output().unwrap();
    assert!(root.status.success());
    let root = String::from_utf8(root.stdout).unwrap();
    for expected in [
        "sense -> decide -> act -> sense",
        "goal add PATH [--id ID]",
        "goal up [ID]",
        ".goal/",
        "goal down [ID]",
        "goal enable ID",
        "goal disable ID",
        "goal remove ID",
        "goal list",
        "goal tail ID --follow",
        "current directory or GOAL_DIR",
        "stats",
        "analysis",
    ] {
        assert!(
            root.contains(expected),
            "missing {expected:?} from root help"
        );
    }
    assert!(!root.contains("--goal-dir"));
    assert!(!root.contains("CONFIG_OR_DIR"));

    let run = goal_command(state.path()).arg("--help").output().unwrap();
    assert!(run.status.success());
    let run = String::from_utf8(run.stdout).unwrap();
    let run = run.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "goal_file = \"GOAL.md\"",
        "SENSOR CONTRACT",
        "GOAL_RESULT_PATH",
        "--output <OUTPUT>",
        "strict JSONL",
        "GOAL SEMANTICS",
        "temporary health is not completion",
        "Never return Complete",
        "implement pagination",
        "{prompt}",
        "run_task",
        "run_tasks",
        "max_concurrency",
        "failure",
        "Neither process may request human input",
        "captured sensor stdout",
        "Each stdout or stderr line is limited to 4 MiB",
        "process group",
        "exit non-zero",
        "FAILURE ANALYSIS",
    ] {
        assert!(run.contains(expected), "missing {expected:?} from run help");
    }
}

#[test]
fn commands_never_infer_a_goal_from_the_current_directory_or_environment() {
    let fixture = Fixture::new(COUNT_SENSOR, r#"exit 99"#, r#"exit 99"#);
    let empty_registry = fixture.dir.path().join("no-inference-registry");

    for args in [
        Vec::<&str>::new(),
        vec!["tail"],
        vec!["stats"],
        vec!["analysis"],
    ] {
        let output = goal_command(&empty_registry)
            .args(args)
            .current_dir(fixture.dir.path())
            .env("GOAL_DIR", fixture.dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
    }

    for args in [vec!["up"], vec!["down"]] {
        let output = goal_command(&empty_registry)
            .args(args)
            .current_dir(fixture.dir.path())
            .env("GOAL_DIR", fixture.dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "empty bulk command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(listed_goals(&empty_registry).is_empty());
    assert!(!fixture.dir.path().join(".goal").exists());
}

#[test]
fn registration_accepts_files_and_directories_and_rejects_collisions() {
    let root = test_tempdir();
    let registry = root.path().join("registry");
    let alpha = write_project(root.path(), "alpha");
    let beta = write_project(root.path(), "beta");
    let gamma = write_project(root.path(), "gamma");
    let needs_override = write_project(root.path(), "_needs-override");

    let added = goal_command(&registry)
        .arg("add")
        .arg(alpha.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let added = goal_command(&registry)
        .arg("add")
        .arg(&beta)
        .args(["--id", "custom.beta"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let invalid_derived_id = goal_command(&registry)
        .arg("add")
        .arg(needs_override.parent().unwrap())
        .output()
        .unwrap();
    assert!(!invalid_derived_id.status.success());
    let overridden = goal_command(&registry)
        .arg("add")
        .arg(needs_override.parent().unwrap())
        .args(["--id", "valid-override"])
        .output()
        .unwrap();
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );

    let duplicate_path = goal_command(&registry)
        .arg("add")
        .arg(&alpha)
        .args(["--id", "alpha-alias"])
        .output()
        .unwrap();
    assert!(!duplicate_path.status.success());
    let alpha_symlink = root.path().join("alpha-link.toml");
    std::os::unix::fs::symlink(&alpha, &alpha_symlink).unwrap();
    let duplicate_symlink = goal_command(&registry)
        .arg("add")
        .arg(&alpha_symlink)
        .args(["--id", "alpha-symlink"])
        .output()
        .unwrap();
    assert!(!duplicate_symlink.status.success());
    let duplicate_id = goal_command(&registry)
        .arg("add")
        .arg(&gamma)
        .args(["--id", "alpha"])
        .output()
        .unwrap();
    assert!(!duplicate_id.status.success());

    let listed = listed_goals(&registry);
    assert_eq!(
        listed.iter().map(|goal| goal["id"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["alpha", "custom.beta", "valid-override"]
    );
    assert!(listed.iter().all(|goal| {
        goal["enabled"] == true
            && goal["status"] == "stopped"
            && goal["pid"].is_null()
            && goal["config_path"].is_string()
    }));
    assert!(registry.join("goals.json").is_file());

    let alias = goal_command(&registry)
        .args(["ls", "--output", "json"])
        .output()
        .unwrap();
    assert!(alias.status.success());
    assert_eq!(
        serde_json::from_slice::<Vec<serde_json::Value>>(&alias.stdout).unwrap(),
        listed
    );
}

#[test]
fn malformed_registration_file_is_reported_without_being_replaced() {
    let root = test_tempdir();
    let registry = root.path().join("registry");
    fs::create_dir(&registry).unwrap();
    let registrations = registry.join("goals.json");
    let malformed = b"{ definitely-not-valid-json\n";
    fs::write(&registrations, malformed).unwrap();
    let config = write_project(root.path(), "valid-project");

    for args in [vec!["list"], vec!["add", config.to_str().unwrap()]] {
        let output = goal_command(&registry).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("parse goal registrations"));
        assert_eq!(fs::read(&registrations).unwrap(), malformed);
    }
}

#[test]
fn enable_disable_now_and_remove_keep_intent_separate_from_runtime() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{"type":"wait","reason":"stay active","retry_after_seconds":1}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let mut cleanup = ServicesGuard::new(&fixture.registry);

    let disabled = goal_command(&fixture.registry)
        .args(["disable", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    let listed = listed_goals(&fixture.registry);
    assert_eq!(listed[0]["enabled"], false);
    assert_eq!(listed[0]["status"], "stopped");

    let explicit_up = goal_command(&fixture.registry)
        .args(["up", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(!explicit_up.status.success());
    assert!(String::from_utf8_lossy(&explicit_up.stderr).contains("disabled"));

    let enabled_without_start = goal_command(&fixture.registry)
        .args(["enable", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(enabled_without_start.status.success());
    let listed = listed_goals(&fixture.registry);
    assert_eq!(listed[0]["enabled"], true);
    assert_eq!(listed[0]["status"], "stopped");

    let enabled_now = goal_command(&fixture.registry)
        .args(["enable", TEST_GOAL_ID, "--now"])
        .output()
        .unwrap();
    assert!(
        enabled_now.status.success(),
        "{}",
        String::from_utf8_lossy(&enabled_now.stderr)
    );
    let listed = listed_goals(&fixture.registry);
    assert_eq!(listed[0]["enabled"], true);
    assert_eq!(listed[0]["status"], "running");
    assert!(listed[0]["pid"].is_u64());

    let disabled_without_stop = goal_command(&fixture.registry)
        .args(["disable", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(disabled_without_stop.status.success());
    let listed = listed_goals(&fixture.registry);
    assert_eq!(listed[0]["enabled"], false);
    assert_eq!(listed[0]["status"], "running");

    let remove_running = goal_command(&fixture.registry)
        .args(["remove", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(!remove_running.status.success());
    assert!(String::from_utf8_lossy(&remove_running.stderr).contains("running"));

    let explicit_up = goal_command(&fixture.registry)
        .args(["up", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(!explicit_up.status.success());
    assert!(String::from_utf8_lossy(&explicit_up.stderr).contains("disabled"));

    let disabled_now = goal_command(&fixture.registry)
        .args(["disable", TEST_GOAL_ID, "--now"])
        .output()
        .unwrap();
    assert!(
        disabled_now.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled_now.stderr)
    );
    let listed = listed_goals(&fixture.registry);
    assert_eq!(listed[0]["enabled"], false);
    assert_eq!(listed[0]["status"], "stopped");
    assert!(listed[0]["pid"].is_null());

    let config_before_remove = fs::read(&fixture.config).unwrap();
    let artifacts = fixture.dir.path().join(".goal");
    assert!(artifacts.is_dir());
    let removed = goal_command(&fixture.registry)
        .args(["remove", TEST_GOAL_ID])
        .output()
        .unwrap();
    assert!(removed.status.success());
    assert!(listed_goals(&fixture.registry).is_empty());
    assert_eq!(fs::read(&fixture.config).unwrap(), config_before_remove);
    assert!(artifacts.is_dir());
    cleanup.disarm();
}

#[test]
fn bulk_operations_continue_after_failures_and_include_disabled_goals() {
    let broken = Fixture::new(COUNT_SENSOR, r#"exit 99"#, r#"exit 99"#);
    let healthy = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{"type":"wait","reason":"bulk service active","retry_after_seconds":1}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let skipped = Fixture::new(COUNT_SENSOR, r#"exit 99"#, r#"exit 99"#);
    let registry = healthy.dir.path().join("bulk-registry");
    for (id, config) in [
        ("a-broken", &broken.config),
        ("m-disabled", &skipped.config),
        ("z-healthy", &healthy.config),
    ] {
        let added = goal_command(&registry)
            .arg("add")
            .arg(config)
            .args(["--id", id])
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
    }
    assert!(goal_command(&registry)
        .args(["disable", "a-broken"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(goal_command(&registry)
        .args(["disable", "m-disabled"])
        .output()
        .unwrap()
        .status
        .success());
    fs::remove_file(&broken.config).unwrap();
    let failed_enable = goal_command(&registry)
        .args(["enable", "a-broken", "--now"])
        .output()
        .unwrap();
    assert!(!failed_enable.status.success());
    let broken_goal = listed_goals(&registry)
        .into_iter()
        .find(|goal| goal["id"] == "a-broken")
        .unwrap();
    assert_eq!(broken_goal["enabled"], true);
    assert_eq!(broken_goal["status"], "stopped");
    let mut cleanup = ServicesGuard::new(&registry);

    let first = goal_command(&registry)
        .args(["up", "--output", "json"])
        .output()
        .unwrap();
    assert!(!first.status.success());
    let first_text = format!(
        "{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first_text.contains("a-broken"));
    assert!(first_text.contains("z-healthy"));
    let listed = listed_goals(&registry);
    let healthy_goal = listed.iter().find(|goal| goal["id"] == "z-healthy").unwrap();
    assert_eq!(healthy_goal["status"], "running");
    let skipped_goal = listed.iter().find(|goal| goal["id"] == "m-disabled").unwrap();
    assert_eq!(skipped_goal["enabled"], false);
    assert_eq!(skipped_goal["status"], "stopped");
    assert!(skipped_goal["pid"].is_null());
    let pid = healthy_goal["pid"].as_u64().unwrap();

    let second = goal_command(&registry)
        .args(["up", "--output", "json"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    let listed = listed_goals(&registry);
    assert_eq!(
        listed.iter().find(|goal| goal["id"] == "z-healthy").unwrap()["pid"],
        pid
    );

    let disabled = goal_command(&registry)
        .args(["disable", "z-healthy"])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    assert_eq!(
        listed_goals(&registry)
            .iter()
            .find(|goal| goal["id"] == "z-healthy")
            .unwrap()["status"],
        "running"
    );

    let stopped = goal_command(&registry).arg("down").output().unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let listed = listed_goals(&registry);
    assert!(listed.iter().all(|goal| goal["status"] == "stopped"));
    assert_eq!(
        listed.iter().find(|goal| goal["id"] == "z-healthy").unwrap()["enabled"],
        false
    );
    assert!(goal_command(&registry).arg("down").output().unwrap().status.success());

    for id in ["a-broken", "m-disabled", "z-healthy"] {
        let removed = goal_command(&registry)
            .args(["remove", id])
            .output()
            .unwrap();
        assert!(
            removed.status.success(),
            "failed to remove {id}: {}",
            String::from_utf8_lossy(&removed.stderr)
        );
    }
    assert!(listed_goals(&registry).is_empty());
    cleanup.disarm();
}

#[test]
fn id_only_commands_reject_unknown_paths_and_foreground_without_an_id() {
    let fixture = Fixture::new(COUNT_SENSOR, r#"exit 99"#, r#"exit 99"#);
    let foreground_without_id = goal_command(&fixture.registry)
        .args(["up", "--foreground"])
        .output()
        .unwrap();
    assert!(!foreground_without_id.status.success());

    for args in [
        vec!["up", "unknown"],
        vec!["down", "unknown"],
        vec!["tail", "unknown"],
        vec!["stats", "unknown"],
        vec!["analysis", "unknown"],
        vec!["enable", "unknown"],
        vec!["disable", "unknown"],
        vec!["remove", "unknown"],
        vec!["down", fixture.config.to_str().unwrap()],
    ] {
        let output = goal_command(&fixture.registry).args(args).output().unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn relative_state_root_is_shared_with_the_detached_service() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{"type":"wait","reason":"relative registry active","retry_after_seconds":1}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let caller = test_tempdir();
    let relative_registry = Path::new("r");
    let absolute_registry = caller.path().join(relative_registry);
    let mut cleanup = ServicesGuard::new(&absolute_registry);

    let added = goal_command(relative_registry)
        .current_dir(caller.path())
        .arg("add")
        .arg(&fixture.config)
        .args(["--id", "relative-goal"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let started = goal_command(relative_registry)
        .current_dir(caller.path())
        .args(["up", "relative-goal"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let listed = listed_goals(&absolute_registry);
    assert_eq!(listed[0]["id"], "relative-goal");
    assert_eq!(listed[0]["status"], "running");

    let stopped = goal_command(relative_registry)
        .current_dir(caller.path())
        .args(["down", "relative-goal"])
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert_eq!(listed_goals(&absolute_registry)[0]["status"], "stopped");
    cleanup.disarm();
}

#[test]
fn json_output_is_strict_jsonl_with_one_common_envelope() {
    let fixture = Fixture::new(
        r#"echo sensor-diagnostic >&2; printf '{"healthy":true}'"#,
        r#"echo '{"pi_event":"diagnostic"}'; echo plain-diagnostic >&2; if test -f decider-once; then r='{"type":"complete","summary":"JSON complete"}'; else touch decider-once; r='{"type":"wait","reason":"observe JSON wait","retry_after_seconds":0}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = command(&fixture.config)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "JSON mode leaked stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid JSONL {line:?}: {error}"))
        })
        .collect();
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(line["timestamp"].is_u64());
        assert!(line["type"].is_string());
        assert!(line["details"].is_object());
    }
    assert!(lines.iter().any(|line| line["type"] == "wait"));
    assert!(lines.iter().any(|line| line["type"] == "complete"));
    let phase_starts: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "phase_started")
        .collect();
    assert!(!phase_starts.is_empty());
    for event in phase_starts {
        let details = &event["details"];
        assert!(details["run_id"].is_string());
        assert!(details["command"].is_array());
        assert_eq!(details["cwd"], fixture.dir.path().to_str().unwrap());
        assert_eq!(details["timeout_seconds"], 5);
        assert!(details["prompt_path"].is_string());
        assert!(details["result_path"].is_string());
        assert!(details["prompt_delivery"].is_string());
        if details["phase"] == "decider" {
            assert_eq!(details["prompt_delivery"], "path_argument");
            assert!(
                details["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|argument| argument != "{prompt}")
            );
        }
    }
    assert!(
        lines
            .iter()
            .any(|line| line["details"]["payload"]["pi_event"] == "diagnostic")
    );
    assert!(
        lines
            .iter()
            .any(|line| line["details"]["content"] == "plain-diagnostic")
    );
    assert!(
        lines
            .iter()
            .any(|line| line["details"]["content"] == "sensor-diagnostic")
    );
}

#[test]
fn rejects_a_second_controller_for_the_same_config_and_releases_after_exit() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{\"type\":\"wait\",\"reason\":\"hold lock\",\"retry_after_seconds\":1}' > \"$GOAL_RESULT_PATH\""#,
        r#"exit 99"#,
    );
    let mut first = command(&fixture.config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lock_ready = fixture.dir.path().join(".goal/state.json");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !lock_ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(lock_ready.exists(), "first controller did not start");

    // Configuration editors commonly publish via atomic rename. Locking the
    // config inode itself would let a second controller lock the replacement.
    let replacement = fixture.config.with_extension("toml.new");
    fs::copy(&fixture.config, &replacement).unwrap();
    fs::rename(replacement, &fixture.config).unwrap();

    let second = command(&fixture.config).output().unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    first.kill().unwrap();
    first.wait().unwrap();
    let mut after_exit = command(&fixture.config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(
        after_exit.try_wait().unwrap().is_none(),
        "lock was not released"
    );
    after_exit.kill().unwrap();
    after_exit.wait().unwrap();
}

#[test]
fn background_service_can_be_listed_tailed_and_stopped() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{"type":"wait","reason":"keep service running","retry_after_seconds":1}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let registry = fixture.dir.path().join("service-registry");
    let id = "background-goal";

    let added = goal_command(&registry)
        .arg("add")
        .arg(&fixture.config)
        .args(["--id", id])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let started = goal_command(&registry)
        .args(["up", id, "--output", "json"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    let pid = started["service"]["pid"].as_u64().unwrap();

    struct Cleanup {
        armed: bool,
        id: String,
        registry: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if self.armed {
                let _ = goal_command(&self.registry)
                    .args(["down", &self.id])
                    .output();
            }
        }
    }
    let mut cleanup = Cleanup {
        armed: true,
        id: id.to_owned(),
        registry: registry.clone(),
    };

    let listed = listed_goals(&registry);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["enabled"], true);
    assert_eq!(listed[0]["status"], "running");
    assert_eq!(listed[0]["pid"], pid);
    assert_eq!(
        listed[0]["config_path"],
        fs::canonicalize(&fixture.config)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );

    let duplicate = goal_command(&registry)
        .args(["up", id, "--output", "json"])
        .output()
        .unwrap();
    assert!(
        duplicate.status.success(),
        "{}",
        String::from_utf8_lossy(&duplicate.stderr)
    );
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("already_running"));

    let service_log = fixture.dir.path().join(".goal/service.log");
    wait_for("service log output", Duration::from_secs(3), || {
        fs::read_to_string(&service_log)
            .map(|log| log.contains("keep service running"))
            .unwrap_or(false)
    });

    let tailed = goal_command(&registry)
        .args(["tail", id, "--lines", "20"])
        .output()
        .unwrap();
    assert!(tailed.status.success());
    assert!(String::from_utf8_lossy(&tailed.stdout).contains("keep service running"));

    let stopped = goal_command(&registry)
        .args(["down", id, "--output", "json"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    cleanup.armed = false;

    let listed = listed_goals(&registry);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["enabled"], true);
    assert_eq!(listed[0]["status"], "stopped");
    assert!(listed[0]["pid"].is_null());
    assert_eq!(
        listed[0]["config_path"],
        fs::canonicalize(&fixture.config).unwrap().to_string_lossy().as_ref()
    );
}

#[test]
fn default_non_tty_output_falls_back_to_plain_and_hides_sensor_protocol() {
    let fixture = Fixture::new(
        r#"printf '{\"private_observation\":\"sensor-payload\"}'"#,
        r#"grep -q 'sensor-payload' "$GOAL_PROMPT_PATH"; printf '{\"type\":\"complete\",\"summary\":\"observation received\"}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = fixture.run();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("goal complete: observation received"));
    assert!(!stdout.contains("sensor-payload"));
    assert!(
        !stdout.contains("\u{1b}["),
        "non-TTY output contained terminal escapes"
    );
}

#[test]
fn sensor_protocol_is_hidden_in_pretty_but_retained_in_json_and_artifacts() {
    for mode in ["pretty", "json"] {
        let fixture = Fixture::new(
            r#"printf '{"private_observation":"sensor-payload"}'; echo sensor-diagnostic >&2"#,
            r#"grep -q 'sensor-payload' "$GOAL_PROMPT_PATH"; printf '{"type":"complete","summary":"observation received"}' > "$GOAL_RESULT_PATH""#,
            r#"exit 99"#,
        );
        let output = command(&fixture.config)
            .args(["--output", mode])
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("observation received"));
        if mode == "pretty" {
            assert!(!stdout.contains("sensor-payload"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("sensor-diagnostic"));
        } else {
            assert!(output.stderr.is_empty());
            let messages = stdout.lines().map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()
            }).collect::<Vec<_>>();
            assert!(messages.iter().any(|message| {
                message["type"] == "child_output"
                    && message["details"]["role"] == "sensor"
                    && message["details"]["payload"]["private_observation"] == "sensor-payload"
            }));
            assert!(stdout.contains("sensor-diagnostic"));
        }
        assert!(fs::read_dir(fixture.dir.path().join(".goal/runs"))
            .unwrap()
            .any(|entry| {
                fs::read(entry.unwrap().path().join("stdout.log"))
                    .is_ok_and(|bytes| bytes == br#"{"private_observation":"sensor-payload"}"#)
            }));
    }
}

#[test]
fn pretty_output_formats_child_json_but_keeps_run_log_exact() {
    let diagnostic =
        r#"{"type":"diagnostic","nested":{"values":[1,true,null]},"text":"first\nsecond"}"#;
    let fixture = Fixture::new(
        r#"printf '{"healthy":true}'"#,
        &format!(
            "printf '%s\\n' '{diagnostic}'; printf '%s' '{{\"type\":\"complete\",\"summary\":\"pretty complete\"}}' > \"$GOAL_RESULT_PATH\""
        ),
        r#"exit 99"#,
    );
    let output = command(&fixture.config)
        .args(["--output", "pretty"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[decider] {"));
    let nested = stdout
        .lines()
        .find(|line| line.contains("\"nested\": {"))
        .expect("missing pretty nested object");
    let values = stdout
        .lines()
        .find(|line| line.contains("\"values\": ["))
        .expect("missing pretty nested array");
    assert!(!nested.contains("[decider]"));
    assert!(!values.contains("[decider]"));
    assert!(!stdout.contains(&format!("[decider] {diagnostic}")));

    let decider_dir = fs::read_dir(fixture.dir.path().join(".goal/runs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-decider")
        })
        .expect("missing decider run directory");
    assert_eq!(
        fs::read_to_string(decider_dir.join("stdout.log")).unwrap(),
        format!("{diagnostic}\n")
    );
}

#[test]
fn wait_retry_delay_is_not_extended_by_the_cycle_interval() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"wait","reason":"retry immediately","retry_after_seconds":0}'; else r='{"type":"complete","summary":"retried"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    fixture.set_interval(5);

    let output = command(&fixture.config)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let messages = stdout.lines().map(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()
    }).collect::<Vec<_>>();
    let wait_index = messages.iter().position(|event| event["type"] == "wait").unwrap();
    let wait = &messages[wait_index];
    assert_eq!(wait["details"]["actual_seconds"], 0);
    let next_cycle = messages[wait_index + 1..].iter()
        .find(|event| event["type"] == "cycle_started")
        .expect("wait must be followed by another cycle");
    // Measure the retry boundary, not unrelated startup and child process time.
    // An incorrectly added five-second interval still exceeds this bound.
    let elapsed = next_cycle["timestamp"].as_u64().unwrap()
        - wait["timestamp"].as_u64().unwrap();
    assert!(elapsed < 3, "zero-second retry took {elapsed} seconds");
    assert_eq!(fixture.count("sensor-count"), 2);
}

#[test]
fn sense_task_done_resense_complete() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"do one thing"}'; else r='{"type":"complete","summary":"observed done"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf '{"type":"done","summary":"changed and checked"}' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("{\"sense\":"));
}

#[test]
fn goal_file_is_reloaded_between_cycles_with_one_snapshot_per_cycle() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then grep -q 'Reach the fake goal safely.' "$1"; r='{"type":"run_task","task":"update the goal"}'; else grep -q 'Use the updated goal.' "$1"; r='{"type":"complete","summary":"observed updated goal"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"cat > worker-prompt; grep -q 'Reach the fake goal safely.' worker-prompt; printf 'Use the updated goal.\n' > GOAL.md; printf '{"type":"done","summary":"updated goal file"}' > "$GOAL_RESULT_PATH""#,
    );

    let output = fixture.run();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert!(
        fs::read_to_string(fixture.dir.path().join("worker-prompt"))
            .unwrap()
            .contains("Reach the fake goal safely.")
    );
}

#[test]
fn stats_and_analysis_require_and_use_a_registered_id() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"do one thing"}'; else r='{"type":"complete","summary":"observed done"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"printf '{"type":"done","summary":"changed and checked"}' > "$GOAL_RESULT_PATH""#,
    );
    assert!(fixture.run().status.success());

    let output = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .args(["--since", "24h", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 5);
    assert_eq!(report["legacy_runs_without_metadata_all_time"], 0);
    assert_eq!(report["outcomes"]["success"], 5);
    assert_eq!(report["worker_success_rate"], 1.0);
    assert_eq!(report["roles"]["worker"]["duration_ms"]["count"], 1);

    let output = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("5 recorded"));

    let output = goal_command(&fixture.registry)
        .arg("analysis")
        .arg(TEST_GOAL_ID)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["window"]["label"],
        format!("local date {}", chrono::Local::now().date_naive())
    );
    assert_eq!(report["recorded_runs"], 5);
    assert_eq!(report["activity"]["senses_succeeded"], 2);
    assert_eq!(report["activity"]["decisions"]["run_task"], 1);
    assert_eq!(report["activity"]["decisions"]["complete"], 1);
    assert_eq!(report["issues"].as_array().unwrap().len(), 0);
    assert!(
        report["quality_caveat"]
            .as_str()
            .unwrap()
            .contains("not independent proof")
    );
}

#[test]
fn sensor_process_failure_is_recorded_and_retried_after_resensing() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; if test "$n" = 1; then echo 'transient sensor failure' >&2; exit 1; fi; printf '{"sense":%s}\n' "$n""#,
        r#"echo 1 > decider-count; printf '{"type":"complete","summary":"recovered after sensor failure"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 0);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "sense_failed")
        .expect("missing sensor failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("process exited unsuccessfully")
    );
    assert!(failure["details"]["run_id"].is_string());
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));

    let stats = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(stats.status.success());
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 3);
    assert_eq!(report["outcomes"]["success"], 2);
    assert_eq!(report["outcomes"]["failure"], 1);
    assert_eq!(report["failures_by_kind"]["process"], 1);
}

#[test]
fn decider_process_failure_is_recorded_and_retried_after_resensing() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then exit 1; fi; printf '{"type":"complete","summary":"recovered after decider process failure"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 0);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "decider_failed")
        .expect("missing decider failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("process exited unsuccessfully")
    );
    assert!(failure["details"]["run_id"].is_string());
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn malformed_decider_result_is_recorded_and_retried_after_resensing() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"unterminated"'; else r='{"type":"complete","summary":"recovered after malformed decision"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 0);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "decider_failed")
        .expect("missing decider failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("protocol failure")
    );
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn worker_reported_failure_is_recorded_and_the_next_cycle_can_complete() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"requires unavailable access"}'; else r='{"type":"complete","summary":"continued after task failure"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf '{"type":"failure","reason":"deployment credentials are unavailable"}' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    let completion = events
        .iter()
        .find(|event| event["type"] == "worker_completed")
        .expect("missing worker completion event");
    assert_eq!(completion["details"]["completion"]["type"], "failure");
    assert_eq!(
        completion["details"]["completion"]["reason"],
        "deployment credentials are unavailable"
    );
    assert!(events.iter().any(|event| event["type"] == "complete"));

    let stats = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(stats.status.success());
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 5);
    assert_eq!(report["outcomes"]["success"], 4);
    assert_eq!(report["outcomes"]["failure"], 1);
    assert_eq!(report["worker_success_rate"], 0.0);
    assert_eq!(report["failures_by_kind"]["logical"], 1);
}

#[test]
fn worker_process_failure_is_recorded_and_the_controller_resenses() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"possibly partial"}'; else grep -q 'Worker invocation failed after it may have modified external state' "$1"; r='{"type":"complete","summary":"recovered after worker process failure"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; exit 8"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "worker_failed")
        .expect("missing worker failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("process exited unsuccessfully")
    );
    assert_eq!(failure["details"]["completion"]["type"], "failure");
    assert_eq!(failure["details"]["recovery"], "resense");
    assert_eq!(failure["details"]["retry_after_seconds"], 5);
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn missing_worker_result_is_recorded_and_the_controller_resenses() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"missing result"}'; else r='{"type":"complete","summary":"recovered after missing result"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; exit 0"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);
    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "worker_failed")
        .expect("missing worker failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("protocol failure")
    );
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn malformed_worker_result_is_recorded_and_the_controller_resenses() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"malformed result"}'; else r='{"type":"complete","summary":"recovered after malformed result"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf 'not json' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);
    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "worker_failed")
        .expect("missing worker failure event");
    assert!(
        failure["details"]["error"]
            .as_str()
            .unwrap()
            .contains("protocol failure")
    );
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn decider_reported_failure_is_recorded_and_the_next_cycle_can_complete() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"failure","reason":"goal requires unavailable authority"}'; else r='{"type":"complete","summary":"recovered after decider failure"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count"#,
    );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 0);

    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| {
            event["type"] == "decision" && event["details"]["action"]["type"] == "failure"
        })
        .expect("missing decider failure decision");
    assert_eq!(
        failure["details"]["action"]["reason"],
        "goal requires unavailable authority"
    );
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));

    let stats = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(stats.status.success());
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 4);
    assert_eq!(report["outcomes"]["success"], 3);
    assert_eq!(report["outcomes"]["failure"], 1);
    assert_eq!(report["failures_by_kind"]["logical"], 1);
}

#[test]
fn worker_timeout_is_recorded_and_the_controller_resenses() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"times out"}'; else r='{"type":"complete","summary":"recovered after worker timeout"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; sleep 10"#,
    );
    fixture.set_timeout("worker", 3);
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);
    let events = fs::read_to_string(fixture.dir.path().join(".goal/events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = events
        .iter()
        .find(|event| event["type"] == "worker_failed")
        .expect("missing worker failure event");
    assert_eq!(failure["details"]["error"], "process timed out");
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
}

#[test]
fn json_decider_failure_output_is_structured_and_the_next_cycle_can_complete() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"failure","reason":"automatic authority is unavailable"}'; else r='{"type":"complete","summary":"recovered"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = command(&fixture.config)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());

    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let failure = events
        .iter()
        .find(|event| {
            event["type"] == "decision" && event["details"]["action"]["type"] == "failure"
        })
        .unwrap();
    assert_eq!(
        failure["details"]["action"]["reason"],
        "automatic authority is unavailable"
    );
    assert!(!events.iter().any(|event| event["type"] == "failure"));
    assert!(events.iter().any(|event| event["type"] == "complete"));
    assert!(!events.iter().any(|event| event["type"] == "error"));
}

#[test]
fn ctrl_c_terminates_worker_and_does_not_start_another_cycle() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"long task"}' > "$GOAL_RESULT_PATH""#,
        r#"echo started > worker-started; sleep 10; printf '{"type":"done","summary":"late"}' > "$GOAL_RESULT_PATH""#,
    );
    fixture.set_timeout("worker", 10);
    let mut child = command(&fixture.config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marker = fixture.dir.path().join("worker-started");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "worker did not start");
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let controller_status = child.wait().unwrap();
    assert!(
        controller_status.success(),
        "intentional interruption should be a clean exit: {controller_status}"
    );
    thread::sleep(Duration::from_millis(150));
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
}

#[test]
fn bounded_batch_overlaps_to_the_cap_rolls_the_fixed_queue_and_orders_results() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; if test "$n" -gt 1; then for i in 0 1 2 3; do test -f "finished-$i" || touch sensor-ran-early; done; fi; printf '{"sense":%s}\n' "$n""#,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_tasks","tasks":["BATCH task zero","BATCH task one","BATCH task two","BATCH task three"],"concurrency":4}'; else cat "$1" > next-decider-prompt; r='{"type":"complete","summary":"observed the whole batch"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"
acquire() { while ! mkdir active-lock 2>/dev/null; do sleep 0.01; done; }
release() { rmdir active-lock; }
wait_marker() { n=0; while test ! -f "$1"; do n=$((n+1)); test "$n" -lt 300 || exit 70; sleep 0.01; done; }
prompt=$(cat)
case "$prompt" in
  *"BATCH task zero"*) task=0 ;;
  *"BATCH task one"*) task=1 ;;
  *"BATCH task two"*) task=2 ;;
  *"BATCH task three"*) task=3 ;;
  *) exit 71 ;;
esac
acquire
active=0; test ! -f active-count || active=$(cat active-count)
active=$((active+1)); echo "$active" > active-count
maximum=0; test ! -f max-active || maximum=$(cat max-active)
if test "$active" -gt "$maximum"; then echo "$active" > max-active; fi
release
finish() { acquire; active=$(cat active-count); active=$((active-1)); echo "$active" > active-count; release; }
trap finish EXIT
touch "started-$task"
case "$task" in
  0) wait_marker started-1; touch finished-0 ;;
  1) wait_marker finished-3; touch finished-1 ;;
  2) test -f finished-0 || touch rolling-admission-violation; touch finished-2 ;;
  3) touch finished-3 ;;
esac
printf '{"type":"done","summary":"completed task %s"}' "$task" > "$GOAL_RESULT_PATH"
"#,
    );
    fixture.set_max_concurrency(2);

    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("max-active"), 2);
    assert!(!fixture.dir.path().join("rolling-admission-violation").exists());
    assert!(!fixture.dir.path().join("sensor-ran-early").exists());

    let events = events(&fixture);
    let started = events
        .iter()
        .find(|event| event["type"] == "worker_batch_started")
        .expect("missing worker_batch_started");
    assert_eq!(started["details"]["task_count"], 4);
    assert_eq!(started["details"]["requested_concurrency"], 4);
    assert_eq!(started["details"]["concurrency"], 2);
    let finished = events
        .iter()
        .find(|event| event["type"] == "worker_batch_finished")
        .expect("missing worker_batch_finished");
    assert_eq!(finished["details"]["task_count"], 4);

    let mut completions = events
        .iter()
        .filter(|event| event["type"] == "worker_completed")
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 4);
    let arrived = completions.iter()
        .map(|event| event["details"]["task_index"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(arrived.iter().position(|index| *index == 2).unwrap()
        < arrived.iter().position(|index| *index == 1).unwrap());
    completions.sort_by_key(|event| event["details"]["task_index"].as_u64().unwrap());
    let batch_id = started["details"]["batch_id"].as_str().unwrap();
    let mut run_ids = Vec::new();
    for (index, completion) in completions.iter().enumerate() {
        assert_eq!(completion["details"]["task_index"], index);
        assert_eq!(completion["details"]["batch_id"], batch_id);
        assert_eq!(
            completion["details"]["task"],
            format!("BATCH task {}", ["zero", "one", "two", "three"][index])
        );
        run_ids.push(completion["details"]["run_id"].as_str().unwrap());
    }
    run_ids.sort_unstable();
    run_ids.dedup();
    assert_eq!(run_ids.len(), 4);

    let state = state(&fixture);
    assert!(state["latest_worker_completion"].is_null());
    let batch = &state["latest_worker_batch"];
    assert_eq!(batch["batch_id"], batch_id);
    assert_eq!(batch["task_count"], 4);
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result["task_index"], index);
        assert_eq!(result["completion"]["type"], "done");
        assert!(result["run_id"].is_string());
    }

    let prompt = fs::read_to_string(fixture.dir.path().join("next-decider-prompt")).unwrap();
    let mut cursor = 0;
    for task in [
        "BATCH task zero",
        "BATCH task one",
        "BATCH task two",
        "BATCH task three",
    ] {
        let position = prompt[cursor..]
            .find(task)
            .unwrap_or_else(|| panic!("missing ordered task {task:?} from next prompt"));
        cursor += position + task.len();
    }
}

#[test]
fn max_concurrency_defaults_to_one_without_truncating_the_batch() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; if test "$n" -gt 1; then for i in 0 1 2; do test -f "serial-finished-$i" || touch sensor-ran-early; done; fi; printf '{"sense":%s}\n' "$n""#,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_tasks","tasks":["SERIAL zero","SERIAL one","SERIAL two"],"concurrency":3}'; else r='{"type":"complete","summary":"serial batch settled"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"
acquire() { while ! mkdir serial-active-lock 2>/dev/null; do sleep 0.01; done; }
release() { rmdir serial-active-lock; }
prompt=$(cat)
case "$prompt" in *"SERIAL zero"*) task=0 ;; *"SERIAL one"*) task=1 ;; *"SERIAL two"*) task=2 ;; *) exit 72 ;; esac
acquire
active=0; test ! -f serial-active || active=$(cat serial-active)
active=$((active+1)); echo "$active" > serial-active
maximum=0; test ! -f serial-max-active || maximum=$(cat serial-max-active)
if test "$active" -gt "$maximum"; then echo "$active" > serial-max-active; fi
release
finish() { acquire; active=$(cat serial-active); active=$((active-1)); echo "$active" > serial-active; release; }
trap finish EXIT
sleep 0.05
touch "serial-finished-$task"
printf '{"type":"done","summary":"serial %s"}' "$task" > "$GOAL_RESULT_PATH"
"#,
    );

    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("serial-max-active"), 1);
    assert!(!fixture.dir.path().join("sensor-ran-early").exists());
    for index in 0..3 {
        assert!(fixture.dir.path().join(format!("serial-finished-{index}")).exists());
    }
    let events = events(&fixture);
    let started = events
        .iter()
        .find(|event| event["type"] == "worker_batch_started")
        .unwrap();
    assert_eq!(started["details"]["requested_concurrency"], 3);
    assert_eq!(started["details"]["concurrency"], 1);
}

#[test]
fn requested_one_is_respected_and_requests_are_clamped_to_task_count() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; if test "$n" = 2; then test -f a-finished-0 && test -f a-finished-1 || touch sensor-ran-early; elif test "$n" -gt 2; then test -f b-finished-0 && test -f b-finished-1 || touch sensor-ran-early; fi; printf '{"sense":%s}\n' "$n""#,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_tasks","tasks":["REQUEST ONE A0","REQUEST ONE A1"],"concurrency":1}'; elif test "$n" = 2; then r='{"type":"run_tasks","tasks":["CLAMP COUNT B0","CLAMP COUNT B1"],"concurrency":9}'; else r='{"type":"complete","summary":"both batches settled"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"
wait_marker() { n=0; while test ! -f "$1"; do n=$((n+1)); test "$n" -lt 300 || exit 73; sleep 0.01; done; }
prompt=$(cat)
case "$prompt" in
  *"CLAMP COUNT B0"*) touch b-started-0; wait_marker b-started-1; touch b-finished-0; summary=B0 ;;
  *"CLAMP COUNT B1"*) touch b-started-1; wait_marker b-started-0; touch b-finished-1; summary=B1 ;;
  *"REQUEST ONE A0"*) test ! -f a-finished-1 || touch requested-one-reordered; touch a-finished-0; summary=A0 ;;
  *"REQUEST ONE A1"*) test -f a-finished-0 || touch requested-one-overlap; touch a-finished-1; summary=A1 ;;
  *) exit 74 ;;
esac
printf '{"type":"done","summary":"%s"}' "$summary" > "$GOAL_RESULT_PATH"
"#,
    );
    fixture.set_max_concurrency(4);

    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 3);
    assert!(!fixture.dir.path().join("sensor-ran-early").exists());
    assert!(!fixture.dir.path().join("requested-one-overlap").exists());
    assert!(!fixture.dir.path().join("requested-one-reordered").exists());

    let events = events(&fixture);
    let starts = events
        .iter()
        .filter(|event| event["type"] == "worker_batch_started")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0]["details"]["requested_concurrency"], 1);
    assert_eq!(starts[0]["details"]["concurrency"], 1);
    assert_eq!(starts[1]["details"]["requested_concurrency"], 9);
    assert_eq!(starts[1]["details"]["concurrency"], 2);
}

#[test]
fn batch_collects_logical_process_protocol_and_timeout_failures_without_stopping_peers() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; if test "$n" -gt 1; then for task in process malformed logical timeout done; do test -f "mixed-$task" || touch sensor-ran-early; done; fi; printf '{"sense":%s}\n' "$n""#,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_tasks","tasks":["MIXED process","MIXED malformed","MIXED logical","MIXED timeout","MIXED done"],"concurrency":2}'; else cat "$1" > mixed-next-prompt; r='{"type":"complete","summary":"observed mixed outcomes"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"
prompt=$(cat)
case "$prompt" in
  *"MIXED done"*) touch mixed-done; printf '{"type":"done","summary":"verified success"}' > "$GOAL_RESULT_PATH" ;;
  *"MIXED logical"*) touch mixed-logical; printf '{"type":"failure","reason":"expected logical refusal"}' > "$GOAL_RESULT_PATH" ;;
  *"MIXED process"*) touch mixed-process; exit 8 ;;
  *"MIXED malformed"*) touch mixed-malformed; printf 'not json' > "$GOAL_RESULT_PATH" ;;
  *"MIXED timeout"*) touch mixed-timeout; sleep 10 ;;
  *) exit 75 ;;
esac
"#,
    );
    fixture.set_max_concurrency(2);
    // Leave startup headroom under parallel test load. The timeout task still
    // sleeps for 10 seconds, so its real timeout and all outcomes remain tested.
    fixture.set_timeout("worker", 3);

    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert!(!fixture.dir.path().join("sensor-ran-early").exists());
    for task in ["process", "malformed", "logical", "timeout", "done"] {
        assert!(fixture.dir.path().join(format!("mixed-{task}")).exists());
    }

    let events = events(&fixture);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "worker_completed")
            .count(),
        2
    );
    let failures = events
        .iter()
        .filter(|event| event["type"] == "worker_failed")
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 3);
    assert!(failures.iter().all(|event| {
        event["details"]["retry_after_seconds"] == 5
            && event["details"]["recovery"] == "resense"
            && event["details"]["batch_id"].is_string()
            && event["details"]["task_index"].is_number()
            && event["details"]["task"].is_string()
    }));
    assert!(failures.iter().any(|event| {
        event["details"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("process exited unsuccessfully"))
    }));
    assert!(failures.iter().any(|event| {
        event["details"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("protocol failure"))
    }));
    assert!(events
        .iter()
        .any(|event| event["type"] == "worker_batch_finished"));

    let state = state(&fixture);
    let results = state["latest_worker_batch"]["results"]
        .as_array()
        .unwrap();
    assert_eq!(results.len(), 5);
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result["task_index"], index);
        assert!(result["run_id"].is_string());
        assert_eq!(result["completion"]["type"], if index == 4 { "done" } else { "failure" });
    }

    let prompt = fs::read_to_string(fixture.dir.path().join("mixed-next-prompt")).unwrap();
    let mut cursor = 0;
    for task in ["MIXED process", "MIXED malformed", "MIXED logical", "MIXED timeout", "MIXED done"] {
        let position = prompt[cursor..]
            .find(task)
            .unwrap_or_else(|| panic!("missing ordered mixed result for {task:?}"));
        cursor += position + task.len();
    }
    assert!(prompt.contains("expected logical refusal"));
    assert!(prompt.contains("Worker invocation failed after it may have modified external state"));

    let stats = goal_command(&fixture.registry)
        .arg("stats")
        .arg(TEST_GOAL_ID)
        .args(["--output", "json"])
        .output()
        .unwrap();
    assert!(stats.status.success());
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 9);
    assert_eq!(report["outcomes"]["success"], 5);
    assert_eq!(report["outcomes"]["failure"], 4);
    assert_eq!(report["roles"]["worker"]["duration_ms"]["count"], 5);
    assert_eq!(report["failures_by_kind"]["timeout"], 1);
    assert_eq!(report["failures_by_kind"]["logical"], 1);
    assert_eq!(report["failures_by_kind"]["process"], 1);
    assert_eq!(report["failures_by_kind"]["protocol"], 1);
}

#[test]
fn cancelling_a_batch_drains_active_workers_and_never_starts_the_queue() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_tasks","tasks":["CANCEL zero","CANCEL one","CANCEL queued"],"concurrency":2}' > "$GOAL_RESULT_PATH""#,
        r#"
prompt=$(cat)
case "$prompt" in *"CANCEL zero"*) task=0 ;; *"CANCEL one"*) task=1 ;; *"CANCEL queued"*) touch queued-started; task=2 ;; *) exit 76 ;; esac
printf '%s\n' "$$" > "worker-pid-$task"
printf '%s\n' "$GOAL_WORK_DIR" > "worker-work-dir-$task"
touch "worker-started-$task"
sleep 30
printf '{"type":"done","summary":"late %s"}' "$task" > "$GOAL_RESULT_PATH"
"#,
    );
    fixture.set_max_concurrency(2);
    fixture.set_timeout("worker", 30);
    let mut child = ChildGuard::new(
        command(&fixture.config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    wait_for("both active workers", Duration::from_secs(10), || {
        fixture.dir.path().join("worker-started-0").exists()
            && fixture.dir.path().join("worker-started-1").exists()
    });
    child.interrupt();
    let controller_status = child.wait();
    assert!(
        controller_status.success(),
        "intentional interruption should be a clean exit: {controller_status}"
    );

    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert!(!fixture.dir.path().join("queued-started").exists());
    let pids = [0, 1].map(|index| {
        fs::read_to_string(fixture.dir.path().join(format!("worker-pid-{index}")))
            .unwrap()
            .trim()
            .to_owned()
    });
    wait_for("worker process groups to terminate", Duration::from_secs(5), || {
        pids.iter().all(|pid| !process_exists(pid))
    });
    for index in 0..2 {
        let work_dir = fs::read_to_string(
            fixture
                .dir
                .path()
                .join(format!("worker-work-dir-{index}")),
        )
        .unwrap();
        assert!(
            !Path::new(work_dir.trim()).exists(),
            "worker {index} temporary directory was not removed"
        );
    }

    let events = events(&fixture);
    let cancelled = events
        .iter()
        .filter(|event| event["type"] == "worker_cancelled")
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 2);
    assert!(cancelled.iter().all(|event| {
        event["details"]["batch_id"].is_string()
            && event["details"]["task_index"].is_number()
            && event["details"]["task"].is_string()
    }));
    assert!(!events
        .iter()
        .any(|event| event["type"] == "worker_batch_finished"));
    let state = state(&fixture);
    let batch = &state["latest_worker_batch"];
    assert_eq!(batch["task_count"], 3);
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["task_index"], 0);
    assert_eq!(results[1]["task_index"], 1);
    assert!(results
        .iter()
        .all(|result| result["completion"]["type"] == "failure"));
}

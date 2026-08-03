#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new(sensor: &str, decider: &str, worker: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
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
        Self { dir, config }
    }

    fn run(&self) -> Output {
        command(&self.config).output().unwrap()
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

    fn failure_event(&self) -> serde_json::Value {
        fs::read_to_string(self.dir.path().join(".goal/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|event| event["type"] == "failure")
            .expect("missing terminal failure event")
    }
}

fn command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_goal"));
    command
        .current_dir(config.parent().unwrap())
        .env_remove("GOAL_DIR");
    command
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

const COUNT_SENSOR: &str = r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; printf '{"sense":%s}\n' "$n""#;

#[test]
fn help_fully_describes_configuration_and_process_contracts() {
    let root = Command::new(env!("CARGO_BIN_EXE_goal"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(root.status.success());
    let root = String::from_utf8(root.stdout).unwrap();
    for expected in [
        "sense -> decide -> act -> sense",
        "multiple goals",
        ".goal/",
        "goal run",
        "GOAL_DIR=goals/ci goal",
        "GOAL_DIR",
        "stats",
    ] {
        assert!(
            root.contains(expected),
            "missing {expected:?} from root help"
        );
    }
    assert!(!root.contains("--goal-dir"));
    assert!(!root.contains("CONFIG_OR_DIR"));

    let run = Command::new(env!("CARGO_BIN_EXE_goal"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(run.status.success());
    let run = String::from_utf8(run.stdout).unwrap();
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
        "failure",
        "Neither process may request human input",
        "exit non-zero",
        "FAILURE ANALYSIS",
    ] {
        assert!(run.contains(expected), "missing {expected:?} from run help");
    }
}

#[test]
fn removed_explicit_goal_selectors_are_rejected() {
    for args in [["some-goal", ""], ["-C", "some-goal"]] {
        let args = args.into_iter().filter(|arg| !arg.is_empty());
        let output = Command::new(env!("CARGO_BIN_EXE_goal"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn json_output_is_strict_jsonl_with_one_common_envelope() {
    let fixture = Fixture::new(
        r#"echo sensor-diagnostic >&2; printf '{"healthy":true}'"#,
        r#"echo '{"pi_event":"diagnostic"}'; echo plain-diagnostic >&2; if test -f decider-once; then r='{"type":"complete","summary":"JSON complete"}'; else touch decider-once; r='{"type":"wait","reason":"observe JSON wait","retry_after_seconds":0}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_goal"))
        .args(["--output", "json"])
        .current_dir(fixture.dir.path())
        .env_remove("GOAL_DIR")
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
fn stats_reports_metadata_from_goal_dir_or_current_directory() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"do one thing"}'; else r='{"type":"complete","summary":"observed done"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"printf '{"type":"done","summary":"changed and checked"}' > "$GOAL_RESULT_PATH""#,
    );
    assert!(fixture.run().status.success());

    let output = Command::new(env!("CARGO_BIN_EXE_goal"))
        .args(["stats", "--since", "24h", "--output", "json"])
        .env("GOAL_DIR", fixture.dir.path())
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

    let output = Command::new(env!("CARGO_BIN_EXE_goal"))
        .arg("stats")
        .env("GOAL_DIR", fixture.dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("worker success rate: 100.0%"));

    let output = Command::new(env!("CARGO_BIN_EXE_goal"))
        .arg("stats")
        .current_dir(fixture.dir.path())
        .env_remove("GOAL_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("5 recorded"));
}

#[test]
fn sensor_timeout_is_terminal_and_never_calls_decider() {
    let fixture = Fixture::new(
        r#"echo 1 > sensor-count; sleep 10"#,
        r#"echo 1 > decider-count"#,
        r#"echo 1 > worker-count"#,
    );
    fixture.set_timeout("sensor", 3);
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 0);
    assert_eq!(fixture.count("worker-count"), 0);
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "sensor");
    assert_eq!(failure["details"]["reason"], "process timed out");
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn decider_timeout_is_terminal_and_never_calls_worker() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; sleep 10"#,
        r#"echo 1 > worker-count"#,
    );
    fixture.set_timeout("decider", 3);
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 0);
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "decider");
    assert_eq!(failure["details"]["reason"], "process timed out");
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn worker_reported_failure_is_recorded_and_exits_without_another_cycle() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"requires unavailable access"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf '{"type":"failure","reason":"deployment credentials are unavailable"}' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("worker reported failure"), "{stderr}");
    assert!(
        stderr.contains("deployment credentials are unavailable"),
        "{stderr}"
    );

    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "worker");
    assert_eq!(
        failure["details"]["reason"],
        "deployment credentials are unavailable"
    );
    assert!(failure["details"]["run_id"].is_string());

    let stats = Command::new(env!("CARGO_BIN_EXE_goal"))
        .args(["stats", "--output", "json"])
        .current_dir(fixture.dir.path())
        .env_remove("GOAL_DIR")
        .output()
        .unwrap();
    assert!(stats.status.success());
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["recorded_runs"], 3);
    assert_eq!(report["outcomes"]["success"], 2);
    assert_eq!(report["outcomes"]["failure"], 1);
    assert_eq!(report["worker_success_rate"], 0.0);
    assert_eq!(report["failures_by_kind"]["logical"], 1);
}

#[test]
fn worker_process_failure_is_terminal_and_is_not_retried() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"possibly partial"}' > "$GOAL_RESULT_PATH""#,
        r#"n=0; test ! -f worker-count || n=$(cat worker-count); n=$((n+1)); echo "$n" > worker-count; exit 8"#,
    );
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("worker reported failure"), "{stderr}");
    assert!(stderr.contains("process exited unsuccessfully"), "{stderr}");
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "worker");
    assert!(
        failure["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("process exited unsuccessfully")
    );
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn missing_worker_result_is_terminal_protocol_failure() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"missing result"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; exit 0"#,
    );
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("protocol failure"));
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "worker");
    assert!(
        failure["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("protocol failure")
    );
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn malformed_worker_result_is_terminal_protocol_failure() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"malformed result"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf 'not json' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 1);
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "worker");
    assert!(
        failure["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("protocol failure")
    );
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn decider_reported_failure_exits_without_starting_worker() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"failure","reason":"goal requires unavailable authority"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count"#,
    );
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("decider reported failure"), "{stderr}");
    assert!(
        stderr.contains("goal requires unavailable authority"),
        "{stderr}"
    );
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "decider");
    assert_eq!(
        failure["details"]["reason"],
        "goal requires unavailable authority"
    );
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn worker_timeout_is_terminal_and_is_not_retried() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"times out"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; sleep 10"#,
    );
    fixture.set_timeout("worker", 3);
    let output = fixture.run();
    assert!(!output.status.success());
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 1);
    assert_eq!(fixture.count("worker-count"), 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("process timed out"));
    let failure = fixture.failure_event();
    assert_eq!(failure["details"]["source"], "worker");
    assert_eq!(failure["details"]["reason"], "process timed out");
    assert!(failure["details"]["run_id"].is_string());
}

#[test]
fn json_failure_output_is_structured_and_keeps_the_reason() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"printf '{"type":"failure","reason":"automatic authority is unavailable"}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_goal"))
        .args(["--output", "json"])
        .current_dir(fixture.dir.path())
        .env_remove("GOAL_DIR")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());

    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let failure = events
        .iter()
        .find(|event| event["type"] == "failure")
        .unwrap();
    assert_eq!(failure["details"]["source"], "decider");
    assert_eq!(
        failure["details"]["reason"],
        "automatic authority is unavailable"
    );
    assert!(failure["details"]["run_id"].is_string());
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

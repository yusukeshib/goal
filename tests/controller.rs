#![cfg(unix)]

use std::{
    fs,
    io::Write,
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
retry_seconds = 0
max_wait_seconds = 1
[sensor]
command = ["{}"]
timeout_seconds = 1
[decider]
command = ["{}", "{{prompt}}"]
timeout_seconds = 1
[worker]
command = ["{}"]
timeout_seconds = 1
"#,
                sensor.display(),
                decider.display(),
                worker.display()
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn run(&self, input: &str) -> Output {
        let mut child = command(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn count(&self, name: &str) -> u64 {
        fs::read_to_string(self.dir.path().join(name))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }
}

fn command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_goal"));
    command.arg(config);
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
        "goal goals/ci/goal.toml",
        "goal goals/ci/",
        "CONFIG_OR_DIR",
    ] {
        assert!(
            root.contains(expected),
            "missing {expected:?} from root help"
        );
    }

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
        "needs_input",
        "EOF or interruption",
        "failed worker is not blindly rerun",
    ] {
        assert!(run.contains(expected), "missing {expected:?} from run help");
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
        .arg(&fixture.config)
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

    let second = command(fixture.dir.path()).output().unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    first.kill().unwrap();
    first.wait().unwrap();
    let mut after_exit = command(fixture.dir.path())
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
fn sensor_stdout_is_hidden_in_plain_mode_but_reaches_decider() {
    let fixture = Fixture::new(
        r#"printf '{\"private_observation\":\"sensor-payload\"}'"#,
        r#"grep -q 'sensor-payload' "$GOAL_PROMPT_PATH"; printf '{\"type\":\"complete\",\"summary\":\"observation received\"}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = fixture.run("");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("goal complete: observation received"));
    assert!(!stdout.contains("sensor-payload"));
}

#[test]
fn sense_task_done_resense_complete() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"do one thing"}'; else r='{"type":"complete","summary":"observed done"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; printf '{"type":"done","summary":"changed and checked"}' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run("");
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
fn worker_needs_input_goes_directly_to_human_then_fresh_decision() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"prepare choice"}'; else grep -q 'Human answer: blue' "$GOAL_PROMPT_PATH"; r='{"type":"complete","summary":"choice recorded"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"printf '{"type":"needs_input","question":"Which color?","context":"Red or blue","resume_hint":"Use the answer"}' > "$GOAL_RESULT_PATH""#,
    );
    let output = fixture.run("blue\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Which color?"));
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("sensor-count"), 2);
}

#[test]
fn decider_prompt_human_then_fresh_decision() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"prompt_human","question":"Proceed?","context":"Safe boundary"}'; else grep -q 'Human answer: yes' "$GOAL_PROMPT_PATH"; r='{"type":"complete","summary":"approved"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = fixture.run("yes\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("sensor-count"), 2);
}

#[test]
fn japanese_human_answer_is_preserved() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"prompt_human","question":"続行しますか？","context":null}'; else grep -q 'Human answer: はい、続行してください' "$GOAL_PROMPT_PATH"; r='{"type":"complete","summary":"回答を確認"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = fixture.run("はい、続行してください\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("decider-count"), 2);
}

#[test]
fn sensor_failures_never_call_decider_until_valid_observation() {
    let fixture = Fixture::new(
        r#"n=0; test ! -f sensor-count || n=$(cat sensor-count); n=$((n+1)); echo "$n" > sensor-count; case "$n" in 1) exit 3;; 2) printf 'not json';; 3) sleep 2;; *) printf '{"healthy":true}';; esac"#,
        r#"echo 1 > decider-count; printf '{"type":"complete","summary":"valid observation"}' > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let output = fixture.run("");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 4);
    assert_eq!(fixture.count("decider-count"), 1);
}

#[test]
fn decider_failure_retries_without_acting_or_resensing() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then exit 4; fi; printf '{"type":"complete","summary":"retried safely"}' > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; exit 0"#,
    );
    let output = fixture.run("");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 1);
    assert_eq!(fixture.count("decider-count"), 2);
    assert_eq!(fixture.count("worker-count"), 0);
}

#[test]
fn worker_failure_resenses_and_is_not_blindly_rerun() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"possibly partial"}'; else r='{"type":"complete","summary":"reobserved"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"n=0; test ! -f worker-count || n=$(cat worker-count); n=$((n+1)); echo "$n" > worker-count; exit 8"#,
    );
    let output = fixture.run("");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert_eq!(fixture.count("worker-count"), 1);
}

#[test]
fn exit_zero_without_result_is_protocol_failure_then_resense() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"run_task","task":"missing result"}'; else r='{"type":"complete","summary":"recovered"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"echo 1 > worker-count; exit 0"#,
    );
    let output = fixture.run("");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("protocol failure"));
}

#[test]
fn pending_question_survives_eof_and_is_answered_before_restart_senses() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"n=0; test ! -f decider-count || n=$(cat decider-count); n=$((n+1)); echo "$n" > decider-count; if test "$n" = 1; then r='{"type":"prompt_human","question":"Resume?","context":null}'; else grep -q 'Human answer: continue' "$GOAL_PROMPT_PATH"; r='{"type":"complete","summary":"resumed"}'; fi; printf '%s' "$r" > "$GOAL_RESULT_PATH""#,
        r#"exit 99"#,
    );
    let first = fixture.run("");
    assert!(!first.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.dir.path().join(".goal/state.json")).unwrap())
            .unwrap();
    assert!(state["pending_human_question"].is_object());
    assert_eq!(fixture.count("sensor-count"), 1);

    let second = fixture.run("continue\n");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fixture.count("sensor-count"), 2);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.dir.path().join(".goal/state.json")).unwrap())
            .unwrap();
    assert!(state["pending_human_question"].is_null());
}

#[test]
fn ctrl_c_terminates_worker_and_does_not_start_another_cycle() {
    let fixture = Fixture::new(
        COUNT_SENSOR,
        r#"echo 1 > decider-count; printf '{"type":"run_task","task":"long task"}' > "$GOAL_RESULT_PATH""#,
        r#"echo started > worker-started; sleep 10; printf '{"type":"done","summary":"late"}' > "$GOAL_RESULT_PATH""#,
    );
    let mut child = command(&fixture.config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marker = fixture.dir.path().join("worker-started");
    let deadline = Instant::now() + Duration::from_secs(3);
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

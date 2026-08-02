mod analytics;
mod cancel;
mod config;
mod controller;
mod model;
mod output;
mod prompt;
mod runner;
mod state;

use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::Result;
use clap::{Parser, Subcommand};

const ABOUT: &str = "A foreground controller that continuously pursues one natural-language goal";

const ROOT_HELP: &str = r#"HOW IT WORKS
  Each cycle is: sense -> decide -> act -> sense.
  The sensor observes the world, a read-only one-shot decider selects one typed
  action, and at most one disposable worker performs one task. The controller
  runs in the foreground without interactive input or human approval gates.

  One process controls exactly one goal. To run multiple goals, give each goal
  its own directory and goal.toml, then start a separate `goal` process for each.
  Processes that modify the same project are not coordinated.

TARGET SELECTION
  GOAL_DIR selects a goal directory for every command. Without it, goal.toml in
  the current directory is used. Change directory or set GOAL_DIR to work with a
  different goal.

COMMANDS
  With no subcommand, goal runs the controller. `goal run` is the explicit form.
  `goal stats` summarizes recorded child outcomes and durations without starting
  sensor, decider, or worker processes.

FILES
  goal.toml   Commands, timeouts, and the path to the natural-language goal.
  GOAL.md     The goal text (the filename is configurable).
  .goal/      Restart state, events, prompts, results, and process logs.
"#;

const RUN_HELP: &str = r#"CONFIGURATION
  goal.toml is loaded from GOAL_DIR, or from the current directory when GOAL_DIR
  is unset. That directory is the project and child working directory. Relative
  goal and command paths resolve from there.

  Required goal.toml shape:

    goal_file = "GOAL.md"
    interval_seconds = 60
    max_wait_seconds = 3600

    [sensor]
    command = ["./sensor.sh"]
    timeout_seconds = 60

    [decider]
    command = ["agent-cli", "--non-interactive", "{prompt}"]
    timeout_seconds = 300

    [worker]
    command = ["agent-cli", "--non-interactive", "{prompt}"]
    timeout_seconds = 1800

  Commands are argv arrays and are never passed through an implicit shell. To
  use shell syntax, explicitly configure ["sh", "-c", "..."] instead.

GOAL SEMANTICS
  GOAL.md must define what success means and whether the goal is finite or
  continuous.

  For a finite goal, the decider may return `complete` after a fresh observation
  proves the success conditions are satisfied.

  For a continuous goal, explicitly say that temporary health is not completion:
  the decider must return `wait` when no action is currently needed, then sense
  again later. For example:

    Continuously keep all authored open pull requests mergeable.
    Mergeable means all required CI passes, all actionable review feedback is
    resolved, and all merge conflicts are resolved.
    Never return Complete merely because everything is currently healthy.
    When no action is currently needed, return Wait and check again later.

  A goal can only be enforced as completely as its sensor observes reality.
  Define ambiguous terms such as "all feedback", include every relevant source,
  and implement pagination when an API can truncate PRs, checks, comments, or
  review threads. Missing sensor data must not be treated as a healthy world.

SENSOR CONTRACT
  The sensor must be read-only and emit exactly one JSON value on stdout.
  Stderr is diagnostic. Non-zero exit, timeout, or invalid JSON prevents the
  decider from running, records a terminal failure, and exits non-zero.

DECIDER AND WORKER CONTRACT
  Both are non-TUI child processes without a PTY. Every invocation receives:

    GOAL_RUN_ID         unique invocation identifier
    GOAL_PROMPT_PATH    generated prompt.md path
    GOAL_RESULT_PATH    required result.json path
    GOAL_PROJECT_DIR    absolute project directory

  If any argv element contains {prompt}, it is replaced with GOAL_PROMPT_PATH
  and child stdin is closed. Otherwise prompt.md is piped to stdin and closed.
  The child must atomically write exactly one tagged JSON object to
  GOAL_RESULT_PATH. Stdout and stderr are diagnostics only and are never parsed.

  Decider actions (`type` field):
    run_task      {"type":"run_task","task":"one bounded task"}
    wait          {"type":"wait","reason":"...","retry_after_seconds":60}
    complete      {"type":"complete","summary":"..."}
    failure       {"type":"failure","reason":"why automatic progress is impossible"}

  Worker completions (`type` field):
    done          {"type":"done","summary":"actual work and verification"}
    failure       {"type":"failure","reason":"why automatic completion is impossible"}

  Neither process may request human input, approval, or intervention. The
  decider must not modify the project or external world. A worker performs only
  its assigned task, writes one completion, and exits. Any sensor, decider, or
  worker process, timeout, or protocol failure is recorded with its run ID and
  causes the controller to exit non-zero; it is never automatically retried.

FAILURE ANALYSIS
  Runtime data lives under .goal/ beside the config file. Successful and failed
  invocations retain prompts, results, stdout, stderr, and metadata under
  .goal/runs/, and compact outcomes are appended to .goal/events.jsonl. Run
  `goal stats --since 24h` for outcome counts, worker success rate, and
  role-specific average, p50, and p95 durations. Historical directories without
  metadata are counted across all time and excluded from filtered duration and
  success rates.
  Automation must not improve apparent success by weakening success criteria or
  silently waiving unmet requirements.

OUTPUT
  --output plain (default) prints human-readable terminal output. For controller
  runs, --output json emits strict JSONL envelopes on stdout. For stats, it emits
  one JSON report. Child JSON is nested in details.payload; non-JSON diagnostics
  use details.content. Oversized child lines are summarized in the foreground
  stream while stdout.log/stderr.log retain the exact output.

EXAMPLES
  goal                                      Run ./goal.toml
  goal run                                  Explicitly run ./goal.toml
  GOAL_DIR=goals/ci goal                    Run another goal
  GOAL_DIR=goals/ci goal stats --since 24h  Human-readable statistics
  goal stats --since 7d --output json
  goal --output json | jq --unbuffered -C .

  See examples/fake for a deterministic full cycle and
  examples/mergeable-prs for a real read-only GitHub sensor."#;

#[derive(Parser)]
#[command(
    name = "goal",
    version,
    about = ABOUT,
    long_about = ROOT_HELP,
    after_help = RUN_HELP
)]
struct Cli {
    /// Emit human-readable output or machine-readable JSON.
    #[arg(
        long,
        value_enum,
        global = true,
        default_value_t = output::OutputMode::Plain
    )]
    output: output::OutputMode,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the foreground controller.
    Run,
    /// Summarize recorded child run outcomes and durations.
    Stats {
        /// Include runs started within this duration, such as 24h or 7d.
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let output_mode = cli.output;
    let output = output::Output::new(output_mode);
    if let Err(error) = dispatch(cli, output.clone()) {
        if error.downcast_ref::<cancel::Interrupted>().is_some() {
            let _ = output.event("stopped", serde_json::json!({"reason": "interrupted"}));
            let _ = output.plain_stderr("controller stopped\n");
            return;
        }
        if let Some(failure) = error.downcast_ref::<controller::GoalFailure>() {
            if output_mode == output::OutputMode::Json {
                let _ = output.event("failure", failure.details());
            } else {
                let _ = output.plain_stderr(&format!("goal failed: {failure}\n"));
            }
        } else if output_mode == output::OutputMode::Json {
            let _ = output.event(
                "error",
                serde_json::json!({"message": format!("{error:#}")}),
            );
        } else {
            let _ = output.plain_stderr(&format!("Error: {error:#}\n"));
        }
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli, output: output::Output) -> Result<()> {
    let target = std::env::var_os("GOAL_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("goal.toml"));
    match cli.command {
        None | Some(Commands::Run) => run_controller(target, output),
        Some(Commands::Stats { since }) => run_stats(target, since.as_deref(), cli.output),
    }
}

fn run_controller(config: PathBuf, output: output::Output) -> Result<()> {
    let loaded = config::LoadedConfig::load(&config)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })?;
    controller::Controller::new(loaded, cancelled, output)?.run()
}

fn run_stats(
    config_or_dir: PathBuf,
    since: Option<&str>,
    output_mode: output::OutputMode,
) -> Result<()> {
    let project_dir = analytics::resolve_project_dir(&config_or_dir)?;
    let report = analytics::stats(&project_dir, since)?;
    let mut stdout = io::stdout().lock();
    match output_mode {
        output::OutputMode::Plain => {
            stdout.write_all(analytics::render_plain(&report).as_bytes())?
        }
        output::OutputMode::Json => {
            serde_json::to_writer(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

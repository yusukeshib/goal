mod config;
mod controller;
mod human;
mod model;
mod prompt;
mod runner;
mod state;

use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::Result;
use clap::Parser;

const ABOUT: &str = "A foreground controller that continuously pursues one natural-language goal";

const ROOT_HELP: &str = r#"HOW IT WORKS
  Each cycle is: sense -> decide -> act -> sense.
  The sensor observes the world, a read-only one-shot decider selects one typed
  action, and at most one disposable worker performs one task. The controller
  runs in the foreground and asks human questions on its terminal.

  One process controls exactly one goal. To run multiple goals, give each goal
  its own directory and goal.toml, then start a separate `goal` process for each.
  Processes that modify the same project are not coordinated.

FILES
  goal.toml   Commands, timeouts, and the path to the natural-language goal.
  GOAL.md     The goal text (the filename is configurable).
  .goal/      Restart state, events, prompts, results, and process logs.
"#;

const RUN_HELP: &str = r#"CONFIGURATION
  The config file's directory is the project directory and child working
  directory. Relative goal paths and command paths are resolved from there.

  Required goal.toml shape:

    goal_file = "GOAL.md"
    interval_seconds = 60
    retry_seconds = 30
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

SENSOR CONTRACT
  The sensor must be read-only and emit exactly one JSON value on stdout.
  Stderr is diagnostic. Non-zero exit, timeout, or invalid JSON prevents the
  decider from running and causes a retry after retry_seconds.

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
    prompt_human  {"type":"prompt_human","question":"...","context":null}
    wait          {"type":"wait","reason":"...","retry_after_seconds":60}
    complete      {"type":"complete","summary":"..."}

  Worker completions (`type` field):
    done          {"type":"done","summary":"actual work and verification"}
    needs_input   {"type":"needs_input","question":"...","context":"...",
                   "resume_hint":null}
    blocked       {"type":"blocked","reason":"..."}

  The decider must not modify the project or external world. A worker performs
  only its assigned task, never reads interactive input, writes one completion,
  and exits. A failed worker is not blindly rerun: current reality is sensed
  before a fresh decision.

HUMAN INPUT AND RESTARTS
  Questions are persisted before being printed. EOF or interruption leaves the
  question pending; the next invocation asks it before sensing or deciding.
  Runtime data lives under .goal/ beside the config file.

EXAMPLES
  goal                             Use ./goal.toml
  goal goals/ci/goal.toml          Use an explicit config path

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
    /// Repo-local TOML configuration file.
    #[arg(default_value = "goal.toml", value_name = "CONFIG")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let Cli { config } = Cli::parse();
    let loaded = config::LoadedConfig::load(&config)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })?;
    controller::Controller::new(loaded, cancelled)?.run()
}

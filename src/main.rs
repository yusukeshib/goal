mod analysis;
mod analytics;
mod cancel;
mod config;
mod controller;
mod model;
mod output;
mod prompt;
mod runner;
mod state;
mod tui;

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
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
  `goal stats` summarizes recorded child outcomes and durations. `goal analysis`
  adds a calendar-day or rolling-window activity report and lists every failed,
  cancelled, or still-running child for artifact inspection. Neither command
  starts sensor, decider, or worker processes.

FILES
  goal.toml   Commands, timeouts, and the path to the natural-language goal.
  GOAL.md     The goal text (the filename is configurable).
  .goal/      Controller lock, restart state, events, prompts, results, and logs.
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
  Each sensor output line is limited to 4 MiB, each output stream to 64 MiB,
  and captured sensor stdout to 16 MiB. Stderr is diagnostic. Non-zero exit,
  timeout, invalid JSON, or exceeding an output limit prevents the decider from
  running in that cycle, records the failed sensor run, and causes a short
  backoff followed by a fresh observation.

DECIDER AND WORKER CONTRACT
  Both are non-TUI child processes without a PTY. Every invocation receives:

    GOAL_RUN_ID         unique invocation identifier
    GOAL_PROMPT_PATH    generated prompt.md path
    GOAL_RESULT_PATH    required result.json path
    GOAL_PROJECT_DIR    absolute project directory

  If any argv element contains {prompt}, it is replaced with GOAL_PROMPT_PATH
  and child stdin is closed. Otherwise prompt.md is piped to stdin and closed.
  The child must atomically write exactly one tagged JSON object to
  GOAL_RESULT_PATH. Stdout and stderr are diagnostics only and are never parsed
  as protocol. Each stdout or stderr line is limited to 4 MiB and each stream
  to 64 MiB; exceeding a limit fails the run while preserving bytes already
  written to its log.

  Decider actions (`type` field):
    run_task      {"type":"run_task","task":"one bounded task"}
    wait          {"type":"wait","reason":"...","retry_after_seconds":60}
    complete      {"type":"complete","summary":"..."}
    failure       {"type":"failure","reason":"why this cycle cannot progress"}

  Worker completions (`type` field):
    done          {"type":"done","summary":"actual work and verification"}
    failure       {"type":"failure","reason":"why automatic completion is impossible"}

  Neither process may request human input, approval, or intervention. The
  decider must not modify the project or external world. A worker performs only
  its assigned task, writes one completion, and exits. A worker's logical
  failure is task-local: it is recorded, followed by a fresh observation, and
  passed to the next decider so other work can continue. Sensor and decider
  failures are recorded and followed by a fresh observation. Sensor failures use
  exponential backoff capped at 60 seconds; retrying is safe because both roles
  are read-only. A decider's logical failure
  marks that decision run as failed without terminating the controller. Worker
  process, timeout, and protocol failures are recorded with a warning that the
  worker may have modified external state, followed by exponential backoff and a
  fresh observation; the next decider must not blindly repeat the task. A wait action's capped
  retry_after_seconds is the complete delay before re-sensing; interval_seconds
  is not added to it.

  Child commands must not deliberately detach descendants into another process
  group or session. On Unix, the controller terminates the invocation's
  process group when it finishes, times out, or is cancelled, but cannot reclaim
  a deliberately detached process.

FAILURE ANALYSIS
  Runtime data lives under .goal/ beside the config file. Successful and failed
  invocations retain prompts, results, stdout, stderr, and metadata under
  .goal/runs/, and compact outcomes are appended to .goal/events.jsonl. Run
  `goal stats --since 24h` for outcome counts, worker success rate, and
  role-specific average, p50, and p95 durations. Run `goal analysis` for the
  current local calendar day, `goal analysis --date YYYY-MM-DD` for a past local
  date, or `goal analysis --since 24h` for a rolling window. Analysis adds
  decision/wait activity and exact non-success run IDs, reasons, and artifact
  paths. Historical directories without metadata are counted across all time
  and excluded from filtered metrics.
  These are process and protocol reports, not independent verification of task
  quality. Audit retained artifacts before changing automation. Automation must
  not improve apparent success by weakening success criteria or silently
  waiving unmet requirements.

OUTPUT
  --output tui (default) opens a fullscreen streaming activity feed on an
  interactive terminal. Selecting a row shows its details beside the activity
  list, or below it when the terminal is narrow. Mouse wheel scrolling follows
  the pane under the pointer. TUI mode falls back to plain output when redirected.
  --output plain prints timestamped text. --output pretty indents child JSON up
  to 16 KiB as one terminal block and leaves larger diagnostics unformatted.
  stdout.log and stderr.log retain the exact received byte streams. For
  controller runs, --output json emits strict JSONL envelopes on stdout. JSON
  diagnostics over 16 KiB are summarized; inputs over 1 MiB use a bounded text
  preview instead of being parsed for display. For stats and analysis, tui,
  plain, and pretty emit a human-readable report, while json emits one JSON
  report.

EXAMPLES
  goal                                      Run ./goal.toml
  goal run                                  Explicitly run ./goal.toml
  GOAL_DIR=goals/ci goal                    Run another goal
  GOAL_DIR=goals/ci goal stats --since 24h  Human-readable statistics
  goal analysis                             Analyze today's local calendar day
  goal analysis --date 2026-08-03 --output json
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
    /// Select fullscreen TUI, plain, pretty-printed, or machine-readable JSON output.
    #[arg(
        long,
        value_enum,
        global = true,
        default_value_t = output::OutputMode::Tui
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
    /// Report activity and non-success runs for offline inspection.
    Analysis {
        /// Analyze a rolling duration instead of today's local calendar day.
        #[arg(long, value_name = "DURATION", conflicts_with = "date")]
        since: Option<String>,
        /// Analyze one local calendar date in YYYY-MM-DD form.
        #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "since")]
        date: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let output_mode = effective_output_mode(cli.output);
    if let Err(error) = dispatch(cli, output_mode) {
        // The fullscreen session has already restored the terminal here. Use a
        // fresh stream backend for final diagnostics rather than its closed channel.
        let report_mode = if output_mode == output::OutputMode::Tui {
            output::OutputMode::Plain
        } else {
            output_mode
        };
        let output = output::Output::new(report_mode);
        if error.downcast_ref::<cancel::Interrupted>().is_some() {
            let _ = output.event("stopped", serde_json::json!({"reason": "interrupted"}));
            let _ = output.plain_stderr("controller stopped\n");
            return;
        }
        if report_mode == output::OutputMode::Json {
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

fn effective_output_mode(requested: output::OutputMode) -> output::OutputMode {
    if requested == output::OutputMode::Tui
        && !(io::stdin().is_terminal() && io::stdout().is_terminal())
    {
        output::OutputMode::Plain
    } else {
        requested
    }
}

fn dispatch(cli: Cli, output_mode: output::OutputMode) -> Result<()> {
    let target = std::env::var_os("GOAL_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("goal.toml"));
    match cli.command {
        None | Some(Commands::Run) => run_controller(target, output_mode),
        Some(Commands::Stats { since }) => run_stats(target, since.as_deref(), output_mode),
        Some(Commands::Analysis { since, date }) => {
            run_analysis(target, since.as_deref(), date.as_deref(), output_mode)
        }
    }
}

fn run_controller(config: PathBuf, output_mode: output::OutputMode) -> Result<()> {
    let loaded = config::LoadedConfig::load(&config)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })?;
    if output_mode == output::OutputMode::Tui {
        let (sender, receiver) = mpsc::sync_channel(1_024);
        let project = loaded.project_dir.display().to_string();
        let output = output::Output::tui(sender);
        let controller = controller::Controller::new(loaded, Arc::clone(&cancelled), output)?;
        tui::run(controller, project, cancelled, receiver)
    } else {
        let output = output::Output::new(output_mode);
        controller::Controller::new(loaded, cancelled, output)?.run()
    }
}

fn run_analysis(
    config_or_dir: PathBuf,
    since: Option<&str>,
    date: Option<&str>,
    output_mode: output::OutputMode,
) -> Result<()> {
    let project_dir = analytics::resolve_project_dir(&config_or_dir)?;
    let report = analysis::analyze(&project_dir, since, date)?;
    let mut stdout = io::stdout().lock();
    match output_mode {
        output::OutputMode::Tui | output::OutputMode::Plain | output::OutputMode::Pretty => {
            stdout.write_all(analysis::render_plain(&report).as_bytes())?
        }
        output::OutputMode::Json => {
            serde_json::to_writer(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
        }
    }
    stdout.flush()?;
    Ok(())
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
        output::OutputMode::Tui | output::OutputMode::Plain | output::OutputMode::Pretty => {
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

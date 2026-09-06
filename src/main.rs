mod analysis;
mod analytics;
mod cancel;
mod config;
mod controller;
mod model;
mod output;
mod prompt;
mod registry;
mod runner;
mod service;
mod state;
mod tui;

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool, mpsc},
};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

const ABOUT: &str = "A service controller that continuously pursues natural-language goals";

const ROOT_HELP: &str = r#"HOW IT WORKS
  Each cycle is: sense -> decide -> act -> sense.
  The sensor observes the world, a read-only one-shot decider selects one typed
  action, and either one disposable worker or a fixed bounded batch performs
  the work. Every task in a normally completed batch settles before re-sensing.

TARGET SELECTION
  Commands never infer a goal from the current directory or GOAL_DIR.
  Register a goal.toml file or its directory with `goal add PATH [--id ID]`.
  The default ID is its canonical directory name; all later commands use IDs.

COMMANDS
  `goal up [ID]` starts enabled goals in the background, logging to .goal/service.log.
  `goal up ID --foreground` runs one goal attached with its observational TUI.
  `goal down [ID]` stops registered goals, including disabled ones.
  Without an ID, up/down operate on all eligible registered goals. Already
  running/stopped goals are skipped. Batch failures do not prevent other goals
  from being processed, but make the command exit non-zero.
  `goal enable ID` / `goal disable ID` change eligibility, not running state;
  add --now to also start/stop. Disabled goals must be enabled before starting.
  `goal remove ID` unregisters a stopped goal without deleting its files.
  `goal list` (or `goal ls`) shows all registrations and their running state.
  `goal tail ID --follow` streams a service log. `goal stats ID` and
  `goal analysis ID` inspect retained artifacts without starting child processes.
  Existing path-based services must be registered with add to appear in list.

FILES
  goal.toml          Commands, timeouts, and natural-language goal path.
  GOAL.md            Goal text (the filename is configurable).
  .goal/service.log  Background controller output.
  .goal/              Lock, service state, events, prompts, results, and logs.
"#;

const RUN_HELP: &str = r#"CONFIGURATION
  Register a goal.toml file with `goal add PATH`. Its parent directory is the
  project and child working directory. Relative goal and command paths resolve
  from there.

  Required goal.toml shape:

    goal_file = "GOAL.md"
    interval_seconds = 60
    max_wait_seconds = 3600
    max_concurrency = 1          # optional worker cap; defaults to 1
    worker_observation = "full" # optional: "none" for self-contained tasks
    max_completed_runs = 200    # optional: retain newest finished runs

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
  continuous. It is reloaded at the start of every cycle; the decider and any
  worker dispatched in that cycle receive the same goal snapshot. The decider
  always receives the full observation. Set worker_observation = "none" only
  when every worker task is self-contained and carries all required identity.

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
  and captured sensor stdout to 16 MiB. Stderr is diagnostic. If the sensor
  exits non-zero, times out, emits invalid JSON, or exceeds an output limit, it
  prevents the decider from running in that cycle, records the failed sensor
  run, and causes a short
  backoff followed by a fresh observation.

DECIDER AND WORKER CONTRACT
  Both are non-TUI child processes without a PTY. Every invocation receives:

    GOAL_RUN_ID         unique invocation identifier
    GOAL_PROMPT_PATH    generated prompt.md path
    GOAL_RESULT_PATH    required result.json path
    GOAL_PROJECT_DIR    absolute project directory

  Worker invocations also receive GOAL_WORK_DIR, a fresh disposable directory
  for checkouts and temporary artifacts. It is removed after the worker exits,
  fails, times out, or is cancelled. Workers still share the project working
  directory and external resources; GOAL_WORK_DIR is not a full sandbox.

  If any argv element contains {prompt}, it is replaced with GOAL_PROMPT_PATH
  and child stdin is closed. Otherwise prompt.md is piped to stdin and closed.
  The child must atomically write exactly one tagged JSON object to
  GOAL_RESULT_PATH. Stdout and stderr are diagnostics only and are never parsed
  as protocol. Each stdout or stderr line is limited to 4 MiB and each stream
  to 64 MiB; exceeding a limit fails the run while preserving bytes already
  written to its log.

  Decider actions (`type` field):
    run_task      {"type":"run_task","task":"one bounded task"}
    run_tasks     {"type":"run_tasks","tasks":["task A","task B"],"concurrency":2}
    wait          {"type":"wait","reason":"...","retry_after_seconds":60}
    complete      {"type":"complete","summary":"..."}
    failure       {"type":"failure","reason":"why this cycle cannot progress"}

  Worker completions (`type` field):
    done          {"type":"done","summary":"actual work and verification"}
    failure       {"type":"failure","reason":"why automatic completion is impossible"}

  `run_tasks` is a fixed nonempty list with positive requested concurrency.
  Effective concurrency is min(requested, max_concurrency, task count), and all
  listed tasks run; the list is not truncated. Select only independent,
  non-overlapping tasks. Siblings may run concurrently, and the controller
  collects every task result before re-sensing. Dependent work should use one
  `run_task`, then re-observe before deciding the next step.

  Neither process may request human input, approval, or intervention. The
  decider must not modify the project or external world. A worker performs only
  its assigned task, writes one completion, and exits. A worker's logical or
  infrastructure failure is task-local while a batch is active: independent
  siblings and queued work continue, then the controller re-observes after all
  results settle. Sensor and decider failures are recorded and followed by a
  fresh observation. Sensor failures use
  exponential backoff capped at 60 seconds; retrying is safe because both roles
  are read-only. A decider's logical failure marks that decision run as failed
  without terminating the controller. Worker process, timeout, and protocol
  failures are recorded with a warning that the worker may have modified
  external state, followed by exponential backoff and a fresh observation; the
  next decider must not blindly repeat the task. Worker timeouts apply to each
  invocation. Cancellation stops queued admission, terminates active process
  groups, and can leave an incomplete batch; batches are not durable queues, so
  re-observe rather than automatically replay missing tasks after restart. A
  wait action's capped retry_after_seconds is the complete delay before
  re-sensing; interval_seconds is not added to it.

  Child commands must not deliberately detach descendants into another process
  group or session. On Unix, the controller terminates the invocation's process
  group when it finishes, times out, or is cancelled, but cannot reclaim a
  deliberately detached process.

FAILURE ANALYSIS
  Runtime data lives under .goal/ beside the registered TOML file. Successful and failed
  invocations retain prompts, results, stdout, stderr, outcome metadata, and an
  exact launch.json (expanded argv, working directory, timeout, and prompt
  delivery mode) under .goal/runs/. Compact outcomes are appended to
  .goal/events.jsonl. Run `goal stats ID --since 24h` for outcome counts,
  worker success rate, and role-specific average, p50, and p95 durations. Run
  `goal analysis ID` for the current local calendar day,
  `goal analysis ID --date YYYY-MM-DD` for a past local date, or
  `goal analysis ID --since 24h` for a rolling window. Analysis adds
  decision/wait activity and exact non-success run IDs, reasons, and artifact
  paths. Historical directories without metadata are counted across all time
  and excluded from filtered metrics.
  These are process and protocol reports, not independent verification of task
  quality. Audit retained artifacts before changing automation. Automation must
  not improve apparent success by weakening success criteria or silently
  waiving unmet requirements.

OUTPUT
  Foreground `up` defaults to a fullscreen streaming activity feed on an
  interactive terminal. Selecting a row shows its details beside the activity
  list, or below it when the terminal is narrow. Mouse wheel scrolling follows
  the pane under the pointer. TUI mode falls back to plain output when redirected.
  --output plain prints timestamped text. --output pretty indents child JSON up
  to 16 KiB as one terminal block and leaves larger diagnostics unformatted.
  stdout.log and stderr.log retain the exact received byte streams. For
  foreground controllers, --output json emits strict JSONL envelopes on stdout.
  Background controllers always append plain output to .goal/service.log.
  phase_started includes the run ID, expanded argv, working directory, timeout,
  prompt/result paths, and prompt delivery mode; worker phases also include the
  selected task. JSON diagnostics over 16 KiB are summarized; inputs over 1 MiB
  use a bounded text preview instead of being parsed for display. For stats and
  analysis, tui, plain, and pretty emit a human-readable report, while json emits
  one JSON report.

EXAMPLES
  goal add /srv/goals/ci/goal.toml --id ci
  goal up
  goal up ci --foreground
  goal list --output json
  goal tail ci --follow
  goal down ci
  goal stats ci --since 24h
  goal analysis ci --date 2026-08-03 --output json
  goal up ci --foreground --output json | jq --unbuffered -C .

  See examples/fake for a deterministic full cycle. Keep operational goal
  configurations outside this source checkout, for example under ~/goals/."#;

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
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Register a goal without starting it (enabled by default).
    Add {
        /// Path to goal.toml or its directory.
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Stable ID; defaults to the canonical project directory name.
        #[arg(long, value_name = "ID")]
        id: Option<String>,
    },
    /// Unregister a stopped goal without deleting any files.
    Remove {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Start one enabled goal, or all enabled registered goals.
    Up {
        #[arg(value_name = "ID")]
        id: Option<String>,
        /// Run one goal attached to this terminal (requires ID).
        #[arg(long, requires = "id")]
        foreground: bool,
    },
    /// Stop one goal, or all registered goals, including disabled ones.
    Down {
        #[arg(value_name = "ID")]
        id: Option<String>,
    },
    /// Include a goal in bulk up; does not start it unless --now is given.
    Enable {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long)]
        now: bool,
    },
    /// Exclude a goal from bulk up; does not stop it unless --now is given.
    Disable {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long)]
        now: bool,
    },
    /// List all registered goals, including stopped and disabled goals.
    #[command(visible_alias = "ls")]
    List,
    /// Print or follow a background goal service log.
    Tail {
        #[arg(value_name = "ID")]
        id: String,
        /// Continue streaming until the service stops or this command is interrupted.
        #[arg(short, long)]
        follow: bool,
        /// Number of existing log lines to print.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Summarize recorded child run outcomes and durations.
    Stats {
        #[arg(value_name = "ID")]
        id: String,
        /// Include runs started within this duration, such as 24h or 7d.
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
    },
    /// Report activity and non-success runs for offline inspection.
    Analysis {
        #[arg(value_name = "ID")]
        id: String,
        /// Analyze a rolling duration instead of today's local calendar day.
        #[arg(long, value_name = "DURATION", conflicts_with = "date")]
        since: Option<String>,
        /// Analyze one local calendar date in YYYY-MM-DD form.
        #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "since")]
        date: Option<String>,
    },
    /// Internal entry point for a detached service process.
    #[command(name = "__service", hide = true)]
    Service {
        #[arg(value_name = "GOAL_FILE")]
        goal_file: PathBuf,
        #[arg(long, hide = true)]
        ready: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let output_mode = effective_output_mode(cli.output);
    if let Err(error) = dispatch(cli, output_mode) {
        let report_mode = if output_mode == output::OutputMode::Tui {
            output::OutputMode::Plain
        } else {
            output_mode
        };
        let output = output::Output::new(report_mode);
        if error.downcast_ref::<cancel::Interrupted>().is_some() {
            let _ = output.event("stopped", json!({"reason": "interrupted"}));
            let _ = output.plain_stderr("controller stopped\n");
            return;
        }
        if report_mode == output::OutputMode::Json {
            let _ = output.event("error", json!({"message": format!("{error:#}")}));
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
    match cli.command {
        Commands::Add { path, id } => {
            let goal = registry::Registry::open()?.add(&path, id.as_deref())?;
            print_goal_action("added", &goal, None, output_mode)
        }
        Commands::Remove { id } => {
            let goal = registry::Registry::open()?.remove(&id)?;
            print_goal_action("removed", &goal, None, output_mode)
        }
        Commands::Up { id, foreground } => {
            // Keep the administrative lock until startup has published its runtime
            // record, so remove/disable cannot race an admitted start. Detached
            // children only acquire the separate services.json lock.
            let registry = registry::Registry::open()?;
            if foreground {
                let goal = registry.get(id.as_deref().expect("foreground requires ID"))?;
                require_enabled(&goal)?;
                run_controller(goal.config_path, output_mode, true, None, Some(registry))
            } else {
                let goals = match id {
                    Some(id) => vec![registry.get(&id)?],
                    None => registry
                        .goals()
                        .into_iter()
                        .filter(|goal| goal.enabled)
                        .collect(),
                };
                run_goal_actions(&goals, true, output_mode)
            }
        }
        Commands::Down { id } => {
            // Serialize stop with registration changes and other up/down calls.
            let registry = registry::Registry::open()?;
            let goals = match id {
                Some(id) => vec![registry.get(&id)?],
                None => registry.goals(),
            };
            run_goal_actions(&goals, false, output_mode)
        }
        Commands::Enable { id, now } => set_enabled(&id, true, now, output_mode),
        Commands::Disable { id, now } => set_enabled(&id, false, now, output_mode),
        Commands::List => run_list(output_mode),
        Commands::Tail { id, follow, lines } => {
            let goal = registry::Registry::open()?.get(&id)?;
            service::tail(&goal.config_path, lines, follow)
        }
        Commands::Stats { id, since } => {
            let goal = registry::Registry::open()?.get(&id)?;
            run_stats(goal.config_path, since.as_deref(), output_mode)
        }
        Commands::Analysis { id, since, date } => {
            let goal = registry::Registry::open()?.get(&id)?;
            run_analysis(goal.config_path, since.as_deref(), date.as_deref(), output_mode)
        }
        Commands::Service { goal_file, ready } => {
            let result = run_controller(
                goal_file,
                output::OutputMode::Plain,
                false,
                Some(&ready),
                None,
            );
            if let Err(error) = &result
                && !ready.exists()
            {
                let _ = service::write_start_error(&ready, error);
            }
            result
        }
    }
}

fn require_enabled(goal: &registry::Goal) -> Result<()> {
    if !goal.enabled {
        bail!(
            "goal {} is disabled; run goal enable {} first",
            goal.id,
            goal.id
        );
    }
    Ok(())
}

fn set_enabled(id: &str, enabled: bool, now: bool, output_mode: output::OutputMode) -> Result<()> {
    let mut registry = registry::Registry::open()?;
    let goal = registry.set_enabled(id, enabled)?;
    print_goal_action(
        if enabled { "enabled" } else { "disabled" },
        &goal,
        None,
        output_mode,
    )?;
    if now {
        run_goal_actions(&[goal], enabled, output_mode)?;
    }
    Ok(())
}

fn run_goal_actions(
    goals: &[registry::Goal],
    start: bool,
    output_mode: output::OutputMode,
) -> Result<()> {
    let mut failed = Vec::new();
    for goal in goals {
        if let Err(error) = run_goal_action(goal, start, output_mode) {
            let message = format!("{}: {error:#}", goal.id);
            if output_mode == output::OutputMode::Json {
                let mut stdout = io::stdout().lock();
                serde_json::to_writer(
                    &mut stdout,
                    &json!({"status": "error", "id": goal.id, "message": message}),
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            } else {
                eprintln!("{message}");
            }
            failed.push(goal.id.as_str());
        }
    }
    if !failed.is_empty() {
        bail!(
            "goal {} failed for: {}",
            if start { "up" } else { "down" },
            failed.join(", ")
        );
    }
    Ok(())
}

fn run_goal_action(
    goal: &registry::Goal,
    start: bool,
    output_mode: output::OutputMode,
) -> Result<()> {
    if start {
        require_enabled(goal)?;
    }
    let running = service::service_for_config(&goal.config_path)?;
    let (action, record) = if start {
        if let Some(record) = running {
            ("already_running", Some(record))
        } else {
            let loaded = config::LoadedConfig::load(&goal.config_path)?;
            if loaded.config_path != goal.config_path {
                bail!(
                    "registered config path changed; remove and add {} again",
                    goal.id
                );
            }
            (
                "started",
                Some(service::start_background(&loaded.config_path, &loaded.project_dir)?),
            )
        }
    } else if running.is_some() {
        ("stopped", Some(service::stop(&goal.config_path)?))
    } else {
        ("already_stopped", None)
    };
    print_goal_action(action, goal, record.as_ref(), output_mode)
}

fn run_controller(
    config: PathBuf,
    output_mode: output::OutputMode,
    foreground: bool,
    ready: Option<&Path>,
    registry_guard: Option<registry::Registry>,
) -> Result<()> {
    let loaded = config::LoadedConfig::load(&config)?;
    if registry_guard.is_some() && loaded.config_path != config {
        bail!("registered config path changed; remove and add the goal again");
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })?;

    if output_mode == output::OutputMode::Tui {
        let (sender, receiver) = mpsc::sync_channel(1_024);
        let project = loaded.project_dir.display().to_string();
        let config_path = loaded.config_path.clone();
        let project_dir = loaded.project_dir.clone();
        let output = output::Output::tui(sender);
        let controller = controller::Controller::new(loaded, Arc::clone(&cancelled), output)?;
        let registration = service::Registration::create(
            &config_path,
            &project_dir,
            foreground,
            Arc::clone(&cancelled),
        )?;
        announce_ready(ready, registration.record())?;
        drop(registry_guard);
        tui::run(controller, project, cancelled, receiver)
    } else {
        let config_path = loaded.config_path.clone();
        let project_dir = loaded.project_dir.clone();
        let output = output::Output::new(output_mode);
        let controller = controller::Controller::new(loaded, Arc::clone(&cancelled), output)?;
        let registration =
            service::Registration::create(&config_path, &project_dir, foreground, cancelled)?;
        announce_ready(ready, registration.record())?;
        drop(registry_guard);
        controller.run()
    }
}

fn announce_ready(ready: Option<&Path>, record: &service::ServiceRecord) -> Result<()> {
    if let Some(path) = ready {
        service::write_start_success(path, record)?;
    }
    Ok(())
}

fn print_goal_action(
    action: &str,
    goal: &registry::Goal,
    record: Option<&service::ServiceRecord>,
    output_mode: output::OutputMode,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    if output_mode == output::OutputMode::Json {
        serde_json::to_writer(
            &mut stdout,
            &json!({
                "status": action,
                "id": goal.id,
                "goal": goal,
                "service": record.map(|record| record.info()),
            }),
        )?;
        stdout.write_all(b"\n")?;
    } else {
        writeln!(
            stdout,
            "goal {action}: {}{}",
            goal.id,
            record
                .map(|record| format!(" (pid {})", record.pid))
                .unwrap_or_default()
        )?;
    }
    stdout.flush()?;
    Ok(())
}

fn run_list(output_mode: output::OutputMode) -> Result<()> {
    let registry = registry::Registry::open()?;
    let goals = registry.goals();
    let services = service::list()?;
    let mut stdout = io::stdout().lock();
    if output_mode == output::OutputMode::Json {
        let entries = goals.iter().map(|goal| {
            let service = services.iter().find(|record| record.config_path == goal.config_path);
            json!({
                "id": goal.id,
                "enabled": goal.enabled,
                "status": if service.is_some() { "running" } else { "stopped" },
                "config_path": goal.config_path,
                "project_dir": goal.config_path.parent(),
                "pid": service.map(|record| record.pid),
                "started_at": service.map(|record| record.started_at),
                "foreground": service.map(|record| record.foreground),
                "log_path": service.and_then(|record| record.log_path.as_deref()),
            })
        }).collect::<Vec<_>>();
        serde_json::to_writer(&mut stdout, &entries)?;
        stdout.write_all(b"\n")?;
    } else if goals.is_empty() {
        writeln!(stdout, "no registered goals; use goal add <path>")?;
    } else {
        writeln!(stdout, "ID\tENABLED\tSTATUS\tPID\tGOAL_FILE")?;
        for goal in goals {
            let service = services.iter().find(|record| record.config_path == goal.config_path);
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}",
                goal.id,
                goal.enabled,
                if service.is_some() { "running" } else { "stopped" },
                service
                    .map(|record| record.pid.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                goal.config_path.display()
            )?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn run_analysis(
    goal_file: PathBuf,
    since: Option<&str>,
    date: Option<&str>,
    output_mode: output::OutputMode,
) -> Result<()> {
    let project_dir = analytics::resolve_project_dir(&goal_file)?;
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
    goal_file: PathBuf,
    since: Option<&str>,
    output_mode: output::OutputMode,
) -> Result<()> {
    let project_dir = analytics::resolve_project_dir(&goal_file)?;
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

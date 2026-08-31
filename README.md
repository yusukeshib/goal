# goal

A foreground controller that continuously pursues one natural-language goal:

```text
sense → decide → run one task → sense
             ↘ wait          → sense
             ↘ complete      → exit 0
```

The decider is one-shot and read-only. Workers are disposable and non-interactive. There is no daemon, child PTY, worker pool, or persistent agent conversation; the TUI is observational only.

## Run

Requires Rust 1.85+ and a directory containing `goal.toml` plus the configured goal file.

```sh
cargo install --path .
cd /path/to/goal && goal
# or
GOAL_DIR=/path/to/goal goal
```

`GOAL_DIR` defaults to the current directory, which is also the child working directory. The configured goal file is reloaded at the start of every cycle. The decider always receives the full per-cycle observation; workers receive it by default, or only their assigned task when `worker_observation = "none"`. Run `goal --help` for the complete configuration and protocol reference.

A minimal `goal.toml`:

```toml
goal_file = "GOAL.md"
interval_seconds = 60
max_wait_seconds = 3600
worker_observation = "full" # or "none" when the task is self-contained
max_completed_runs = 200    # optional; prunes only finished run directories

[sensor]
command = ["./sensor.sh"]
timeout_seconds = 60

[decider]
command = ["agent-cli", "--non-interactive", "{prompt}"]
timeout_seconds = 300

[worker]
command = ["agent-cli", "--non-interactive", "{prompt}"]
timeout_seconds = 1800
```

## Contract

- The sensor is read-only and prints one JSON value to stdout.
- The decider returns `run_task`, `wait`, `complete`, or `failure`.
- The worker returns `done` or `failure` in `GOAL_RESULT_PATH`.
- Deciders and workers cannot request human input or approval.
- Sensor and decider failures are recorded and retried after re-sensing.
- Worker process, timeout, and protocol failures are recorded, exponentially backed off, and followed by a fresh observation. The failure context warns the next decider not to blindly repeat a task that may have partially changed external state. A valid logical worker `failure` follows the same re-observation path without infrastructure backoff.

Each invocation receives `GOAL_RUN_ID`, `GOAL_PROMPT_PATH`, `GOAL_RESULT_PATH`, and `GOAL_PROJECT_DIR`. `{prompt}` is replaced with the prompt path; without it, the prompt is piped to stdin.

## Observe

```sh
goal                                # fullscreen TUI on a terminal
goal --output plain                 # timestamped text
goal --output json | jq --unbuffered
goal stats --since 24h
goal analysis                       # today
goal analysis --since 7d
goal analysis --date 2026-08-03
```

TUI: `↑/↓` or `j/k` selects, `PgUp/PgDn` scrolls details, `End` follows, and `q` stops. Redirection automatically falls back to plain output.

State, events, prompts, results, exact child logs, and run metadata are stored under `.goal/`. `stats` and `analysis` inspect these artifacts without starting children. When `max_completed_runs` is set, the controller retains the newest finished run directories and never prunes running, malformed, state, or event artifacts.

## Examples

- [`examples/fake`](examples/fake): deterministic runnable cycle (`cd examples/fake && ./run.sh`)
- [`examples/mergeable-prs`](examples/mergeable-prs): continuously keep authored pull requests approval-ready

The pull-request example uses Pi's configured default model. To reuse existing local checkouts, set `GOAL_CHECKOUT_ROOTS` to an OS-path-separated list of directories that contain repositories; otherwise workers use bounded disposable checkouts in the system temporary directory.

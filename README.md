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

`GOAL_DIR` defaults to the current directory, which is also the child working directory. Run `goal --help` for the complete configuration and protocol reference.

A minimal `goal.toml`:

```toml
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
```

## Contract

- The sensor is read-only and prints one JSON value to stdout.
- The decider returns `run_task`, `wait`, `complete`, or `failure`.
- The worker returns `done` or `failure` in `GOAL_RESULT_PATH`.
- Deciders and workers cannot request human input or approval.
- Sensor and decider failures are recorded and retried after re-sensing.
- Worker process, timeout, or protocol failures are terminal; a valid worker `failure` is task-local and the loop continues.

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

State, events, prompts, results, exact child logs, and run metadata are stored under `.goal/`. `stats` and `analysis` inspect these artifacts without starting children.

## Examples

- [`examples/fake`](examples/fake): deterministic runnable cycle (`cd examples/fake && ./run.sh`)
- [`examples/mergeable-prs`](examples/mergeable-prs): read-only GitHub sensor

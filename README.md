# goal

A service controller that continuously pursues one natural-language goal:

```text
sense → decide → run one task or fixed bounded batch → sense
             ↘ wait                               → sense
             ↘ complete                           → exit 0
```

The decider is one-shot and read-only. Workers are disposable and non-interactive. A decision may dispatch one worker or a fixed batch through a bounded worker pool; every admitted task settles before the controller senses again. There is no child PTY or persistent agent conversation, and the foreground TUI is observational only.

## Run

Requires Rust 1.85+ and a `goal.toml` plus its configured goal file.

```sh
cargo install --path .
goal up /path/to/goal/goal.toml
```

Goal selection is always explicit. `goal` never infers `goal.toml` from the current directory or `GOAL_DIR`. The directory containing the specified TOML file is the child working directory.

Service commands:

```sh
goal up /path/to/goal.toml                 # detached; logs to .goal/service.log
goal up /path/to/goal.toml --foreground    # attached observational TUI
goal list
goal tail /path/to/goal.toml
goal tail /path/to/goal.toml --follow
goal down /path/to/goal.toml
```

Paths are canonicalized before being used as service identities. A lock under `.goal/` prevents multiple controllers from running for the same project directory. `goal list --output json` provides structured service discovery.

A minimal `goal.toml`:

```toml
goal_file = "GOAL.md"
interval_seconds = 60
max_wait_seconds = 3600
max_concurrency = 1          # optional worker cap; defaults to serial execution
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

The configured goal file is reloaded at the start of every cycle. The decider always receives the full per-cycle observation; workers receive it by default, or only their assigned task when `worker_observation = "none"`. Run `goal --help` for the complete configuration and protocol reference.

## Contract

- The sensor is read-only and prints one JSON value to stdout.
- The decider returns `run_task`, `run_tasks`, `wait`, `complete`, or `failure`. `run_tasks` contains a nonempty fixed `tasks` list and a positive requested `concurrency`, for example `{"type":"run_tasks","tasks":["task A","task B"],"concurrency":2}`.
- Effective batch concurrency is the minimum of the requested value, `max_concurrency`, and task count. The default cap is 1. The task list is not truncated, and the controller does not re-sense until all tasks in a normally completed batch settle.
- Batch tasks must be independent and non-overlapping. Workers share the project working directory and external resources; only each worker's disposable `GOAL_WORK_DIR` is isolated. Use `run_task` and re-observe between steps when work is dependent.
- The worker returns `done` or `failure` in `GOAL_RESULT_PATH`. Logical and infrastructure failures remain task-local while a batch is active, so independent siblings and queued tasks continue. Infrastructure failures cause one batch-level backoff after all results are collected.
- Deciders and workers cannot request human input or approval.
- Sensor and decider failures are recorded and retried after re-sensing.
- Worker process, timeout, and protocol failures are recorded with run IDs and followed by a fresh observation. The failure context warns the next decider not to blindly repeat a task that may have partially changed external state. A valid logical worker `failure` does not cause infrastructure backoff.

Each invocation receives `GOAL_RUN_ID`, `GOAL_PROMPT_PATH`, `GOAL_RESULT_PATH`, and `GOAL_PROJECT_DIR`. Workers also receive a fresh `GOAL_WORK_DIR` for disposable checkouts and temporary artifacts; the controller removes it after every worker outcome. `{prompt}` is replaced with the prompt path; without it, the prompt is piped to stdin. Worker timeouts apply separately to each invocation. Cancellation stops admission of queued tasks and reclaims active worker process groups before exit.

A batch is a cycle boundary, not a durable queue or resumable scheduler. State records partial results if cancellation or interruption leaves a batch incomplete; after restart, re-observe the world and do not automatically replay missing tasks because unrecorded work may already have changed external state.

## Observe

```sh
goal tail /path/to/goal.toml --follow
goal stats /path/to/goal.toml --since 24h
goal analysis /path/to/goal.toml
goal analysis /path/to/goal.toml --since 7d
goal analysis /path/to/goal.toml --date 2026-08-03
```

Foreground TUI: `↑/↓` or `j/k` selects, `PgUp/PgDn` scrolls details, `End` follows, and `q` stops. Redirection automatically falls back to plain output.

State, events, prompts, results, exact child logs, and run metadata are stored under `.goal/`. `stats` and `analysis` inspect these artifacts without starting children. When `max_completed_runs` is set, the controller retains the newest finished run directories and never prunes running, malformed, state, or event artifacts.

## Examples

- [`examples/fake`](examples/fake): deterministic runnable cycle (`cd examples/fake && ./run.sh`)

Operational goal configurations live outside this source checkout, for example under `~/goals/`. The former `mergeable-prs` example is maintained at `~/goals/mergeable-prs`.

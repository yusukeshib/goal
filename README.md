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
goal add /path/to/goal/goal.toml --id my-goal
goal up my-goal
```

Only `add` accepts a path (a TOML file or a directory containing `goal.toml`). All subsequent commands use the registered ID. `goal` never infers a goal from the current directory or `GOAL_DIR`. The directory containing the canonical TOML file is the child working directory.

Service commands:

```sh
goal add /path/to/goal                    # register enabled, but do not start
goal add /another/goal.toml --id my-goal   # override the directory-derived ID
goal up                                  # start all enabled registered goals
goal up my-goal                           # start one; logs to .goal/service.log
goal up my-goal --foreground              # attached observational TUI; ID required
goal list                                # all registrations, even stopped/disabled
goal ls                                  # alias for list
goal ls --watch                          # watch registrations and runtime events
goal ls --watch --output json            # initial snapshot, then JSONL changes
goal disable my-goal                      # exclude from up; do not stop it
goal enable my-goal                       # include in up; do not start it
goal disable my-goal --now                # disable and stop
goal enable my-goal --now                 # enable and start
goal tail my-goal --follow
goal down my-goal                         # stop one without unregistering it
goal down                                # stop all registered goals, even disabled
goal remove my-goal                      # unregister; refuses while running
```

IDs default to the canonical project directory name and remain fixed after registration. Use letters, digits, `.`, `_`, and `-`, starting with a letter or digit. Invalid names or ID collisions require an explicit `--id`; a project can only be registered once, including through symlinks.

Enabled/disabled and running/stopped are independent states. A disabled goal must be enabled before an explicit `up` too. Background `up` and `down` skip already-running/already-stopped goals. Bulk operations attempt every eligible goal and exit nonzero if any fail; successful operations are not rolled back. `--now` saves the enabled flag first, so a subsequent start/stop failure does not revert it. `up` is a one-time start operation, not a supervisor or automatic restart policy.

Registrations persist in `goals.json` under `$GOAL_STATE_DIR`, or `$XDG_STATE_HOME/goal`, or `~/.local/state/goal`. They are separate from transient `services.json` runtime records and survive completion, failure, and stopping. `remove` deletes neither configuration nor `.goal/` artifacts. A lock under `.goal/` still prevents multiple controllers for the same project directory. `goal list --output json` returns all registrations with `id`, `enabled`, `status` (`running`/`stopped`), `config_path`, and runtime fields such as `pid` (null when stopped).

**Upgrading from path-based commands:** register each existing configuration with `goal add /path/to/goal.toml --id my-goal`, including services already running. Registration does not restart them. There is no automatic migration; unregistered old services are not shown in the new `list` or targeted by bulk `down`. Existing controllers can continue running without overwriting the new registrations.

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

Each invocation receives `GOAL_RUN_ID`, `GOAL_PROMPT_PATH`, `GOAL_RESULT_PATH`, `GOAL_USAGE_PATH`, and `GOAL_PROJECT_DIR`. Workers also receive a fresh `GOAL_WORK_DIR` for disposable checkouts and temporary artifacts; the controller removes it after every worker outcome. `{prompt}` is replaced with the prompt path; without it, the prompt is piped to stdin. Worker timeouts apply separately to each invocation. Cancellation stops admission of queued tasks and reclaims active worker process groups before exit.

A batch is a cycle boundary, not a durable queue or resumable scheduler. State records partial results if cancellation or interruption leaves a batch incomplete; after restart, re-observe the world and do not automatically replay missing tasks because unrecorded work may already have changed external state.

## Observe

```sh
goal tail my-goal --follow
goal stats my-goal --since 24h
goal analysis my-goal
goal analysis my-goal --since 7d
goal analysis my-goal --date 2026-08-03
```

### Optional usage and cost reporting

Goal is a general-purpose command runner, not an LLM-specific runtime. Any sensor,
decider, or worker may report usage by atomically replacing the JSON file at
`GOAL_USAGE_PATH` (the run's durable `usage.json`):

```json
{
  "schema_version": 1,
  "complete": true,
  "costs": [{"currency": "USD", "amount": 0.25}],
  "metrics": [{"name": "cpu_time", "unit": "seconds", "value": 12}]
}
```

Each replacement is a **cumulative snapshot for this run**, not a delta. Write a
sibling temporary file, then rename it over `GOAL_USAGE_PATH`; do not modify the
published file in place. Multiple writers must coordinate outside goal. A producer
can update the snapshot while running; leave `complete` false (the default) until
it has finished reporting. The latest snapshot survives command failure or
cancellation and is pruned together with its run.

`stats` and `analysis` aggregate only the selected runs, keeping currencies and
metric name/unit pairs separate. Missing reports, invalid reports, absent costs,
and incomplete snapshots are exposed as coverage limits, not silently counted as
zero. An explicit zero amount is a reported zero. Costs are producer-reported
values, not verified invoices; goal does not calculate prices or exchange rates.
Amounts and metric values must be finite and nonnegative. Reports are bounded to
64 KiB. Existing metadata and stdout/result protocols are unchanged, and commands
that do not report usage continue working normally.

For pi, the optional `examples/pi_usage.py` adapter translates finalized pi usage
messages before a wrapper discards or condenses them. It can also be imported as
`PiUsageReporter` by a Python wrapper. The CLI passes stdout through unchanged:

```sh
pi --mode json ... | python3 /path/to/pi_usage.py | your-existing-filter
```

The adapter uses `GOAL_USAGE_PATH`, falling back to a sibling of
`GOAL_RESULT_PATH` for older goal controllers. With neither variable it is a
pass-through. It ignores streaming/turn/agent copies and counts finalized
assistant and nested tool-result usage. CLI EOF alone does not prove successful
completion, so it leaves the snapshot incomplete. An owning Python wrapper may
call `finish(complete=True)` after confirming normal exit. Unreported or already
lost historical usage cannot be recovered. No pi parsing lives in goal's core.

### Readable terminal tables

In a terminal, `goal ls` renders a borderless ratatui table with a gray header and
aligned columns. It prints once and exits, leaving the table in scrollback.
The path column is hidden below 100 columns; below 60 columns, only ID and status
are shown. `--output plain`, redirected output, and JSON retain their existing
formats. `goal ls --watch` uses the same table layout without a title or borders.

### Watch all registered goals

`goal ls --watch` samples once per second. It prints the initial table, then
appends runtime events and updated tables when registrations change. It stays
in normal scrollback: no fullscreen mode, screen clearing, or cursor hiding.
`--output plain` or redirected output appends changes without terminal control sequences. Ctrl-C exits only the watcher,
not the goal services. The watcher does not run sensors, workers, or models.

`goal ls --watch --output json` emits one JSON object per line, with
`timestamp` (Unix seconds), `type`, and `details`. The first event is `snapshot`, whose
`details.goals` is the same array returned by ordinary `goal ls --output json`.
After that, only changes are emitted:

| Type | Details |
| --- | --- |
| `goal_added` | `goal`: newly registered list row |
| `goal_changed` | `goal`: current row; `previous`: previous row |
| `goal_removed` | `goal`: last observed row |
| `goal_event` | `id`: registered ID; `event`: original `.goal/events.jsonl` object |
| `watch_gap` | `id`: affected goal; `reason`: unreadable, malformed, oversized, or replaced/truncated log |

Runtime events include decisions, waits, sensor results, worker completions and
failures, and batch activity. A task decision is not proof that its worker has
started. Existing log history is not replayed when a goal is first discovered;
use `analysis` and retained artifacts for historical inspection.

This is a **best-effort live view**, not a durable event subscription or a health
verdict. Registry transitions shorter than the sample interval can be missed.
Detected log replacement/truncation emits `watch_gap`; changes that truncate and
regrow a file between samples may not be detectable. Reads and partial-line
buffers are bounded, so heavy event traffic can lag. Human-readable event lines
are shortened; JSON event objects remain intact. Persistent source errors can
repeat `watch_gap` notices. The watcher does not infer hangs or verify task
outcomes. Plain and JSON `ls` output remain unchanged.

Foreground controller TUI: `↑/↓` or `j/k` selects, `PgUp/PgDn` scrolls details, `End` follows, and `q` stops. Redirection automatically falls back to plain output.

State, events, prompts, results, exact child logs, and run metadata are stored under `.goal/`. `stats` and `analysis` inspect these artifacts without starting children. When `max_completed_runs` is set, the controller retains the newest finished run directories and never prunes running, malformed, state, or event artifacts.

## Examples

- [`examples/fake`](examples/fake): deterministic runnable cycle (`cd examples/fake && ./run.sh`)

Operational goal configurations live outside this source checkout, for example under `~/goals/`. The former `mergeable-prs` example is maintained at `~/goals/mergeable-prs`.

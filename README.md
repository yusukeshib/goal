# goal

`goal` is a small foreground controller that repeatedly senses the world, asks a one-shot read-only decider for one typed action, and runs at most one disposable worker.

```text
sense -> decide -> run task -- done/failure -> sense
                  wait ---------------------> sense
                  complete -----------------> exit 0
                  failure ------------------> sense
```

It has no daemon, child PTY, interactive workflow, worker pool, or persistent agent conversation. Its optional terminal interaction is an observational activity viewer only.

## Build and run

Stable Rust 1.85 or newer is required.

```sh
cargo build
cargo run -- .
```

`goal` uses `GOAL_DIR`, or the current directory when it is unset. That directory must contain `goal.toml` and is also the child working directory. Relative goal and command paths resolve from there.

```sh
cd ~/goals/mergeable-prs && goal
GOAL_DIR=~/goals/mergeable-prs goal
GOAL_DIR=~/goals/mergeable-prs goal run
```

See [`examples/fake`](examples/fake) for a runnable deterministic cycle:

```sh
cargo build
cd examples/fake
./run.sh
```

The example resets its fake progress, performs 20 deterministic worker cycles, and completes without interactive input in roughly one minute. This produces enough activity to exercise the TUI scrollbar. Set `GOAL_FAKE_STEPS` or `GOAL_FAKE_DELAY_SECONDS` to adjust its length, for example `GOAL_FAKE_STEPS=40 ./run.sh`.

## Agent protocol

Deciders and workers are ordinary argv commands, without an implicit shell or PTY. `{prompt}` in any argv element is replaced with the generated prompt path. Without it, the prompt is piped to stdin and then closed. Every invocation receives:

- `GOAL_RUN_ID`
- `GOAL_PROMPT_PATH`
- `GOAL_RESULT_PATH`
- `GOAL_PROJECT_DIR`

The process must atomically write one tagged JSON object to `GOAL_RESULT_PATH`. Stdout and stderr are diagnostics only. Decider action tags are `run_task`, `wait`, `complete`, and `failure`; worker completion tags are `done` and `failure`. Neither process may request human input, approval, or intervention.

Runtime state, compact events, prompts, results, logs, and per-run `metadata.json` files are kept under `.goal/`. In human output modes, sensor stdout is treated as protocol data and hidden from the terminal; it remains available in the run's `stdout.log` and is passed unchanged to the decider. Sensor stderr diagnostics are still displayed. Sensor and decider failures are recorded with their reason and run ID, followed by a short backoff and a fresh observation; retrying is safe because both roles are read-only. A decider `failure` marks that decision run as failed without terminating the controller. Worker process, timeout, or protocol failures terminate the controller with a non-zero status because the worker may have modified external state. A worker `failure` is task-local: it is recorded and passed to the next decider after a fresh observation, allowing other safe work to continue without retrying the failed task blindly. The retained artifacts support offline analysis of successful and failed runs. Improvements must not weaken success criteria or silently waive unmet requirements.

## Statistics and analysis

`stats` and `analysis` read existing artifacts without starting a sensor, decider, or worker:

```sh
goal stats --since 24h
goal analysis                              # current local calendar day
goal analysis --date 2026-08-03
goal analysis --since 24h --output json
GOAL_DIR=~/goals/mergeable-prs goal stats --since 7d --output json
```

`stats` reports outcome counts, worker success rate, failure kinds, and average/p50/p95 duration by role. `analysis` adds sense/decision/worker activity, requested versus actual wait time, and a chronological list of every failed, cancelled, or still-running child with its exact run ID, recorded reason, and artifact path. `--date` uses the machine's local calendar day, while `--since` selects a rolling duration; without either, `analysis` selects today locally.

Both commands report process and protocol outcomes, not independent proof that successful work was semantically correct. Audit the retained prompt, result, and logs before using the report to change automation or success criteria. Directories created before metadata support are counted separately across all time and excluded from filtered calculations rather than inferred.

## Output modes

The default `--output tui` opens a fullscreen streaming activity feed when stdin and stdout are terminals. Each newline-delimited child diagnostic is one row, and the selected row's details are always shown in a separate pane. The detail pane sits to the right on wide terminals and moves below the activity list when the terminal is narrower than 100 columns. Use Up/Down or `j`/`k` to select, click a row to select it, PageUp/PageDown to scroll its details, and End or `a` to resume following new activity. The mouse wheel scrolls whichever pane is under the pointer; the activity scrollbar can also be dragged or clicked. Use `q` or Ctrl-C to stop. Scrolling the activity list away from the end pauses automatic following, while reaching the end resumes it.

TUI mode falls back silently to plain output when redirected or run without an interactive terminal. `goal stats` and `goal analysis` always print their reports instead of opening the fullscreen viewer.

Use `--output plain` for timestamped streaming text. Use `--output pretty` to indent JSON diagnostics as one prefixed terminal block without omitting any values. Display formatting affects only the foreground; each run's `stdout.log` and `stderr.log` retains the original bytes. Non-JSON diagnostics remain unchanged.

Use `--output json` for a machine-readable controller stream:

```sh
goal --output json | jq --unbuffered -C .
```

Every stdout line has the same `timestamp`, `type`, and `details` envelope. Controller events such as waits, completions, failures, and errors are structured. Child JSON is nested under `details.payload`; non-JSON child diagnostics are represented by `details.content`. Oversized child lines are replaced in the JSON foreground stream by bounded metadata with `truncated` and `original_bytes`; exact output remains in each run's `stdout.log` or `stderr.log`. Intentional Ctrl-C/SIGTERM emits `stopped` in JSON mode and exits successfully.

## Read-only GitHub sensor example

[`examples/mergeable-prs`](examples/mergeable-prs) contains a read-only `gh api graphql` sensor for authored open pull requests. It reports CI checks, merge state, and unresolved review threads. Replace the placeholder agent commands in its `goal.toml` with locally available non-TUI decider and worker commands.

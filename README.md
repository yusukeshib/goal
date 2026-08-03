# goal

`goal` is a small foreground controller that repeatedly senses the world, asks a one-shot read-only decider for one typed action, and runs at most one disposable worker.

```text
sense -> decide -> run task -- done/failure -> sense
                  wait ---------------------> sense
                  complete -----------------> exit 0
                  failure ------------------> exit 1
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
../../target/debug/goal
```

The example completes without interactive input.

## Agent protocol

Deciders and workers are ordinary argv commands, without an implicit shell or PTY. `{prompt}` in any argv element is replaced with the generated prompt path. Without it, the prompt is piped to stdin and then closed. Every invocation receives:

- `GOAL_RUN_ID`
- `GOAL_PROMPT_PATH`
- `GOAL_RESULT_PATH`
- `GOAL_PROJECT_DIR`

The process must atomically write one tagged JSON object to `GOAL_RESULT_PATH`. Stdout and stderr are diagnostics only. Decider action tags are `run_task`, `wait`, `complete`, and `failure`; worker completion tags are `done` and `failure`. Neither process may request human input, approval, or intervention.

Runtime state, compact events, prompts, results, logs, and per-run `metadata.json` files are kept under `.goal/`. In human output modes, sensor stdout is treated as protocol data and hidden from the terminal; it remains available in the run's `stdout.log` and is passed unchanged to the decider. Sensor stderr diagnostics are still displayed. A decider protocol failure is recorded with its reason and run ID, followed by a short backoff and a fresh observation; retrying is safe because the decider is read-only. Sensor failures, decider process failures, and worker process, timeout, or protocol failures terminate the controller with a non-zero status. A decider `failure` is also terminal because it describes the goal as a whole. A worker `failure` is task-local: it is recorded and passed to the next decider after a fresh observation, allowing other safe work to continue without retrying the failed task blindly. The retained artifacts support offline analysis of successful and failed runs. Improvements must not weaken success criteria or silently waive unmet requirements.

## Statistics

`stats` reads existing artifacts without starting a sensor, decider, or worker:

```sh
goal stats --since 24h
GOAL_DIR=~/goals/mergeable-prs goal stats --since 7d --output json
```

It reports outcome counts, worker success rate, failure kinds, and average/p50/p95 duration by role. Directories created before metadata support are counted separately across all time and excluded from filtered success and duration calculations rather than inferred.

## Output modes

The default `--output tui` opens a fullscreen streaming activity feed when stdin and stdout are terminals. Each newline-delimited child diagnostic is one card. A scrollbar at the right shows the visible position when the activity buffer overflows. Use Up/Down or `j`/`k` to select, Enter/Space or a mouse click to expand, the mouse wheel or PageUp/PageDown to scroll, End or `a` to resume following new activity, and `q` or Ctrl-C to stop. Scrolling away from the end pauses automatic following.

TUI mode falls back silently to plain output when redirected or run without an interactive terminal. `goal stats` always prints its report instead of opening the fullscreen viewer.

Use `--output plain` for timestamped streaming text. Use `--output pretty` to indent JSON diagnostics as one prefixed terminal block without omitting any values. Display formatting affects only the foreground; each run's `stdout.log` and `stderr.log` retains the original bytes. Non-JSON diagnostics remain unchanged.

Use `--output json` for a machine-readable controller stream:

```sh
goal --output json | jq --unbuffered -C .
```

Every stdout line has the same `timestamp`, `type`, and `details` envelope. Controller events such as waits, completions, failures, and errors are structured. Child JSON is nested under `details.payload`; non-JSON child diagnostics are represented by `details.content`. Oversized child lines are replaced in the JSON foreground stream by bounded metadata with `truncated` and `original_bytes`; exact output remains in each run's `stdout.log` or `stderr.log`. Intentional Ctrl-C/SIGTERM emits `stopped` in JSON mode and exits successfully.

## Read-only GitHub sensor example

[`examples/mergeable-prs`](examples/mergeable-prs) contains a read-only `gh api graphql` sensor for authored open pull requests. It reports CI checks, merge state, and unresolved review threads. Replace the placeholder agent commands in its `goal.toml` with locally available non-TUI decider and worker commands.

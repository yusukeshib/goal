# goal

`goal` is a small foreground controller that repeatedly senses the world, asks a one-shot read-only decider for one typed action, and runs at most one disposable worker.

```text
sense -> decide -> run task / wait / complete -> sense
                  failure -> record reason -> exit 1
```

It has no daemon, PTY, TUI, interactive input, worker pool, or persistent agent conversation.

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
../../target/debug/goal .
```

The example completes without interactive input.

## Agent protocol

Deciders and workers are ordinary argv commands, without an implicit shell or PTY. `{prompt}` in any argv element is replaced with the generated prompt path. Without it, the prompt is piped to stdin and then closed. Every invocation receives:

- `GOAL_RUN_ID`
- `GOAL_PROMPT_PATH`
- `GOAL_RESULT_PATH`
- `GOAL_PROJECT_DIR`

The process must atomically write one tagged JSON object to `GOAL_RESULT_PATH`. Stdout and stderr are diagnostics only. Decider action tags are `run_task`, `wait`, `complete`, and `failure`; worker completion tags are `done` and `failure`. Neither process may request human input, approval, or intervention.

Runtime state, compact events, prompts, results, logs, and per-run `metadata.json` files are kept under `.goal/`. In plain output mode, sensor stdout is treated as protocol data and hidden from the terminal; it remains available in the run's `stdout.log` and is passed unchanged to the decider. Sensor stderr diagnostics are still displayed. Any sensor, decider, or worker process, timeout, or protocol failure—and any logical `failure`—is recorded with its reason and run ID, then terminates the controller with a non-zero status. Internal retries are not performed; an external scheduler may start a separate run. The retained artifacts support offline analysis of successful and failed runs. Improvements must not weaken success criteria or silently waive unmet requirements.

## Statistics

`stats` reads existing artifacts without starting a sensor, decider, or worker:

```sh
goal stats --since 24h
GOAL_DIR=~/goals/mergeable-prs goal stats --since 7d --output json
```

It reports outcome counts, worker success rate, failure kinds, and average/p50/p95 duration by role. Directories created before metadata support are counted separately across all time and excluded from filtered success and duration calculations rather than inferred.

## Controller JSONL output

Use `--output json` for a machine-readable controller stream:

```sh
goal --output json goal.toml | jq --unbuffered -C .
```

Every stdout line has the same `timestamp`, `type`, and `details` envelope. Controller events such as waits, completions, failures, and errors are structured. Child JSON is nested under `details.payload`; non-JSON child diagnostics are represented by `details.content`. Oversized child lines are replaced in the foreground stream by bounded metadata with `truncated` and `original_bytes`; exact output remains in each run's `stdout.log` or `stderr.log`. The default `--output plain` preserves human-readable terminal output. Intentional Ctrl-C/SIGTERM emits `stopped` in JSON mode and exits successfully.

## Read-only GitHub sensor example

[`examples/mergeable-prs`](examples/mergeable-prs) contains a read-only `gh api graphql` sensor for authored open pull requests. It reports CI checks, merge state, and unresolved review threads. Replace the placeholder agent commands in its `goal.toml` with locally available non-TUI decider and worker commands.

# goal

`goal` is a small foreground controller that repeatedly senses the world, asks a one-shot read-only decider for one typed action, and runs at most one disposable worker.

```text
sense -> decide -> run task / prompt human / wait / complete -> sense
```

It has no daemon, PTY, TUI, worker pool, or persistent agent conversation.

## Build and run

Stable Rust 1.85 or newer is required.

```sh
cargo build
cargo run -- goal.toml
```

The config path defines the project directory. Relative goal and command paths are resolved by running child processes in that directory. See [`examples/fake`](examples/fake) for a runnable deterministic cycle:

```sh
cargo build
cd examples/fake
../../target/debug/goal goal.toml
```

The example asks one question. If stdin reaches EOF, rerun the same command; the pending question is presented before sensing or deciding.

## Agent protocol

Deciders and workers are ordinary argv commands, without an implicit shell or PTY. `{prompt}` in any argv element is replaced with the generated prompt path. Without it, the prompt is piped to stdin and then closed. Every invocation receives:

- `GOAL_RUN_ID`
- `GOAL_PROMPT_PATH`
- `GOAL_RESULT_PATH`
- `GOAL_PROJECT_DIR`

The process must atomically write one tagged JSON object to `GOAL_RESULT_PATH`. Stdout and stderr are diagnostics only. Decider action tags are `run_task`, `prompt_human`, `wait`, and `complete`; worker completion tags are `done`, `needs_input`, and `blocked`.

Runtime state, compact events, prompts, results, and logs are kept under `.goal/`. A worker process or protocol failure is never blindly retried: the controller senses current reality and asks a fresh decider.

## Read-only GitHub sensor example

[`examples/mergeable-prs`](examples/mergeable-prs) contains a read-only `gh api graphql` sensor for authored open pull requests. It reports CI checks, merge state, and unresolved review threads. Replace the placeholder agent commands in its `goal.toml` with locally available non-TUI decider and worker commands.

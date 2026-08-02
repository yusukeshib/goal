goal — implementation plan
==========================

Purpose
-------

Build a small, foreground controller that continuously works toward one goal
written in natural language.

The controller has a fixed control flow:

    sense -> decide -> act -> sense -> ...

The control flow is deterministic, but the LLM's decision and work are not. A
single disposable worker performs one task at a time and then exits.

Greenfield constraint
---------------------

Implement this project from this plan and the Rust/library documentation. Do
not inspect, copy, port, or reuse implementation code or architecture from
another project. Prefer the smallest design that demonstrates the experiment.

Confirmed design
----------------

1. There is exactly one natural-language goal.
2. The controller runs in the foreground.
3. Each cycle senses the current world before deciding what to do.
4. A non-TUI, one-shot decider receives the goal and observation and returns one
   typed action.
5. At most one non-TUI worker process runs at a time.
6. A worker receives exactly one task, returns a structured completion, and
   exits.
7. A worker never pauses for human input and never reads interactive stdin.
8. After every worker exit, the controller senses the world again. Worker claims
   are never accepted as proof that the goal was reached.
9. If a worker needs a human decision, it exits successfully with a
   `needs_input` completion containing a question and enough context to resume
   with a new worker.
10. Human input is collected by the foreground controller, recorded, and given
    to the next decider invocation.
11. No daemon, PTY management, worker fleet, parallel workers, mailbox,
    scheduler, fairness policy, or TUI is part of v0.

State machine
-------------

    SENSE
      |
      | observation
      v
    DECIDE
      |
      +-- run_task ------> RUN WORKER ------> worker completion --+
      |                                                        |
      +-- prompt_human --> READ ANSWER -------------------------+
      |                                                        |
      +-- wait ----------> SLEEP -------------------------------+
      |                                                        |
      +-- complete ------> EXIT                                |
                                                               |
                                                               +--> SENSE

A worker completion of `needs_input` bypasses another LLM call: the controller
shows the worker's question verbatim, reads the human's answer, records both,
and then returns to SENSE. The next decider receives the question, context, and
answer.

Protocol types
--------------

Use Rust enums with serde's tagged JSON representation. Keep v0 fields small.
The exact names may change during implementation, but semantic distinctions
must remain.

Decider action:

    RunTask {
        task: String,
    }

    PromptHuman {
        question: String,
        context: Option<String>,
    }

    Wait {
        reason: String,
        retry_after_seconds: u64,
    }

    Complete {
        summary: String,
    }

Worker completion:

    Done {
        summary: String,
    }

    NeedsInput {
        question: String,
        context: String,
        resume_hint: Option<String>,
    }

    Blocked {
        reason: String,
    }

Do not conflate a logical completion with an operating-system process result:

* exit 0 plus valid completion: valid worker outcome;
* non-zero exit: runner/infrastructure failure;
* exit 0 without valid completion: protocol failure;
* timeout: process failure terminated by the controller.

`Blocked` means the agent ran correctly but determined that it cannot safely or
meaningfully proceed. It is not an authentication, network, process, or protocol
error.

Runner contract
---------------

Both decider and worker are ordinary non-TUI child processes. Do not allocate a
PTY.

For every invocation, create a run directory containing:

    prompt.md
    result.json
    stdout.log
    stderr.log

Expose at least these environment variables to the child:

    GOAL_RUN_ID
    GOAL_PROMPT_PATH
    GOAL_RESULT_PATH
    GOAL_PROJECT_DIR

The configured command is argv, not an implicit shell string. If a shell is
needed, the user must explicitly configure `sh -c` or equivalent. Support a
`{prompt}` placeholder in an argv element for CLIs that require a prompt file
argument. Commands that can read the prompt from stdin may omit the placeholder;
the controller then pipes `prompt.md` to stdin and closes it.

The agent must write exactly one JSON object to `GOAL_RESULT_PATH`. Stdout and
stderr are for diagnostics only and must never be parsed as the protocol. Stream
them to the foreground terminal while also saving them in the run directory.

A result should be published atomically when practical. A partially written,
missing, malformed, or schema-invalid result is a protocol failure.

The decider is not an actor. Its contract forbids modifying the project or the
external world. Run it with the least authority supported by the configured
agent CLI. Only the worker is authorized to perform the selected task.

Worker contract
---------------

Inject these rules into every worker prompt:

* Perform only the one assigned task.
* Do not broaden the task or select a new task.
* Do not wait for, prompt, or read input from a human.
* Complete all safe work possible before returning `needs_input`.
* Before an irreversible or human-only decision, stop at a safe boundary and
  return `needs_input`; do not perform the operation.
* Include enough context for a fresh decider and worker to continue without the
  current process's conversational context.
* Write one structured completion and exit.
* Do not claim success based only on commands attempted; describe what actually
  changed or was verified.

The worker process is disposable. No resume, attach, session restoration, or
conversation continuation is required.

Configuration
-------------

Use a repo-local TOML file, tentatively `goal.toml`, and a natural-language goal
file, tentatively `GOAL.md`.

Minimum configuration:

    goal_file
    sensor command argv
    decider command argv
    worker command argv
    normal observation interval
    infrastructure retry delay
    decider timeout
    worker timeout

Example shape (names are provisional):

    goal_file = "GOAL.md"
    interval_seconds = 60
    retry_seconds = 30

    [sensor]
    command = ["./sensor.sh"]
    timeout_seconds = 60

    [decider]
    command = ["agent-cli", "--non-interactive", "{prompt}"]
    timeout_seconds = 300

    [worker]
    command = ["agent-cli", "--non-interactive", "{prompt}"]
    timeout_seconds = 1800

The sensor command must emit exactly one JSON value on stdout. Sensor stderr is
diagnostic. A non-zero exit, timeout, or invalid JSON is a sensing failure and
must never be interpreted as an empty or healthy world.

Persistent files
----------------

Keep runtime artifacts under a repo-local `.goal/` directory:

    .goal/
      state.json
      events.jsonl
      runs/<run-id>/...

`state.json` should contain only the minimum information needed to restart:

* latest worker completion, if any;
* pending human question, if any;
* latest human answer, if any;
* timestamp and identifier of the latest cycle.

Write state atomically. Append compact events for debugging, but do not build an
event-sourcing framework. Large observations and process logs belong in their
run directories rather than `state.json`.

If the process exits while waiting for human input, preserve the pending
question. On the next invocation, present that question before calling the
sensor or decider again. Record the answer before resuming the loop.

Human-in-the-loop behavior
--------------------------

Human prompts are synchronous and terminal-based in v0.

For a `PromptHuman` decider action:

1. persist the pending question;
2. print the question and optional context;
3. read an answer from the controlling terminal;
4. persist the answer and clear the pending marker;
5. return to SENSE.

For `WorkerCompletion::NeedsInput`, use the same flow, showing the worker's
question verbatim. Do not ask the decider to rewrite or summarize it.

If stdin reaches EOF, keep the pending question and exit with a clear message.
Do not invent, default, or auto-approve an answer.

Failure policy
--------------

Keep failure behavior explicit and conservative:

* Sensor failure: do not call the decider; report it, wait the configured retry
  delay, and sense again.
* Decider process/protocol failure: it is safe to retry after a delay because the
  decider is forbidden from acting. Keep the same observation available for
  diagnostics.
* Worker process/protocol failure: never blindly rerun the worker because it may
  have changed the world before failing. Record the failure, return to SENSE,
  and let a fresh decider choose the next task from current reality.
* Worker `Blocked`: record it and return to SENSE; the decider may prompt the
  human, choose another safe task, wait, or complete.
* Ctrl-C/SIGTERM: terminate the active child, save the run outcome if possible,
  and exit. Do not start another cycle.

Use bounded delays and timeouts. Do not implement elaborate exponential
backoff, error classification by log regex, or automatic provider failover in
v0.

Convergence semantics
---------------------

Support both finite and continuous natural-language goals:

* `Complete` exits successfully when the decider determines a finite goal is
  satisfied.
* `Wait` represents temporary convergence or a world that currently needs no
  action. Sleep for the requested bounded interval, then sense again.

The controller should cap `retry_after_seconds` to a configured maximum so an
LLM cannot make the foreground loop disappear for an unreasonable duration.

Initial CLI
-----------

Start with one user-facing command:

    goal run [--config goal.toml]

Optional debugging commands may be added only if implementation or testing
shows a clear need. Avoid building init, status, watch, attach, worker-management,
or background-service commands in v0.

Rust implementation
-------------------

Use stable Rust and edition 2024. Prefer synchronous code unless asynchronous
code clearly removes more complexity than it adds.

Suggested dependencies:

* `clap` for the CLI;
* `serde` and `serde_json` for protocol/state;
* `toml` for configuration;
* `anyhow` for top-level error context;
* a small, focused crate for child-process timeout or signal handling only if
  the standard library implementation becomes distracting.

Avoid a large async/runtime dependency for the first vertical slice. The
controller has only one child at a time, so threads are sufficient to stream
stdout and stderr concurrently while the parent waits.

Suggested modules:

    src/main.rs          CLI and process exit
    src/config.rs        TOML loading and validation
    src/model.rs         actions, completions, persisted state
    src/controller.rs    foreground state machine
    src/sensor.rs        sensor invocation and JSON validation
    src/runner.rs        decider/worker subprocess execution
    src/prompt.rs        decider and worker prompt construction
    src/state.rs         atomic state and event persistence
    src/human.rs         terminal prompt handling

Keep module boundaries pragmatic; do not introduce traits until tests or a
second implementation require them.

Implementation milestones
-------------------------

Milestone 1 — scaffold and types

* Create the Rust binary and CLI.
* Define and test serde schemas for decider actions and worker completions.
* Load and validate `goal.toml` and `GOAL.md`.
* Add `.goal/` to `.gitignore`.

Milestone 2 — process protocol

* Create run directories and prompt/result paths.
* Execute argv commands without an implicit shell.
* Support prompt-file placeholder or stdin prompt delivery.
* Stream and save stdout/stderr.
* Validate result files and distinguish process, timeout, and protocol failures.

Milestone 3 — one complete vertical cycle

* Run a fake JSON sensor.
* Invoke a fake decider returning `RunTask`.
* Invoke a fake worker returning `Done`.
* Re-sense and invoke the decider again.
* Exit on `Complete`.

Milestone 4 — human input

* Implement `PromptHuman`.
* Implement direct handling of worker `NeedsInput`.
* Persist pending questions and answers.
* Verify restart behavior after EOF or interruption while awaiting input.

Milestone 5 — waiting and failures

* Implement bounded `Wait`.
* Add timeouts and signal cancellation.
* Add the conservative sensor/decider/worker failure policies.
* Ensure a failed worker is never automatically rerun before another sense.

Milestone 6 — real experiment

Create an example for a goal such as keeping authored open pull requests
mergeable. Its sensor should report, in structured JSON, open pull requests,
CI state, merge conflicts, and unresolved feedback. Keep goal-specific GitHub
logic outside the generic controller.

Tests and completion criteria
-----------------------------

Unit tests:

* every valid and invalid protocol variant;
* configuration validation;
* state atomic-write/read round trips;
* prompt construction and placeholder substitution;
* wait-duration capping.

Integration tests with temporary fake executables:

* sense -> run task -> done -> re-sense -> complete;
* worker `needs_input` -> human answer -> fresh decision;
* decider `prompt_human` -> answer -> fresh decision;
* sensor non-zero/timeout/invalid JSON does not call the decider;
* decider failure retries without acting;
* worker failure causes re-sense and is not blindly rerun;
* exit 0 without `result.json` is a protocol failure;
* pending human question survives restart;
* Ctrl-C does not start another cycle.

Before declaring v0 complete, run:

    cargo fmt --check
    cargo check
    cargo clippy --all-targets -- -D warnings
    cargo test

v0 is complete when a fake end-to-end goal and one real, read-only sensor can
run through the full foreground loop, including a worker question and restart,
without PTY, background processes, or parallel workers.

Explicit non-goals for v0
-------------------------

* multiple goals;
* multiple or parallel workers;
* detached/background execution;
* PTY or TUI support;
* worker pause/resume or attach;
* persistent worker conversations;
* asynchronous mailbox or remote notifications;
* scheduling beyond `Wait`;
* automatic merging, deployment, or deletion without human approval;
* generic workflow DAGs;
* plugins or a stable public SDK;
* database storage;
* automatic multi-provider failover;
* complex retry, fairness, or cost-budget policies.

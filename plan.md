goal — design
=============

Purpose
-------

`goal` is a small foreground controller that continuously pursues one natural-
language goal:

    sense -> decide -> act -> sense -> ...

A one-shot read-only decider chooses one typed action. At most one disposable
worker performs one bounded task. Neither process can ask for human input,
approval, or intervention.

Confirmed design
----------------

1. There is exactly one natural-language goal.
2. Every cycle senses current reality before deciding.
3. The decider is read-only and returns one typed action.
4. At most one non-TUI worker runs at a time.
5. A worker receives exactly one task, writes one structured completion, and
   exits.
6. Worker claims do not prove convergence. After `done`, the controller senses
   reality again.
7. Human input is not part of the runtime. There are no questions, pending
   approvals, resumable conversations, or interactive terminal state.
8. If the decider or worker determines that automatic progress is impossible,
   it returns `failure` with a concrete reason.
9. Every sensor, decider, or worker process, timeout, or protocol failure is
   terminal. The controller records it and exits non-zero without an internal
   retry. A scheduler may start a separate run later.
10. No daemon, PTY, TUI, worker pool, parallel workers, scheduler, workflow DAG,
    or persistent agent conversation is part of the controller.

State machine
-------------

    SENSE
      |
      v
    DECIDE
      |
      +-- run_task --> RUN WORKER -- done ----> SENSE
      |                          |
      |                          +-- failure --> RECORD --> EXIT 1
      |
      +-- wait -------------------------------> SLEEP --> SENSE
      |
      +-- complete ---------------------------> RECORD --> EXIT 0
      |
      +-- failure ----------------------------> RECORD --> EXIT 1

Protocol
--------

Decider actions use serde's `type` tag:

    {"type":"run_task","task":"one bounded task"}
    {"type":"wait","reason":"temporary reason","retry_after_seconds":60}
    {"type":"complete","summary":"fresh evidence of convergence"}
    {"type":"failure","reason":"why automatic progress is impossible"}

Worker completions:

    {"type":"done","summary":"actual changes and verification"}
    {"type":"failure","reason":"why automatic completion is impossible"}

Every text field must be non-empty. Unknown and extra fields are rejected.
`failure` reasons must contain concrete evidence useful for diagnosing the run
and improving future goals or automation.

`wait` is only for a temporary world condition that can resolve automatically.
A human-only decision, unavailable authority, missing credential, or unsafe
operation is `failure`, not `wait`.

Runner contract
---------------

Sensor, decider, and worker commands are argv arrays, not implicit shell
strings. No PTY is allocated. Each agent invocation receives:

    GOAL_RUN_ID
    GOAL_PROMPT_PATH
    GOAL_RESULT_PATH
    GOAL_PROJECT_DIR

`{prompt}` in argv is replaced with the generated prompt path. Without the
placeholder, prompt text is piped to stdin and stdin is closed. The child must
atomically write exactly one JSON object to `GOAL_RESULT_PATH`. Stdout and
stderr are diagnostics and are never parsed as protocol output.

Each invocation has a run directory:

    .goal/runs/<run-id>/
      prompt.md
      result.json
      stdout.log
      stderr.log

Process and logical outcomes are distinct:

* exit 0 plus a valid result: valid outcome;
* non-zero exit: process failure;
* exit 0 without a valid result: protocol failure;
* timeout: process failure terminated by the controller.

Worker contract
---------------

Every worker prompt requires the worker to:

* perform only its assigned task;
* never request or read human input;
* complete all safe automatic work possible;
* avoid unauthorized, irreversible, or human-only operations;
* return `failure` with concrete evidence if completion is impossible;
* write exactly one completion and exit;
* report what changed and what was actually verified.

A worker process/protocol failure and a logical worker `failure` both terminate
the controller. This prevents a scheduler or decider loop inside one invocation
from repeatedly launching the same unsafe or impossible task.

Persistence and analysis
------------------------

`.goal/state.json` contains only:

* latest worker completion, if any;
* latest cycle identifier and timestamp.

`.goal/events.jsonl` contains compact structured lifecycle and outcome events.
Exact prompts, results, stdout, stderr, and versioned `metadata.json` stay in
individual run directories. Metadata records role, start/end timestamps,
duration, outcome, failure kind, result type, and reason. Existing state files
containing legacy human-question fields are migrated by discarding those fields
on load.

The artifacts must distinguish:

* successful worker `done`;
* logical decider or worker `failure` and its reason;
* worker process, timeout, and protocol failures;
* finite goal `complete`;
* terminal sensor, decider, and worker infrastructure failures.

Target selection is shared by run and analysis commands: `GOAL_DIR` selects the
goal directory, otherwise the current directory is used. `goal stats --since
24h` scans metadata without starting child processes and reports role-specific
outcomes, worker success rate, failure kinds, and average/p50/p95 durations.
Legacy run directories without metadata are counted across all time but excluded
from filtered rates and durations.

Daily goal improvement
----------------------

A separate offline command will periodically analyze all successful and failed
run artifacts and propose improvements to GOAL.md or automation. It is not part
of the foreground execution loop.

The improvement process must:

* use complete run artifacts, not only summaries;
* identify repeated failure classes and successful patterns;
* preserve or strengthen the goal's success criteria;
* never improve apparent success by silently waiving an unmet requirement;
* produce an auditable GOAL.md diff and validation evidence;
* remain independently schedulable and testable.

Configuration
-------------

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

Failure policy
--------------

* Sensor process/protocol failure or timeout: preserve artifacts, record the
  reason, and exit non-zero. Never treat missing or invalid sensor data as a
  healthy world.
* Decider process/protocol failure or timeout: preserve artifacts, record the
  reason, and exit non-zero.
* Decider `failure`: record the reason and exit non-zero.
* Worker `failure`: record the result and reason, then exit non-zero.
* Worker process/protocol failure or timeout: preserve artifacts, record the
  reason, and exit non-zero without re-sensing or launching another worker.
* Ctrl-C/SIGTERM: terminate the active child and exit cleanly without starting
  another cycle.

Verification
------------

Integration tests cover:

* sense -> run task -> done -> re-sense -> complete;
* finite completion and continuous waiting;
* sensor failure exits non-zero without invoking the decider;
* decider process failure exits non-zero without acting or re-sensing;
* decider `failure` exits non-zero without a worker;
* worker `failure` exits non-zero after one worker;
* worker non-zero, timeout, and missing/invalid result are terminal;
* full failure reason and run ID are retained in events;
* legacy human state loads without restoring an interaction path;
* Ctrl-C does not start another cycle.

Run the final gates with:

    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test

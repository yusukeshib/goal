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
7. Human input is not part of the controller workflow. There are no questions,
   pending approvals, or resumable conversations. The default fullscreen TUI is
   observational only and cannot steer child work.
8. If the decider determines that the current cycle cannot make automatic
   progress, or a worker cannot complete its assigned task, it returns `failure`
   with a concrete reason.
9. Sensor and decider failures are recorded and retried after a short backoff and
   fresh observation because both roles are read-only. A logical decider
   `failure` marks that run as failed without terminating the controller. Worker
   process, timeout, and protocol failures are recorded, exponentially backed
   off, and followed by a fresh observation. Logical failures are recorded and
   re-sensed without infrastructure backoff. Because a failed worker may have modified external state,
   its failure context tells the next decider not to repeat the task unless the
   newly observed reality materially changed.
10. No daemon, child PTY, worker pool, parallel workers, scheduler, workflow DAG,
    or persistent agent conversation is part of the controller. A bounded,
    observational TUI may render controller and child activity.

State machine
-------------

    SENSE
      |
      v
    DECIDE
      |
      +-- run_task --> RUN WORKER -- done ----> SENSE
      |                          |
      |                          +-- failure --> RECORD --> SENSE
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
    {"type":"failure","reason":"why this decision cycle cannot progress"}

Worker completions:

    {"type":"done","summary":"actual changes and verification"}
    {"type":"failure","reason":"why automatic completion is impossible"}

Every text field must be non-empty. Unknown and extra fields are rejected.
`failure` reasons must contain concrete evidence useful for diagnosing the run
and improving future goals or automation.

`wait` is for a temporary world condition expected to resolve automatically. A
decider uses `failure` when the current cycle cannot make safe automatic
progress and waiting is not the more accurate action; the run is recorded as
failed, then the controller backs off and re-senses. A worker uses `failure` when
its bounded task cannot be completed; that task-local outcome becomes context
for the next decider rather than terminating the goal.

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
* exit 0 without a valid result: protocol failure, recorded before a fresh
  observation and decision;
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

A worker process, timeout, or protocol failure is recorded, exponentially backed
off, and followed by fresh observation and decision. A logical failure follows
the same observation path without infrastructure backoff. The next decider receives failure context that
warns the worker may have modified external state and instructs it not to repeat
the same task unless reality materially changed. This prevents a blind loop while
allowing the continuous controller and independent work to continue.

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
* recoverable sensor, decider, and worker infrastructure failures;
* terminal controller and cancellation failures.

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
    worker_observation = "full" # or "none" for self-contained tasks
    max_completed_runs = 200    # optional finished-run retention

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
  reason, back off, and obtain a fresh observation. Never treat missing or
  invalid sensor data as a healthy world.
* Decider process/protocol failure or timeout: preserve artifacts, record the
  reason, back off, and obtain a fresh observation.
* Decider `failure`: record the failed run, back off, and obtain a fresh
  observation.
* Worker `failure`: record the result and reason, re-sense, and pass the
  task-local failure to the next decider so other work can continue.
* Worker process/protocol failure or timeout: preserve artifacts, record the
  reason and partial-side-effect warning, back off, re-sense, and pass that
  context to the next decider without blindly relaunching the same task.
* Ctrl-C/SIGTERM: terminate the active child and exit cleanly without starting
  another cycle.

Verification
------------

Integration tests cover:

* sense -> run task -> done -> re-sense -> complete;
* finite completion and continuous waiting;
* sensor process failure is recorded and a later cycle can complete the goal;
* decider process failure is recorded and a later cycle can complete the goal;
* decider `failure` is recorded and a later cycle can complete the goal;
* worker `failure` is recorded and a later cycle can complete the goal;
* worker non-zero, timeout, and missing/invalid result are recorded and a later
  cycle can complete the goal;
* full failure reason and run ID are retained in events;
* legacy human state loads without restoring an interaction path;
* Ctrl-C does not start another cycle.

Run the final gates with:

    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test

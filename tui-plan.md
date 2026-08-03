# Default streaming TUI implementation plan

## Purpose

Make `goal` easy to observe while it runs. The default interactive experience should be a fullscreen streaming activity feed, not a dump of child JSON. Each child output line is one independently collapsible card. The UI is observational only: it must not add approval gates, questions, a PTY for children, or any other human dependency to the controller state machine.

This file is the implementation plan used after context compaction. The work described here is now implemented.

## Confirmed product decisions

- Add `--output tui` and make it the CLI default.
- Keep explicit `plain`, `pretty`, and `json` modes.
- `goal` or `goal --output tui` uses the fullscreen UI only when stdin and stdout are terminals.
- Effective `tui` mode falls back silently to `plain` when not attached to a terminal. This preserves redirection, cron usage, `Command::output()`, and existing integration tests.
- `goal stats` renders the existing human-readable report when output mode is `tui`; it does not open a fullscreen UI. `--output json` remains the way to get structured stats.
- Child stdout/stderr files remain exact. TUI formatting, collapsing, retention, and truncation affect display only.
- One newline-delimited child output record is one card. Do not combine Pi `message_start`, `message_update`, or `message_end` events; that would make the UI depend on Pi's schema.
- Collapsed summaries are derived generically from arbitrary JSON shape. Unknown JSON and non-JSON output must remain usable.
- The controller's exit codes, failure policy, timeout policy, and Ctrl-C behavior must not change.
- On terminal completion, restore the normal screen immediately and print one concise final outcome line. Do not wait for confirmation before exiting.

## Implementation status

Implemented on branch `main` in `/Users/yusuke/projects/goal`:

- Ratatui/crossterm fullscreen activity feed and terminal guard
- default `tui` mode with non-TTY plain fallback
- generic one-line child summaries and one card per diagnostic
- keyboard and mouse collapse/expand, scrolling, and auto-follow
- bounded card/detail retention and lazy exact-log expansion
- controller cycle/phase activities
- exact artifact byte ranges emitted after run-log writes
- documentation and focused unit/integration coverage

The `pretty` fallback formats each child stdout line as one block with timestamp/role prefixed once. Child stderr remains on the plain passthrough path.

Verification completed with formatting, full tests, clippy with warnings denied, successful-completion PTY smoke testing, `q` cancellation, and terminal-failure restoration.

## User-facing behavior

### CLI

The output enum becomes:

```rust
pub enum OutputMode {
    Tui,
    Plain,
    Pretty,
    Json,
}
```

Clap default:

```rust
default_value_t = output::OutputMode::Tui
```

Effective behavior:

| Invocation | TTY | Effective output |
|---|---:|---|
| `goal` | yes | fullscreen TUI |
| `goal --output tui` | yes | fullscreen TUI |
| `goal` | no | plain stream |
| `goal --output tui >run.log` | no | plain stream |
| `goal --output plain` | either | plain stream |
| `goal --output pretty` | either | block-formatted stream |
| `goal --output json` | either | strict JSONL |
| `goal stats` | either | human-readable stats |
| `goal stats --output json` | either | one JSON report |

Use `std::io::IsTerminal` for terminal detection. Require both stdin and stdout to be terminals for fullscreen mode. Do not inspect or allocate a PTY for child processes.

### Screen

Suggested layout:

```text
 goal  ~/goals/mergeable-prs       cycle 12 · DECIDING · 00:18
──────────────────────────────────────────────────────────────────

  08:05:14  SENSOR   completed · 1.2s
▶ 08:05:15  DECIDER  {cwd: "/…", type: "session", version: 3}
▶ 08:05:15  DECIDER  {type: "agent_start"}
▶ 08:05:15  DECIDER  {type: "turn_start"}
▼ 08:05:15  DECIDER  {message: {… 4 fields}, type: "message_start"}
    {
      "message": {
        …
      },
      "type": "message_start"
    }
▶ 08:05:16  DECIDER  {message: {… 7 fields}, type: "message_start"}

──────────────────────────────────────────────────────────────────
 ↑↓/jk select  Enter/Click expand  PgUp/PgDn scroll  End follow  q quit
```

Header:

- goal/project directory
- current cycle number or ID
- current controller phase (`SENSING`, `DECIDING`, `WORKING`, `WAITING`, terminal state)
- phase elapsed time
- whether auto-follow is paused and how many newer cards exist

Body:

- chronological cards
- role and stream visually distinguished
- controller lifecycle entries and child diagnostic entries share one timeline
- collapsed cards occupy one row whenever possible
- expanded cards use wrapped, pretty-rendered detail rows

Footer:

- relevant key help
- auto-follow state
- eviction/truncation notice when applicable

### Controls

- `Up`/`Down`, `j`/`k`: move selection
- `Enter`/`Space`: toggle selected card
- left mouse click on a card header: select and toggle it
- mouse wheel and `PageUp`/`PageDown`: scroll
- `Home`: first retained card
- `End` or `a`: last card and resume auto-follow
- `Esc`: collapse the selected card
- `q` or Ctrl-C: set the existing cancellation flag and stop cleanly

Scrolling away from the bottom pauses auto-follow. New events continue to be collected and a count is shown. Returning to the end resumes auto-follow.

## Generic card summary rules

The summarizer must not switch on Pi event names.

For a JSON object:

- Render top-level scalar key/value pairs compactly.
- Render nested objects as `{… N fields}`.
- Render nested arrays as `[… N items]`.
- Render strings on one line with escaped control characters.
- Cap each displayed scalar and the total summary width.
- If there are more fields than fit, append an ellipsis marker.

For a JSON array:

- Show `JSON array · N items` plus a shallow preview when it fits.

For scalar JSON:

- Show the scalar directly, bounded to the summary width.

For non-JSON:

- Decode lossily as UTF-8 for display and show a bounded one-line preview.
- Keep stream and byte length metadata.

Expanded rendering:

- Load the exact child line from its run log.
- Pretty-print JSON generically with `serde_json`; show non-JSON as text.
- Wrap to the current terminal width.
- Apply a display cap (proposed: 256 KiB or 2,000 rendered lines) so one diagnostic cannot make the UI unusable.
- When capped, show the exact run-log path and original byte count.

The raw line must not be modified in `stdout.log` or `stderr.log`.

## Data model

Add a presentation-neutral activity model, separate from Ratatui widgets.

```rust
#[derive(Debug)]
enum Activity {
    Controller {
        timestamp: SystemTime,
        kind: String,
        details: serde_json::Value,
    },
    Child {
        timestamp: SystemTime,
        role: String,
        stream: ChildStream,
        run_id: String,
        artifact: ArtifactRange,
        summary: ChildSummary,
    },
    Notice {
        timestamp: SystemTime,
        level: NoticeLevel,
        text: String,
    },
}

#[derive(Debug, Clone)]
struct ArtifactRange {
    path: PathBuf,
    offset: u64,
    length: u64,
}

struct Card {
    id: u64,
    activity: Activity,
    expanded: bool,
    loaded_detail: Option<RenderedDetail>,
}

struct UiState {
    cards: VecDeque<Card>,
    selected: Option<usize>,
    scroll_row: usize,
    auto_follow: bool,
    unseen: usize,
    phase: ControllerPhase,
    hit_regions: Vec<HitRegion>,
    evicted: u64,
}
```

Keep reducer/state transitions independent from Ratatui so selection, collapsing, retention, and auto-follow can be unit tested without a terminal.

## Runtime architecture

Ratatui/crossterm should own the main thread. Move the synchronous controller execution to a scoped or joined worker thread only in effective TUI mode.

```text
main thread
  ├─ crossterm input polling
  ├─ drain Activity channel
  ├─ reduce UiState
  └─ ratatui rendering

controller thread
  └─ Controller
       └─ Runner tee threads
            ├─ write exact run log
            └─ Output sends Activity
```

Recommended dependencies compatible with the repository's Rust 1.85 MSRV:

```toml
ratatui = "0.29"
crossterm = "0.28"
```

Confirm the selected crate versions still support Rust 1.85 before committing. Prefer `std::sync::mpsc::sync_channel` initially; no additional channel crate is necessary unless implementation proves otherwise.

Use a bounded channel (proposed capacity: 1,024). The UI loop should drain all currently available activities before each render and render at a capped rate (roughly 30 FPS), rather than rendering once per message. This prevents diagnostic bursts from causing an unbounded queue while avoiding unnecessary draws.

### Output backend

Refactor `Output` so callers do not know whether output is streamed or sent to the UI.

Possible shape:

```rust
enum OutputBackend {
    Stream { write_lock: Arc<Mutex<()>> },
    Tui { sender: SyncSender<Activity> },
}

pub struct Output {
    mode: OutputMode,
    backend: Arc<OutputBackend>,
}
```

- `event()` writes a JSON envelope in JSON mode, sends a controller activity in TUI mode, and remains silent in plain/pretty where it is currently silent.
- `plain_stdout()` and `plain_stderr()` send notice/controller cards in TUI mode rather than touching the terminal.
- `child_line()` sends one child activity in TUI mode.
- No controller or tee thread may write directly to the terminal while the fullscreen UI is active.

Do not initialize the alternate screen until configuration loading and controller-lock acquisition can succeed, or ensure preflight failures are rendered normally. A clean approach is to resolve/load config before starting `TuiRuntime`, then construct the controller with a TUI-backed `Output`.

## Exact log references

Change `runner::tee` so presentation is notified only after exact bytes are safely recorded:

1. Record current log offset.
2. Write the complete line to the run log.
3. Flush the run log.
4. Append to the existing captured protocol buffer.
5. Call `output.child_line(...)` with `ArtifactRange { path, offset, length }` and the current line for summary construction.

The exact-log write must happen before UI notification. TUI failure or eviction must never lose diagnostics.

`tee` currently retains the entire stream in `captured` because sensor/protocol handling needs it. Do not accidentally add a second persistent raw copy in `UiState`.

## Controller lifecycle activities

Child diagnostics alone do not explain the `sense -> decide -> act` state. Emit explicit controller activities around phases:

- `cycle_started`
- `phase_started { phase: "sensor" }`
- `phase_finished { phase: "sensor", outcome, run_id, duration }`
- `phase_started { phase: "decider" }`
- existing `decision`
- `phase_started { phase: "worker", task }`
- existing `worker_completed` / `worker_failed`
- existing `wait`, `complete`, and failures

Prefer deriving durations and run IDs from run artifacts/metadata rather than duplicating timing logic where practical.

These are internal `goal` activities and may receive semantic rendering. Child JSON remains schema-independent.

`Controller::begin_cycle` currently stores a generated cycle ID but returns nothing. Change it to return or expose the cycle ID so the TUI can display a stable cycle boundary.

## Terminal lifecycle and failures

Implement an RAII terminal guard that always performs:

- disable raw mode
- disable mouse capture
- leave alternate screen
- show cursor

Restoration must happen on:

- successful completion
- logical/process/protocol failure
- Ctrl-C or `q`
- channel disconnect
- controller thread panic
- TUI render/input error

The TUI is presentation, not controller correctness. If the UI loop fails after the controller starts:

1. set the existing cancellation flag
2. restore the terminal
3. join the controller thread
4. report the UI error normally

Do not allow the controller to continue invisibly after the fullscreen UI exits.

When the controller finishes, send/observe a terminal runtime message, perform one final state reduction if possible, restore the normal screen, then print the concise final result or failure. Do not wait for a keypress.

## Retention

The goal may run continuously, so UI state must be bounded.

Initial policy:

- retain at most 2,000 cards
- retain summaries and artifact references, not raw child lines
- evict oldest collapsed cards first
- never evict the currently selected expanded card until it is collapsed or selection moves
- track and display the number of evicted cards

Exact history remains under `.goal/runs/` and `.goal/events.jsonl`.

If this policy complicates v1, a strict oldest-first 2,000-card limit is acceptable initially as long as selection indices remain valid and eviction is tested.

## File-level implementation map

### `Cargo.toml`

- Add Ratatui and crossterm dependencies with Rust 1.85-compatible versions.

### `src/main.rs`

- Make `OutputMode::Tui` the default.
- Resolve effective mode using `IsTerminal`.
- Keep stats non-fullscreen.
- Split normal controller execution from `run_controller_tui`.
- Ensure errors emitted after TUI teardown do not go to a closed TUI channel.
- Update CLI help text and examples.

### `src/output.rs`

- Add `Tui` mode.
- Introduce stream and TUI output backends.
- Add activity channel emission.
- Keep strict JSON output behavior unchanged.
- Keep the current one-prefix-per-message pretty formatting.
- Move generic JSON summary logic to a separate module if this file becomes unwieldy.

### `src/tui.rs` (new)

- `TuiRuntime` and `TerminalGuard`
- crossterm event loop
- state reducer
- Ratatui rendering
- hit-region calculation
- keyboard/mouse commands
- auto-follow and scroll calculations
- lazy artifact detail loading

Consider splitting later into `src/tui/model.rs`, `render.rs`, and `runtime.rs`; start with one module only if it remains readable.

### `src/runner.rs`

- Pass exact artifact byte ranges to `Output::child_line`.
- Preserve write/flush-before-notify ordering.

### `src/controller.rs`

- Emit phase/cycle activities.
- Expose cycle ID as needed.
- Do not change the controller state machine or failure policy.

### `tests/controller.rs`

- Preserve plain/json/pretty integration coverage.
- Add non-TTY default fallback coverage.
- Make tests explicit about output mode where behavior matters.

### `README.md`, `src/main.rs` long help, `plan.md`

- Document TUI as default.
- Document keys and non-TTY fallback.
- Correct the old `plan.md` statement that no TUI is part of the controller: an observational presentation TUI is now allowed, while child PTYs, approvals, and interactive workflow remain forbidden.

## Test plan

### Pure unit tests

- Arbitrary JSON object produces a bounded shallow summary.
- Nested objects/arrays are represented structurally without Pi-specific matching.
- Long strings and non-UTF-8 diagnostics are bounded safely.
- One child line creates exactly one card.
- Toggle affects only the selected/clicked card.
- Scrolling up disables auto-follow.
- `End` resumes auto-follow and selects/reveals the newest card.
- New cards increment unseen count while follow is paused.
- Retention eviction preserves valid selection and scroll state.
- Artifact range loading reads exactly the requested bytes.
- Expanded output cap reports the exact log path.

### Renderer tests

Use `ratatui::backend::TestBackend`:

- collapsed and expanded card layout
- narrow terminal wrapping
- header/footer and active phase
- selected/error/stderr styling
- unseen and evicted indicators
- hit regions correspond to rendered card rows

Avoid brittle full-screen snapshots if focused buffer assertions are clearer.

### Integration tests

- Default mode under `Command::output()` is non-TTY and falls back to plain.
- Explicit `plain`, `pretty`, and `json` retain their contracts.
- Pretty mode still treats one child line as one prefixed block.
- Exact run logs remain unchanged in TUI-related paths.
- Controller success/failure exit codes are unchanged.

### PTY smoke test

Run the deterministic `examples/fake` controller in a real PTY or tmux pane and verify:

- fullscreen entry
- live cards
- keyboard collapse/expand
- mouse collapse/expand
- wheel scrolling and paused follow
- `End` auto-follow
- Ctrl-C and `q`
- terminal restoration after success, failure, and interruption

A PTY automation test is desirable if reliable on CI; otherwise document and perform the manual smoke test before completion.

### Final gates

Run once implementation is complete:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Then perform the PTY smoke test.

## Implementation sequence

1. Re-read `/Users/yusuke/AGENTS.md`, inspect `git status`, and review the existing `src/output.rs` diff.
2. Add `Tui` to `OutputMode`, make it the declared default, and implement effective non-TTY fallback. Update exhaustive matches and add CLI/default tests before adding Ratatui.
3. Add the pure `Activity`, summary, `Card`, and `UiState` model with unit tests.
4. Add Ratatui/crossterm and build a static TestBackend-rendered timeline.
5. Refactor controller execution so the TUI owns the main thread and receives activities from a controller thread.
6. Add exact artifact ranges in `runner::tee` and lazy detail expansion.
7. Add keyboard navigation, auto-follow, and bounded retention.
8. Add mouse capture, hit regions, and click toggling.
9. Emit controller cycle/phase activities and render them semantically.
10. Add terminal guards, channel/panic/error cleanup, and final normal-screen summary.
11. Update README, long help, and `plan.md`.
12. Run focused tests during development, then all final gates and a real PTY smoke test.
13. Review `git diff` for unintended protocol, logging, exit-code, or state-machine changes.

## Acceptance criteria

- Running `goal` interactively opens the fullscreen activity feed by default.
- Every newline-delimited child diagnostic is represented by exactly one card.
- Cards can be collapsed/expanded with Enter and mouse click.
- The UI scrolls, pauses/resumes auto-follow correctly, and remains bounded during continuous runs.
- Generic child rendering does not require Pi event names or schemas.
- Exact child stdout/stderr logs are byte-for-byte unchanged.
- Non-TTY default execution remains useful and does not emit terminal control sequences.
- `plain`, `pretty`, and strict `json` modes still work.
- Sensor/decider/worker protocol behavior and failure semantics are unchanged.
- The terminal is restored on every exit path.
- Full formatting, clippy, tests, and PTY smoke verification pass.

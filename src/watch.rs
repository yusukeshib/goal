use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

use crate::output::OutputMode;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_READ_PER_POLL: u64 = 64 * 1024;
const MAX_PENDING: usize = 1024 * 1024;
const MAX_HUMAN_LINE_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity { dev: metadata.dev(), ino: metadata.ino() }
}

#[derive(Debug, Default)]
struct TailCursor {
    identity: Option<FileIdentity>,
    offset: u64,
    pending: Vec<u8>,
    discarding_until_newline: bool,
}

#[derive(Debug)]
struct Notice {
    kind: &'static str,
    details: Value,
}

impl Notice {
    fn gap(id: &str, reason: impl Into<String>) -> Self {
        Self { kind: "watch_gap", details: json!({ "id": id, "reason": reason.into() }) }
    }
}

pub fn run(output_mode: OutputMode) -> Result<()> {
    let (cancel_tx, cancel_rx) = mpsc::sync_channel(1);
    ctrlc::set_handler(move || {
        let _ = cancel_tx.try_send(());
    })
    .context("install watch cancellation handler")?;

    let mut rows = crate::list_snapshot()?;
    let mut cursors = HashMap::new();
    let mut baseline_notices = Vec::new();
    for row in &rows {
        if let Some((id, path)) = event_path(row) {
            let (cursor, error) = cursor_at_end(&path);
            if let Some(reason) = error {
                baseline_notices.push(Notice::gap(&id, reason));
            }
            cursors.insert(id, cursor);
        }
    }

    let tty = output_mode == OutputMode::Tui && io::stdout().is_terminal();
    let mut display = Display::new(output_mode, tty)?;
    if let Err(error) = display.snapshot(&rows) {
        return finish_io(error);
    }
    for notice in baseline_notices {
        if let Err(error) = display.notice(&notice, &rows) {
            return finish_io(error);
        }
    }

    loop {
        if cancel_rx.recv_timeout(POLL_INTERVAL).is_ok() {
            return Ok(());
        }

        let next = crate::list_snapshot()?;
        let notices = diff_rows(&rows, &next);
        for notice in notices {
            if let Err(error) = display.notice(&notice, &next) {
                return finish_io(error);
            }
        }

        let next_by_id = rows_by_id(&next);
        let old_by_id = rows_by_id(&rows);
        cursors.retain(|id, _| next_by_id.contains_key(id));
        for (id, row) in &next_by_id {
            let Some((_, path)) = event_path(row) else { continue };
            if should_rebase(old_by_id.get(id), row) {
                let (cursor, error) = cursor_at_end(&path);
                cursors.insert(id.clone(), cursor);
                if let Some(reason) = error {
                    let notice = Notice::gap(id, reason);
                    if let Err(error) = display.notice(&notice, &next) {
                        return finish_io(error);
                    }
                }
                continue;
            }
            let cursor = cursors.entry(id.clone()).or_default();
            for notice in poll_tail(id, &path, cursor) {
                if let Err(error) = display.notice(&notice, &next) {
                    return finish_io(error);
                }
            }
        }
        rows = next;
    }
}

fn finish_io(error: io::Error) -> Result<()> {
    if error.kind() == io::ErrorKind::BrokenPipe { Ok(()) } else { Err(error.into()) }
}

fn rows_by_id(rows: &[Value]) -> BTreeMap<String, Value> {
    rows.iter().filter_map(|row| {
        row.get("id").and_then(Value::as_str).map(|id| (id.to_owned(), row.clone()))
    }).collect()
}

fn diff_rows(previous: &[Value], current: &[Value]) -> Vec<Notice> {
    let before = rows_by_id(previous);
    let after = rows_by_id(current);
    let mut notices = Vec::new();
    for (id, goal) in &after {
        match before.get(id) {
            None => notices.push(Notice { kind: "goal_added", details: json!({ "goal": goal }) }),
            Some(old) if old != goal => notices.push(Notice {
                kind: "goal_changed",
                details: json!({ "goal": goal, "previous": old }),
            }),
            _ => {}
        }
    }
    for (id, goal) in &before {
        if !after.contains_key(id) {
            notices.push(Notice { kind: "goal_removed", details: json!({ "goal": goal }) });
        }
    }
    notices
}

fn event_path(row: &Value) -> Option<(String, PathBuf)> {
    let id = row.get("id")?.as_str()?.to_owned();
    let config = Path::new(row.get("config_path")?.as_str()?);
    Some((id, config.parent()?.join(".goal/events.jsonl")))
}

fn should_rebase(previous: Option<&Value>, current: &Value) -> bool {
    let Some(previous) = previous else { return true };
    previous.get("config_path") != current.get("config_path")
}

fn cursor_at_end(path: &Path) -> (TailCursor, Option<String>) {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (TailCursor::default(), None),
        Err(error) => return (TailCursor::default(), Some(format!("events open error: {error}"))),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return (TailCursor::default(), Some(format!("events metadata error: {error}"))),
    };
    let mut cursor = TailCursor {
        identity: Some(identity(&metadata)),
        offset: metadata.len(),
        pending: Vec::new(),
        discarding_until_newline: false,
    };
    if metadata.len() > 0 {
        let mut last = [0_u8; 1];
        if let Err(error) = file.seek(SeekFrom::Start(metadata.len() - 1)).and_then(|_| file.read_exact(&mut last)) {
            return (cursor, Some(format!("events baseline read error: {error}")));
        }
        cursor.discarding_until_newline = last[0] != b'\n';
    }
    (cursor, None)
}

fn poll_tail(id: &str, path: &Path, cursor: &mut TailCursor) -> Vec<Notice> {
    let mut notices = Vec::new();
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if cursor.identity.take().is_some() {
                cursor.offset = 0;
                cursor.pending.clear();
                cursor.discarding_until_newline = false;
                notices.push(Notice::gap(id, "events file disappeared"));
            }
            return notices;
        }
        Err(error) => {
            notices.push(Notice::gap(id, format!("events open error: {error}")));
            return notices;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            notices.push(Notice::gap(id, format!("events metadata error: {error}")));
            return notices;
        }
    };
    let current_identity = identity(&metadata);
    if let Some(old) = cursor.identity {
        if old != current_identity {
            notices.push(Notice::gap(id, "events file replaced"));
            cursor.offset = 0;
            cursor.pending.clear();
            cursor.discarding_until_newline = false;
        } else if metadata.len() < cursor.offset {
            notices.push(Notice::gap(id, "events file truncated"));
            cursor.offset = 0;
            cursor.pending.clear();
            cursor.discarding_until_newline = false;
        }
    }
    cursor.identity = Some(current_identity);

    let available = metadata.len().saturating_sub(cursor.offset).min(MAX_READ_PER_POLL);
    if available == 0 { return notices; }
    if let Err(error) = file.seek(SeekFrom::Start(cursor.offset)) {
        notices.push(Notice::gap(id, format!("events seek error: {error}")));
        return notices;
    }
    let mut bytes = Vec::with_capacity(available as usize);
    if let Err(error) = file.take(available).read_to_end(&mut bytes) {
        notices.push(Notice::gap(id, format!("events read error: {error}")));
        return notices;
    }
    cursor.offset += bytes.len() as u64;
    let bytes = if cursor.discarding_until_newline {
        match bytes.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                cursor.discarding_until_newline = false;
                &bytes[newline + 1..]
            }
            None => return notices,
        }
    } else {
        &bytes[..]
    };
    cursor.pending.extend_from_slice(bytes);

    while let Some(newline) = cursor.pending.iter().position(|byte| *byte == b'\n') {
        let mut line = cursor.pending.drain(..=newline).collect::<Vec<_>>();
        line.pop();
        if line.last() == Some(&b'\r') { line.pop(); }
        if line.len() > MAX_PENDING {
            notices.push(Notice::gap(id, "oversized event line discarded"));
        } else if !line.is_empty() {
            match serde_json::from_slice::<Value>(&line) {
                Ok(event) if event.is_object() => notices.push(Notice { kind: "goal_event", details: json!({ "id": id, "event": event }) }),
                Ok(_) => notices.push(Notice::gap(id, "non-object event JSON discarded")),
                Err(error) => notices.push(Notice::gap(id, format!("invalid event JSON discarded: {error}"))),
            }
        }
    }
    if cursor.pending.len() > MAX_PENDING {
        cursor.pending.clear();
        cursor.discarding_until_newline = true;
        notices.push(Notice::gap(id, "oversized partial event line discarded"));
    }
    notices
}

struct Display {
    mode: OutputMode,
    tty: bool,
}

impl Display {
    fn new(mode: OutputMode, tty: bool) -> io::Result<Self> {
        Ok(Self { mode, tty })
    }

    fn snapshot(&mut self, rows: &[Value]) -> io::Result<()> {
        if self.mode == OutputMode::Json {
            return write_envelope("snapshot", json!({ "goals": rows }));
        }
        if self.tty { self.render(rows) } else { write_plain_snapshot(rows) }
    }

    fn notice(&mut self, notice: &Notice, rows: &[Value]) -> io::Result<()> {
        if self.mode == OutputMode::Json {
            return write_envelope(notice.kind, notice.details.clone());
        }
        let line = bounded_human_line(format!("{} [{}] {}", Utc::now().to_rfc3339(), notice.kind, notice.details));
        {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{line}")?;
            stdout.flush()?;
        }
        if self.tty && matches!(notice.kind, "goal_added" | "goal_changed" | "goal_removed") {
            self.render(rows)?;
        }
        Ok(())
    }

    fn render(&self, rows: &[Value]) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let (width, _) = crossterm::terminal::size().unwrap_or((80, 24));
        crate::list_table::write(&mut stdout, rows, width, crate::list_table::height(rows))
    }
}

fn write_envelope(kind: &str, details: Value) -> io::Result<()> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let envelope = json!({ "timestamp": timestamp, "type": kind, "details": details });
    let mut bytes = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()
}

fn bounded_human_line(line: String) -> String {
    crop_line(&line, MAX_HUMAN_LINE_CHARS)
}

fn crop_line(line: &str, width: usize) -> String {
    // Do not let tabs/control sequences or wide Unicode text wrap a dashboard
    // row. Conservatively reserve two cells for non-ASCII characters.
    let mut cropped = String::new();
    let mut cells = 0;
    for character in line.chars() {
        let character = if character.is_control() { ' ' } else { character };
        let size = if character.is_ascii() { 1 } else { 2 };
        if cells + size > width {
            break;
        }
        cropped.push(character);
        cells += size;
    }
    cropped
}

fn write_plain_snapshot(rows: &[Value]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if rows.is_empty() {
        writeln!(stdout, "no registered goals; use goal add <path>")?;
    } else {
        writeln!(stdout, "ID\tENABLED\tSTATUS\tPID\tGOAL_FILE")?;
        for row in rows { writeln!(stdout, "{}", plain_row(row))?; }
    }
    stdout.flush()
}

fn plain_row(row: &Value) -> String {
    let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("-");
    let enabled = row.get("enabled").and_then(Value::as_bool).map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    let pid = row.get("pid").and_then(Value::as_u64).map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    format!("{}\t{}\t{}\t{}\t{}", text("id"), enabled, text("status"), pid, text("config_path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, enabled: bool) -> Value {
        json!({ "id": id, "enabled": enabled, "config_path": format!("/tmp/{id}/goal.md") })
    }

    #[test]
    fn diff_reports_add_change_remove_and_ignores_unchanged() {
        let notices = diff_rows(&[row("same", true), row("changed", true), row("gone", true)],
                                &[row("same", true), row("changed", false), row("new", true)]);
        assert_eq!(notices.iter().map(|n| n.kind).collect::<Vec<_>>(),
                   vec!["goal_changed", "goal_added", "goal_removed"]);
    }

    #[test]
    fn diff_emits_nothing_for_identical_rows() {
        let rows = vec![row("same", true)];
        assert!(diff_rows(&rows, &rows).is_empty());
    }

    #[test]
    fn row_changes_rebase_only_when_config_path_changes() {
        let before = row("same", true);
        let enabled_changed = row("same", false);
        let moved = json!({ "id": "same", "enabled": true, "config_path": "/elsewhere/goal.md" });
        assert!(!should_rebase(Some(&before), &enabled_changed));
        let mut restarted = before.clone();
        restarted["status"] = json!("running");
        restarted["pid"] = json!(42);
        assert!(!should_rebase(Some(&before), &restarted));
        assert!(should_rebase(Some(&before), &moved));
        assert!(should_rebase(None, &before));
    }

    #[test]
    fn baseline_partial_suffix_is_skipped_before_next_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, b"{\"historical\":").unwrap();
        let mut cursor = cursor_at_end(&path).0;
        use std::fs::OpenOptions;
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"true}\n{\"new\":true}\n").unwrap();
        let notices = poll_tail("g", &path, &mut cursor);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].details["event"]["new"], true);
    }

    #[test]
    fn tail_retains_partial_then_emits_appended_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, b"{\"type\":\"par").unwrap();
        let mut cursor = TailCursor::default();
        assert!(poll_tail("g", &path, &mut cursor).is_empty());
        use std::fs::OpenOptions;
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"tial\"}\n").unwrap();
        let notices = poll_tail("g", &path, &mut cursor);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, "goal_event");
    }

    #[test]
    fn tail_rejects_valid_non_object_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, b"[1,2,3]\n\"scalar\"\n").unwrap();
        let notices = poll_tail("g", &path, &mut TailCursor::default());
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|n| n.kind == "watch_gap"));
        assert!(notices.iter().all(|n| n.details["reason"] == "non-object event JSON discarded"));
    }

    #[test]
    fn tail_reports_replacement_and_reads_new_file_from_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, b"{\"old\":true}\n").unwrap();
        let mut cursor = cursor_at_end(&path).0;
        fs::rename(&path, dir.path().join("events.old")).unwrap();
        fs::write(&path, b"{\"new\":true}\n").unwrap();
        let notices = poll_tail("g", &path, &mut cursor);
        assert_eq!(notices[0].details["reason"], "events file replaced");
        assert!(notices.iter().any(|n| n.kind == "goal_event" && n.details["event"]["new"] == true));
    }

    #[test]
    fn tail_reports_truncation_invalid_and_oversized_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, b"long old contents\n").unwrap();
        let mut cursor = cursor_at_end(&path).0;
        fs::write(&path, b"not-json\n").unwrap();
        let notices = poll_tail("g", &path, &mut cursor);
        assert!(notices.iter().any(|n| n.kind == "watch_gap"));

        fs::write(&path, vec![b'x'; MAX_PENDING + 1]).unwrap();
        cursor.offset = 0;
        cursor.pending.clear();
        let notices = poll_tail("g", &path, &mut cursor);
        // The per-poll read bound means several polls are required to reach the cap.
        let mut all = notices;
        for _ in 0..20 { all.extend(poll_tail("g", &path, &mut cursor)); }
        assert!(all.iter().any(|n| n.details["reason"] == "oversized partial event line discarded"));
    }

    #[test]
    fn oversized_line_suffix_is_discarded_and_next_event_is_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        // Cross the partial-buffer cap before a later read brings the newline.
        // A syntactically valid suffix must not masquerade as a separate event.
        let mut contents = vec![b'x'; MAX_PENDING + MAX_READ_PER_POLL as usize];
        contents.extend_from_slice(b"{\"suffix\":true}\n{\"after\":true}\n");
        fs::write(&path, contents).unwrap();
        let mut cursor = TailCursor::default();
        let mut notices = Vec::new();
        for _ in 0..20 { notices.extend(poll_tail("g", &path, &mut cursor)); }
        assert!(notices.iter().any(|n| n.details["reason"] == "oversized partial event line discarded"));
        assert_eq!(notices.iter().filter(|n| n.kind == "goal_event").count(), 1);
        assert!(notices.iter().any(|n| n.kind == "goal_event" && n.details["event"]["after"] == true));
    }
}

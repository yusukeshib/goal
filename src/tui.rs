//! Full-screen, observational activity feed for a running controller.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{Local, TimeZone};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use serde_json::Value;

const MAX_CARDS: usize = 2_000;
const SUMMARY_CHARS: usize = 160;
const DETAIL_BYTES: usize = 256 * 1024;
const DETAIL_LINES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRange {
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub enum Activity {
    Controller {
        timestamp: u64,
        kind: String,
        details: Value,
    },
    Child {
        timestamp: u64,
        role: String,
        stream: String,
        run_id: String,
        artifact: ArtifactRange,
        summary: String,
        original_bytes: usize,
    },
    Notice {
        timestamp: u64,
        level: NoticeLevel,
        text: String,
    },
}

/// Produce a schema-independent, single-line and bounded description.
pub fn summarize_line(bytes: &[u8]) -> String {
    let value = serde_json::from_slice::<Value>(bytes);
    let summary = match value {
        Ok(Value::Object(map)) => {
            let mut parts = Vec::new();
            for (key, value) in map {
                let value = match value {
                    Value::Object(ref v) => format!("{{… {} fields}}", v.len()),
                    Value::Array(ref v) => format!("[… {} items]", v.len()),
                    other => scalar(&other, 48),
                };
                parts.push(format!("{key}: {value}"));
            }
            format!("{{{}}}", parts.join(", "))
        }
        Ok(Value::Array(values)) => {
            let preview = values
                .iter()
                .take(3)
                .map(|v| scalar(v, 32))
                .collect::<Vec<_>>()
                .join(", ");
            if preview.is_empty() {
                format!("JSON array · {} items", values.len())
            } else {
                format!("JSON array · {} items [{preview}]", values.len())
            }
        }
        Ok(value) => scalar(&value, SUMMARY_CHARS),
        Err(_) => String::from_utf8_lossy(bytes).replace(['\n', '\r', '\t'], " "),
    };
    truncate(&summary, SUMMARY_CHARS)
}

fn scalar(value: &Value, cap: usize) -> String {
    let text = match value {
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"?\"".into()),
        _ => value.to_string(),
    };
    truncate(&text, cap)
}

fn truncate(text: &str, cap: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

#[derive(Debug)]
struct Card {
    activity: Activity,
    expanded: bool,
    detail: Option<String>,
}

#[derive(Debug, Default)]
struct UiState {
    cards: VecDeque<Card>,
    selected: Option<usize>,
    top: usize,
    auto_follow: bool,
    unseen: usize,
    evicted: u64,
    phase: String,
    phase_started_at: Option<u64>,
    cycle: String,
    project: String,
    hit_regions: Vec<(u16, usize)>,
}

impl UiState {
    fn new() -> Self {
        Self {
            auto_follow: true,
            phase: "STARTING".to_owned(),
            phase_started_at: None,
            cycle: "-".to_owned(),
            project: "-".to_owned(),
            ..Self::default()
        }
    }

    fn push(&mut self, activity: Activity) {
        if let Activity::Controller { kind, details, .. } = &activity {
            match kind.as_str() {
                "cycle_started" => {
                    if let Some(cycle) = details.get("cycle_id").and_then(Value::as_str) {
                        self.cycle = cycle.to_owned();
                    }
                }
                "phase_started" => {
                    if let Some(phase) = details.get("phase").and_then(Value::as_str) {
                        self.phase = phase.to_uppercase();
                        self.phase_started_at = match &activity {
                            Activity::Controller { timestamp, .. } => Some(*timestamp),
                            _ => None,
                        };
                    }
                }
                "wait" => self.phase = "WAITING".to_owned(),
                "complete" => self.phase = "COMPLETE".to_owned(),
                kind if kind.ends_with("failed") => self.phase = "FAILED".to_owned(),
                _ => {}
            }
        }
        let was_empty = self.cards.is_empty();
        self.cards.push_back(Card {
            activity,
            expanded: false,
            detail: None,
        });
        if self.auto_follow || was_empty {
            self.selected = Some(self.cards.len() - 1);
        } else {
            self.unseen += 1;
        }
        while self.cards.len() > MAX_CARDS {
            self.cards.pop_front();
            self.evicted += 1;
            self.selected = self.selected.map(|i| i.saturating_sub(1));
            self.top = self.top.saturating_sub(1);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.cards.is_empty() {
            return;
        }
        let old = self.selected.unwrap_or(0) as isize;
        self.selected = Some((old + delta).clamp(0, self.cards.len() as isize - 1) as usize);
        self.auto_follow = self.selected == Some(self.cards.len() - 1);
        if !self.auto_follow {
            self.top = self.selected.unwrap_or(0).saturating_sub(5);
        } else {
            self.unseen = 0;
        }
    }

    fn end(&mut self) {
        if !self.cards.is_empty() {
            self.selected = Some(self.cards.len() - 1);
        }
        self.auto_follow = true;
        self.unseen = 0;
    }

    fn home(&mut self) {
        if !self.cards.is_empty() {
            self.selected = Some(0);
        }
        self.top = 0;
        self.auto_follow = false;
    }
    fn scroll(&mut self, amount: isize) {
        self.auto_follow = false;
        self.top = (self.top as isize + amount)
            .clamp(0, self.cards.len().saturating_sub(1) as isize) as usize;
    }
    fn toggle(&mut self) -> Result<()> {
        let Some(index) = self.selected else {
            return Ok(());
        };
        let expanding = !self.cards[index].expanded;
        for (card_index, card) in self.cards.iter_mut().enumerate() {
            if card_index != index || !expanding {
                card.expanded = false;
                card.detail = None;
            }
        }
        let card = &mut self.cards[index];
        card.expanded = expanding;
        if expanding {
            card.detail = Some(load_detail(&card.activity)?);
        }
        Ok(())
    }
    fn collapse(&mut self) {
        if let Some(i) = self.selected {
            self.cards[i].expanded = false;
            self.cards[i].detail = None;
        }
    }
}

fn load_detail(activity: &Activity) -> Result<String> {
    let Activity::Child {
        artifact,
        original_bytes,
        run_id,
        ..
    } = activity
    else {
        return Ok(match activity {
            Activity::Controller { details, .. } => serde_json::to_string_pretty(details)?,
            Activity::Notice { text, .. } => text.clone(),
            _ => unreachable!(),
        });
    };
    let requested = usize::try_from(artifact.length).unwrap_or(usize::MAX);
    let read_len = requested.min(DETAIL_BYTES);
    let mut file =
        File::open(&artifact.path).with_context(|| format!("open {}", artifact.path.display()))?;
    file.seek(SeekFrom::Start(artifact.offset))?;
    let mut bytes = vec![0; read_len];
    file.read_exact(&mut bytes)?;
    let mut text = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if requested <= DETAIL_BYTES => serde_json::to_string_pretty(&value)?,
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    };
    let mut lines = text.lines();
    let limited = lines
        .by_ref()
        .take(DETAIL_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let capped = requested > DETAIL_BYTES || lines.next().is_some();
    text = limited;
    if capped {
        text.push_str(&format!(
            "\n… display capped; exact {} bytes at {}",
            original_bytes,
            artifact.path.display()
        ));
    }
    Ok(format!(
        "run: {run_id}\nlog: {}:{}+{}\n\n{text}",
        artifact.path.display(),
        artifact.offset,
        artifact.length
    ))
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

/// Run the controller on a worker while the calling thread owns terminal I/O.
pub fn run(
    controller: crate::controller::Controller,
    project: String,
    cancelled: Arc<AtomicBool>,
    receiver: Receiver<Activity>,
) -> Result<()> {
    let worker = thread::spawn(move || controller.run());
    let guard = TerminalGuard::enter();
    if let Err(error) = guard {
        cancelled.store(true, Ordering::SeqCst);
        drop(receiver);
        let _ = worker.join();
        return Err(error);
    }
    let guard = guard.unwrap();
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            cancelled.store(true, Ordering::SeqCst);
            drop(receiver);
            let _ = worker.join();
            drop(guard);
            return Err(error.into());
        }
    };
    if let Err(error) = terminal.clear() {
        cancelled.store(true, Ordering::SeqCst);
        drop(receiver);
        let _ = worker.join();
        drop(terminal);
        drop(guard);
        return Err(error.into());
    }
    let mut state = UiState::new();
    state.project = project;
    let mut ui_error = None;

    'ui: loop {
        loop {
            match receiver.try_recv() {
                Ok(activity) => state.push(activity),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Err(error) = terminal.draw(|frame| render(frame, &mut state)) {
            ui_error = Some(error.into());
            break;
        }
        if worker.is_finished() {
            break;
        }
        match event::poll(Duration::from_millis(33)) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if let Err(error) = handle_event(event, &mut state, &cancelled) {
                        ui_error = Some(error);
                        break 'ui;
                    }
                }
                Err(error) => {
                    ui_error = Some(error.into());
                    break;
                }
            },
            Ok(false) => {}
            Err(error) => {
                ui_error = Some(error.into());
                break;
            }
        }
    }
    if let Some(error) = ui_error {
        cancelled.store(true, Ordering::SeqCst);
        drop(receiver);
        let joined = worker.join();
        drop(terminal);
        drop(guard);
        if joined.is_err() {
            return Err(anyhow!(
                "controller thread panicked while stopping after UI failure"
            ));
        }
        return Err(error);
    }
    let result = worker
        .join()
        .map_err(|_| anyhow!("controller thread panicked"))?;
    while let Ok(activity) = receiver.try_recv() {
        state.push(activity);
    }
    let final_notice = state
        .cards
        .iter()
        .rev()
        .find_map(|card| match &card.activity {
            Activity::Notice { text, .. } => Some(text.clone()),
            _ => None,
        });
    drop(terminal);
    drop(guard);
    if result.is_ok()
        && let Some(text) = final_notice
    {
        println!("{text}");
    }
    result
}

fn handle_event(event: Event, state: &mut UiState, cancelled: &AtomicBool) -> Result<()> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL) => cancelled.store(true, Ordering::SeqCst),
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            ..
        }) => cancelled.store(true, Ordering::SeqCst),
        Event::Key(KeyEvent {
            code: KeyCode::Up | KeyCode::Char('k'),
            ..
        }) => state.move_selection(-1),
        Event::Key(KeyEvent {
            code: KeyCode::Down | KeyCode::Char('j'),
            ..
        }) => state.move_selection(1),
        Event::Key(KeyEvent {
            code: KeyCode::Enter | KeyCode::Char(' '),
            ..
        }) => state.toggle()?,
        Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) => state.collapse(),
        Event::Key(KeyEvent {
            code: KeyCode::Home,
            ..
        }) => state.home(),
        Event::Key(KeyEvent {
            code: KeyCode::End | KeyCode::Char('a'),
            ..
        }) => state.end(),
        Event::Key(KeyEvent {
            code: KeyCode::PageUp,
            ..
        }) => state.scroll(-10),
        Event::Key(KeyEvent {
            code: KeyCode::PageDown,
            ..
        }) => state.scroll(10),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => state.scroll(-3),
            MouseEventKind::ScrollDown => state.scroll(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) =
                    state.hit_regions.iter().find(|(row, _)| *row == mouse.row)
                {
                    state.selected = Some(*index);
                    state.toggle()?;
                }
            }
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn render(frame: &mut Frame<'_>, state: &mut UiState) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(frame.area());
    let status = if state.auto_follow {
        "following".into()
    } else {
        format!("paused · {} new", state.unseen)
    };
    let elapsed = state
        .phase_started_at
        .map(|started| unix_now().saturating_sub(started))
        .map(|seconds| format!(" · {seconds}s"))
        .unwrap_or_default();
    let project = truncate(&state.project, (areas[0].width as usize / 3).max(12));
    let cycle = truncate(&state.cycle, 28);
    frame.render_widget(
        Paragraph::new(format!(
            " goal  {project} · cycle {cycle} · {}{elapsed}\n {status}",
            state.phase
        ))
        .block(Block::default().borders(Borders::BOTTOM)),
        areas[0],
    );

    let body = areas[1];
    state.hit_regions.clear();
    let start = if state.auto_follow {
        visible_start_from_end(&state.cards, body.width, body.height)
    } else {
        state.top.min(state.cards.len())
    };
    let mut lines = Vec::new();
    let mut row = body.y;
    for (index, card) in state.cards.iter().enumerate().skip(start) {
        if lines.len() >= body.height as usize {
            break;
        }
        state.hit_regions.push((row, index));
        let selected = state.selected == Some(index);
        let marker = if card.expanded { '▼' } else { '▶' };
        let (timestamp, label, summary, color) = card_parts(&card.activity);
        let style = Style::default().fg(color).add_modifier(if selected {
            Modifier::REVERSED
        } else {
            Modifier::empty()
        });
        let header = format!("{marker} {} {label:<10} {summary}", clock(timestamp));
        lines.push(Line::from(vec![Span::styled(
            truncate(&header, body.width as usize),
            style,
        )]));
        row += 1;
        if card.expanded {
            if let Some(detail) = &card.detail {
                for detail_line in detail.lines() {
                    for segment in wrap_line(detail_line, body.width.saturating_sub(4) as usize) {
                        if lines.len() >= body.height as usize {
                            break;
                        }
                        lines.push(Line::from(format!("    {segment}")));
                        row += 1;
                    }
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
    let evicted = if state.evicted == 0 {
        String::new()
    } else {
        format!(" · {} evicted", state.evicted)
    };
    frame.render_widget(
        Paragraph::new(format!(
            " ↑↓/jk select  Enter/Click expand  PgUp/PgDn scroll  End follow  q quit{evicted}"
        ))
        .block(Block::default().borders(Borders::TOP)),
        areas[2],
    );
}

fn visible_start_from_end(cards: &VecDeque<Card>, width: u16, height: u16) -> usize {
    let mut used = 0_usize;
    for (index, card) in cards.iter().enumerate().rev() {
        let detail_rows = card
            .detail
            .as_deref()
            .filter(|_| card.expanded)
            .map(|detail| {
                detail
                    .lines()
                    .map(|line| wrap_line(line, width.saturating_sub(4) as usize).len())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        let rows = 1 + detail_rows;
        if used + rows > height as usize && used > 0 {
            return index + 1;
        }
        used += rows;
    }
    0
}

fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn card_parts(activity: &Activity) -> (u64, String, String, Color) {
    match activity {
        Activity::Controller {
            timestamp,
            kind,
            details,
        } => (
            *timestamp,
            "GOAL".into(),
            format!("{kind} {}", summarize_line(details.to_string().as_bytes())),
            Color::Cyan,
        ),
        Activity::Child {
            timestamp,
            role,
            stream,
            summary,
            original_bytes,
            ..
        } => (
            *timestamp,
            role.to_uppercase(),
            format!("[{stream}] {summary} · {original_bytes} B"),
            if stream == "stderr" {
                Color::Yellow
            } else {
                Color::White
            },
        ),
        Activity::Notice {
            timestamp,
            level,
            text,
        } => (
            *timestamp,
            "NOTICE".into(),
            text.clone(),
            if *level == NoticeLevel::Error {
                Color::Red
            } else {
                Color::Green
            },
        ),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn clock(timestamp: u64) -> String {
    Local
        .timestamp_opt(timestamp as i64, 0)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseEvent;
    use ratatui::backend::TestBackend;
    use std::io::Write;

    fn notice(text: &str) -> Activity {
        Activity::Notice {
            timestamp: 1,
            level: NoticeLevel::Info,
            text: text.into(),
        }
    }

    #[test]
    fn summaries_are_generic_and_bounded() {
        let result = summarize_line(br#"{"kind":"anything","nested":{"a":1},"items":[1,2]}"#);
        assert!(result.contains("nested: {… 1 fields}"));
        assert!(result.chars().count() <= SUMMARY_CHARS + 1);
        assert!(summarize_line(&[0xff, b'\n', b'x']).chars().count() <= SUMMARY_CHARS + 1);
    }

    #[test]
    fn reducer_pauses_and_resumes_follow() {
        let mut state = UiState::new();
        state.push(notice("one"));
        state.push(notice("two"));
        state.move_selection(-1);
        state.push(notice("three"));
        assert!(!state.auto_follow);
        assert_eq!(state.unseen, 1);
        state.end();
        assert!(state.auto_follow);
        assert_eq!(state.selected, Some(2));
    }

    #[test]
    fn toggle_only_changes_selected_card() {
        let mut state = UiState::new();
        state.push(notice("one"));
        state.push(notice("two"));
        state.selected = Some(0);
        state.toggle().unwrap();
        assert!(state.cards[0].expanded);
        assert!(!state.cards[1].expanded);
    }

    #[test]
    fn expanding_another_card_releases_the_previous_detail() {
        let mut state = UiState::new();
        state.push(notice("one"));
        state.push(notice("two"));
        state.selected = Some(0);
        state.toggle().unwrap();
        assert!(state.cards[0].detail.is_some());
        state.selected = Some(1);
        state.toggle().unwrap();
        assert!(!state.cards[0].expanded);
        assert!(state.cards[0].detail.is_none());
        assert!(state.cards[1].expanded);
    }

    #[test]
    fn retention_keeps_selection_valid() {
        let mut state = UiState::new();
        for index in 0..=MAX_CARDS {
            state.push(notice(&index.to_string()));
        }
        assert_eq!(state.cards.len(), MAX_CARDS);
        assert_eq!(state.evicted, 1);
        assert_eq!(state.selected, Some(MAX_CARDS - 1));
    }

    #[test]
    fn keyboard_and_mouse_toggle_the_selected_card() {
        let cancelled = AtomicBool::new(false);
        let mut state = UiState::new();
        state.push(notice("one"));
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert!(state.cards[0].expanded);

        state.hit_regions = vec![(4, 0)];
        handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert!(!state.cards[0].expanded);
    }

    #[test]
    fn artifact_loader_reads_only_range() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "prefix{{\"a\":1}}suffix").unwrap();
        let activity = Activity::Child {
            timestamp: 0,
            role: "x".into(),
            stream: "stdout".into(),
            run_id: "r".into(),
            artifact: ArtifactRange {
                path: file.path().into(),
                offset: 6,
                length: 7,
            },
            summary: String::new(),
            original_bytes: 7,
        };
        let detail = load_detail(&activity).unwrap();
        assert!(detail.contains("run: r"));
        assert!(detail.ends_with("{\n  \"a\": 1\n}"));
    }

    #[test]
    fn test_backend_renders_header_card_and_footer() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        state.push(notice("finished"));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("goal  - · cycle - · STARTING"));
        assert!(screen.contains("finished"));
        assert!(screen.contains("Enter/Click"));
        assert_eq!(state.hit_regions.len(), 1);
    }
}

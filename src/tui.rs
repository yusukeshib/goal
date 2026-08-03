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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
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
}

#[derive(Debug)]
struct ExpandedCard {
    index: usize,
    detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollbarGeometry {
    area: Rect,
    content_height: usize,
    viewport_height: usize,
    max_top: usize,
    thumb_start: usize,
    thumb_length: usize,
}

impl ScrollbarGeometry {
    fn new(area: Rect, content_height: usize, viewport_height: usize, top: usize) -> Option<Self> {
        if area.width == 0
            || area.height == 0
            || viewport_height == 0
            || content_height <= viewport_height
        {
            return None;
        }

        let max_top = content_height - viewport_height;
        let track_length = f64::from(area.height);
        let top = top.min(max_top);
        let thumb_start = ((top as f64 * track_length / content_height as f64).round() as usize)
            .min(area.height as usize - 1);
        let thumb_end = (((top + viewport_height) as f64 * track_length / content_height as f64)
            .round() as usize)
            .min(area.height as usize);
        let thumb_length = thumb_end
            .saturating_sub(thumb_start)
            .max(1)
            .min(area.height as usize - thumb_start);

        Some(Self {
            area,
            content_height,
            viewport_height,
            max_top,
            thumb_start,
            thumb_length,
        })
    }

    fn contains(self, column: u16, row: u16) -> bool {
        column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    fn relative_row(self, row: u16) -> usize {
        usize::from(row.clamp(self.area.y, self.area.bottom() - 1) - self.area.y)
    }

    fn thumb_contains(self, row: u16) -> bool {
        let row = self.relative_row(row);
        row >= self.thumb_start && row < self.thumb_start + self.thumb_length
    }

    fn top_for_thumb_start(self, thumb_start: usize) -> usize {
        let last_thumb_start = self.area.height as usize - self.thumb_length;
        let thumb_start = thumb_start.min(last_thumb_start);
        if thumb_start == last_thumb_start {
            return self.max_top;
        }
        ((thumb_start as f64 * self.content_height as f64 / f64::from(self.area.height)).round()
            as usize)
            .min(self.max_top)
    }

    fn top_for_click(self, row: u16) -> usize {
        let centered_thumb_start = self.relative_row(row).saturating_sub(self.thumb_length / 2);
        self.top_for_thumb_start(centered_thumb_start)
    }

    fn top_for_drag(self, row: u16, grab_offset: usize) -> usize {
        let thumb_start = self.relative_row(row).saturating_sub(grab_offset);
        self.top_for_thumb_start(thumb_start)
    }
}

#[derive(Debug, Default)]
struct UiState {
    cards: VecDeque<Card>,
    selected: Option<usize>,
    expanded: Option<ExpandedCard>,
    top: usize,
    content_width: u16,
    viewport_height: u16,
    auto_follow: bool,
    unseen: usize,
    evicted: u64,
    phase: String,
    phase_started_at: Option<u64>,
    cycle: String,
    project: String,
    hit_regions: Vec<(u16, usize)>,
    scrollbar: Option<ScrollbarGeometry>,
    scrollbar_drag_offset: Option<usize>,
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
        let stick_to_end = self.auto_follow || self.scrollbar_is_at_bottom();
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
        self.cards.push_back(Card { activity });
        if stick_to_end || was_empty {
            self.selected = Some(self.cards.len() - 1);
            self.auto_follow = true;
            self.unseen = 0;
        } else {
            self.unseen += 1;
        }
        while self.cards.len() > MAX_CARDS {
            let removed_height = card_height(0, self.expanded.as_ref(), self.content_width.max(1));
            self.cards.pop_front();
            self.evicted += 1;
            self.selected = self.selected.map(|i| i.saturating_sub(1));
            self.expanded = self.expanded.take().and_then(|mut expanded| {
                if expanded.index == 0 {
                    None
                } else {
                    expanded.index -= 1;
                    Some(expanded)
                }
            });
            self.top = self.top.saturating_sub(removed_height);
        }
    }

    fn visual_start(&self, index: usize) -> usize {
        (0..index.min(self.cards.len()))
            .map(|index| card_height(index, self.expanded.as_ref(), self.content_width.max(1)))
            .sum()
    }

    fn visual_height(&self) -> usize {
        (0..self.cards.len())
            .map(|index| card_height(index, self.expanded.as_ref(), self.content_width.max(1)))
            .sum()
    }

    fn max_top(&self) -> usize {
        self.visual_height()
            .saturating_sub(self.viewport_height as usize)
    }

    fn scrollbar_is_at_bottom(&self) -> bool {
        self.scrollbar.is_some_and(|_| self.top == self.max_top())
    }

    fn set_scroll_top(&mut self, top: usize) {
        self.top = top.min(self.max_top());
        self.auto_follow = self.top == self.max_top();
        if self.auto_follow {
            self.selected = self.cards.len().checked_sub(1);
            self.unseen = 0;
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
            self.top = self
                .visual_start(self.selected.unwrap_or(0))
                .saturating_sub(5);
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
        let top = (self.top as isize + amount).clamp(0, self.max_top() as isize) as usize;
        self.set_scroll_top(top);
    }

    fn toggle(&mut self) -> Result<()> {
        let Some(index) = self.selected else {
            return Ok(());
        };
        if self
            .expanded
            .as_ref()
            .is_some_and(|card| card.index == index)
        {
            self.top = self.visual_start(index);
            self.expanded = None;
        } else {
            self.expanded = Some(ExpandedCard {
                index,
                detail: load_detail(&self.cards[index].activity)?,
            });
            self.top = self.visual_start(index);
        }
        self.auto_follow = false;
        Ok(())
    }

    fn collapse(&mut self) {
        if let Some(index) = self.expanded.as_ref().map(|expanded| expanded.index) {
            self.top = self.visual_start(index);
        }
        self.expanded = None;
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
            MouseEventKind::ScrollUp => state.scroll(-1),
            MouseEventKind::ScrollDown => state.scroll(1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(scrollbar) = state
                    .scrollbar
                    .filter(|scrollbar| scrollbar.contains(mouse.column, mouse.row))
                {
                    if scrollbar.thumb_contains(mouse.row) {
                        state.scrollbar_drag_offset = Some(
                            scrollbar
                                .relative_row(mouse.row)
                                .saturating_sub(scrollbar.thumb_start),
                        );
                    } else {
                        state.set_scroll_top(scrollbar.top_for_click(mouse.row));
                        if let Some(updated) = ScrollbarGeometry::new(
                            scrollbar.area,
                            scrollbar.content_height,
                            scrollbar.viewport_height,
                            state.top,
                        ) {
                            state.scrollbar_drag_offset = Some(
                                updated
                                    .relative_row(mouse.row)
                                    .saturating_sub(updated.thumb_start)
                                    .min(updated.thumb_length - 1),
                            );
                            state.scrollbar = Some(updated);
                        }
                    }
                } else {
                    state.scrollbar_drag_offset = None;
                    if let Some((_, index)) =
                        state.hit_regions.iter().find(|(row, _)| *row == mouse.row)
                    {
                        state.selected = Some(*index);
                        state.toggle()?;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (Some(scrollbar), Some(grab_offset)) =
                    (state.scrollbar, state.scrollbar_drag_offset)
                {
                    state.set_scroll_top(scrollbar.top_for_drag(mouse.row, grab_offset));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => state.scrollbar_drag_offset = None,
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
    let body_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(body);
    let content = body_columns[0];
    let scrollbar_area = body_columns[1];
    state.hit_regions.clear();
    state.content_width = content.width;
    state.viewport_height = content.height;
    let content_height = state.visual_height();
    state.top = if state.auto_follow {
        content_height.saturating_sub(content.height as usize)
    } else {
        state
            .top
            .min(content_height.saturating_sub(content.height as usize))
    };
    let start = state.top;
    let mut lines = Vec::new();
    let mut visual_row = 0_usize;
    'cards: for (index, card) in state.cards.iter().enumerate() {
        let height = card_height(index, state.expanded.as_ref(), content.width);
        if visual_row + height <= start {
            visual_row += height;
            continue;
        }

        if visual_row >= start {
            let row = content.y + lines.len() as u16;
            state.hit_regions.push((row, index));
            let selected = state.selected == Some(index);
            let (timestamp, label, summary, color) = card_parts(&card.activity);
            let style = Style::default().fg(color).add_modifier(if selected {
                Modifier::REVERSED
            } else {
                Modifier::empty()
            });
            let header = format!("{} {label:<10} {summary}", clock(timestamp));
            lines.push(Line::from(vec![Span::styled(
                truncate(&header, content.width as usize),
                style,
            )]));
            if lines.len() >= content.height as usize {
                break;
            }
        }
        visual_row += 1;

        if let Some(expanded) = state
            .expanded
            .as_ref()
            .filter(|expanded| expanded.index == index)
        {
            for detail_line in expanded.detail.lines() {
                for segment in wrap_line(detail_line, content.width.saturating_sub(4) as usize) {
                    if visual_row >= start {
                        lines.push(Line::from(format!("    {segment}")));
                        if lines.len() >= content.height as usize {
                            break 'cards;
                        }
                    }
                    visual_row += 1;
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), content);

    state.scrollbar = ScrollbarGeometry::new(
        scrollbar_area,
        content_height,
        content.height as usize,
        start,
    );
    if state.scrollbar.is_some() {
        let mut scrollbar_state = ScrollbarState::new(state.max_top() + 1)
            .position(start)
            .viewport_content_length(content.height as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│")),
            scrollbar_area,
            &mut scrollbar_state,
        );
    } else {
        state.scrollbar_drag_offset = None;
    }
    let evicted = if state.evicted == 0 {
        String::new()
    } else {
        format!(" · {} evicted", state.evicted)
    };
    frame.render_widget(
        Paragraph::new(format!(
            " ↑↓/jk select  Enter expand  Wheel/Pg scroll  Click/Drag bar  End follow  q quit{evicted}"
        ))
        .block(Block::default().borders(Borders::TOP)),
        areas[2],
    );
}

fn card_height(index: usize, expanded: Option<&ExpandedCard>, width: u16) -> usize {
    1 + expanded
        .filter(|expanded| expanded.index == index)
        .map(|expanded| {
            expanded
                .detail
                .lines()
                .map(|line| wrap_line(line, width.saturating_sub(4) as usize).len())
                .sum::<usize>()
        })
        .unwrap_or(0)
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
            Color::White,
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
            format!(
                "{}{summary} · {original_bytes} B",
                if stream == "stderr" { "[stderr] " } else { "" }
            ),
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

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn summaries_are_generic_and_bounded() {
        let result = summarize_line(br#"{"kind":"anything","nested":{"a":1},"items":[1,2]}"#);
        assert!(result.contains("nested: {… 1 fields}"));
        assert!(result.chars().count() <= SUMMARY_CHARS + 1);
        assert!(summarize_line(&[0xff, b'\n', b'x']).chars().count() <= SUMMARY_CHARS + 1);
    }

    #[test]
    fn child_cards_only_label_stderr() {
        let child = |stream: &str| Activity::Child {
            timestamp: 1,
            role: "worker".into(),
            stream: stream.into(),
            run_id: "run".into(),
            artifact: ArtifactRange {
                path: PathBuf::new(),
                offset: 0,
                length: 0,
            },
            summary: "diagnostic".into(),
            original_bytes: 10,
        };
        let (_, _, stdout_summary, _) = card_parts(&child("stdout"));
        let (_, _, stderr_summary, _) = card_parts(&child("stderr"));
        assert_eq!(stdout_summary, "diagnostic · 10 B");
        assert_eq!(stderr_summary, "[stderr] diagnostic · 10 B");
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
    fn toggle_tracks_one_expanded_card_globally() {
        let mut state = UiState::new();
        state.push(notice("one"));
        state.push(notice("two"));
        state.selected = Some(0);
        state.toggle().unwrap();
        assert_eq!(state.expanded.as_ref().map(|card| card.index), Some(0));

        state.selected = Some(1);
        state.toggle().unwrap();
        assert_eq!(state.expanded.as_ref().map(|card| card.index), Some(1));

        state.move_selection(-1);
        state.collapse();
        assert!(state.expanded.is_none());
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
        assert_eq!(state.expanded.as_ref().map(|card| card.index), Some(0));

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
        assert!(state.expanded.is_none());
    }

    #[test]
    fn expanded_card_scrolls_by_wrapped_display_line() {
        let backend = TestBackend::new(30, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        state.push(notice("details"));
        state.toggle().unwrap();
        state.expanded.as_mut().unwrap().detail =
            "abcdefghijklmnopqrstuvwxyz\none\ntwo\nthree".into();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();

        let body_line = |terminal: &Terminal<TestBackend>, row| {
            (0..29)
                .map(|column| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((column, row))
                        .unwrap()
                        .symbol()
                })
                .collect::<String>()
        };
        assert!(body_line(&terminal, 3).contains("details"));

        let cancelled = AtomicBool::new(false);
        handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }),
            &mut state,
            &cancelled,
        )
        .unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert_eq!(state.top, 1);
        assert!(body_line(&terminal, 3).contains("abcdefghijklmnopqrstuvwxy"));

        state.scroll(1);
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert_eq!(state.top, 2);
        assert_eq!(body_line(&terminal, 3).trim(), "z");
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
    fn scrollbar_geometry_maps_track_ends_to_content_ends() {
        let geometry = ScrollbarGeometry::new(Rect::new(29, 3, 1, 6), 30, 6, 0).unwrap();
        assert_eq!(geometry.area.width, 1);
        assert_eq!(geometry.thumb_start, 0);
        assert_eq!(geometry.top_for_click(3), 0);
        assert_eq!(geometry.top_for_click(8), geometry.max_top);
        assert_eq!(geometry.top_for_drag(8, 0), geometry.max_top);

        let bottom = ScrollbarGeometry::new(Rect::new(29, 3, 1, 6), 30, 6, 24).unwrap();
        assert_eq!(bottom.thumb_start + bottom.thumb_length, 6);
    }

    #[test]
    fn scrollbar_tracks_visual_position_and_aligns_content_end() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        for index in 0..30 {
            state.push(notice(&index.to_string()));
        }
        state.home();

        let thumb_row = |terminal: &Terminal<TestBackend>| {
            let buffer = terminal.backend().buffer();
            (3..9)
                .find(|row| {
                    buffer
                        .cell((29, *row))
                        .is_some_and(|cell| cell.symbol() == "█")
                })
                .expect("missing scrollbar thumb")
        };
        let row_text = |terminal: &Terminal<TestBackend>, row| {
            (0..29)
                .map(|column| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((column, row))
                        .unwrap()
                        .symbol()
                })
                .collect::<String>()
        };

        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let top = thumb_row(&terminal);
        assert!((3..9).any(|row| {
            terminal
                .backend()
                .buffer()
                .cell((29, row))
                .is_some_and(|cell| cell.symbol() == "│")
        }));
        state.scroll(12);
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let middle = thumb_row(&terminal);
        state.scroll(100);
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let bottom = thumb_row(&terminal);
        assert!(top < middle, "top={top}, middle={middle}");
        assert!(middle < bottom, "middle={middle}, bottom={bottom}");
        assert_eq!(bottom, 8);
        assert!(row_text(&terminal, 8).contains("29"));
        assert!(state.auto_follow);
    }

    #[test]
    fn scrollbar_track_clicks_and_thumb_drags_resume_following_at_end() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        for index in 0..30 {
            state.push(notice(&index.to_string()));
        }
        state.home();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let cancelled = AtomicBool::new(false);

        handle_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 29, 8),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert_eq!(state.top, state.max_top());
        assert!(state.auto_follow);

        state.push(notice("new last row"));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert_eq!(state.top, state.max_top());
        let last_row = (0..29)
            .map(|column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, 8))
                    .unwrap()
                    .symbol()
            })
            .collect::<String>();
        assert!(last_row.contains("new last"));

        handle_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 29, 3),
            &mut state,
            &cancelled,
        )
        .unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert_eq!(state.top, 0);
        assert!(!state.auto_follow);
        state.push(notice("unseen while paused"));
        assert_eq!(state.top, 0);
        assert!(!state.auto_follow);
        assert_eq!(state.unseen, 1);
        terminal.draw(|frame| render(frame, &mut state)).unwrap();

        handle_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 29, 3),
            &mut state,
            &cancelled,
        )
        .unwrap();
        handle_event(
            mouse(MouseEventKind::Drag(MouseButton::Left), 29, 8),
            &mut state,
            &cancelled,
        )
        .unwrap();
        handle_event(
            mouse(MouseEventKind::Up(MouseButton::Left), 29, 8),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert_eq!(state.top, state.max_top());
        assert!(state.auto_follow);
        assert!(state.scrollbar_drag_offset.is_none());

        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        handle_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 29, 8),
            &mut state,
            &cancelled,
        )
        .unwrap();
        handle_event(
            mouse(MouseEventKind::Drag(MouseButton::Left), 29, 3),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert_eq!(state.top, 0);
        assert!(!state.auto_follow);
    }

    #[test]
    fn scrollbar_drag_clamps_outside_track_after_resize() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        for index in 0..30 {
            state.push(notice(&index.to_string()));
        }
        state.home();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let cancelled = AtomicBool::new(false);

        handle_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 29, 3),
            &mut state,
            &cancelled,
        )
        .unwrap();
        let backend = TestBackend::new(20, 10);
        terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert_eq!(state.scrollbar.unwrap().area, Rect::new(19, 3, 1, 4));

        handle_event(
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 100),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert_eq!(state.top, state.max_top());
        assert!(state.auto_follow);
        handle_event(
            mouse(MouseEventKind::Up(MouseButton::Left), 0, 100),
            &mut state,
            &cancelled,
        )
        .unwrap();
        assert!(state.scrollbar_drag_offset.is_none());
    }

    #[test]
    fn scrollbar_handles_no_overflow_and_narrow_terminals() {
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        state.push(notice("one"));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        for row in 3..7 {
            let symbol = terminal
                .backend()
                .buffer()
                .cell((19, row))
                .unwrap()
                .symbol();
            assert!(!matches!(symbol, "▲" | "█" | "│" | "║" | "▼"));
        }

        let backend = TestBackend::new(1, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        state.push(notice("one"));
        state.push(notice("two"));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
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
        assert!(screen.contains("Click/Drag bar"));
        assert_eq!(state.hit_regions.len(), 1);
    }
}

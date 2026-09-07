//! Shared, scrollback-friendly table rendering for list and watch.
use std::io::{self, Write};

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::Span,
    widgets::{Row, Table, Widget},
};
use serde_json::Value;

const HEADER_COLOR: Color = Color::Rgb(150, 150, 150);

pub fn height(rows: &[Value]) -> u16 {
    rows.len().saturating_add(1).min(u16::MAX as usize) as u16
}

fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("-").chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

fn render(rows: &[Value], width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    if area.is_empty() {
        return buffer;
    }
    let wide = width >= 100;
    let compact = width < 60;
    let (headers, widths) = if compact {
        (vec!["ID", "STATUS"], vec![Constraint::Min(1), Constraint::Length(9)])
    } else if wide {
        (vec!["ID", "ENABLED", "STATUS", "PID", "GOAL FILE"],
         vec![Constraint::Fill(2), Constraint::Length(7), Constraint::Length(9), Constraint::Length(7), Constraint::Fill(3)])
    } else {
        (vec!["ID", "ENABLED", "STATUS", "PID"],
         vec![Constraint::Min(1), Constraint::Length(7), Constraint::Length(9), Constraint::Length(7)])
    };
    let visible = rows.len().min(height.saturating_sub(1) as usize);
    let body = rows.iter().take(visible).map(|row| {
        let id = text(row, "id");
        let status = text(row, "status");
        let enabled = if row["enabled"].as_bool() == Some(true) { "yes" } else { "no" }.to_owned();
        let pid = row["pid"].as_u64().map(|pid| pid.to_string()).unwrap_or_else(|| "-".to_owned());
        let cells = if compact {
            vec![id, status]
        } else if wide {
            vec![id, enabled, status, pid, text(row, "config_path")]
        } else {
            vec![id, enabled, status, pid]
        };
        Row::new(cells)
    });
    Table::new(body, widths)
        .header(Row::new(headers).style(Style::default().fg(HEADER_COLOR)))
        .column_spacing(2)
        .render(area, &mut buffer);
    buffer
}

/// Render without entering an alternate screen or reading terminal input.
/// Both list and watch leave their output in normal scrollback.
pub fn write(out: &mut impl Write, rows: &[Value], width: u16, height: u16) -> io::Result<()> {
    let buffer = render(rows, width, height);
    for y in 0..height {
        let mut x = 0;
        let mut gray = false;
        while x < width {
            let cell = &buffer[(x, y)];
            let next_gray = cell.fg == HEADER_COLOR;
            if next_gray != gray {
                out.write_all(if next_gray { b"\x1b[38;2;150;150;150m" } else { b"\x1b[39m" })?;
                gray = next_gray;
            }
            out.write_all(cell.symbol().as_bytes())?;
            // Continuation cells of a wide glyph must not be printed a second time.
            x = x.saturating_add(Span::raw(cell.symbol()).width().max(1) as u16);
        }
        if gray {
            out.write_all(b"\x1b[39m")?;
        }
        out.write_all(b"\n")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows() -> Vec<Value> {
        vec![json!({"id":"triage-client-errors","enabled":true,"status":"running","pid":42,"config_path":"/tmp/日本語/goal.toml"})]
    }

    fn line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect()
    }

    #[test]
    fn wide_table_has_gray_header_and_aligned_data() {
        let buffer = render(&rows(), 120, 2);
        for label in ["ID", "ENABLED", "STATUS", "PID", "GOAL FILE"] {
            assert!(line(&buffer, 0).contains(label));
        }
        let header_cell = (0..120).find(|x| buffer[(*x, 0)].symbol() == "I").unwrap();
        assert_eq!(buffer[(header_cell, 0)].fg, HEADER_COLOR);
        assert!(line(&buffer, 1).contains("triage-client-errors"));
        assert!(line(&buffer, 1).contains("running"));
    }

    #[test]
    fn narrower_tables_prioritize_identity_and_status() {
        let medium = render(&rows(), 80, 4);
        assert!(line(&medium, 0).contains("PID"));
        assert!(!line(&medium, 0).contains("GOAL FILE"));
        let narrow = render(&rows(), 45, 4);
        assert!(line(&narrow, 0).contains("STATUS"));
        assert!(!line(&narrow, 0).contains("ENABLED"));
        for width in [0, 1, 5] {
            let _ = render(&rows(), width, 2);
        }
    }

    #[test]
    fn table_starts_with_header_without_title_or_extra_rows() {
        assert_eq!(height(&[]), 1);
        assert_eq!(height(&rows()), 2);
        let empty = render(&[], 80, 1);
        assert!(line(&empty, 0).starts_with("ID"));
        assert!(!line(&empty, 0).contains("Goals"));
    }

    #[test]
    fn writer_resets_gray_and_preserves_wide_text() {
        let mut output = Vec::new();
        write(&mut output, &rows(), 120, 4).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[38;2;150;150;150m"));
        assert!(output.contains("\x1b[39m"));
        assert!(output.contains("/tmp/日本語/goal.toml"));
    }
}

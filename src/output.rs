use std::{
    io::{self, Write},
    sync::{Arc, Mutex, mpsc::SyncSender},
};

use anyhow::{Context, Result};
use chrono::Local;
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;

use crate::{
    state::unix_timestamp,
    tui::{Activity, ArtifactRange, NoticeLevel, summarize_line},
};

const MAX_STREAM_PAYLOAD_BYTES: usize = 16 * 1024;
const CONTENT_PREVIEW_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    Tui,
    Plain,
    Pretty,
    Json,
}

#[derive(Clone)]
pub struct Output {
    mode: OutputMode,
    backend: Arc<OutputBackend>,
}

enum OutputBackend {
    Stream { write_lock: Mutex<()> },
    Tui { sender: SyncSender<Activity> },
}

#[derive(Serialize)]
struct Envelope<'a> {
    timestamp: u64,
    #[serde(rename = "type")]
    kind: &'a str,
    details: Value,
}

impl Output {
    pub fn new(mode: OutputMode) -> Self {
        debug_assert_ne!(mode, OutputMode::Tui, "use Output::tui for TUI output");
        Self {
            mode,
            backend: Arc::new(OutputBackend::Stream {
                write_lock: Mutex::new(()),
            }),
        }
    }

    pub fn tui(sender: SyncSender<Activity>) -> Self {
        Self {
            mode: OutputMode::Tui,
            backend: Arc::new(OutputBackend::Tui { sender }),
        }
    }

    pub fn event(&self, kind: &str, details: Value) -> Result<()> {
        match self.mode {
            OutputMode::Json => self.write_envelope(kind, details),
            OutputMode::Tui => {
                self.send_activity(Activity::Controller {
                    timestamp: unix_timestamp(),
                    kind: kind.to_owned(),
                    details,
                });
                Ok(())
            }
            OutputMode::Plain | OutputMode::Pretty => Ok(()),
        }
    }

    pub fn plain_stdout(&self, message: &str) -> Result<()> {
        if self.mode == OutputMode::Tui {
            self.send_notice(NoticeLevel::Info, message);
        } else if self.mode != OutputMode::Json {
            self.write_plain(false, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn plain_stderr(&self, message: &str) -> Result<()> {
        if self.mode == OutputMode::Tui {
            self.send_notice(NoticeLevel::Error, message);
        } else if self.mode != OutputMode::Json {
            self.write_plain(true, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn child_line(
        &self,
        role: &str,
        stream: &str,
        run_id: &str,
        artifact: ArtifactRange,
        line: &[u8],
    ) -> Result<()> {
        if role == "sensor" && stream == "stdout" && self.mode != OutputMode::Json {
            return Ok(());
        }
        if self.mode == OutputMode::Tui {
            self.send_activity(Activity::Child {
                timestamp: unix_timestamp(),
                role: role.to_owned(),
                stream: stream.to_owned(),
                run_id: run_id.to_owned(),
                artifact,
                summary: summarize_line(line),
                original_bytes: line.len(),
            });
            return Ok(());
        }
        if self.mode != OutputMode::Json {
            if self.mode == OutputMode::Pretty && stream == "stdout" {
                let rendered = prettify_json_line(line)?;
                let prefix = format!("[{}] [{role}] ", Local::now().format("%Y-%m-%d %H:%M:%S"));
                let tagged = prefix_block(&rendered, prefix.as_bytes());
                return self.write_bytes(false, &tagged);
            }
            let tagged = prefix_lines(line, format!("[{role}] ").as_bytes());
            return self.write_plain(stream == "stderr", &tagged);
        }

        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        let mut details = serde_json::json!({
            "role": role,
            "stream": stream,
            "run_id": run_id,
        });
        if let Ok(payload) = serde_json::from_slice::<Value>(trimmed) {
            details["payload"] = if trimmed.len() > MAX_STREAM_PAYLOAD_BYTES {
                summarize_large_payload(&payload, trimmed.len())
            } else {
                payload
            };
        } else {
            let preview = &trimmed[..trimmed.len().min(CONTENT_PREVIEW_BYTES)];
            details["content"] = Value::String(String::from_utf8_lossy(preview).into_owned());
            if preview.len() < trimmed.len() {
                details["truncated"] = Value::Bool(true);
                details["original_bytes"] = Value::from(trimmed.len() as u64);
            }
        }
        self.write_envelope("child_output", details)
    }

    fn send_notice(&self, level: NoticeLevel, message: &str) {
        self.send_activity(Activity::Notice {
            timestamp: unix_timestamp(),
            level,
            text: message.trim_end().to_owned(),
        });
    }

    fn send_activity(&self, activity: Activity) {
        if let OutputBackend::Tui { sender } = self.backend.as_ref() {
            // A disconnected UI is handled by the runtime, not converted into
            // a child-process or protocol failure.
            let _ = sender.send(activity);
        }
    }

    fn write_envelope(&self, kind: &str, details: Value) -> Result<()> {
        let envelope = Envelope {
            timestamp: unix_timestamp(),
            kind,
            details,
        };
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        self.write_bytes(false, &bytes)
    }

    fn write_plain(&self, stderr: bool, bytes: &[u8]) -> Result<()> {
        let prefix = format!("[{}] ", Local::now().format("%Y-%m-%d %H:%M:%S"));
        self.write_bytes(stderr, &prefix_lines(bytes, prefix.as_bytes()))
    }

    fn write_bytes(&self, stderr: bool, bytes: &[u8]) -> Result<()> {
        let OutputBackend::Stream { write_lock } = self.backend.as_ref() else {
            return Ok(());
        };
        let _guard = write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("output lock poisoned"))?;
        if stderr {
            let mut writer = io::stderr().lock();
            writer.write_all(bytes).context("write stderr")?;
            writer.flush().context("flush stderr")?;
        } else {
            let mut writer = io::stdout().lock();
            writer.write_all(bytes).context("write stdout")?;
            writer.flush().context("flush stdout")?;
        }
        Ok(())
    }
}

fn prefix_lines(bytes: &[u8], prefix: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len() + prefix.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        output.extend_from_slice(prefix);
        output.extend_from_slice(line);
    }
    output
}

fn prefix_block(bytes: &[u8], prefix: &[u8]) -> Vec<u8> {
    let continuation = vec![b' '; prefix.len()];
    let mut output = Vec::with_capacity(bytes.len() + prefix.len());
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        output.extend_from_slice(if index == 0 { prefix } else { &continuation });
        output.extend_from_slice(line);
    }
    output
}

fn prettify_json_line(line: &[u8]) -> Result<Vec<u8>> {
    let (trimmed, ending): (&[u8], &[u8]) = if let Some(trimmed) = line.strip_suffix(b"\r\n") {
        (trimmed, b"\r\n")
    } else if let Some(trimmed) = line.strip_suffix(b"\n") {
        (trimmed, b"\n")
    } else {
        (line, b"")
    };
    let Ok(payload) = serde_json::from_slice::<Value>(trimmed) else {
        return Ok(line.to_vec());
    };
    let mut rendered = serde_json::to_vec_pretty(&payload)?;
    rendered.extend_from_slice(ending);
    Ok(rendered)
}

fn summarize_large_payload(payload: &Value, original_bytes: usize) -> Value {
    let mut summary = serde_json::Map::from_iter([
        ("truncated".to_owned(), Value::Bool(true)),
        (
            "original_bytes".to_owned(),
            Value::from(original_bytes as u64),
        ),
    ]);
    for key in [
        "type",
        "role",
        "toolName",
        "stopReason",
        "isError",
        "timestamp",
    ] {
        if let Some(value) = payload
            .get(key)
            .filter(|value| !value.is_array() && !value.is_object())
        {
            summary.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(message) = payload.get("message") {
        let mut message_summary = serde_json::Map::new();
        for key in ["type", "role", "stopReason", "timestamp"] {
            if let Some(value) = message
                .get(key)
                .filter(|value| !value.is_array() && !value.is_object())
            {
                message_summary.insert(key.to_owned(), value.clone());
            }
        }
        if !message_summary.is_empty() {
            summary.insert("message".to_owned(), Value::Object(message_summary));
        }
    }
    Value::Object(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_prefixes_every_line() {
        assert_eq!(
            prefix_lines(b"first\nsecond\n", b"[timestamp] "),
            b"[timestamp] first\n[timestamp] second\n"
        );
    }

    #[test]
    fn pretty_json_preserves_all_values_and_line_ending() {
        let original = concat!(
            r#"{"type":"tool_execution_end","nested":{"values":[1,true,null]},"text":"first\nsecond"}"#,
            "\r\n"
        )
        .as_bytes();
        let rendered = prettify_json_line(original).unwrap();
        assert!(rendered.ends_with(b"\r\n"));
        assert!(
            rendered
                .windows(b"  \"nested\"".len())
                .any(|window| window == b"  \"nested\"")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&rendered).unwrap(),
            serde_json::from_slice::<Value>(original).unwrap()
        );
    }

    #[test]
    fn pretty_json_leaves_non_json_unchanged() {
        let original = b"plain diagnostic\n";
        assert_eq!(prettify_json_line(original).unwrap(), original);
    }

    #[test]
    fn pretty_child_output_is_one_prefixed_block() {
        let rendered = prettify_json_line(b"{\"outer\":{\"inner\":1}}\n").unwrap();
        assert_eq!(
            prefix_block(&rendered, b"[timestamp] [worker] "),
            concat!(
                "[timestamp] [worker] {\n",
                "                       \"outer\": {\n",
                "                         \"inner\": 1\n",
                "                       }\n",
                "                     }\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn tui_emits_one_card_per_diagnostic_and_hides_sensor_protocol() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(4);
        let output = Output::tui(sender);
        let artifact = ArtifactRange {
            path: "stdout.log".into(),
            offset: 0,
            length: 13,
        };
        output
            .child_line(
                "decider",
                "stdout",
                "run-1",
                artifact.clone(),
                b"{\"type\":\"x\"}\n",
            )
            .unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Activity::Child { role, run_id, .. } if role == "decider" && run_id == "run-1"
        ));
        output
            .child_line(
                "sensor",
                "stdout",
                "run-2",
                artifact,
                b"{\"secret\":true}\n",
            )
            .unwrap();
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn large_child_payload_is_bounded_but_keeps_event_metadata() {
        let payload = serde_json::json!({
            "type": "message_update",
            "role": "assistant",
            "message": {"type": "tool_call", "content": "x".repeat(100_000)},
        });
        let summary = summarize_large_payload(&payload, 100_100);
        assert_eq!(summary["truncated"], true);
        assert_eq!(summary["original_bytes"], 100_100);
        assert_eq!(summary["type"], "message_update");
        assert_eq!(summary["role"], "assistant");
        assert_eq!(summary["message"]["type"], "tool_call");
        assert!(serde_json::to_vec(&summary).unwrap().len() < 1024);
    }
}

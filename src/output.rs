use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use chrono::Local;
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;

use crate::state::unix_timestamp;

const MAX_STREAM_PAYLOAD_BYTES: usize = 16 * 1024;
const CONTENT_PREVIEW_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    Plain,
    Pretty,
    Json,
}

#[derive(Clone)]
pub struct Output {
    mode: OutputMode,
    write_lock: Arc<Mutex<()>>,
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
        Self {
            mode,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn event(&self, kind: &str, details: Value) -> Result<()> {
        if self.mode == OutputMode::Json {
            self.write_envelope(kind, details)?;
        }
        Ok(())
    }

    pub fn plain_stdout(&self, message: &str) -> Result<()> {
        if self.mode != OutputMode::Json {
            self.write_plain(false, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn plain_stderr(&self, message: &str) -> Result<()> {
        if self.mode != OutputMode::Json {
            self.write_plain(true, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn child_line(&self, role: &str, stream: &str, run_id: &str, line: &[u8]) -> Result<()> {
        if self.mode != OutputMode::Json {
            // Sensor stdout is protocol data, not a diagnostic. It is still
            // captured in stdout.log and passed unchanged to the decider.
            if role == "sensor" && stream == "stdout" {
                return Ok(());
            }
            let rendered = if self.mode == OutputMode::Pretty {
                prettify_json_line(line)?
            } else {
                line.to_vec()
            };
            let tagged = prefix_lines(&rendered, format!("[{role}] ").as_bytes());
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
        let _guard = self
            .write_lock
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
    fn pretty_child_output_prefixes_every_rendered_line() {
        let rendered = prettify_json_line(b"{\"outer\":{\"inner\":1}}\n").unwrap();
        assert_eq!(
            prefix_lines(&rendered, b"[worker] "),
            b"[worker] {\n[worker]   \"outer\": {\n[worker]     \"inner\": 1\n[worker]   }\n[worker] }\n"
        );
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

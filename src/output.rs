use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;

use crate::state::unix_timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    Plain,
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
        if self.mode == OutputMode::Plain {
            self.write_bytes(false, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn plain_stderr(&self, message: &str) -> Result<()> {
        if self.mode == OutputMode::Plain {
            self.write_bytes(true, message.as_bytes())?;
        }
        Ok(())
    }

    pub fn child_line(&self, role: &str, stream: &str, run_id: &str, line: &[u8]) -> Result<()> {
        if self.mode == OutputMode::Plain {
            return self.write_bytes(stream == "stderr", line);
        }

        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        let mut details = serde_json::json!({
            "role": role,
            "stream": stream,
            "run_id": run_id,
        });
        if let Ok(payload) = serde_json::from_slice::<Value>(trimmed) {
            details["payload"] = payload;
        } else {
            details["content"] = Value::String(String::from_utf8_lossy(trimmed).into_owned());
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

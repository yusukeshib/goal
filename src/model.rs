use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeciderAction {
    RunTask {
        task: String,
    },
    Wait {
        reason: String,
        retry_after_seconds: u64,
    },
    Complete {
        summary: String,
    },
    Failure {
        reason: String,
    },
}

impl DeciderAction {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::RunTask { task } => require_text("task", task),
            Self::Wait { reason, .. } => require_text("reason", reason),
            Self::Complete { summary } => require_text("summary", summary),
            Self::Failure { reason } => require_text("reason", reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCompletion {
    Done { summary: String },
    Failure { reason: String },
}

impl WorkerCompletion {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Done { summary } => require_text("summary", summary),
            Self::Failure { reason } => require_text("reason", reason),
        }
    }
}

fn require_text(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_decider_variant() {
        let cases = [
            r#"{"type":"run_task","task":"fix it"}"#,
            r#"{"type":"wait","reason":"CI","retry_after_seconds":10}"#,
            r#"{"type":"complete","summary":"done"}"#,
            r#"{"type":"failure","reason":"missing authority"}"#,
        ];
        for json in cases {
            serde_json::from_str::<DeciderAction>(json)
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn rejects_invalid_decider_messages() {
        let cases = [
            r#"{"type":"unknown"}"#,
            r#"{"type":"run_task"}"#,
            r#"{"type":"run_task","task":"","extra":1}"#,
            r#"{"type":"wait","reason":"x","retry_after_seconds":-1}"#,
            r#"{"type":"prompt_human","question":"Choose?","context":null}"#,
            r#"{"type":"failure","reason":""}"#,
        ];
        for json in cases {
            let parsed = serde_json::from_str::<DeciderAction>(json);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn parses_every_worker_variant() {
        let cases = [
            r#"{"type":"done","summary":"fixed"}"#,
            r#"{"type":"failure","reason":"No credentials"}"#,
        ];
        for json in cases {
            serde_json::from_str::<WorkerCompletion>(json)
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn rejects_invalid_worker_messages() {
        let cases = [
            r#"{"type":"done","summary":""}"#,
            r#"{"type":"needs_input","question":"Q","context":"C","resume_hint":null}"#,
            r#"{"type":"blocked","reason":"No credentials"}"#,
            r#"{"type":"failure","reason":42}"#,
            r#"{"summary":"done"}"#,
        ];
        for json in cases {
            let parsed = serde_json::from_str::<WorkerCompletion>(json);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "{json}"
            );
        }
    }
}

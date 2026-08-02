use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeciderAction {
    RunTask {
        task: String,
    },
    PromptHuman {
        question: String,
        context: Option<String>,
    },
    Wait {
        reason: String,
        retry_after_seconds: u64,
    },
    Complete {
        summary: String,
    },
}

impl DeciderAction {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::RunTask { task } => require_text("task", task),
            Self::PromptHuman { question, context } => {
                require_text("question", question)?;
                if let Some(context) = context {
                    require_text("context", context)?;
                }
                Ok(())
            }
            Self::Wait { reason, .. } => require_text("reason", reason),
            Self::Complete { summary } => require_text("summary", summary),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCompletion {
    Done {
        summary: String,
    },
    NeedsInput {
        question: String,
        context: String,
        resume_hint: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

impl WorkerCompletion {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Done { summary } => require_text("summary", summary),
            Self::NeedsInput {
                question,
                context,
                resume_hint,
            } => {
                require_text("question", question)?;
                require_text("context", context)?;
                if let Some(hint) = resume_hint {
                    require_text("resume_hint", hint)?;
                }
                Ok(())
            }
            Self::Blocked { reason } => require_text("reason", reason),
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
            r#"{"type":"prompt_human","question":"Choose?","context":null}"#,
            r#"{"type":"wait","reason":"CI","retry_after_seconds":10}"#,
            r#"{"type":"complete","summary":"done"}"#,
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
            r#"{"type":"needs_input","question":"Which?","context":"Two choices","resume_hint":null}"#,
            r#"{"type":"blocked","reason":"No credentials"}"#,
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
            r#"{"type":"needs_input","question":"Q"}"#,
            r#"{"type":"blocked","reason":42}"#,
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

use serde_json::Value;

use crate::model::WorkerCompletion;

pub fn decider_prompt(goal: &str, observation: &Value, state_context: &str) -> String {
    format!(
        r#"You are a one-shot, read-only decider for a foreground goal controller.
You MUST NOT modify the project or external world. Inspect only the information in this prompt.
Choose exactly one next action and atomically write one JSON object to GOAL_RESULT_PATH.
Valid actions use a `type` tag:
- {{"type":"run_task","task":"one bounded task"}}
- {{"type":"prompt_human","question":"verbatim question","context":"optional context"}}
- {{"type":"wait","reason":"why","retry_after_seconds":60}}
- {{"type":"complete","summary":"why the finite goal is satisfied"}}
Do not write protocol JSON to stdout.

GOAL:
{goal}

CURRENT OBSERVATION:
{observation}

PRIOR CONTEXT:
{state_context}
"#,
        observation = serde_json::to_string_pretty(observation).expect("JSON value serializes")
    )
}

pub fn worker_prompt(goal: &str, observation: &Value, task: &str, result_path: &str) -> String {
    format!(
        r#"You are a disposable, non-interactive worker. Perform exactly the assigned task below.

Rules:
- Perform only the one assigned task. Do not broaden it or select a new task.
- Never wait for, prompt, or read input from a human.
- Complete all safe work possible before returning needs_input.
- Before an irreversible or human-only decision, stop at a safe boundary and return needs_input; do not perform the operation.
- Include enough context for a fresh decider and worker to continue without this process's conversational context.
- Write exactly one structured completion atomically when practical to `{result_path}`, then exit.
- Do not claim success based only on commands attempted; describe what actually changed or was verified.
- Stdout and stderr are diagnostics, not protocol output.

Valid completions use a `type` tag:
- {{"type":"done","summary":"actual changes and verification"}}
- {{"type":"needs_input","question":"verbatim question","context":"resume context","resume_hint":"optional hint"}}
- {{"type":"blocked","reason":"why safe progress is impossible"}}

GOAL:
{goal}

CURRENT OBSERVATION:
{observation}

ASSIGNED TASK:
{task}
"#,
        observation = serde_json::to_string_pretty(observation).expect("JSON value serializes")
    )
}

pub fn prior_context(
    completion: Option<&WorkerCompletion>,
    human_exchange: Option<(&str, Option<&str>, &str)>,
) -> String {
    let mut lines = Vec::new();
    if let Some(completion) = completion {
        lines.push(format!(
            "Latest worker completion: {}",
            serde_json::to_string(completion).expect("completion serializes")
        ));
    }
    if let Some((question, context, answer)) = human_exchange {
        lines.push(format!("Latest human question: {question}"));
        if let Some(context) = context {
            lines.push(format!("Question context: {context}"));
        }
        lines.push(format!("Human answer: {answer}"));
    }
    if lines.is_empty() {
        "No prior worker completion or human answer.".to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_include_contract_and_inputs() {
        let observation = serde_json::json!({"healthy": true});
        let decider = decider_prompt("Keep it green", &observation, "No history");
        assert!(decider.contains("read-only"));
        assert!(decider.contains("Keep it green"));
        assert!(decider.contains("\"healthy\": true"));
        let worker = worker_prompt("Keep it green", &observation, "Fix CI", "/tmp/result.json");
        assert!(worker.contains("Perform only the one assigned task"));
        assert!(worker.contains("Fix CI"));
        assert!(worker.contains("/tmp/result.json"));
    }
}

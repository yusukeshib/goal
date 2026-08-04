use serde_json::Value;

use crate::model::WorkerCompletion;

pub fn decider_prompt(goal: &str, observation: &Value, state_context: &str) -> String {
    format!(
        r#"You are a one-shot, read-only decider for a foreground goal controller.
You MUST NOT modify the project or external world. Inspect only the information in this prompt.
Choose exactly one next action and atomically write one JSON object to GOAL_RESULT_PATH.
Never request human input, approval, or intervention.
Valid actions use a `type` tag:
- {{"type":"run_task","task":"one bounded task"}}
- {{"type":"wait","reason":"why automatic progress is temporarily unavailable","retry_after_seconds":60}}
- {{"type":"complete","summary":"why the finite goal is satisfied"}}
- {{"type":"failure","reason":"specific reason this decision cycle cannot make automatic progress"}}
Use failure when this cycle cannot make safe automatic progress and waiting is not the more accurate action. The failed decider run is recorded, then the controller backs off and obtains a fresh observation. A prior worker failure is task-local: do not repeat the same task unless the observation materially changed; choose other safe work when available, or wait if a world condition may change. Include concrete evidence useful for diagnosing and improving future runs.
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
- Never request human approval or intervention.
- Complete all safe, automatic work possible before returning failure.
- If completion requires a human-only decision, unavailable authority, missing credentials, or an operation you cannot perform safely and automatically, do not perform it; return failure with a specific reason.
- A failure reason must include concrete evidence and enough context to diagnose the run and improve future goals or automation.
- Write exactly one structured completion atomically when practical to `{result_path}`, then exit.
- Do not claim success based only on commands attempted; describe what actually changed or was verified.
- Stdout and stderr are diagnostics, not protocol output.

Valid completions use a `type` tag:
- {{"type":"done","summary":"actual changes and verification"}}
- {{"type":"failure","reason":"specific reason automatic task completion is impossible"}}

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

pub fn prior_context(completion: Option<&WorkerCompletion>) -> String {
    match completion {
        Some(completion) => format!(
            "Latest worker completion: {}",
            serde_json::to_string(completion).expect("completion serializes")
        ),
        None => "No prior worker completion.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_include_non_interactive_contract_and_inputs() {
        let observation = serde_json::json!({"healthy": true});
        let decider = decider_prompt("Keep it green", &observation, "No history");
        assert!(decider.contains("read-only"));
        assert!(decider.contains("Never request human input"));
        assert!(decider.contains(r#"{"type":"failure""#));
        assert!(decider.contains("A prior worker failure is task-local"));
        assert!(!decider.contains("prompt_human"));
        assert!(decider.contains("Keep it green"));
        assert!(decider.contains("\"healthy\": true"));

        let worker = worker_prompt("Keep it green", &observation, "Fix CI", "/tmp/result.json");
        assert!(worker.contains("Perform only the one assigned task"));
        assert!(worker.contains("Never request human approval"));
        assert!(worker.contains(r#"{"type":"failure""#));
        assert!(!worker.contains("needs_input"));
        assert!(!worker.contains("blocked"));
        assert!(worker.contains("Fix CI"));
        assert!(worker.contains("/tmp/result.json"));
    }
}

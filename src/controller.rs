use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde_json::Value;

use crate::{
    analytics::RunOutcome,
    cancel::Interrupted,
    config::LoadedConfig,
    model::{DeciderAction, WorkerCompletion},
    output::Output,
    prompt,
    runner::{RunArtifacts, RunError, Runner},
    state::{ControllerLock, PersistentState, StateStore, unix_timestamp},
};

#[derive(Debug)]
pub struct GoalFailure {
    source: String,
    reason: String,
    run_id: Option<String>,
}

impl GoalFailure {
    fn new(source: &str, reason: String, run_id: Option<String>) -> Self {
        Self {
            source: source.to_owned(),
            reason,
            run_id,
        }
    }

    pub fn details(&self) -> Value {
        serde_json::json!({
            "source": self.source,
            "reason": self.reason,
            "run_id": self.run_id,
        })
    }
}

impl std::fmt::Display for GoalFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} reported failure: {}",
            self.source, self.reason
        )
    }
}

impl std::error::Error for GoalFailure {}

pub struct Controller {
    loaded: LoadedConfig,
    _lock: ControllerLock,
    runner: Runner,
    store: StateStore,
    state: PersistentState,
    cancelled: Arc<AtomicBool>,
    output: Output,
}

impl Controller {
    pub fn new(loaded: LoadedConfig, cancelled: Arc<AtomicBool>, output: Output) -> Result<Self> {
        let controller_lock = ControllerLock::acquire(&loaded.config_path)?;
        let store = StateStore::new(&loaded.project_dir)?;
        let state = store.load()?;
        let runner = Runner::new(&loaded.project_dir, Arc::clone(&cancelled), output.clone())?;
        Ok(Self {
            loaded,
            _lock: controller_lock,
            runner,
            store,
            state,
            cancelled,
            output,
        })
    }

    pub fn run(mut self) -> Result<()> {
        loop {
            self.ensure_running()?;
            let cycle_id = self.begin_cycle()?;
            self.output
                .event("cycle_started", serde_json::json!({"cycle_id": cycle_id}))?;
            self.output
                .event("phase_started", serde_json::json!({"phase": "sensor"}))?;
            let phase_started = Instant::now();
            let observation = match self.runner.run_sensor(&self.loaded.config.sensor) {
                Ok((observation, artifacts)) => {
                    artifacts.finish(RunOutcome::Success, None, Some("observation"), None)?;
                    let details = serde_json::json!({
                        "run_id": artifacts.id,
                        "phase": "sensor",
                        "duration_ms": phase_started.elapsed().as_millis() as u64,
                    });
                    self.store.event("sense_succeeded", details.clone())?;
                    self.output.event("phase_finished", details)?;
                    observation
                }
                Err((RunError::Cancelled, artifacts)) => {
                    finish_run_error(artifacts.as_deref(), &RunError::Cancelled)?;
                    return Err(Interrupted.into());
                }
                Err((error, artifacts)) => {
                    finish_run_error(artifacts.as_deref(), &error)?;
                    let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                    let reason = error.to_string();
                    self.record_run_error("sense_failed", &error, run_id.as_deref())?;
                    self.output.event(
                        "sense_failed",
                        serde_json::json!({"run_id": run_id, "error": reason}),
                    )?;
                    self.sleep(self.loaded.config.interval_seconds.max(1))?;
                    continue;
                }
            };

            let context = prompt::prior_context(self.state.latest_worker_completion.as_ref());
            let decider_prompt = prompt::decider_prompt(&self.loaded.goal, &observation, &context);
            self.output
                .event("phase_started", serde_json::json!({"phase": "decider"}))?;
            let phase_started = Instant::now();
            let (action, artifacts) = match self.runner.run_json::<DeciderAction>(
                "decider",
                &self.loaded.config.decider,
                &decider_prompt,
            ) {
                Ok(result) => result,
                Err((RunError::Cancelled, artifacts)) => {
                    finish_run_error(artifacts.as_deref(), &RunError::Cancelled)?;
                    return Err(Interrupted.into());
                }
                Err((error, artifacts)) => {
                    finish_run_error(artifacts.as_deref(), &error)?;
                    let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                    let reason = error.to_string();
                    self.record_run_error("decider_failed", &error, run_id.as_deref())?;
                    self.output.event(
                        "decider_failed",
                        serde_json::json!({"run_id": run_id, "error": reason}),
                    )?;
                    if matches!(error, RunError::Protocol(_)) {
                        self.sleep(self.loaded.config.interval_seconds.max(1))?;
                        continue;
                    }
                    return self.terminal_failure("decider", reason, run_id);
                }
            };
            if let Err(error) = action.validate() {
                let reason = format!("schema validation: {error:#}");
                artifacts.finish(RunOutcome::Failure, Some("protocol"), None, Some(&reason))?;
                let details = serde_json::json!({
                    "run_id": artifacts.id,
                    "error": reason,
                });
                self.store.event("decider_failed", details.clone())?;
                self.output.event("decider_failed", details)?;
                self.sleep(self.loaded.config.interval_seconds.max(1))?;
                continue;
            }
            let (outcome, failure_kind, result_type, failure_reason) = match &action {
                DeciderAction::RunTask { .. } => (RunOutcome::Success, None, "run_task", None),
                DeciderAction::Wait { .. } => (RunOutcome::Success, None, "wait", None),
                DeciderAction::Complete { .. } => (RunOutcome::Success, None, "complete", None),
                DeciderAction::Failure { reason } => (
                    RunOutcome::Failure,
                    Some("logical"),
                    "failure",
                    Some(reason.as_str()),
                ),
            };
            artifacts.finish(outcome, failure_kind, Some(result_type), failure_reason)?;
            self.output.event(
                "phase_finished",
                serde_json::json!({
                    "phase": "decider",
                    "run_id": artifacts.id,
                    "duration_ms": phase_started.elapsed().as_millis() as u64,
                    "outcome": outcome,
                }),
            )?;
            let decider_run_id = artifacts.id;
            let details = serde_json::json!({
                "run_id": decider_run_id,
                "action": action,
            });
            self.store.event("decision", details.clone())?;
            self.output.event("decision", details)?;

            match action {
                DeciderAction::RunTask { task } => {
                    let worker_prompt = prompt::worker_prompt(
                        &self.loaded.goal,
                        &observation,
                        &task,
                        "$GOAL_RESULT_PATH",
                    );
                    self.output.event(
                        "phase_started",
                        serde_json::json!({"phase": "worker", "task": task}),
                    )?;
                    let phase_started = Instant::now();
                    match self.runner.run_json::<WorkerCompletion>(
                        "worker",
                        &self.loaded.config.worker,
                        &worker_prompt,
                    ) {
                        Ok((completion, artifacts)) => {
                            if let Err(error) = completion.validate() {
                                let reason = format!("schema validation: {error:#}");
                                artifacts.finish(
                                    RunOutcome::Failure,
                                    Some("protocol"),
                                    None,
                                    Some(&reason),
                                )?;
                                let details = serde_json::json!({
                                    "run_id": artifacts.id,
                                    "error": reason,
                                });
                                self.store.event("worker_failed", details.clone())?;
                                self.output.event("worker_failed", details)?;
                                return self.terminal_failure("worker", reason, Some(artifacts.id));
                            }
                            let (outcome, failure_kind, result_type, failure_reason) =
                                match &completion {
                                    WorkerCompletion::Done { .. } => {
                                        (RunOutcome::Success, None, "done", None)
                                    }
                                    WorkerCompletion::Failure { reason } => (
                                        RunOutcome::Failure,
                                        Some("logical"),
                                        "failure",
                                        Some(reason.as_str()),
                                    ),
                                };
                            artifacts.finish(
                                outcome,
                                failure_kind,
                                Some(result_type),
                                failure_reason,
                            )?;
                            self.output.event(
                                "phase_finished",
                                serde_json::json!({
                                    "phase": "worker",
                                    "run_id": artifacts.id,
                                    "duration_ms": phase_started.elapsed().as_millis() as u64,
                                    "outcome": outcome,
                                }),
                            )?;
                            let details = serde_json::json!({
                                "run_id": artifacts.id,
                                "completion": completion,
                            });
                            self.store.event("worker_completed", details.clone())?;
                            self.output.event("worker_completed", details)?;
                            self.state.latest_worker_completion = Some(completion);
                            self.store.save(&self.state)?;
                        }
                        Err((RunError::Cancelled, artifacts)) => {
                            finish_run_error(artifacts.as_deref(), &RunError::Cancelled)?;
                            return Err(Interrupted.into());
                        }
                        Err((error, artifacts)) => {
                            finish_run_error(artifacts.as_deref(), &error)?;
                            let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                            let reason = error.to_string();
                            self.record_run_error("worker_failed", &error, run_id.as_deref())?;
                            self.output.event(
                                "worker_failed",
                                serde_json::json!({"run_id": run_id, "error": reason}),
                            )?;
                            return self.terminal_failure("worker", reason, run_id);
                        }
                    }
                }
                DeciderAction::Wait {
                    reason,
                    retry_after_seconds,
                } => {
                    let seconds =
                        cap_wait(retry_after_seconds, self.loaded.config.max_wait_seconds);
                    self.output
                        .plain_stdout(&format!("[decision] waiting {seconds}s: {reason}\n"))?;
                    let details = serde_json::json!({
                        "reason": reason,
                        "requested_seconds": retry_after_seconds,
                        "actual_seconds": seconds,
                    });
                    self.store.event("wait", details.clone())?;
                    self.output.event("wait", details)?;
                    self.sleep(seconds)?;
                }
                DeciderAction::Complete { summary } => {
                    let details = serde_json::json!({"summary": summary});
                    self.store.event("complete", details.clone())?;
                    self.output.event("complete", details)?;
                    self.output
                        .plain_stdout(&format!("goal complete: {summary}\n"))?;
                    return Ok(());
                }
                DeciderAction::Failure { reason } => {
                    return self.terminal_failure("decider", reason, Some(decider_run_id));
                }
            }

            if self.loaded.config.interval_seconds > 0 {
                self.sleep(self.loaded.config.interval_seconds)?;
            }
        }
    }

    fn begin_cycle(&mut self) -> Result<String> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let cycle_id = format!("cycle-{nonce}");
        self.state.latest_cycle_id = Some(cycle_id.clone());
        self.state.latest_cycle_timestamp = Some(unix_timestamp());
        self.store.save(&self.state)?;
        Ok(cycle_id)
    }

    fn terminal_failure(&self, source: &str, reason: String, run_id: Option<String>) -> Result<()> {
        let failure = GoalFailure::new(source, reason, run_id);
        self.store.event("failure", failure.details())?;
        Err(failure.into())
    }

    fn record_run_error(&self, kind: &str, error: &RunError, run_id: Option<&str>) -> Result<()> {
        self.store.event(
            kind,
            serde_json::json!({"run_id": run_id, "error": error.to_string()}),
        )
    }

    fn sleep(&self, seconds: u64) -> Result<()> {
        let mut remaining = Duration::from_secs(seconds);
        while !remaining.is_zero() {
            self.ensure_running()?;
            let slice = remaining.min(Duration::from_millis(100));
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
        self.ensure_running()
    }

    fn ensure_running(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        Ok(())
    }
}

fn finish_run_error(artifacts: Option<&RunArtifacts>, error: &RunError) -> Result<()> {
    if let Some(artifacts) = artifacts {
        let outcome = if matches!(error, RunError::Cancelled) {
            RunOutcome::Cancelled
        } else {
            RunOutcome::Failure
        };
        artifacts.finish(outcome, Some(error.kind()), None, Some(&error.to_string()))?;
    }
    Ok(())
}

fn cap_wait(requested: u64, maximum: u64) -> u64 {
    requested.min(maximum)
}

#[cfg(test)]
mod tests {
    use super::cap_wait;

    #[test]
    fn wait_duration_is_capped() {
        assert_eq!(cap_wait(120, 30), 30);
        assert_eq!(cap_wait(10, 30), 10);
    }
}

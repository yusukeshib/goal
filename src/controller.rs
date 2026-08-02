use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde_json::Value;

use crate::{
    cancel::Interrupted,
    config::LoadedConfig,
    model::{DeciderAction, WorkerCompletion},
    output::Output,
    prompt,
    runner::{RunError, Runner},
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
            self.begin_cycle()?;
            let observation = match self.runner.run_sensor(&self.loaded.config.sensor) {
                Ok((observation, artifacts)) => {
                    self.store.event(
                        "sense_succeeded",
                        serde_json::json!({"run_id": artifacts.id}),
                    )?;
                    observation
                }
                Err((RunError::Cancelled, _)) => return Err(Interrupted.into()),
                Err((error, artifacts)) => {
                    let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                    let reason = error.to_string();
                    self.record_run_error("sense_failed", &error, run_id.as_deref())?;
                    self.output.event(
                        "sense_failed",
                        serde_json::json!({"run_id": run_id, "error": reason}),
                    )?;
                    return self.terminal_failure("sensor", reason, run_id);
                }
            };

            let context = prompt::prior_context(self.state.latest_worker_completion.as_ref());
            let decider_prompt = prompt::decider_prompt(&self.loaded.goal, &observation, &context);
            let (action, artifacts) = match self.runner.run_json::<DeciderAction>(
                "decider",
                &self.loaded.config.decider,
                &decider_prompt,
            ) {
                Ok(result) => result,
                Err((RunError::Cancelled, _)) => return Err(Interrupted.into()),
                Err((error, artifacts)) => {
                    let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                    let reason = error.to_string();
                    self.record_run_error("decider_failed", &error, run_id.as_deref())?;
                    self.output.event(
                        "decider_failed",
                        serde_json::json!({"run_id": run_id, "error": reason}),
                    )?;
                    return self.terminal_failure("decider", reason, run_id);
                }
            };
            if let Err(error) = action.validate() {
                let reason = format!("schema validation: {error:#}");
                let details = serde_json::json!({
                    "run_id": artifacts.id,
                    "error": reason,
                });
                self.store.event("decider_failed", details.clone())?;
                self.output.event("decider_failed", details)?;
                return self.terminal_failure("decider", reason, Some(artifacts.id));
            }
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
                    match self.runner.run_json::<WorkerCompletion>(
                        "worker",
                        &self.loaded.config.worker,
                        &worker_prompt,
                    ) {
                        Ok((completion, artifacts)) => {
                            if let Err(error) = completion.validate() {
                                let reason = format!("schema validation: {error:#}");
                                let details = serde_json::json!({
                                    "run_id": artifacts.id,
                                    "error": reason,
                                });
                                self.store.event("worker_failed", details.clone())?;
                                self.output.event("worker_failed", details)?;
                                return self.terminal_failure("worker", reason, Some(artifacts.id));
                            }
                            let details = serde_json::json!({
                                "run_id": artifacts.id,
                                "completion": completion,
                            });
                            self.store.event("worker_completed", details.clone())?;
                            self.output.event("worker_completed", details)?;
                            self.state.latest_worker_completion = Some(completion.clone());
                            self.store.save(&self.state)?;
                            if let WorkerCompletion::Failure { reason } = completion {
                                return self.terminal_failure("worker", reason, Some(artifacts.id));
                            }
                        }
                        Err((RunError::Cancelled, _)) => return Err(Interrupted.into()),
                        Err((error, artifacts)) => {
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

    fn begin_cycle(&mut self) -> Result<()> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        self.state.latest_cycle_id = Some(format!("cycle-{nonce}"));
        self.state.latest_cycle_timestamp = Some(unix_timestamp());
        self.store.save(&self.state)
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

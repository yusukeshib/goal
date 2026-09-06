use std::{
    sync::{
        Arc, mpsc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow, bail};

use crate::{
    analytics::RunOutcome,
    cancel::Interrupted,
    config::{LoadedConfig, WorkerObservation},
    model::{DeciderAction, WorkerBatchCompletion, WorkerCompletion, WorkerTaskResult},
    output::Output,
    prompt,
    runner::{RunArtifacts, RunError, RunResult, Runner},
    state::{ControllerLock, PersistentState, StateStore, unix_timestamp},
};

const FAILURE_INITIAL_BACKOFF_SECONDS: u64 = 5;
const FAILURE_MAX_BACKOFF_SECONDS: u64 = 60;
const SENSOR_RETRY_HINT_MARKER: &str = "goal-retry-after-seconds=";
const SENSOR_RETRY_HINT_MAX_SECONDS: u64 = 2 * 60 * 60;

#[derive(Default)]
struct FailureBackoff {
    consecutive_failures: u32,
}

impl FailureBackoff {
    fn next_delay(&mut self) -> u64 {
        let exponent = self.consecutive_failures.min(4);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        FAILURE_INITIAL_BACKOFF_SECONDS
            .saturating_mul(1_u64 << exponent)
            .min(FAILURE_MAX_BACKOFF_SECONDS)
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

pub struct Controller {
    loaded: LoadedConfig,
    _lock: ControllerLock,
    runner: Arc<Runner>,
    store: StateStore,
    state: PersistentState,
    cancelled: Arc<AtomicBool>,
    output: Output,
}

impl Controller {
    pub fn new(loaded: LoadedConfig, cancelled: Arc<AtomicBool>, output: Output) -> Result<Self> {
        let controller_lock = ControllerLock::acquire(&loaded.project_dir, &loaded.config_path)?;
        let store = StateStore::new(&loaded.project_dir)?;
        let state = store.load()?;
        let runner = Runner::new(
            &loaded.project_dir,
            Arc::clone(&cancelled),
            output.clone(),
            loaded.config.max_completed_runs,
        )?;
        Ok(Self {
            loaded,
            _lock: controller_lock,
            runner: Arc::new(runner),
            store,
            state,
            cancelled,
            output,
        })
    }

    pub fn run(mut self) -> Result<()> {
        let mut sensor_failure_backoff = FailureBackoff::default();
        let mut worker_failure_backoff = FailureBackoff::default();

        loop {
            self.ensure_running()?;
            let cycle_id = self.begin_cycle()?;
            self.output.marker("↓")?;
            self.output
                .event("cycle_started", serde_json::json!({"cycle_id": cycle_id}))?;
            let goal = self.loaded.read_goal()?;
            let phase_started = Instant::now();
            let observation = match self.runner.run_sensor(&self.loaded.config.sensor) {
                Ok((observation, artifacts)) => {
                    sensor_failure_backoff.reset();
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
                    let retry_hint_seconds = artifacts
                        .as_deref()
                        .and_then(sensor_retry_after_hint);
                    let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                    let reason = error.to_string();
                    let retry_after_seconds = self
                        .loaded
                        .config
                        .interval_seconds
                        .max(sensor_failure_backoff.next_delay())
                        .max(retry_hint_seconds.unwrap_or(0));
                    let details = serde_json::json!({
                        "run_id": run_id,
                        "error": reason,
                        "retry_after_seconds": retry_after_seconds,
                        "sensor_retry_hint_seconds": retry_hint_seconds,
                    });
                    self.store.event("sense_failed", details.clone())?;
                    self.output.event("sense_failed", details)?;
                    self.sleep(retry_after_seconds)?;
                    continue;
                }
            };

            let context = prompt::prior_context(
                self.state.latest_worker_completion.as_ref(),
                self.state.latest_worker_batch.as_ref(),
            );
            let decider_prompt = prompt::decider_prompt(
                &goal,
                &observation,
                &context,
                self.loaded.config.max_concurrency,
            );
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
                    self.sleep(self.loaded.config.interval_seconds.max(1))?;
                    continue;
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
                DeciderAction::RunTasks { .. } => (RunOutcome::Success, None, "run_tasks", None),
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
                    if self.state.latest_worker_batch.take().is_some() {
                        self.store.save(&self.state)?;
                    }
                    let worker_observation = match self.loaded.config.worker_observation {
                        WorkerObservation::Full => Some(&observation),
                        WorkerObservation::None => None,
                    };
                    let worker_prompt = prompt::worker_prompt(
                        &goal,
                        worker_observation,
                        &task,
                        "$GOAL_RESULT_PATH",
                    );
                    let phase_started = Instant::now();
                    let mut phase_details = serde_json::Map::new();
                    phase_details.insert("task".into(), serde_json::json!(task));
                    match self.runner.run_json_with_details::<WorkerCompletion>(
                        "worker",
                        &self.loaded.config.worker,
                        &worker_prompt,
                        phase_details,
                    ) {
                        Ok((completion, artifacts)) => match completion.validate() {
                            Err(error) => {
                                let reason = format!("schema validation: {error:#}");
                                artifacts.finish(
                                    RunOutcome::Failure,
                                    Some("protocol"),
                                    None,
                                    Some(&reason),
                                )?;
                                let retry_after_seconds = self
                                    .loaded
                                    .config
                                    .interval_seconds
                                    .max(worker_failure_backoff.next_delay());
                                self.record_recoverable_worker_failure(
                                    reason,
                                    Some(artifacts.id.clone()),
                                    retry_after_seconds,
                                )?;
                                self.sleep(retry_after_seconds)?;
                                continue;
                            }
                            Ok(()) => {
                                worker_failure_backoff.reset();
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
                        }
                        Err((RunError::Cancelled, artifacts)) => {
                            finish_run_error(artifacts.as_deref(), &RunError::Cancelled)?;
                            return Err(Interrupted.into());
                        }
                        Err((error, artifacts)) => {
                            finish_run_error(artifacts.as_deref(), &error)?;
                            let run_id = artifacts.as_ref().map(|artifact| artifact.id.clone());
                            let retry_after_seconds = self
                                .loaded
                                .config
                                .interval_seconds
                                .max(worker_failure_backoff.next_delay());
                            self.record_recoverable_worker_failure(
                                error.to_string(),
                                run_id,
                                retry_after_seconds,
                            )?;
                            self.sleep(retry_after_seconds)?;
                            continue;
                        }
                    }
                }
                DeciderAction::RunTasks { tasks, concurrency } => {
                    if let Some(delay) = self.run_worker_batch(
                        &goal,
                        &observation,
                        &tasks,
                        concurrency,
                        &cycle_id,
                        &mut worker_failure_backoff,
                    )? {
                        self.sleep(delay)?;
                        continue;
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
                    // The action's retry delay fully determines when to sense again;
                    // adding the cycle interval here would make the recorded
                    // `actual_seconds` understate the real wait.
                    continue;
                }
                DeciderAction::Complete { summary } => {
                    let details = serde_json::json!({"summary": summary});
                    self.store.event("complete", details.clone())?;
                    self.output.event("complete", details)?;
                    self.output
                        .plain_stdout(&format!("goal complete: {summary}\n"))?;
                    return Ok(());
                }
                DeciderAction::Failure { .. } => {
                    self.sleep(self.loaded.config.interval_seconds.max(1))?;
                    continue;
                }
            }

            if self.loaded.config.interval_seconds > 0 {
                self.sleep(self.loaded.config.interval_seconds)?;
            }
        }
    }

    fn run_worker_batch(
        &mut self,
        goal: &str,
        observation: &serde_json::Value,
        tasks: &[String],
        requested_concurrency: usize,
        batch_id: &str,
        backoff: &mut FailureBackoff,
    ) -> Result<Option<u64>> {
        self.ensure_running()?;
        let concurrency = requested_concurrency
            .min(self.loaded.config.max_concurrency)
            .min(tasks.len());
        self.state.latest_worker_completion = None;
        self.state.latest_worker_batch = Some(WorkerBatchCompletion {
            batch_id: batch_id.to_owned(),
            task_count: tasks.len(),
            results: Vec::new(),
        });
        self.store.save(&self.state)?;
        let details = serde_json::json!({
            "batch_id": batch_id,
            "task_count": tasks.len(),
            "requested_concurrency": requested_concurrency,
            "concurrency": concurrency,
        });
        self.store.event("worker_batch_started", details.clone())?;
        self.output.event("worker_batch_started", details)?;

        let runner = Arc::clone(&self.runner);
        let cancelled = Arc::clone(&self.cancelled);
        let config = self.loaded.config.worker.clone();
        let worker_observation = match self.loaded.config.worker_observation {
            WorkerObservation::Full => Some(observation),
            WorkerObservation::None => None,
        };
        let next_index = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::sync_channel(concurrency);
        let mut retry_delay = None;
        thread::scope(|scope| -> Result<()> {
            let mut handles = Vec::new();
            let mut fatal_error = None;
            for slot in 0..concurrency {
                let sender = sender.clone();
                let runner = &runner;
                let cancelled = &cancelled;
                let config = &config;
                let next_index = &next_index;
                let handle = thread::Builder::new()
                    .name(format!("goal-worker-{slot}"))
                    .spawn_scoped(scope, move || {
                        while !cancelled.load(Ordering::SeqCst) {
                            let index = next_index.fetch_add(1, Ordering::SeqCst);
                            let Some(task) = tasks.get(index) else {
                                break;
                            };
                            if cancelled.load(Ordering::SeqCst) {
                                break;
                            }
                            let worker_prompt = prompt::worker_prompt(
                                goal,
                                worker_observation,
                                task,
                                "$GOAL_RESULT_PATH",
                            );
                            let mut details = serde_json::Map::new();
                            details.insert("task".into(), serde_json::json!(task));
                            details.insert("task_index".into(), serde_json::json!(index));
                            details.insert("batch_id".into(), serde_json::json!(batch_id));
                            let started = Instant::now();
                            let result = runner.run_json_with_details::<WorkerCompletion>(
                                "worker",
                                config,
                                &worker_prompt,
                                details,
                            );
                            let duration_ms = started.elapsed().as_millis() as u64;
                            if sender.send((index, result, duration_ms)).is_err() {
                                break;
                            }
                        }
                    });
                match handle {
                    Ok(handle) => handles.push(handle),
                    Err(error) => {
                        cancelled.store(true, Ordering::SeqCst);
                        fatal_error = Some(anyhow!(error).context("spawn batch worker"));
                        break;
                    }
                }
            }
            drop(sender);
            // State and event persistence stay on this thread. Even after a
            // collector failure, drain every result and join all active workers.
            for (index, result, duration_ms) in receiver {
                let recorded = self
                    .record_batch_worker_result(
                        batch_id,
                        index,
                        &tasks[index],
                        result,
                        duration_ms,
                        backoff,
                        &mut retry_delay,
                    )
                    .and_then(|result| {
                        let batch = self.state.latest_worker_batch.as_mut()
                            .expect("batch initialized before dispatch");
                        batch.results.push(result);
                        batch.results.sort_by_key(|result| result.task_index);
                        self.store.save(&self.state)
                    });
                if let Err(error) = recorded {
                    cancelled.store(true, Ordering::SeqCst);
                    fatal_error.get_or_insert(error);
                }
            }
            for handle in handles {
                if handle.join().is_err() {
                    cancelled.store(true, Ordering::SeqCst);
                    fatal_error.get_or_insert_with(|| anyhow!("batch worker thread panicked"));
                }
            }
            if let Some(error) = fatal_error {
                return Err(error);
            }
            Ok(())
        })?;
        self.ensure_running()?;
        let batch = self.state.latest_worker_batch.as_ref().expect("batch initialized");
        if batch.results.len() != tasks.len() {
            bail!("worker batch ended without collecting every task");
        }
        let failed = batch.results.iter()
            .filter(|result| matches!(result.completion, WorkerCompletion::Failure { .. }))
            .count();
        let details = serde_json::json!({
            "batch_id": batch_id,
            "task_count": tasks.len(),
            "completed": batch.results.len(),
            "failed": failed,
        });
        self.store.event("worker_batch_finished", details.clone())?;
        self.output.event("worker_batch_finished", details)?;
        if retry_delay.is_none() {
            backoff.reset();
        }
        Ok(retry_delay)
    }

    fn record_batch_worker_result(
        &mut self,
        batch_id: &str,
        task_index: usize,
        task: &str,
        result: RunResult<WorkerCompletion>,
        duration_ms: u64,
        backoff: &mut FailureBackoff,
        retry_delay: &mut Option<u64>,
    ) -> Result<WorkerTaskResult> {
        let result = result.and_then(|(completion, artifacts)| {
            match completion.validate() {
                Ok(()) => Ok((completion, artifacts)),
                Err(error) => Err((
                    RunError::Protocol(anyhow!("schema validation: {error:#}")),
                    Some(Box::new(artifacts)),
                )),
            }
        });
        let (run_id, completion) = match result {
            Ok((completion, artifacts)) => {
                let (outcome, failure_kind, result_type, reason) = match &completion {
                    WorkerCompletion::Done { .. } => (RunOutcome::Success, None, "done", None),
                    WorkerCompletion::Failure { reason } => (
                        RunOutcome::Failure, Some("logical"), "failure", Some(reason.as_str()),
                    ),
                };
                artifacts.finish(outcome, failure_kind, Some(result_type), reason)?;
                self.output.event("phase_finished", serde_json::json!({
                    "phase": "worker", "run_id": artifacts.id,
                    "batch_id": batch_id, "task_index": task_index,
                    "duration_ms": duration_ms, "outcome": outcome,
                }))?;
                let details = serde_json::json!({
                    "run_id": artifacts.id, "batch_id": batch_id,
                    "task_index": task_index, "task": task, "completion": completion,
                });
                self.store.event("worker_completed", details.clone())?;
                self.output.event("worker_completed", details)?;
                (Some(artifacts.id), completion)
            }
            Err((error, artifacts)) => {
                finish_run_error(artifacts.as_deref(), &error)?;
                let run_id = artifacts.as_ref().map(|run| run.id.clone());
                let completion = uncertain_worker_completion(&error.to_string());
                let kind = if matches!(error, RunError::Cancelled) {
                    "worker_cancelled"
                } else {
                    retry_delay.get_or_insert_with(|| {
                        self.loaded.config.interval_seconds.max(backoff.next_delay())
                    });
                    "worker_failed"
                };
                let details = serde_json::json!({
                    "run_id": run_id, "batch_id": batch_id,
                    "task_index": task_index, "task": task,
                    "error": error.to_string(), "completion": completion,
                    "recovery": "resense", "retry_after_seconds": retry_delay,
                });
                self.store.event(kind, details.clone())?;
                self.output.event(kind, details)?;
                (run_id, completion)
            }
        };
        Ok(WorkerTaskResult {
            task_index,
            task: task.to_owned(),
            run_id,
            completion,
        })
    }

    fn begin_cycle(&mut self) -> Result<String> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let cycle_id = format!("cycle-{nonce}");
        self.state.latest_cycle_id = Some(cycle_id.clone());
        self.state.latest_cycle_timestamp = Some(unix_timestamp());
        self.store.save(&self.state)?;
        Ok(cycle_id)
    }

    fn record_recoverable_worker_failure(
        &mut self,
        error: String,
        run_id: Option<String>,
        retry_after_seconds: u64,
    ) -> Result<()> {
        let completion = uncertain_worker_completion(&error);
        let details = serde_json::json!({
            "run_id": run_id,
            "error": error,
            "completion": &completion,
            "recovery": "resense",
            "retry_after_seconds": retry_after_seconds,
        });
        self.store.event("worker_failed", details.clone())?;
        self.output.event("worker_failed", details)?;
        self.state.latest_worker_completion = Some(completion);
        self.store.save(&self.state)
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

fn uncertain_worker_completion(error: &str) -> WorkerCompletion {
    WorkerCompletion::Failure {
        reason: format!(
            "Worker invocation failed after it may have modified external state: {error}. A fresh observation is required; do not repeat the same task unless reality materially changed."
        ),
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

fn sensor_retry_after_hint(artifacts: &RunArtifacts) -> Option<u64> {
    let stderr = std::fs::read_to_string(artifacts.dir.join("stderr.log")).ok()?;
    parse_sensor_retry_after_hint(&stderr)
}

fn parse_sensor_retry_after_hint(stderr: &str) -> Option<u64> {
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix(SENSOR_RETRY_HINT_MARKER))
        .filter_map(|value| value.parse::<u64>().ok())
        .max()
        .map(|seconds| seconds.min(SENSOR_RETRY_HINT_MAX_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::{FailureBackoff, cap_wait, parse_sensor_retry_after_hint};

    #[test]
    fn wait_duration_is_capped() {
        assert_eq!(cap_wait(120, 30), 30);
        assert_eq!(cap_wait(10, 30), 10);
    }

    #[test]
    fn failure_backoff_grows_and_caps() {
        let mut backoff = FailureBackoff::default();

        assert_eq!(backoff.next_delay(), 5);
        assert_eq!(backoff.next_delay(), 10);
        assert_eq!(backoff.next_delay(), 20);
        assert_eq!(backoff.next_delay(), 40);
        assert_eq!(backoff.next_delay(), 60);
        assert_eq!(backoff.next_delay(), 60);
    }

    #[test]
    fn failure_backoff_resets_after_success() {
        let mut backoff = FailureBackoff::default();
        assert_eq!(backoff.next_delay(), 5);
        assert_eq!(backoff.next_delay(), 10);

        backoff.reset();

        assert_eq!(backoff.next_delay(), 5);
    }

    #[test]
    fn sensor_retry_hint_is_parsed_and_bounded() {
        assert_eq!(
            parse_sensor_retry_after_hint("diagnostic\ngoal-retry-after-seconds=125\n"),
            Some(125)
        );
        assert_eq!(
            parse_sensor_retry_after_hint("goal-retry-after-seconds=999999\n"),
            Some(7200)
        );
        assert_eq!(parse_sensor_retry_after_hint("diagnostic only\n"), None);
    }
}

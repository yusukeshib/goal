use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

use crate::{
    cancel::Interrupted,
    config::LoadedConfig,
    human,
    model::{DeciderAction, WorkerCompletion},
    output::Output,
    prompt,
    runner::{RunError, Runner},
    state::{
        ControllerLock, HumanAnswer, HumanQuestion, PersistentState, StateStore, unix_timestamp,
    },
};

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
        if self.state.pending_human_question.is_some() {
            self.collect_pending_answer()?;
        }

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
                    self.record_run_error(
                        "sense_failed",
                        &error,
                        artifacts.as_ref().map(|a| a.id.as_str()),
                    )?;
                    self.output.event(
                        "sense_failed",
                        serde_json::json!({"error": error.to_string(), "retrying": true}),
                    )?;
                    self.output
                        .plain_stderr(&format!("sensor failed: {error}; retrying\n"))?;
                    self.sleep(self.loaded.config.retry_seconds)?;
                    continue;
                }
            };

            let action = loop {
                let context = prompt::prior_context(
                    self.state.latest_worker_completion.as_ref(),
                    self.state.latest_human_answer.as_ref().map(|answer| {
                        (
                            answer.question.as_str(),
                            answer.context.as_deref(),
                            answer.answer.as_str(),
                        )
                    }),
                );
                let decider_prompt =
                    prompt::decider_prompt(&self.loaded.goal, &observation, &context);
                match self.runner.run_json::<DeciderAction>(
                    "decider",
                    &self.loaded.config.decider,
                    &decider_prompt,
                ) {
                    Ok((action, artifacts)) => match action.validate() {
                        Ok(()) => {
                            let details =
                                serde_json::json!({"run_id": artifacts.id, "action": action});
                            self.store.event("decision", details.clone())?;
                            self.output.event("decision", details)?;
                            break action;
                        }
                        Err(error) => {
                            let details = serde_json::json!({"run_id": artifacts.id, "error": format!("schema validation: {error:#}"), "retrying": true});
                            self.store.event("decider_failed", details.clone())?;
                            self.output.event("decider_failed", details)?;
                            self.output.plain_stderr(&format!(
                                "decider protocol failed: {error:#}; retrying\n"
                            ))?;
                        }
                    },
                    Err((RunError::Cancelled, _)) => return Err(Interrupted.into()),
                    Err((error, artifacts)) => {
                        self.record_run_error(
                            "decider_failed",
                            &error,
                            artifacts.as_ref().map(|a| a.id.as_str()),
                        )?;
                        self.output.event(
                            "decider_failed",
                            serde_json::json!({"error": error.to_string(), "retrying": true}),
                        )?;
                        self.output
                            .plain_stderr(&format!("decider failed: {error}; retrying\n"))?;
                    }
                }
                self.sleep(self.loaded.config.retry_seconds)?;
            };

            match action {
                DeciderAction::RunTask { task } => {
                    let result_path_hint = "$GOAL_RESULT_PATH";
                    let worker_prompt = prompt::worker_prompt(
                        &self.loaded.goal,
                        &observation,
                        &task,
                        result_path_hint,
                    );
                    match self.runner.run_json::<WorkerCompletion>(
                        "worker",
                        &self.loaded.config.worker,
                        &worker_prompt,
                    ) {
                        Ok((completion, artifacts)) => {
                            if let Err(error) = completion.validate() {
                                let details = serde_json::json!({"run_id": artifacts.id, "error": format!("schema validation: {error:#}")});
                                self.store.event("worker_failed", details.clone())?;
                                self.output.event("worker_failed", details)?;
                                self.output.plain_stderr(&format!(
                                    "worker protocol failed: {error:#}; sensing current reality\n"
                                ))?;
                                continue;
                            }
                            let details = serde_json::json!({"run_id": artifacts.id, "completion": completion});
                            self.store.event("worker_completed", details.clone())?;
                            self.output.event("worker_completed", details)?;
                            self.state.latest_worker_completion = Some(completion.clone());
                            self.store.save(&self.state)?;
                            if let WorkerCompletion::NeedsInput {
                                question,
                                context,
                                resume_hint,
                            } = completion
                            {
                                let context = match resume_hint {
                                    Some(hint) => Some(format!("{context}\n\nResume hint: {hint}")),
                                    None => Some(context),
                                };
                                self.persist_question(HumanQuestion { question, context })?;
                                self.collect_pending_answer()?;
                            }
                        }
                        Err((RunError::Cancelled, _)) => return Err(Interrupted.into()),
                        Err((error, artifacts)) => {
                            self.record_run_error(
                                "worker_failed",
                                &error,
                                artifacts.as_ref().map(|a| a.id.as_str()),
                            )?;
                            self.output.event(
                                "worker_failed",
                                serde_json::json!({"error": error.to_string()}),
                            )?;
                            self.output.plain_stderr(&format!(
                                "worker failed: {error}; sensing current reality without rerunning it\n"
                            ))?;
                        }
                    }
                }
                DeciderAction::PromptHuman { question, context } => {
                    self.persist_question(HumanQuestion { question, context })?;
                    self.collect_pending_answer()?;
                }
                DeciderAction::Wait {
                    reason,
                    retry_after_seconds,
                } => {
                    let seconds =
                        cap_wait(retry_after_seconds, self.loaded.config.max_wait_seconds);
                    self.output
                        .plain_stdout(&format!("waiting {seconds}s: {reason}\n"))?;
                    let details = serde_json::json!({"reason": reason, "requested_seconds": retry_after_seconds, "actual_seconds": seconds});
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

    fn persist_question(&mut self, question: HumanQuestion) -> Result<()> {
        self.state.pending_human_question = Some(question.clone());
        self.store.save(&self.state)?;
        self.store
            .event("human_question", serde_json::to_value(question)?)
    }

    fn collect_pending_answer(&mut self) -> Result<()> {
        let question = self
            .state
            .pending_human_question
            .clone()
            .context("no pending human question")?;
        let answer =
            human::read_answer(&question, Arc::clone(&self.cancelled), self.output.clone())?;
        self.state.latest_human_answer = Some(HumanAnswer {
            question: question.question.clone(),
            context: question.context.clone(),
            answer: answer.clone(),
        });
        self.state.pending_human_question = None;
        self.store.save(&self.state)?;
        let details = serde_json::json!({"question": question.question, "answer": answer});
        self.store.event("human_answer", details.clone())?;
        self.output.event("human_answer", details)
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

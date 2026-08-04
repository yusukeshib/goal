use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
use chrono::{Days, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::analytics::{
    DurationStats, METADATA_FILE, OutcomeCounts, RoleStats, RunMetadata, RunOutcome,
    parse_duration, summarize_durations, unix_timestamp_millis,
};

const QUALITY_CAVEAT: &str = "This report describes process and protocol evidence only. A successful child result is not independent proof that its task was semantically correct; inspect the retained prompt, result, and logs before changing automation or success criteria.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalysisWindow {
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub start_local: String,
    pub end_local: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalysisReport {
    pub generated_at_ms: u64,
    pub window: AnalysisWindow,
    pub recorded_runs: u64,
    pub legacy_runs_without_metadata_all_time: u64,
    pub outcomes: OutcomeCounts,
    pub worker_success_rate: Option<f64>,
    pub roles: BTreeMap<String, RoleStats>,
    pub failures_by_kind: BTreeMap<String, u64>,
    pub activity: ActivitySummary,
    pub issues: Vec<RunIssue>,
    pub quality_caveat: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ActivitySummary {
    pub senses_succeeded: u64,
    pub senses_failed: u64,
    pub decisions: BTreeMap<String, u64>,
    pub worker_completions: u64,
    pub workers_done: u64,
    pub worker_logical_failures: u64,
    pub worker_run_failures: u64,
    pub waits: u64,
    pub requested_wait_seconds: u64,
    pub actual_wait_seconds: u64,
    pub failure_events: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunIssue {
    pub run_id: String,
    pub role: String,
    pub started_at_ms: u64,
    pub started_at_local: String,
    pub duration_ms: Option<u64>,
    pub outcome: RunOutcome,
    pub failure_kind: Option<String>,
    pub result_type: Option<String>,
    pub reason: Option<String>,
    pub artifacts_dir: String,
}

#[derive(Default)]
struct RoleAccumulator {
    total: u64,
    outcomes: OutcomeCounts,
    durations: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct Event {
    timestamp: u64,
    #[serde(rename = "type")]
    kind: String,
    details: Value,
}

pub fn analyze(
    project_dir: &Path,
    since: Option<&str>,
    date: Option<&str>,
) -> Result<AnalysisReport> {
    if since.is_some() && date.is_some() {
        bail!("--since and --date cannot be used together");
    }
    let generated_at_ms = unix_timestamp_millis();
    let window = analysis_window(generated_at_ms, since, date)?;
    let (mut metadata, legacy_runs) = load_metadata(project_dir, &window)?;
    metadata.sort_by_key(|record| record.started_at_ms);

    let mut outcomes = OutcomeCounts::default();
    let mut roles: BTreeMap<String, RoleAccumulator> = BTreeMap::new();
    let mut failures_by_kind = BTreeMap::new();
    let mut worker_success = 0_u64;
    let mut worker_finished = 0_u64;
    let mut issues = Vec::new();

    for record in &metadata {
        record_outcome(&mut outcomes, record.outcome);
        let role = roles.entry(record.role.clone()).or_default();
        role.total += 1;
        record_outcome(&mut role.outcomes, record.outcome);
        if let Some(duration) = record.duration_ms {
            role.durations.push(duration);
        }
        if record.outcome == RunOutcome::Failure {
            *failures_by_kind
                .entry(
                    record
                        .failure_kind
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_owned(),
                )
                .or_insert(0) += 1;
        }
        if record.role == "worker"
            && matches!(record.outcome, RunOutcome::Success | RunOutcome::Failure)
        {
            worker_finished += 1;
            if record.outcome == RunOutcome::Success {
                worker_success += 1;
            }
        }
        if record.outcome != RunOutcome::Success {
            issues.push(run_issue(project_dir, record));
        }
    }

    let roles = roles
        .into_iter()
        .map(|(name, accumulator)| {
            (
                name,
                RoleStats {
                    total: accumulator.total,
                    outcomes: accumulator.outcomes,
                    duration_ms: summarize_durations(accumulator.durations),
                },
            )
        })
        .collect();

    Ok(AnalysisReport {
        generated_at_ms,
        window: window.clone(),
        recorded_runs: metadata.len() as u64,
        legacy_runs_without_metadata_all_time: legacy_runs,
        outcomes,
        worker_success_rate: (worker_finished > 0)
            .then_some(worker_success as f64 / worker_finished as f64),
        roles,
        failures_by_kind,
        activity: load_activity(project_dir, &window)?,
        issues,
        quality_caveat: QUALITY_CAVEAT,
    })
}

pub fn render_plain(report: &AnalysisReport) -> String {
    let mut lines = vec![
        format!("Goal analysis ({})", report.window.label),
        format!(
            "window: {} to {}",
            report.window.start_local, report.window.end_local
        ),
        format!(
            "runs: {} recorded, {} legacy without metadata (all time)",
            report.recorded_runs, report.legacy_runs_without_metadata_all_time
        ),
        format!(
            "outcomes: {} success, {} failure, {} cancelled, {} running",
            report.outcomes.success,
            report.outcomes.failure,
            report.outcomes.cancelled,
            report.outcomes.running
        ),
    ];
    match report.worker_success_rate {
        Some(rate) => lines.push(format!("worker success rate: {:.1}%", rate * 100.0)),
        None => lines.push("worker success rate: unavailable".to_owned()),
    }
    for (role, stats) in &report.roles {
        lines.push(format!(
            "{role}: {} runs; {} success, {} failure, {} cancelled, {} running; duration {}",
            stats.total,
            stats.outcomes.success,
            stats.outcomes.failure,
            stats.outcomes.cancelled,
            stats.outcomes.running,
            render_duration(&stats.duration_ms)
        ));
    }
    let decisions = report
        .activity
        .decisions
        .iter()
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!(
        "activity: senses {} success/{} failure; decisions {}; workers {} done/{} logical failure/{} run failure",
        report.activity.senses_succeeded,
        report.activity.senses_failed,
        if decisions.is_empty() { "none" } else { &decisions },
        report.activity.workers_done,
        report.activity.worker_logical_failures,
        report.activity.worker_run_failures
    ));
    lines.push(format!(
        "waits: {} decisions, {}s requested, {}s actual",
        report.activity.waits,
        report.activity.requested_wait_seconds,
        report.activity.actual_wait_seconds
    ));
    if !report.failures_by_kind.is_empty() {
        let failures = report
            .failures_by_kind
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("failure kinds: {failures}"));
    }
    lines.push(format!(
        "issues requiring inspection: {}",
        report.issues.len()
    ));
    for issue in &report.issues {
        let kind = issue.failure_kind.as_deref().unwrap_or("-");
        let reason = issue.reason.as_deref().unwrap_or("no recorded reason");
        lines.push(format!(
            "- {} {} [{}/{}] {}: {}",
            issue.started_at_local,
            issue.run_id,
            issue.role,
            kind,
            outcome_name(issue.outcome),
            reason
        ));
        lines.push(format!("  artifacts: {}", issue.artifacts_dir));
    }
    lines.push(format!("caveat: {}", report.quality_caveat));
    lines.join("\n") + "\n"
}

fn analysis_window(
    generated_at_ms: u64,
    since: Option<&str>,
    date: Option<&str>,
) -> Result<AnalysisWindow> {
    let now = Local
        .timestamp_millis_opt(generated_at_ms as i64)
        .single()
        .context("resolve current local time")?;
    if let Some(value) = since {
        let start_ms = generated_at_ms.saturating_sub(parse_duration(value)?);
        let start = Local
            .timestamp_millis_opt(start_ms as i64)
            .single()
            .context("resolve analysis start time")?;
        return Ok(AnalysisWindow {
            label: format!("last {value}"),
            start_ms,
            end_ms: generated_at_ms.saturating_add(1),
            start_local: start.to_rfc3339(),
            end_local: now.to_rfc3339(),
        });
    }

    let selected_date = match date {
        Some(value) => NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .with_context(|| format!("invalid date {value:?}; use YYYY-MM-DD"))?,
        None => now.date_naive(),
    };
    let next_date = selected_date
        .checked_add_days(Days::new(1))
        .context("analysis date is too large")?;
    let start = Local
        .from_local_datetime(&selected_date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .context("resolve start of local analysis date")?;
    let day_end = Local
        .from_local_datetime(&next_date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .context("resolve end of local analysis date")?;
    let is_today = selected_date == now.date_naive();
    let effective_end = if is_today { now } else { day_end };
    let end_ms = effective_end.timestamp_millis() as u64;
    Ok(AnalysisWindow {
        label: format!("local date {selected_date}"),
        start_ms: start.timestamp_millis() as u64,
        end_ms: if is_today {
            end_ms.saturating_add(1)
        } else {
            end_ms
        },
        start_local: start.to_rfc3339(),
        end_local: effective_end.to_rfc3339(),
    })
}

fn load_metadata(project_dir: &Path, window: &AnalysisWindow) -> Result<(Vec<RunMetadata>, u64)> {
    let runs_dir = project_dir.join(".goal/runs");
    let mut metadata = Vec::new();
    let mut legacy_runs = 0;
    match fs::read_dir(&runs_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("read {}", runs_dir.display()))?;
                if !entry.file_type()?.is_dir()
                    || entry.file_name().to_string_lossy().starts_with('.')
                {
                    continue;
                }
                let path = entry.path().join(METADATA_FILE);
                match fs::read(&path) {
                    Ok(bytes) => {
                        let record: RunMetadata = serde_json::from_slice(&bytes)
                            .with_context(|| format!("parse {}", path.display()))?;
                        if record.started_at_ms >= window.start_ms
                            && record.started_at_ms < window.end_ms
                        {
                            metadata.push(record);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        legacy_runs += 1;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", path.display()));
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", runs_dir.display())),
    }
    Ok((metadata, legacy_runs))
}

fn load_activity(project_dir: &Path, window: &AnalysisWindow) -> Result<ActivitySummary> {
    let path = project_dir.join(".goal/events.jsonl");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut activity = ActivitySummary::default();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), index + 1))?;
        let timestamp_ms = event.timestamp.saturating_mul(1_000);
        // Events are recorded only to whole-second precision. Count a boundary
        // second when its represented interval overlaps the millisecond window.
        if timestamp_ms.saturating_add(1_000) <= window.start_ms || timestamp_ms >= window.end_ms {
            continue;
        }
        match event.kind.as_str() {
            "sense_succeeded" => activity.senses_succeeded += 1,
            "sense_failed" => activity.senses_failed += 1,
            "decision" => {
                let kind = event
                    .details
                    .pointer("/action/type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                *activity.decisions.entry(kind.to_owned()).or_insert(0) += 1;
            }
            "worker_completed" => {
                activity.worker_completions += 1;
                match event
                    .details
                    .pointer("/completion/type")
                    .and_then(Value::as_str)
                {
                    Some("done") => activity.workers_done += 1,
                    Some("failure") => activity.worker_logical_failures += 1,
                    _ => {}
                }
            }
            "worker_failed" => activity.worker_run_failures += 1,
            "wait" => {
                activity.waits += 1;
                activity.requested_wait_seconds += event
                    .details
                    .get("requested_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                activity.actual_wait_seconds += event
                    .details
                    .get("actual_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            "failure" => activity.failure_events += 1,
            _ => {}
        }
    }
    Ok(activity)
}

fn run_issue(project_dir: &Path, record: &RunMetadata) -> RunIssue {
    let started_at_local = Local
        .timestamp_millis_opt(record.started_at_ms as i64)
        .single()
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| record.started_at_ms.to_string());
    RunIssue {
        run_id: record.run_id.clone(),
        role: record.role.clone(),
        started_at_ms: record.started_at_ms,
        started_at_local,
        duration_ms: record.duration_ms,
        outcome: record.outcome,
        failure_kind: record.failure_kind.clone(),
        result_type: record.result_type.clone(),
        reason: record.reason.clone(),
        artifacts_dir: project_dir
            .join(".goal/runs")
            .join(&record.run_id)
            .display()
            .to_string(),
    }
}

fn record_outcome(counts: &mut OutcomeCounts, outcome: RunOutcome) {
    match outcome {
        RunOutcome::Success => counts.success += 1,
        RunOutcome::Failure => counts.failure += 1,
        RunOutcome::Cancelled => counts.cancelled += 1,
        RunOutcome::Running => counts.running += 1,
    }
}

fn render_duration(stats: &DurationStats) -> String {
    match (stats.average, stats.p50, stats.p95) {
        (Some(average), Some(p50), Some(p95)) => {
            format!("avg {average:.0}ms, p50 {p50}ms, p95 {p95}ms")
        }
        _ => "unavailable".to_owned(),
    }
}

fn outcome_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Running => "running",
        RunOutcome::Success => "success",
        RunOutcome::Failure => "failure",
        RunOutcome::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_current_local_calendar_day() {
        let generated = Local
            .with_ymd_and_hms(2026, 8, 3, 21, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64;
        let window = analysis_window(generated, None, None).unwrap();
        assert_eq!(window.label, "local date 2026-08-03");
        assert!(window.start_local.contains("2026-08-03T00:00:00"));
        assert!(window.end_local.contains("2026-08-03T21:00:00"));
    }

    #[test]
    fn past_calendar_windows_have_an_exclusive_next_midnight_boundary() {
        let generated = Local
            .with_ymd_and_hms(2026, 8, 5, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64;
        let first = analysis_window(generated, None, Some("2026-08-03")).unwrap();
        let second = analysis_window(generated, None, Some("2026-08-04")).unwrap();
        assert_eq!(first.end_ms, second.start_ms);
    }

    #[test]
    fn reports_non_success_runs_and_wait_activity() {
        let dir = tempfile::tempdir().unwrap();
        let now = Local::now();
        let run_id = "worker-failure";
        let run_dir = dir.path().join(".goal/runs").join(run_id);
        fs::create_dir_all(&run_dir).unwrap();
        RunMetadata {
            schema_version: 1,
            run_id: run_id.into(),
            role: "worker".into(),
            started_at_ms: now.timestamp_millis() as u64,
            finished_at_ms: Some(now.timestamp_millis() as u64 + 10),
            duration_ms: Some(10),
            outcome: RunOutcome::Failure,
            failure_kind: Some("logical".into()),
            result_type: Some("failure".into()),
            reason: Some("external gate".into()),
        }
        .save(&run_dir.join(METADATA_FILE))
        .unwrap();
        fs::write(
            dir.path().join(".goal/events.jsonl"),
            format!(
                "{{\"timestamp\":{},\"type\":\"wait\",\"details\":{{\"requested_seconds\":300,\"actual_seconds\":60}}}}\n",
                now.timestamp()
            ),
        )
        .unwrap();

        let report = analyze(
            dir.path(),
            None,
            Some(&now.date_naive().format("%Y-%m-%d").to_string()),
        )
        .unwrap();
        assert_eq!(report.recorded_runs, 1);
        assert_eq!(report.outcomes.failure, 1);
        assert_eq!(report.issues[0].run_id, run_id);
        assert_eq!(report.activity.waits, 1);
        assert_eq!(report.activity.requested_wait_seconds, 300);
        assert_eq!(report.activity.actual_wait_seconds, 60);
    }

    #[test]
    fn rejects_overlapping_window_selectors() {
        let error = analyze(Path::new("."), Some("24h"), Some("2026-08-03")).unwrap_err();
        assert!(error.to_string().contains("cannot be used together"));
    }
}

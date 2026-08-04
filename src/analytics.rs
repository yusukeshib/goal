use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const METADATA_FILE: &str = "metadata.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Running,
    Success,
    Failure,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunMetadata {
    pub schema_version: u32,
    pub run_id: String,
    pub role: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub outcome: RunOutcome,
    pub failure_kind: Option<String>,
    pub result_type: Option<String>,
    pub reason: Option<String>,
}

impl RunMetadata {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes)?;
        if record.schema_version != 1 {
            bail!(
                "unsupported run metadata schema version {}",
                record.schema_version
            );
        }
        if record.run_id.trim().is_empty() || record.role.trim().is_empty() {
            bail!("run metadata must contain non-empty run_id and role");
        }
        Ok(record)
    }

    pub fn running(run_id: &str, role: &str, started_at_ms: u64) -> Self {
        Self {
            schema_version: 1,
            run_id: run_id.to_owned(),
            role: role.to_owned(),
            started_at_ms,
            finished_at_ms: None,
            duration_ms: None,
            outcome: RunOutcome::Running,
            failure_kind: None,
            result_type: None,
            reason: None,
        }
    }

    pub fn finished(
        run_id: &str,
        role: &str,
        started_at_ms: u64,
        outcome: RunOutcome,
        failure_kind: Option<&str>,
        result_type: Option<&str>,
        reason: Option<&str>,
    ) -> Self {
        let finished_at_ms = unix_timestamp_millis();
        Self {
            schema_version: 1,
            run_id: run_id.to_owned(),
            role: role.to_owned(),
            started_at_ms,
            finished_at_ms: Some(finished_at_ms),
            duration_ms: Some(finished_at_ms.saturating_sub(started_at_ms)),
            outcome,
            failure_kind: failure_kind.map(str::to_owned),
            result_type: result_type.map(str::to_owned),
            reason: reason.map(str::to_owned),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsReport {
    pub generated_at_ms: u64,
    pub since: Option<String>,
    pub recorded_runs: u64,
    pub legacy_runs_without_metadata_all_time: u64,
    pub outcomes: OutcomeCounts,
    pub worker_success_rate: Option<f64>,
    pub roles: BTreeMap<String, RoleStats>,
    pub failures_by_kind: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OutcomeCounts {
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
    pub running: u64,
}

impl OutcomeCounts {
    fn record(&mut self, outcome: RunOutcome) {
        match outcome {
            RunOutcome::Success => self.success += 1,
            RunOutcome::Failure => self.failure += 1,
            RunOutcome::Cancelled => self.cancelled += 1,
            RunOutcome::Running => self.running += 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RoleStats {
    pub total: u64,
    pub outcomes: OutcomeCounts,
    pub duration_ms: DurationStats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DurationStats {
    pub count: u64,
    pub average: Option<f64>,
    pub p50: Option<u64>,
    pub p95: Option<u64>,
}

#[derive(Default)]
struct RoleAccumulator {
    total: u64,
    outcomes: OutcomeCounts,
    durations: Vec<u64>,
}

pub fn stats(project_dir: &Path, since: Option<&str>) -> Result<StatsReport> {
    let generated_at_ms = unix_timestamp_millis();
    let cutoff_ms = match since {
        Some(value) => Some(generated_at_ms.saturating_sub(parse_duration(value)?)),
        None => None,
    };
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
                        let record = RunMetadata::from_slice(&bytes)
                            .with_context(|| format!("parse {}", path.display()))?;
                        if cutoff_ms.is_none_or(|cutoff| record.started_at_ms >= cutoff) {
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

    let mut outcomes = OutcomeCounts::default();
    let mut roles: BTreeMap<String, RoleAccumulator> = BTreeMap::new();
    let mut failures_by_kind = BTreeMap::new();
    let mut worker_success = 0_u64;
    let mut worker_finished = 0_u64;
    for record in &metadata {
        outcomes.record(record.outcome);
        let role = roles.entry(record.role.clone()).or_default();
        role.total += 1;
        role.outcomes.record(record.outcome);
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

    Ok(StatsReport {
        generated_at_ms,
        since: since.map(str::to_owned),
        recorded_runs: metadata.len() as u64,
        legacy_runs_without_metadata_all_time: legacy_runs,
        outcomes,
        worker_success_rate: (worker_finished > 0)
            .then_some(worker_success as f64 / worker_finished as f64),
        roles,
        failures_by_kind,
    })
}

pub fn render_plain(report: &StatsReport) -> String {
    let scope = report.since.as_deref().unwrap_or("all time");
    let mut lines = vec![
        format!("Goal statistics ({scope})"),
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
            "{role}: {} runs; {} success, {} failure; duration {}",
            stats.total,
            stats.outcomes.success,
            stats.outcomes.failure,
            render_duration(&stats.duration_ms)
        ));
    }
    if !report.failures_by_kind.is_empty() {
        let failures = report
            .failures_by_kind
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("failure kinds: {failures}"));
    }
    lines.join("\n") + "\n"
}

fn render_duration(stats: &DurationStats) -> String {
    match (stats.average, stats.p50, stats.p95) {
        (Some(average), Some(p50), Some(p95)) => {
            format!("avg {average:.0}ms, p50 {p50}ms, p95 {p95}ms")
        }
        _ => "unavailable".to_owned(),
    }
}

pub(crate) fn summarize_durations(mut values: Vec<u64>) -> DurationStats {
    if values.is_empty() {
        return DurationStats::default();
    }
    values.sort_unstable();
    let average = values.iter().map(|value| *value as f64).sum::<f64>() / values.len() as f64;
    DurationStats {
        count: values.len() as u64,
        average: Some(average),
        p50: Some(percentile(&values, 50)),
        p95: Some(percentile(&values, 95)),
    }
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * values.len()).div_ceil(100);
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

pub(crate) fn parse_duration(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    if amount.is_empty() || unit.is_empty() || unit.len() != 1 {
        bail!("invalid duration {value:?}; use values such as 30m, 24h, or 7d");
    }
    let amount: u64 = amount
        .parse()
        .with_context(|| format!("invalid duration {value:?}"))?;
    let multiplier = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => bail!("invalid duration unit in {value:?}; use s, m, h, d, or w"),
    };
    amount
        .checked_mul(multiplier)
        .with_context(|| format!("duration {value:?} is too large"))
}

pub fn unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn resolve_project_dir(config_or_dir: &Path) -> Result<PathBuf> {
    let config = if config_or_dir.is_dir() {
        config_or_dir.join("goal.toml")
    } else {
        config_or_dir.to_owned()
    };
    fs::canonicalize(&config).with_context(|| format!("resolve config {}", config.display()))?;
    let parent = config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent)
        .with_context(|| format!("resolve project directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(dir: &Path, metadata: RunMetadata) {
        let run = dir.join(".goal/runs").join(&metadata.run_id);
        fs::create_dir_all(&run).unwrap();
        metadata.save(&run.join(METADATA_FILE)).unwrap();
    }

    #[test]
    fn stats_group_outcomes_and_durations_without_inventing_legacy_data() {
        let dir = tempfile::tempdir().unwrap();
        let now = unix_timestamp_millis();
        let record = |id: &str, role: &str, outcome, duration, kind: Option<&str>| RunMetadata {
            schema_version: 1,
            run_id: id.into(),
            role: role.into(),
            started_at_ms: now - 1_000,
            finished_at_ms: Some(now),
            duration_ms: Some(duration),
            outcome,
            failure_kind: kind.map(str::to_owned),
            result_type: None,
            reason: None,
        };
        save(
            dir.path(),
            record("sensor-1", "sensor", RunOutcome::Success, 100, None),
        );
        save(
            dir.path(),
            record("worker-1", "worker", RunOutcome::Success, 200, None),
        );
        save(
            dir.path(),
            record(
                "worker-2",
                "worker",
                RunOutcome::Failure,
                400,
                Some("timeout"),
            ),
        );
        fs::create_dir_all(dir.path().join(".goal/runs/legacy")).unwrap();
        fs::create_dir_all(dir.path().join(".goal/runs/.partial.tmp-1")).unwrap();

        let report = stats(dir.path(), Some("24h")).unwrap();
        assert_eq!(report.recorded_runs, 3);
        assert_eq!(report.legacy_runs_without_metadata_all_time, 1);
        assert_eq!(report.outcomes.success, 2);
        assert_eq!(report.outcomes.failure, 1);
        assert_eq!(report.worker_success_rate, Some(0.5));
        assert_eq!(report.failures_by_kind["timeout"], 1);
        let worker = &report.roles["worker"];
        assert_eq!(worker.duration_ms.average, Some(300.0));
        assert_eq!(worker.duration_ms.p50, Some(200));
        assert_eq!(worker.duration_ms.p95, Some(400));
    }

    #[test]
    fn stats_filter_old_records_and_keep_incomplete_runs_without_duration() {
        let dir = tempfile::tempdir().unwrap();
        let now = unix_timestamp_millis();
        save(dir.path(), RunMetadata::running("running", "worker", now));
        save(
            dir.path(),
            RunMetadata {
                schema_version: 1,
                run_id: "old".into(),
                role: "worker".into(),
                started_at_ms: now - 2 * 86_400_000,
                finished_at_ms: Some(now - 2 * 86_400_000 + 100),
                duration_ms: Some(100),
                outcome: RunOutcome::Success,
                failure_kind: None,
                result_type: Some("done".into()),
                reason: None,
            },
        );

        let report = stats(dir.path(), Some("24h")).unwrap();
        assert_eq!(report.recorded_runs, 1);
        assert_eq!(report.outcomes.running, 1);
        assert_eq!(report.outcomes.success, 0);
        assert_eq!(report.roles["worker"].duration_ms.count, 0);
        assert_eq!(report.worker_success_rate, None);
    }

    #[test]
    fn malformed_or_unsupported_metadata_is_reported_instead_of_silently_skipped() {
        for contents in [
            "not json".to_owned(),
            serde_json::to_string(&RunMetadata {
                schema_version: 2,
                run_id: "future".into(),
                role: "worker".into(),
                started_at_ms: 0,
                finished_at_ms: None,
                duration_ms: None,
                outcome: RunOutcome::Running,
                failure_kind: None,
                result_type: None,
                reason: None,
            })
            .unwrap(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let run = dir.path().join(".goal/runs/broken");
            fs::create_dir_all(&run).unwrap();
            fs::write(run.join(METADATA_FILE), contents).unwrap();
            let error = stats(dir.path(), None).unwrap_err();
            assert!(error.to_string().contains("metadata.json"));
        }
    }

    #[test]
    fn since_parser_is_strict() {
        assert_eq!(parse_duration("24h").unwrap(), 86_400_000);
        assert!(parse_duration("24hours").is_err());
        assert!(parse_duration("h").is_err());
    }
}

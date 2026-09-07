//! Optional cumulative producer snapshots. Read once per selected artifact directory;
//! producers must atomically replace usage.json, never append streaming deltas.
use std::{collections::{BTreeMap, BTreeSet}, fs::OpenOptions, io::Read, path::Path};

use serde::{Deserialize, Serialize};

pub const USAGE_FILE: &str = "usage.json";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 128;
const MAX_LABEL_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    #[serde(default)]
    costs: Vec<Cost>,
    #[serde(default)]
    metrics: Vec<Metric>,
    #[serde(default)]
    complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    pub currency: String,
    pub amount: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub value: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageSummary {
    pub selected_runs: u64,
    pub reported_runs: u64,
    pub missing_runs: u64,
    pub invalid_runs: u64,
    pub partial_runs: u64,
    pub runs_with_reported_cost: u64,
    /// Empty means unknown, not a zero bill. Values are reported estimates only.
    pub costs: Vec<Cost>,
    pub metrics: Vec<Metric>,
    /// An overflowing key is omitted entirely, never silently truncated or zeroed.
    pub overflowed_currencies: Vec<String>,
    pub overflowed_metrics: Vec<(String, String)>,
}

fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_LABEL_BYTES
        && !value.chars().any(char::is_control)
}

fn load(path: &Path) -> Result<Option<Snapshot>, ()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A producer-controlled FIFO must not hang stats; do not follow symlinks.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !file.metadata().map_err(|_| ())?.is_file() { return Err(()); }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() as u64 > MAX_BYTES { return Err(()); }
    let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if snapshot.schema_version != 1 || snapshot.costs.len() > MAX_ENTRIES
        || snapshot.metrics.len() > MAX_ENTRIES { return Err(()); }
    let mut currencies = BTreeSet::new();
    let mut metrics = BTreeSet::new();
    for cost in &snapshot.costs {
        if !label(&cost.currency) || !cost.amount.is_finite() || cost.amount < 0.0
            || !currencies.insert(&cost.currency) { return Err(()); }
    }
    for metric in &snapshot.metrics {
        if !label(&metric.name) || !label(&metric.unit) || !metric.value.is_finite()
            || metric.value < 0.0 || !metrics.insert((&metric.name, &metric.unit)) {
            return Err(());
        }
    }
    Ok(Some(snapshot))
}

fn add<K: Ord + Clone>(totals: &mut BTreeMap<K, f64>, overflow: &mut BTreeSet<K>, key: K, value: f64) {
    if overflow.contains(&key) { return; }
    let total = totals.get(&key).copied().unwrap_or(0.0) + value;
    if total.is_finite() { totals.insert(key, total); }
    else { totals.remove(&key); overflow.insert(key); }
}

/// Pass actual artifact directories associated with window-selected metadata,
/// never paths synthesized from producer-controlled run_id. Repeated paths count once.
pub fn summarize<'a>(artifact_dirs: impl IntoIterator<Item = &'a Path>) -> UsageSummary {
    let mut summary = UsageSummary::default();
    let mut seen = BTreeSet::new();
    let mut costs = BTreeMap::new();
    let mut metrics = BTreeMap::new();
    let mut cost_overflow = BTreeSet::new();
    let mut metric_overflow = BTreeSet::new();
    for dir in artifact_dirs {
        if !seen.insert(dir.to_path_buf()) { continue; }
        summary.selected_runs += 1;
        let snapshot = match load(&dir.join(USAGE_FILE)) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => { summary.missing_runs += 1; continue; }
            Err(()) => { summary.invalid_runs += 1; continue; }
        };
        summary.reported_runs += 1;
        if !snapshot.complete { summary.partial_runs += 1; }
        if !snapshot.costs.is_empty() { summary.runs_with_reported_cost += 1; }
        for cost in snapshot.costs {
            add(&mut costs, &mut cost_overflow, cost.currency, cost.amount);
        }
        for metric in snapshot.metrics {
            add(&mut metrics, &mut metric_overflow, (metric.name, metric.unit), metric.value);
        }
    }
    summary.costs = costs.into_iter().map(|(currency, amount)| Cost { currency, amount }).collect();
    summary.metrics = metrics.into_iter().map(|((name, unit), value)| Metric { name, unit, value }).collect();
    summary.overflowed_currencies = cost_overflow.into_iter().collect();
    summary.overflowed_metrics = metric_overflow.into_iter().collect();
    summary
}

pub fn render_plain(summary: &UsageSummary) -> String {
    let mut lines = vec![format!(
        "reported usage: {}/{} runs; {} missing, {} invalid, {} partial; {} with reported cost",
        summary.reported_runs, summary.selected_runs, summary.missing_runs,
        summary.invalid_runs, summary.partial_runs, summary.runs_with_reported_cost,
    )];
    if summary.costs.is_empty() { lines.push("reported cost: unknown (no finite total available)".to_owned()); }
    for cost in &summary.costs { lines.push(format!("reported cost: {} {}", cost.amount, cost.currency)); }
    for metric in &summary.metrics { lines.push(format!("reported metric: {}={} {}", metric.name, metric.value, metric.unit)); }
    for currency in &summary.overflowed_currencies { lines.push(format!("reported cost: {currency} unavailable (overflow)")); }
    for (name, unit) in &summary.overflowed_metrics { lines.push(format!("reported metric: {name} ({unit}) unavailable (overflow)")); }
    lines.push("usage caveat: cumulative reported estimates, not an invoice; missing costs are unknown, not zero".to_owned());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(dir: &Path, costs: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(USAGE_FILE), format!(r#"{{"schema_version":1,"costs":{costs},"metrics":[]}}"#)).unwrap();
    }

    #[test]
    fn zero_missing_invalid_and_empty_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let zero = root.path().join("zero");
        let empty = root.path().join("empty");
        let invalid = root.path().join("invalid");
        let missing = root.path().join("missing");
        snapshot(&zero, r#"[{"currency":"USD","amount":0}]"#);
        snapshot(&empty, "[]");
        snapshot(&invalid, r#"[{"currency":"USD","amount":-1}]"#);
        let summary = summarize([zero.as_path(), empty.as_path(), invalid.as_path(), missing.as_path()]);
        assert_eq!(summary.reported_runs, 2);
        assert_eq!(summary.partial_runs, 2);
        assert_eq!(summary.missing_runs, 1);
        assert_eq!(summary.invalid_runs, 1);
        assert_eq!(summary.runs_with_reported_cost, 1);
        assert_eq!(summary.costs[0].amount, 0.0);
        assert!(summarize([empty.as_path()]).costs.is_empty());
    }

    #[test]
    fn selected_paths_only_and_repeated_cumulative_snapshots_count_once() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected");
        let excluded = root.path().join("excluded");
        snapshot(&selected, r#"[{"currency":"USD","amount":2}]"#);
        snapshot(&excluded, r#"[{"currency":"USD","amount":100}]"#);
        let summary = summarize([selected.as_path(), selected.as_path()]);
        assert_eq!(summary.selected_runs, 1);
        assert_eq!(summary.costs[0].amount, 2.0);
        snapshot(&selected, r#"[{"currency":"USD","amount":3}]"#);
        assert_eq!(summarize([selected.as_path()]).costs[0].amount, 3.0);
    }

    #[test]
    fn currencies_and_metric_units_are_never_combined() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        snapshot(&first, r#"[{"currency":"USD","amount":2}]"#);
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join(USAGE_FILE), r#"{"schema_version":1,"complete":true,"costs":[{"currency":"JPY","amount":100}],"metrics":[{"name":"time","unit":"seconds","value":3},{"name":"time","unit":"milliseconds","value":4}]}"#).unwrap();
        let summary = summarize([first.as_path(), second.as_path()]);
        assert_eq!(summary.costs.len(), 2);
        assert_eq!(summary.metrics.len(), 2);
        assert_eq!(summary.partial_runs, 1);
    }

    #[test]
    fn omitted_optional_arrays_and_malformed_reports() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(USAGE_FILE);
        std::fs::write(&file, r#"{"schema_version":1}"#).unwrap();
        assert_eq!(summarize([root.path()]).reported_runs, 1);
        for text in [r#"{"schema_version":2}"#.to_owned(), "{".to_owned(), " ".repeat(MAX_BYTES as usize + 1)] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(summarize([root.path()]).invalid_runs, 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_symlink_reports_are_invalid_without_blocking() {
        use std::{ffi::CString, os::unix::{ffi::OsStrExt, fs::symlink}};
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(USAGE_FILE);
        let name = CString::new(file.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(summarize([root.path()]).invalid_runs, 1);
        std::fs::remove_file(&file).unwrap();
        let target = root.path().join("target.json");
        std::fs::write(&target, r#"{"schema_version":1}"#).unwrap();
        symlink(&target, &file).unwrap();
        assert_eq!(summarize([root.path()]).invalid_runs, 1);
    }

    #[test]
    fn overflow_is_explicit_and_never_serializes_as_null() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        for dir in [&first, &second] {
            snapshot(dir, r#"[{"currency":"USD","amount":1.7e308}]"#);
        }
        let summary = summarize([first.as_path(), second.as_path()]);
        assert!(summary.costs.is_empty());
        assert_eq!(summary.overflowed_currencies, vec!["USD"]);
        assert!(!serde_json::to_string(&summary).unwrap().contains("null"));
    }
}

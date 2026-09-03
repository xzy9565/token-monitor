//! Read-only compatibility parser for the previous GUI/Node daily archive.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyDailyTotals {
    pub days: usize,
    pub rows: usize,
    pub tokens: i64,
    pub monitor_estimate_usd: f64,
    pub source: String,
}

pub fn read_daily_archive(
    path: &Path,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<LegacyDailyTotals, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    summarize_daily_archive_value(&root, path.to_string_lossy().as_ref(), since, until)
}

pub fn summarize_daily_archive_value(
    root: &Value,
    source: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<LegacyDailyTotals, String> {
    let mut days = BTreeMap::<String, Value>::new();
    for key in ["days", "liveDays"] {
        if let Some(entries) = root.get(key).and_then(Value::as_object) {
            for (date, value) in entries {
                // liveDays is the newer view of an overlapping date, so it
                // intentionally overwrites the historical copy.
                days.insert(date.clone(), value.clone());
            }
        }
    }
    let mut result = LegacyDailyTotals {
        source: source.to_owned(),
        ..Default::default()
    };
    for (date, day) in days {
        if since.is_some_and(|value| date.as_str() < value)
            || until.is_some_and(|value| date.as_str() > value)
        {
            continue;
        }
        result.days += 1;
        if let Some(observations) = day.get("observations").and_then(Value::as_object) {
            for observation in observations.values() {
                result.rows += 1;
                result.tokens = result.tokens.saturating_add(
                    number(observation.get("tokens")).unwrap_or(0.0).max(0.0) as i64,
                );
                result.monitor_estimate_usd +=
                    number(observation.get("cost")).unwrap_or(0.0).max(0.0);
            }
        }
    }
    Ok(result)
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_days_override_historical_days_and_filter_is_inclusive() {
        let path =
            std::env::temp_dir().join(format!("token-monitor-legacy-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"days":{"2026-08-31":{"observations":{"a":{"tokens":10,"cost":1}}},"2026-09-01":{"observations":{"b":{"tokens":20,"cost":2}}}},"liveDays":{"2026-08-31":{"observations":{"a":{"tokens":30,"cost":3}}}}}"#).unwrap();
        let totals = read_daily_archive(&path, Some("2026-08-31"), Some("2026-09-01")).unwrap();
        assert_eq!(totals.days, 2);
        assert_eq!(totals.rows, 2);
        assert_eq!(totals.tokens, 50);
        assert_eq!(totals.monitor_estimate_usd, 5.0);
        let _ = std::fs::remove_file(path);
    }
}

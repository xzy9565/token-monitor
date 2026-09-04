//! Read-only local usage adapter.
//!
//! Tokscale owns the filesystem discovery and session parsers. Token Monitor
//! deliberately converts the result into its own small schema before pricing
//! or storing it, so an upstream cost calculation cannot silently become our
//! subscription ledger.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::pricing::{PriceQuote, PricingEngine, PricingStatus};

pub const TOKSCALE_REVISION: &str = "029a1baf7b7e55dbca176f65b47e1537543f2857";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

impl UsageTokens {
    pub fn add_assign(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }

    /// The total that is safe to present before client-specific reasoning
    /// semantics are resolved. Some clients report reasoning as a subset of
    /// output, so callers must not blindly add `reasoning` a second time.
    pub fn reported_total_without_reasoning(&self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub session_id: String,
    pub date: String,
    pub timestamp: i64,
    pub tokens: UsageTokens,
    pub message_count: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub records: Vec<UsageRecord>,
    pub processing_time_ms: u32,
    pub tokscale_revision: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumptionSummary {
    pub total_tokens: i64,
    pub api_equivalent_usd: Option<f64>,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub exact_rows: usize,
    pub partial_rows: usize,
    pub unknown_rows: usize,
    pub monitor_estimate_usd: Option<f64>,
    pub actual_usd: Option<f64>,
    pub pricing_warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumptionReport {
    pub snapshot: UsageSnapshot,
    pub summary: ConsumptionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quotes: Option<Vec<PriceQuote>>,
}

pub fn build_consumption_report(
    snapshot: UsageSnapshot,
    pricing: &PricingEngine,
    include_quotes: bool,
) -> ConsumptionReport {
    build_consumption_report_with_legacy(snapshot, pricing, include_quotes, None)
}

pub fn build_consumption_report_with_legacy(
    snapshot: UsageSnapshot,
    pricing: &PricingEngine,
    include_quotes: bool,
    legacy: Option<&crate::legacy::LegacyDailyTotals>,
) -> ConsumptionReport {
    let mut api_equivalent = 0.0;
    let mut priced_tokens = 0i64;
    let mut unpriced_tokens = 0i64;
    let mut exact_rows = 0usize;
    let mut partial_rows = 0usize;
    let mut unknown_rows = 0usize;
    let mut warnings = Vec::new();
    let mut quotes = include_quotes.then(Vec::new);

    for record in &snapshot.records {
        let quote = pricing.quote(record);
        if let Some(value) = quote.value_usd {
            api_equivalent += value;
        }
        priced_tokens = priced_tokens.saturating_add(quote.priced_tokens);
        unpriced_tokens = unpriced_tokens.saturating_add(quote.unpriced_tokens);
        match quote.status {
            PricingStatus::Exact => exact_rows += 1,
            PricingStatus::Partial => partial_rows += 1,
            PricingStatus::Unknown => unknown_rows += 1,
        }
        for warning in &quote.warnings {
            if warnings.len() < 32 && !warnings.iter().any(|existing| existing == warning) {
                warnings.push(warning.clone());
            }
        }
        if let Some(all_quotes) = quotes.as_mut() {
            all_quotes.push(quote);
        }
    }

    let total_tokens = priced_tokens.saturating_add(unpriced_tokens);
    ConsumptionReport {
        snapshot,
        summary: ConsumptionSummary {
            total_tokens,
            api_equivalent_usd: (api_equivalent > 0.0).then_some(api_equivalent),
            priced_tokens,
            unpriced_tokens,
            exact_rows,
            partial_rows,
            unknown_rows,
            monitor_estimate_usd: legacy.map(|value| value.monitor_estimate_usd),
            actual_usd: None,
            pricing_warnings: warnings,
        },
        quotes,
    }
}

impl UsageSnapshot {
    pub fn total_tokens(&self) -> UsageTokens {
        self.records
            .iter()
            .fold(UsageTokens::default(), |mut total, record| {
                total.add_assign(&record.tokens);
                total
            })
    }

    pub fn clients(&self) -> Vec<String> {
        let mut values = self
            .records
            .iter()
            .map(|record| record.client.clone())
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        values
    }

    pub fn models(&self) -> Vec<String> {
        let mut values = self
            .records
            .iter()
            .map(|record| record.model_id.clone())
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        values
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageOptions {
    pub home_dir: Option<PathBuf>,
    pub clients: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub year: Option<String>,
}

/// Parse local session files through the pinned Tokscale Rust core.
///
/// This function is synchronous by design: the caller must run it in a
/// blocking worker when the interactive TUI is active.
pub fn collect_local_usage(options: UsageOptions) -> Result<UsageSnapshot, String> {
    let clients = options.clients.or_else(|| {
        let mut list: Vec<String> = tokscale_core::ClientId::iter()
            .map(|c| c.as_str().to_string())
            .collect();
        list.push("cursor".to_string());
        list.push("synthetic".to_string());
        Some(list)
    });

    let parsed = tokscale_core::parse_local_clients(tokscale_core::LocalParseOptions {
        home_dir: options
            .home_dir
            .map(|path| path.to_string_lossy().into_owned()),
        use_env_roots: true,
        clients,
        since: options.since.clone(),
        until: options.until,
        year: options.year,
        scanner_settings: tokscale_core::scanner::ScannerSettings::default(),
    })
    .map_err(|error| format!("tokscale local parse failed: {error}"))?;

    let mut records: Vec<UsageRecord> = parsed
        .messages
        .into_iter()
        .map(|message| UsageRecord {
            client: message.client,
            model_id: message.model_id,
            provider_id: message.provider_id,
            session_id: message.session_id,
            date: message.date,
            timestamp: message.timestamp,
            tokens: UsageTokens {
                input: message.input.max(0),
                output: message.output.max(0),
                cache_read: message.cache_read.max(0),
                cache_write: message.cache_write.max(0),
                reasoning: message.reasoning.max(0),
            },
            message_count: message.message_count.max(0),
        })
        .collect();

    collect_commandcode_v3_records(&mut records, options.since.as_deref());
    collect_cursor_cache_records(&mut records, options.since.as_deref());

    Ok(UsageSnapshot {
        records,
        processing_time_ms: parsed.processing_time_ms,
        tokscale_revision: TOKSCALE_REVISION.to_owned(),
    })
}

fn collect_cursor_cache_records(records: &mut Vec<UsageRecord>, since: Option<&str>) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let cursor_cache = home.join(".config/tokscale/cursor-cache");
    if !cursor_cache.exists() {
        return;
    }
    let has_cursor = records.iter().any(|r| r.client == "cursor");
    if has_cursor {
        return;
    }

    let Ok(entries) = std::fs::read_dir(cursor_cache) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".csv") {
            continue;
        }
        let msgs = tokscale_core::sessions::cursor::parse_cursor_file(&path);
        for m in msgs {
            if let Some(s) = since {
                if !m.date.is_empty() && m.date.as_str() < s {
                    continue;
                }
            }
            records.push(UsageRecord {
                client: "cursor".into(),
                model_id: m.model_id,
                provider_id: m.provider_id,
                session_id: m.session_id,
                date: m.date,
                timestamp: m.timestamp,
                tokens: UsageTokens {
                    input: m.tokens.input.max(0),
                    output: m.tokens.output.max(0),
                    cache_read: m.tokens.cache_read.max(0),
                    cache_write: m.tokens.cache_write.max(0),
                    reasoning: m.tokens.reasoning.max(0),
                },
                message_count: m.message_count.max(0),
            });
        }
    }
}

fn collect_commandcode_v3_records(records: &mut Vec<UsageRecord>, since: Option<&str>) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let projects_dir = home.join(".commandcode/projects");
    if !projects_dir.exists() {
        return;
    }
    let existing_ids: std::collections::HashSet<String> = records
        .iter()
        .filter(|r| r.client == "commandcode")
        .map(|r| r.session_id.clone())
        .collect();

    let Ok(project_entries) = std::fs::read_dir(projects_dir) else {
        return;
    };
    for p_entry in project_entries.flatten() {
        let p_path = p_entry.path();
        if !p_path.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&p_path) else {
            continue;
        };
        for f in files.flatten() {
            let f_path = f.path();
            let f_name = f_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !f_name.ends_with(".jsonl") || f_name.ends_with(".checkpoints.jsonl") {
                continue;
            }
            let session_id = f_name.trim_end_matches(".jsonl").to_string();
            if existing_ids.contains(&session_id) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&f_path) else {
                continue;
            };
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                    continue;
                };
                if val.get("type").and_then(|t| t.as_str()) != Some("message") {
                    continue;
                }
                let msg = val.get("message").unwrap_or(&serde_json::Value::Null);
                if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                    continue;
                }
                let usage = val.get("usage").or_else(|| msg.get("usage"));
                let raw_model = val
                    .get("model")
                    .or_else(|| msg.get("model"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");

                let (provider_id, model_id) = if let Some((p, m)) = raw_model.split_once('/') {
                    (p.to_ascii_lowercase(), m.to_string())
                } else {
                    ("commandcode".to_string(), raw_model.to_string())
                };

                let ts_str = val.get("timestamp").and_then(|t| t.as_str());
                let (timestamp, date) = if let Some(ts) = ts_str {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                        (dt.timestamp_millis(), dt.format("%Y-%m-%d").to_string())
                    } else {
                        (0, String::new())
                    }
                } else {
                    (0, String::new())
                };

                if let Some(s) = since {
                    if !date.is_empty() && date.as_str() < s {
                        continue;
                    }
                }

                let tokens = if let Some(u) = usage {
                    UsageTokens {
                        input: u.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0),
                        output: u.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0),
                        cache_read: u
                            .get("cacheReadTokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0),
                        cache_write: u
                            .get("cacheWriteTokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0),
                        reasoning: 0,
                    }
                } else {
                    UsageTokens::default()
                };

                records.push(UsageRecord {
                    client: "commandcode".into(),
                    model_id,
                    provider_id,
                    session_id: session_id.clone(),
                    date,
                    timestamp,
                    tokens,
                    message_count: 1,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_total_does_not_double_count_reasoning() {
        let tokens = UsageTokens {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write: 40,
            reasoning: 20,
        };
        assert_eq!(tokens.reported_total_without_reasoning(), 100);
    }

    #[test]
    fn snapshot_deduplicates_client_and_model_lists() {
        let snapshot = UsageSnapshot {
            records: vec![
                UsageRecord {
                    client: "codex".into(),
                    model_id: "gpt".into(),
                    provider_id: "openai".into(),
                    session_id: "a".into(),
                    date: "2026-09-01".into(),
                    timestamp: 0,
                    tokens: UsageTokens::default(),
                    message_count: 1,
                },
                UsageRecord {
                    client: "codex".into(),
                    model_id: "gpt".into(),
                    provider_id: "openai".into(),
                    session_id: "b".into(),
                    date: "2026-09-01".into(),
                    timestamp: 1,
                    tokens: UsageTokens::default(),
                    message_count: 1,
                },
            ],
            processing_time_ms: 1,
            tokscale_revision: TOKSCALE_REVISION.into(),
        };
        assert_eq!(snapshot.clients(), vec!["codex"]);
        assert_eq!(snapshot.models(), vec!["gpt"]);
    }

    #[test]
    fn report_keeps_usage_when_pricing_is_unknown() {
        let snapshot = UsageSnapshot {
            records: vec![UsageRecord {
                client: "codex".into(),
                model_id: "unknown/model".into(),
                provider_id: "unknown".into(),
                session_id: "s".into(),
                date: "2026-09-01".into(),
                timestamp: 0,
                tokens: UsageTokens {
                    input: 10,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 5,
                },
                message_count: 1,
            }],
            processing_time_ms: 1,
            tokscale_revision: TOKSCALE_REVISION.into(),
        };
        let report = build_consumption_report(snapshot, &PricingEngine::unavailable(), false);
        assert_eq!(report.summary.total_tokens, 30);
        assert_eq!(report.summary.unpriced_tokens, 30);
        assert_eq!(report.summary.unknown_rows, 1);
        assert!(report.summary.api_equivalent_usd.is_none());
    }
}

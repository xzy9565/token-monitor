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
    let parsed = tokscale_core::parse_local_clients(tokscale_core::LocalParseOptions {
        home_dir: options
            .home_dir
            .map(|path| path.to_string_lossy().into_owned()),
        use_env_roots: true,
        clients: options.clients,
        since: options.since,
        until: options.until,
        year: options.year,
        scanner_settings: tokscale_core::scanner::ScannerSettings::default(),
    })
    .map_err(|error| format!("tokscale local parse failed: {error}"))?;

    let records = parsed
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

    Ok(UsageSnapshot {
        records,
        processing_time_ms: parsed.processing_time_ms,
        tokscale_revision: TOKSCALE_REVISION.to_owned(),
    })
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

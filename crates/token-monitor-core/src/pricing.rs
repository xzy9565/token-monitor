//! Strict, auditable API-equivalent pricing.
//!
//! Tokscale supplies maintained pricing datasets and model lookup. This module
//! owns the acceptance policy and ledger contract: fuzzy/ambiguous matches do
//! not become confident dollar amounts, and Codex reasoning is not charged a
//! second time when it is already included in output.

use crate::usage::{UsageRecord, UsageTokens};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PricingStatus {
    Exact,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceQuote {
    pub model_id: String,
    pub provider_id: String,
    pub value_usd: Option<f64>,
    pub status: PricingStatus,
    pub source: Option<String>,
    pub matched_key: Option<String>,
    pub resolution: Option<String>,
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub warnings: Vec<String>,
}

pub struct PricingEngine {
    service: Option<tokscale_core::pricing::PricingService>,
    resolutions:
        Mutex<HashMap<(String, String), Option<tokscale_core::pricing::lookup::LookupResult>>>,
}

impl PricingEngine {
    pub fn unavailable() -> Self {
        Self {
            service: None,
            resolutions: Mutex::new(HashMap::new()),
        }
    }

    pub fn load_cached() -> Self {
        Self {
            service: tokscale_core::pricing::PricingService::load_cached_any_age(),
            resolutions: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_datasets(
        litellm: HashMap<String, tokscale_core::pricing::ModelPricing>,
        openrouter: HashMap<String, tokscale_core::pricing::ModelPricing>,
    ) -> Self {
        Self {
            service: Some(tokscale_core::pricing::PricingService::new(
                litellm, openrouter,
            )),
            resolutions: Mutex::new(HashMap::new()),
        }
    }

    pub fn has_pricing_data(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(|service| service.has_pricing_data())
    }

    pub fn quote(&self, record: &UsageRecord) -> PriceQuote {
        let tokens = safe_tokens(record);
        let total_tokens = tokens
            .input
            .saturating_add(tokens.cache_read)
            .saturating_add(tokens.cache_write)
            .saturating_add(tokens.output);
        let Some(service) = &self.service else {
            return unknown_quote(record, tokens, total_tokens, "pricing dataset unavailable");
        };
        let key = (record.provider_id.clone(), record.model_id.clone());
        let result = {
            let mut cache = self
                .resolutions
                .lock()
                .expect("pricing resolution cache poisoned");
            cache
                .entry(key)
                .or_insert_with(|| {
                    service.lookup_with_source_and_provider(
                        &record.model_id,
                        None,
                        Some(&record.provider_id),
                    )
                })
                .clone()
        };
        let Some(result) = result else {
            return unknown_quote(record, tokens, total_tokens, "model price unavailable");
        };

        let resolution = result.evidence.kind.as_str().to_owned();
        let strict_identity = result.evidence.exact_model_identity
            && result.evidence.price_consensus
            && result.evidence.is_submission_safe()
            && !matches!(
                result.evidence.kind,
                tok_scale_resolution::ResolutionKind::Fuzzy
                    | tok_scale_resolution::ResolutionKind::ModelPart
            );
        if !strict_identity {
            return PriceQuote {
                model_id: record.model_id.clone(),
                provider_id: record.provider_id.clone(),
                value_usd: None,
                status: PricingStatus::Unknown,
                source: Some(result.source.clone()),
                matched_key: Some(result.matched_key.clone()),
                resolution: Some(resolution),
                input_tokens: tokens.input,
                cache_read_tokens: tokens.cache_read,
                cache_write_tokens: tokens.cache_write,
                output_tokens: tokens.output,
                reasoning_tokens: tokens.reasoning,
                priced_tokens: 0,
                unpriced_tokens: total_tokens,
                warnings: vec!["model/provider match is not exact".into()],
            };
        }

        let (value, priced_tokens, mut warnings) = calculate_components(&result.pricing, &tokens);
        let complete = priced_tokens == total_tokens && warnings.is_empty();
        PriceQuote {
            model_id: record.model_id.clone(),
            provider_id: record.provider_id.clone(),
            value_usd: (priced_tokens > 0).then_some(value),
            status: if complete {
                PricingStatus::Exact
            } else {
                PricingStatus::Partial
            },
            source: Some(result.source),
            matched_key: Some(result.matched_key),
            resolution: Some(resolution),
            input_tokens: tokens.input,
            cache_read_tokens: tokens.cache_read,
            cache_write_tokens: tokens.cache_write,
            output_tokens: tokens.output,
            reasoning_tokens: tokens.reasoning,
            priced_tokens,
            unpriced_tokens: total_tokens.saturating_sub(priced_tokens),
            warnings: {
                if !complete && warnings.is_empty() {
                    warnings.push("one or more token component rates unavailable".into());
                }
                warnings
            },
        }
    }
}

mod tok_scale_resolution {
    pub use tokscale_core::pricing::lookup::ResolutionKind;
}

fn calculate_components(
    pricing: &tokscale_core::pricing::ModelPricing,
    tokens: &UsageTokens,
) -> (f64, i64, Vec<String>) {
    let components = [
        (tokens.input, pricing.input_cost_per_token, "input"),
        (
            tokens.cache_read,
            pricing.cache_read_input_token_cost,
            "cache read",
        ),
        (
            tokens.cache_write,
            pricing.cache_creation_input_token_cost,
            "cache write",
        ),
        (tokens.output, pricing.output_cost_per_token, "output"),
    ];
    let mut value = 0.0;
    let mut priced_tokens = 0i64;
    let mut warnings = Vec::new();
    for (amount, rate, label) in components {
        if amount <= 0 {
            continue;
        }
        if let Some(rate) = rate.filter(|rate| rate.is_finite() && *rate >= 0.0) {
            value += amount as f64 * rate;
            priced_tokens = priced_tokens.saturating_add(amount);
        } else {
            warnings.push(format!("{label} rate unavailable"));
        }
    }
    let has_long_context_tiers = pricing.input_cost_per_token_above_128k_tokens.is_some()
        || pricing.input_cost_per_token_above_200k_tokens.is_some()
        || pricing.input_cost_per_token_above_256k_tokens.is_some()
        || pricing.input_cost_per_token_above_272k_tokens.is_some()
        || pricing.output_cost_per_token_above_128k_tokens.is_some()
        || pricing.output_cost_per_token_above_200k_tokens.is_some()
        || pricing.output_cost_per_token_above_256k_tokens.is_some()
        || pricing.output_cost_per_token_above_272k_tokens.is_some();
    if has_long_context_tiers {
        warnings.push("long-context tier unavailable".into());
    }
    // Tokscale's ModelPricing fields are USD per token. The display layer may
    // format rates as USD per million, but conversion happens only when loading
    // human-facing price sheets.
    (value, priced_tokens, warnings)
}

fn safe_tokens(record: &UsageRecord) -> UsageTokens {
    let mut tokens = record.tokens.clone();
    // Current Codex records expose reasoning as a subset of output. Keep the
    // raw field in the audit schema but do not charge it twice.
    if record.client.eq_ignore_ascii_case("codex") {
        tokens.reasoning = 0;
    }
    tokens
}

fn unknown_quote(
    record: &UsageRecord,
    tokens: UsageTokens,
    total_tokens: i64,
    warning: &str,
) -> PriceQuote {
    PriceQuote {
        model_id: record.model_id.clone(),
        provider_id: record.provider_id.clone(),
        value_usd: None,
        status: PricingStatus::Unknown,
        source: None,
        matched_key: None,
        resolution: None,
        input_tokens: tokens.input,
        cache_read_tokens: tokens.cache_read,
        cache_write_tokens: tokens.cache_write,
        output_tokens: tokens.output,
        reasoning_tokens: tokens.reasoning,
        priced_tokens: 0,
        unpriced_tokens: total_tokens,
        warnings: vec![warning.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(client: &str, model: &str) -> UsageRecord {
        UsageRecord {
            client: client.into(),
            model_id: model.into(),
            provider_id: "openai".into(),
            session_id: "session".into(),
            date: "2026-09-01".into(),
            timestamp: 0,
            tokens: UsageTokens {
                input: 10,
                output: 20,
                cache_read: 30,
                cache_write: 40,
                reasoning: 20,
            },
            message_count: 1,
        }
    }

    fn engine() -> PricingEngine {
        let pricing = tokscale_core::pricing::ModelPricing {
            input_cost_per_token: Some(1e-6),
            output_cost_per_token: Some(2e-6),
            cache_read_input_token_cost: Some(0.5e-6),
            cache_creation_input_token_cost: Some(1.5e-6),
            ..Default::default()
        };
        let mut litellm = HashMap::new();
        litellm.insert("openai/test-model".into(), pricing);
        PricingEngine::from_datasets(litellm, HashMap::new())
    }

    #[test]
    fn unknown_models_fail_closed() {
        let quote = engine().quote(&record("claude", "openai/does-not-exist"));
        assert_eq!(quote.status, PricingStatus::Unknown);
        assert_eq!(quote.value_usd, None);
        assert_eq!(quote.unpriced_tokens, 100);
    }

    #[test]
    fn codex_reasoning_is_not_added_to_priced_token_total() {
        let quote = engine().quote(&record("codex", "openai/test-model"));
        assert_eq!(quote.reasoning_tokens, 0);
        assert_eq!(quote.priced_tokens, 100);
        assert!((quote.value_usd.unwrap() - 0.000125).abs() < 1e-12);
    }
}

//! Strict, auditable API-equivalent pricing.
//!
//! Tokscale supplies maintained pricing datasets and model lookup. This module
//! owns the acceptance policy and ledger contract: fuzzy/ambiguous matches do
//! not become confident dollar amounts, list prices (LiteLLM) outrank reseller
//! tiers (OpenRouter), and reasoning is charged only where it is logged apart
//! from output.

use crate::usage::{UsageRecord, UsageTokens};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PricingStatus {
    Exact,
    Partial,
    ProviderReported,
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
            .saturating_add(tokens.output)
            .saturating_add(tokens.reasoning);

        let Some(service) = &self.service else {
            if let Some(micros) = record.reported_cost_micros {
                return PriceQuote {
                    value_usd: Some(micros as f64 / 1e6),
                    status: PricingStatus::ProviderReported,
                    source: Some("provider-reported".into()),
                    priced_tokens: total_tokens,
                    unpriced_tokens: 0,
                    warnings: vec![],
                    ..unknown_quote(record, tokens, total_tokens, "")
                };
            }
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
                    let hints = canonical_provider_hints(&record.provider_id);
                    let candidates = canonical_model_candidates(&record.model_id, &record.provider_id);
                    let mut fallback = None;

                    for cand in &candidates {
                        // 1. Custom dataset overrides
                        let found =
                            service.lookup_with_source_and_provider(cand, Some("custom"), None);
                        if found.as_ref().is_some_and(is_strict) {
                            return found;
                        }

                        // 2. LiteLLM vendor list prices
                        for &hint in &hints {
                            let found =
                                service.lookup_with_source_and_provider(cand, Some("litellm"), hint);
                            if found.as_ref().is_some_and(is_strict) {
                                return found;
                            }
                        }

                        // 3. OpenRouter / reseller fallback
                        for &hint in &hints {
                            let found =
                                service.lookup_with_source_and_provider(cand, None, hint);
                            if found.as_ref().is_some_and(is_strict) {
                                return found;
                            }
                            if fallback.is_none() && cand == &record.model_id {
                                fallback = found;
                            }
                        }
                    }
                    fallback
                })
                .clone()
        };

        // Tier 1 & Tier 2: Strict match from canonical lookup pipeline
        if let Some(result) = result.as_ref().filter(|r| is_strict(r)) {
            let resolution = result.evidence.kind.as_str().to_owned();
            let (value, priced_tokens, mut warnings) = calculate_components(&result.pricing, &tokens);
            let complete = priced_tokens == total_tokens && warnings.is_empty();
            return PriceQuote {
                model_id: record.model_id.clone(),
                provider_id: record.provider_id.clone(),
                value_usd: (priced_tokens > 0).then_some(value),
                status: if complete {
                    PricingStatus::Exact
                } else {
                    PricingStatus::Partial
                },
                source: Some(result.source.clone()),
                matched_key: Some(result.matched_key.clone()),
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
            };
        }

        // Tier 3: Client/Provider-reported cost fallback
        if let Some(micros) = record.reported_cost_micros {
            return PriceQuote {
                value_usd: Some(micros as f64 / 1e6),
                status: PricingStatus::ProviderReported,
                source: Some("provider-reported".into()),
                priced_tokens: total_tokens,
                unpriced_tokens: 0,
                warnings: vec![],
                ..unknown_quote(record, tokens, total_tokens, "")
            };
        }

        // Tier 4: Fail closed as Unknown
        let (source, matched_key, resolution, warning) = if let Some(res) = result {
            (
                Some(res.source.clone()),
                Some(res.matched_key.clone()),
                Some(res.evidence.kind.as_str().to_owned()),
                "model/provider match is not exact",
            )
        } else {
            (None, None, None, "model price unavailable")
        };

        PriceQuote {
            model_id: record.model_id.clone(),
            provider_id: record.provider_id.clone(),
            value_usd: None,
            status: PricingStatus::Unknown,
            source,
            matched_key,
            resolution,
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
}

mod tok_scale_resolution {
    pub use tokscale_core::pricing::lookup::ResolutionKind;
}

const FLAVOR_SUFFIXES: &[&str] = &[
    "-max",
    "-thinking",
    "-abliterated",
    "-preview",
    "-latest",
    "-fast",
    "-turbo",
    "-xhigh",
    "-high",
    "-medium",
    "-low",
    ":free",
];

fn is_valid_calendar_date_suffix(s: &str) -> bool {
    if !s.starts_with('-') {
        return false;
    }
    let digits = &s[1..];
    if digits.len() == 8 && digits.chars().all(|c| c.is_ascii_digit()) {
        let year: u32 = digits[0..4].parse().unwrap_or(0);
        let month: u32 = digits[4..6].parse().unwrap_or(0);
        let day: u32 = digits[6..8].parse().unwrap_or(0);
        (2020..=2035).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day)
    } else if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
        let month: u32 = digits[0..2].parse().unwrap_or(0);
        let day: u32 = digits[2..4].parse().unwrap_or(0);
        (1..=12).contains(&month) && (1..=31).contains(&day)
    } else {
        false
    }
}

/// Uncollapse version punctuation: e.g. "v41" -> "v4.1", "v32" -> "v3.2"
fn uncollapse_version_punct(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut res = String::with_capacity(s.len() + 2);
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'v'
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && (i + 3 == bytes.len() || !bytes[i + 3].is_ascii_alphanumeric())
        {
            res.push('v');
            res.push(bytes[i + 1] as char);
            res.push('.');
            res.push(bytes[i + 2] as char);
            changed = true;
            i += 3;
        } else {
            res.push(bytes[i] as char);
            i += 1;
        }
    }
    if changed {
        Some(res)
    } else {
        None
    }
}

/// Swap dot and hyphen in version numbers (e.g. "3.5" <-> "3-5")
fn swap_version_dot_hyphen(s: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let bytes = s.as_bytes();
    let mut s1 = String::with_capacity(s.len());
    let mut changed1 = false;
    for i in 0..bytes.len() {
        if bytes[i] == b'.' && i > 0 && i + 1 < bytes.len() && bytes[i - 1].is_ascii_digit() && bytes[i + 1].is_ascii_digit() {
            s1.push('-');
            changed1 = true;
        } else {
            s1.push(bytes[i] as char);
        }
    }
    if changed1 {
        variants.push(s1);
    }
    let mut s2 = String::with_capacity(s.len());
    let mut changed2 = false;
    for i in 0..bytes.len() {
        if bytes[i] == b'-' && i > 0 && i + 1 < bytes.len() && bytes[i - 1].is_ascii_digit() && bytes[i + 1].is_ascii_digit() {
            s2.push('.');
            changed2 = true;
        } else {
            s2.push(bytes[i] as char);
        }
    }
    if changed2 {
        variants.push(s2);
    }
    variants
}

fn add_candidate(c: String, candidates: &mut Vec<String>, seen: &mut std::collections::HashSet<String>) {
    let trimmed = c.trim().to_string();
    if !trimmed.is_empty() && seen.insert(trimmed.clone()) {
        candidates.push(trimmed);
    }
}

fn expand_base_model(s: &str) -> Vec<String> {
    let mut list = vec![s.to_string()];
    if let Some(stripped) = s.strip_prefix("cursor-") {
        list.push(stripped.to_string());
    }
    let mut punct_variants = Vec::new();
    for v in &list {
        if let Some(uncollapsed) = uncollapse_version_punct(v) {
            punct_variants.push(uncollapsed);
        }
        punct_variants.extend(swap_version_dot_hyphen(v));
    }
    list.extend(punct_variants);
    list
}

pub fn canonical_model_candidates(model: &str, provider: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. Tier 1: Original unaltered model
    add_candidate(model.to_string(), &mut candidates, &mut seen);

    // 2. Lowercase
    let model_lower = model.to_ascii_lowercase();
    add_candidate(model_lower.clone(), &mut candidates, &mut seen);

    for v in expand_base_model(&model_lower) {
        add_candidate(v, &mut candidates, &mut seen);
    }

    // 3. Strip flavor / effort / execution modifiers
    let current_snapshot = candidates.clone();
    for v in current_snapshot {
        let mut curr = v.clone();
        loop {
            let mut stripped = false;
            for &suf in FLAVOR_SUFFIXES {
                if curr.ends_with(suf) {
                    curr.truncate(curr.len() - suf.len());
                    for exp in expand_base_model(&curr) {
                        add_candidate(exp, &mut candidates, &mut seen);
                    }
                    stripped = true;
                    break;
                }
            }
            if !stripped || curr.is_empty() {
                break;
            }
        }
    }

    // 4. Strip date suffixes (-YYYYMMDD, -MMDD)
    let current_snapshot = candidates.clone();
    for v in current_snapshot {
        if let Some(idx) = v.rfind('-') {
            let suffix = &v[idx..];
            if is_valid_calendar_date_suffix(suffix) {
                let base = &v[..idx];
                for exp in expand_base_model(base) {
                    add_candidate(exp, &mut candidates, &mut seen);
                }
            }
        }
    }

    // 5. Vendor defaults
    let prov = provider.to_ascii_lowercase();
    if prov == "google" || prov == "gemini" {
        for v in candidates.clone() {
            if !v.ends_with("-preview") {
                add_candidate(format!("{v}-preview"), &mut candidates, &mut seen);
            }
        }
    } else if prov == "anthropic" || prov == "claude" {
        for v in candidates.clone() {
            if !v.ends_with("-latest") {
                add_candidate(format!("{v}-latest"), &mut candidates, &mut seen);
            }
        }
    }

    candidates
}

fn canonical_provider_hints<'a>(provider: &'a str) -> Vec<Option<&'a str>> {
    let mut hints = Vec::new();
    match provider {
        "xiaomi" | "mimo" => {
            hints.push(Some("xiaomi_mimo"));
            hints.push(Some(provider));
        }
        "google" | "gemini" => {
            hints.push(Some("gemini"));
            hints.push(Some("google"));
        }
        "anthropic" | "claude" => {
            hints.push(Some("anthropic"));
            hints.push(Some("claude"));
        }
        "openai" | "codex" => {
            hints.push(Some("openai"));
            hints.push(Some("codex"));
        }
        "zhipu" | "zai" => {
            hints.push(Some("zhipu"));
            hints.push(Some("zai"));
        }
        "moonshot" | "kimi" => {
            hints.push(Some("moonshot"));
            hints.push(Some("kimi"));
        }
        "qwen" | "alibaba" | "dashscope" => {
            hints.push(Some("dashscope"));
            hints.push(Some("alibaba"));
            hints.push(Some("qwen"));
        }
        "deepseek" => {
            hints.push(Some("deepseek"));
        }
        "xai" | "grok" => {
            hints.push(Some("xai"));
            hints.push(Some("grok"));
        }
        "meta" | "llama" => {
            hints.push(Some("meta"));
            hints.push(Some("llama"));
        }
        _ => {
            hints.push(Some(provider));
        }
    }
    hints.push(None);
    hints
}

/// Only an exact, agreed, submission-safe model match becomes a dollar amount.
fn is_strict(result: &tokscale_core::pricing::lookup::LookupResult) -> bool {
    result.evidence.exact_model_identity
        && result.evidence.price_consensus
        && result.evidence.is_submission_safe()
        && !matches!(
            result.evidence.kind,
            tok_scale_resolution::ResolutionKind::Fuzzy
                | tok_scale_resolution::ResolutionKind::ModelPart
        )
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
        (tokens.reasoning, pricing.output_cost_per_token, "reasoning"),
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
    // AGY logs thinking apart from output (Gemini `thoughtsTokenCount`; its Claude rows put all
    // output there), and both vendors bill thinking at the output rate. Codex reports reasoning
    // as a subset of output, so it is never charged twice.
    // ponytail: Grok and OpenCode rows also show reasoning > output at times; verify, then bill.
    if !record.client.to_ascii_lowercase().starts_with("antigravity") {
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
            reported_cost_micros: None,
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
    fn provider_reported_cost_is_priced_as_is() {
        let mut rec = record("grokbot", "grok-bot-default");
        rec.reported_cost_micros = Some(197_022);
        let quote = engine().quote(&rec);
        assert_eq!(quote.status, PricingStatus::ProviderReported);
        assert!((quote.value_usd.unwrap() - 0.197022).abs() < 1e-12);
        assert_eq!(quote.unpriced_tokens, 0);
    }

    #[test]
    fn codex_reasoning_is_not_added_to_priced_token_total() {
        let quote = engine().quote(&record("codex", "openai/test-model"));
        assert_eq!(quote.reasoning_tokens, 0);
        assert_eq!(quote.priced_tokens, 100);
        assert!((quote.value_usd.unwrap() - 0.000125).abs() < 1e-12);
    }

    #[test]
    fn antigravity_thinking_is_billed_at_output_rate() {
        let quote = engine().quote(&record("antigravity-cli (a@b.c)", "openai/test-model"));
        assert_eq!(quote.status, PricingStatus::Exact);
        assert_eq!(quote.reasoning_tokens, 20);
        assert_eq!(quote.priced_tokens, 120);
        // The codex case above prices 100 tokens at $0.000125; 20 thinking tokens add 20 × $2/M.
        assert!((quote.value_usd.unwrap() - 0.000165).abs() < 1e-12);
    }

    #[test]
    fn litellm_list_price_beats_openrouter_discount_tier() {
        let price = |input: f64, output: f64| tokscale_core::pricing::ModelPricing {
            input_cost_per_token: Some(input),
            output_cost_per_token: Some(output),
            ..Default::default()
        };
        let mut litellm = HashMap::new();
        litellm.insert("gemini-3.8-flash".into(), price(0.75e-6, 3.75e-6));
        let mut openrouter = HashMap::new();
        // OpenRouter's first Google endpoint for this model is the 50% "flex" tier.
        openrouter.insert("google/gemini-3.8-flash".into(), price(0.375e-6, 1.875e-6));
        let engine = PricingEngine::from_datasets(litellm, openrouter);
        let mut rec = record("test", "gemini-3.8-flash");
        rec.provider_id = "google".into();
        rec.tokens = UsageTokens {
            input: 1_000_000,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        };
        let quote = engine.quote(&rec);
        assert_eq!(quote.source.as_deref(), Some("LiteLLM"));
        assert!((quote.value_usd.unwrap() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn claude_opus_and_sonnet_models_resolve_canonical_pricing() {
        let engine = PricingEngine::load_cached();
        if !engine.has_pricing_data() {
            return;
        }
        for (model, provider) in [
            ("claude-opus-5-5", "anthropic"),
            ("claude-sonnet-4-6", "anthropic"),
        ] {
            let rec = UsageRecord {
                client: "claude".into(),
                model_id: model.into(),
                provider_id: provider.into(),
                session_id: "s".into(),
                date: "2026-09-23".into(),
                timestamp: 0,
                tokens: UsageTokens {
                    input: 2,
                    output: 472,
                    cache_read: 0,
                    cache_write: 199678,
                    reasoning: 0,
                },
                message_count: 1,
                reported_cost_micros: None,
            };
            let quote = engine.quote(&rec);
            assert_eq!(
                quote.status,
                PricingStatus::Exact,
                "Model {} failed to resolve exact pricing: {:?}",
                model,
                quote.warnings
            );
            assert!(quote.value_usd.is_some());
            assert_eq!(quote.priced_tokens, 200152);
            assert_eq!(quote.unpriced_tokens, 0);
        }
    }

    #[test]
    fn fallback_to_unhinted_lookup_when_provider_hint_hits_reseller_key() {
        let canonical_pricing = tokscale_core::pricing::ModelPricing {
            input_cost_per_token: Some(4e-6),
            output_cost_per_token: Some(20e-6),
            cache_read_input_token_cost: Some(0.2e-6),
            cache_creation_input_token_cost: Some(5e-6),
            ..Default::default()
        };
        let bedrock_pricing = tokscale_core::pricing::ModelPricing {
            input_cost_per_token: Some(4e-6),
            output_cost_per_token: Some(20e-6),
            ..Default::default()
        };
        let mut litellm = HashMap::new();
        // Bedrock key has "anthropic" provider tag inside path, which tokscale matches on hint
        litellm.insert(
            "bedrock/us-gov-east-1/anthropic.claude-opus-5-5".into(),
            bedrock_pricing,
        );
        // Canonical key
        litellm.insert("claude-opus-5-5".into(), canonical_pricing);
        let engine = PricingEngine::from_datasets(litellm, HashMap::new());

        let rec = UsageRecord {
            client: "claude".into(),
            model_id: "claude-opus-5-5".into(),
            provider_id: "anthropic".into(),
            session_id: "s".into(),
            date: "2026-09-23".into(),
            timestamp: 0,
            tokens: UsageTokens {
                input: 10,
                output: 20,
                cache_read: 30,
                cache_write: 40,
                reasoning: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
        };

        let quote = engine.quote(&rec);
        assert_eq!(quote.status, PricingStatus::Exact);
        assert_eq!(quote.priced_tokens, 100);
        assert!(quote.value_usd.is_some());
    }

    #[test]
    fn deepseek_v41_flash_1017_max_normalizes_and_resolves_exact_pricing() {
        let engine = PricingEngine::load_cached();
        if !engine.has_pricing_data() {
            return;
        }
        let rec = UsageRecord {
            client: "cursor".into(),
            model_id: "deepseek-v41-flash-1017-max".into(),
            provider_id: "deepseek".into(),
            session_id: "s".into(),
            date: "2026-10-28".into(),
            timestamp: 0,
            tokens: UsageTokens {
                input: 130764,
                output: 56246,
                cache_read: 697344,
                cache_write: 0,
                reasoning: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
        };
        let quote = engine.quote(&rec);
        assert_eq!(quote.status, PricingStatus::Exact);
        assert_eq!(quote.matched_key.as_deref(), Some("deepseek/deepseek-v4.1-flash-1017"));
        assert_eq!(quote.priced_tokens, 884354);
        assert_eq!(quote.unpriced_tokens, 0);
        assert!(quote.value_usd.is_some());
        let val = quote.value_usd.unwrap();
        assert!((val - 0.055454232).abs() < 1e-6);
    }

    #[test]
    fn mimo_v2_6_pro_resolves_canonical_xiaomi_pricing() {
        let engine = PricingEngine::load_cached();
        if !engine.has_pricing_data() {
            return;
        }

        let rec = UsageRecord {
            client: "commandcode".into(),
            model_id: "mimo-v2.6-pro".into(),
            provider_id: "xiaomi".into(),
            session_id: "s".into(),
            date: "2026-09-27".into(),
            timestamp: 0,
            tokens: UsageTokens {
                input: 1_000_000,
                output: 100_000,
                cache_read: 2_000_000,
                cache_write: 0,
                reasoning: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
        };

        let quote = engine.quote(&rec);
        assert_eq!(
            quote.status,
            PricingStatus::Exact,
            "Expected exact pricing for mimo-v2.6-pro, got warnings: {:?}",
            quote.warnings
        );
        assert_eq!(quote.source.as_deref(), Some("LiteLLM"));
        assert_eq!(quote.matched_key.as_deref(), Some("xiaomi_mimo/mimo-v2.6-pro"));
        assert!(quote.value_usd.is_some());
        // Input: 1M * 4.35e-7 = $0.435
        // Output: 0.1M * 8.7e-7 = $0.087
        // Cache read: 2M * 3.6e-9 = $0.0072
        // Total = 0.435 + 0.087 + 0.0072 = $0.5292
        let val = quote.value_usd.unwrap();
        assert!((val - 0.5292).abs() < 1e-4, "Expected ~$0.5292, got ${val}");
    }
}



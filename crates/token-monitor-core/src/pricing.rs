//! Strict, auditable API-equivalent pricing.
//!
//! Tokscale supplies maintained pricing datasets and model lookup. This module
//! owns the acceptance policy and ledger contract: fuzzy/ambiguous matches do
//! not become confident dollar amounts, list prices (LiteLLM) outrank reseller
//! tiers (OpenRouter), and a price borrowed from another model name is never
//! labelled Exact. The arithmetic is tokscale's own `compute_cost`.

use crate::usage::{UsageRecord, UsageTokens};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    /// Each (provider, model)'s match, and whether its price belongs to another model name.
    resolutions: Mutex<
        HashMap<(String, String), Option<(tokscale_core::pricing::lookup::LookupResult, bool)>>,
    >,
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

    /// `load_cached`, kept for as long as tokscale's price files stay the same. A new engine has to
    /// resolve every model again, which took ~15% of a TUI ledger refresh.
    pub fn shared() -> Arc<Self> {
        type Stamps = Vec<(std::path::PathBuf, Option<(std::time::SystemTime, u64)>)>;
        static SHARED: Mutex<Option<(Stamps, Arc<PricingEngine>)>> = Mutex::new(None);
        let mut files: Vec<std::path::PathBuf> =
            std::fs::read_dir(tokscale_core::pricing::cache::get_cache_dir())
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("pricing-"))
                })
                .collect();
        files.push(tokscale_core::pricing::custom::CustomPricing::default_path());
        files.sort();
        let stamps: Stamps = files
            .into_iter()
            .map(|path| {
                let stamp = crate::usage::file_stamp(&path);
                (path, stamp)
            })
            .collect();
        let mut shared = SHARED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((seen, engine)) = shared.as_ref() {
            if *seen == stamps {
                return Arc::clone(engine);
            }
        }
        let engine = Arc::new(Self::load_cached());
        *shared = Some((stamps, Arc::clone(&engine)));
        engine
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
        let tokens = record.tokens.clone();
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
                            return found.map(|r| mark_borrowed(r, &record.model_id));
                        }

                        // 2. LiteLLM vendor list prices
                        for &hint in &hints {
                            let found =
                                service.lookup_with_source_and_provider(cand, Some("litellm"), hint);
                            if found.as_ref().is_some_and(is_strict) {
                                return found.map(|r| mark_borrowed(r, &record.model_id));
                            }
                        }

                        // 3. OpenRouter / reseller fallback
                        for &hint in &hints {
                            let found =
                                service.lookup_with_source_and_provider(cand, None, hint);
                            if found.as_ref().is_some_and(is_strict) {
                                return found.map(|r| mark_borrowed(r, &record.model_id));
                            }
                            if fallback.is_none() && cand == &record.model_id {
                                fallback = found;
                            }
                        }
                    }
                    fallback.map(|r| mark_borrowed(r, &record.model_id))
                })
                .clone()
        };

        // Tier 1 & Tier 2: Strict match from canonical lookup pipeline
        if let Some((result, borrowed)) = result.as_ref().filter(|(r, _)| is_strict(r)) {
            let resolution = result.evidence.kind.as_str().to_owned();
            // Cursor logs a whole agent loop as one request (rows reach 29M tokens), so its rows
            // are sums of requests too.
            let single_request = record.message_count <= 1 && record.client != "cursor";
            let (value, priced_tokens, mut warnings) =
                calculate_components(&result.pricing, &tokens, single_request);
            if *borrowed {
                warnings.push(format!(
                    "priced as {}: no published price for {}",
                    result.matched_key, record.model_id
                ));
            }
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
        let (source, matched_key, resolution, warning) = if let Some((res, _)) = result {
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

/// Suffixes that name the same model at the same price: a reasoning effort, or an alias.
const NEUTRAL_SUFFIXES: &[&str] = &["-thinking", "-latest", "-xhigh", "-high", "-medium", "-low"];

/// Whether two model ids differ only in provider prefix, case, version punctuation, or a neutral
/// suffix. Any other difference (`-fast`, `-max`, a dropped date, an eroded `-n`) means one model is
/// priced at another's rates.
fn same_model(a: &str, b: &str) -> bool {
    let spellings = |id: &str| {
        let mut id = id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
        let mut forms = expand_base_model(&id);
        while let Some(base) = NEUTRAL_SUFFIXES.iter().find_map(|s| id.strip_suffix(s)) {
            id = base.to_owned();
            forms.extend(expand_base_model(&id));
        }
        forms
    };
    let a = spellings(a);
    spellings(b).iter().any(|form| a.contains(form))
}

/// The match, and whether its price was borrowed from another model name. tokscale's own aliases
/// are curated, so they count as the same model unless it also eroded the id.
fn mark_borrowed(
    result: tokscale_core::pricing::lookup::LookupResult,
    model: &str,
) -> (tokscale_core::pricing::lookup::LookupResult, bool) {
    let curated_alias = result.evidence.alias_applied && !result.evidence.stripped;
    let borrowed = !curated_alias && !same_model(&result.matched_key, model);
    (result, borrowed)
}

/// The long-context card a single request falls under: the highest threshold its prompt passes
/// that publishes an input rate. Vendors bill such a request whole at that card (OpenAI past 272k,
/// Google and Anthropic past 200k, Haiku 5.5 past 100k), not only the tokens past the threshold.
/// A bucket the card leaves out follows the input rate's multiplier, as tokscale prices OpenAI's
/// cache writes; output keeps its base rate.
fn long_context_card(
    p: &tokscale_core::pricing::ModelPricing,
    prompt_tokens: i64,
) -> Option<tokscale_core::pricing::ModelPricing> {
    let cards = [
        (
            272_000,
            p.input_cost_per_token_above_272k_tokens,
            p.output_cost_per_token_above_272k_tokens,
            p.cache_read_input_token_cost_above_272k_tokens,
            None,
            None,
        ),
        (
            256_000,
            p.input_cost_per_token_above_256k_tokens,
            p.output_cost_per_token_above_256k_tokens,
            None,
            None,
            None,
        ),
        (
            200_000,
            p.input_cost_per_token_above_200k_tokens,
            p.output_cost_per_token_above_200k_tokens,
            p.cache_read_input_token_cost_above_200k_tokens,
            p.cache_creation_input_token_cost_above_200k_tokens,
            p.cache_creation_input_token_cost_above_1hr_above_200k_tokens,
        ),
        (
            128_000,
            p.input_cost_per_token_above_128k_tokens,
            p.output_cost_per_token_above_128k_tokens,
            None,
            None,
            None,
        ),
        (
            100_000,
            p.input_cost_per_token_above_100k_tokens,
            p.output_cost_per_token_above_100k_tokens,
            p.cache_read_input_token_cost_above_100k_tokens,
            p.cache_creation_input_token_cost_above_100k_tokens,
            p.cache_creation_input_token_cost_above_1hr_above_100k_tokens,
        ),
    ];
    let (_, input, output, cache_read, cache_write, cache_write_1h) = cards
        .into_iter()
        .find(|&(threshold, input, ..)| prompt_tokens > threshold && input.is_some())?;
    let scaled =
        |base: Option<f64>| Some(base? * input? / p.input_cost_per_token.filter(|b| *b > 0.0)?);
    Some(tokscale_core::pricing::ModelPricing {
        input_cost_per_token: input,
        output_cost_per_token: output.or(p.output_cost_per_token),
        cache_read_input_token_cost: cache_read.or_else(|| scaled(p.cache_read_input_token_cost)),
        cache_creation_input_token_cost: cache_write
            .or_else(|| scaled(p.cache_creation_input_token_cost)),
        cache_creation_input_token_cost_above_1hr: cache_write_1h
            .or_else(|| scaled(p.cache_creation_input_token_cost_above_1hr)),
        ..Default::default()
    })
}

fn calculate_components(
    pricing: &tokscale_core::pricing::ModelPricing,
    tokens: &UsageTokens,
    single_request: bool,
) -> (f64, i64, Vec<String>) {
    // A row summing several requests can't tell whether any of them crossed a long-context
    // threshold, so it stays on the base rates, as tokscale prices its multi-call rows.
    let prompt_tokens = tokens
        .input
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write);
    let adjusted = if single_request {
        long_context_card(pricing, prompt_tokens)
    } else {
        Some(tokscale_core::pricing::ModelPricing {
            input_cost_per_token: pricing.input_cost_per_token,
            output_cost_per_token: pricing.output_cost_per_token,
            cache_read_input_token_cost: pricing.cache_read_input_token_cost,
            cache_creation_input_token_cost: pricing.cache_creation_input_token_cost,
            cache_creation_input_token_cost_above_1hr: pricing
                .cache_creation_input_token_cost_above_1hr,
            ..Default::default()
        })
    };
    let pricing = adjusted.as_ref().unwrap_or(pricing);
    // tokscale's arithmetic: 1-hour cache writes at their own rate, and reasoning (which tokscale
    // logs apart from output) at the output rate.
    let value = tokscale_core::pricing::lookup::compute_cost(
        pricing,
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write,
        tokens.cache_write_1h,
        tokens.reasoning,
    );
    // compute_cost charges a missing rate $0; those tokens are unpriced, not free.
    let mut priced_tokens = 0i64;
    let mut warnings = Vec::new();
    for (amount, rate, label) in [
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
    ] {
        if amount <= 0 {
            continue;
        }
        if rate.is_some_and(|rate| rate.is_finite() && rate >= 0.0) {
            priced_tokens = priced_tokens.saturating_add(amount);
        } else {
            warnings.push(format!("{label} rate unavailable"));
        }
    }
    if tokens.cache_write_1h > 0 && pricing.cache_creation_input_token_cost_above_1hr.is_none() {
        warnings.push("1-hour cache write rate unavailable, billed at the 5-minute rate".into());
    }
    (value, priced_tokens, warnings)
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
                cache_write_1h: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
            dedup_id: None,
        }
    }

    fn test_pricing() -> tokscale_core::pricing::ModelPricing {
        tokscale_core::pricing::ModelPricing {
            input_cost_per_token: Some(1e-6),
            output_cost_per_token: Some(2e-6),
            cache_read_input_token_cost: Some(0.5e-6),
            cache_creation_input_token_cost: Some(1.5e-6),
            ..Default::default()
        }
    }

    fn engine_with(pricing: tokscale_core::pricing::ModelPricing) -> PricingEngine {
        let mut litellm = HashMap::new();
        litellm.insert("openai/test-model".into(), pricing);
        PricingEngine::from_datasets(litellm, HashMap::new())
    }

    fn engine() -> PricingEngine {
        engine_with(test_pricing())
    }

    #[test]
    fn unknown_models_fail_closed() {
        let quote = engine().quote(&record("claude", "openai/does-not-exist"));
        assert_eq!(quote.status, PricingStatus::Unknown);
        assert_eq!(quote.value_usd, None);
        assert_eq!(quote.unpriced_tokens, 120);
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
    fn reasoning_is_billed_at_the_output_rate_for_every_client() {
        for client in ["codex", "antigravity-cli (a@b.c)"] {
            let quote = engine().quote(&record(client, "openai/test-model"));
            assert_eq!(quote.status, PricingStatus::Exact);
            assert_eq!(quote.priced_tokens, 120);
            // 10 input, 30 cache-read, 40 cache-write, 20 output and 20 reasoning tokens at
            // $1, $0.5, $1.5, $2 and $2 per million.
            assert!(
                (quote.value_usd.unwrap() - 0.000165).abs() < 1e-12,
                "{client}"
            );
        }
    }

    #[test]
    fn one_hour_cache_writes_are_billed_at_their_own_rate() {
        let mut rec = record("claude", "openai/test-model");
        rec.tokens.cache_write_1h = 30;
        let quote = engine_with(tokscale_core::pricing::ModelPricing {
            cache_creation_input_token_cost_above_1hr: Some(2e-6),
            ..test_pricing()
        })
        .quote(&rec);
        assert_eq!(quote.status, PricingStatus::Exact);
        // 30 of the 40 cache-write tokens move from the 5-minute $1.5/M to the 1-hour $2/M.
        assert!((quote.value_usd.unwrap() - 0.00018).abs() < 1e-12);
        let without_hourly_rate = engine().quote(&rec);
        assert_eq!(without_hourly_rate.status, PricingStatus::Partial);
    }

    #[test]
    fn long_context_tiers_apply_past_their_threshold_only() {
        let engine = engine_with(tokscale_core::pricing::ModelPricing {
            input_cost_per_token_above_200k_tokens: Some(2e-6),
            ..test_pricing()
        });
        let short = engine.quote(&record("codex", "openai/test-model"));
        assert_eq!(short.status, PricingStatus::Exact);
        assert!((short.value_usd.unwrap() - 0.000165).abs() < 1e-12);

        let mut long = record("codex", "openai/test-model");
        long.tokens.input = 300_000;
        let quote = engine.quote(&long);
        assert_eq!(quote.status, PricingStatus::Exact);
        // Past 200k the whole request moves to the long-context card: all 300k input tokens at
        // $2/M, cache reads and writes at twice their rates, output on its base rate.
        assert!((quote.value_usd.unwrap() - 0.60023).abs() < 1e-9);
        long.message_count = 3;
        // A row summing several requests stays on the base rates: 300k at $1/M.
        assert!((engine.quote(&long).value_usd.unwrap() - 0.300155).abs() < 1e-9);
        long.message_count = 1;
        long.client = "cursor".into();
        assert!((engine.quote(&long).value_usd.unwrap() - 0.300155).abs() < 1e-9);
    }

    #[test]
    fn a_price_borrowed_from_another_model_name_is_not_exact() {
        let borrowed = engine().quote(&record("codex", "openai/test-model-fast"));
        assert_eq!(borrowed.matched_key.as_deref(), Some("openai/test-model"));
        assert_eq!(borrowed.status, PricingStatus::Partial);
        assert!(borrowed.value_usd.is_some());
        let effort = engine().quote(&record("codex", "openai/test-model-high"));
        assert_eq!(
            effort.status,
            PricingStatus::Exact,
            "a reasoning effort is the same model"
        );
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
            cache_write_1h: 0,
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
                    cache_write_1h: 0,
                },
                message_count: 1,
                reported_cost_micros: None,
                dedup_id: None,
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
                cache_write_1h: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
            dedup_id: None,
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
                cache_write_1h: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
            dedup_id: None,
        };
        let quote = engine.quote(&rec);
        // Finding it took dropping `-max`, so the price is borrowed, not Exact.
        assert_eq!(quote.status, PricingStatus::Partial);
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
                cache_write_1h: 0,
            },
            message_count: 1,
            reported_cost_micros: None,
            dedup_id: None,
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



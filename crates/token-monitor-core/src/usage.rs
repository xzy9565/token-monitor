//! Read-only local usage adapter.
//!
//! Tokscale owns the filesystem discovery and session parsers. Token Monitor
//! deliberately converts the result into its own small schema before pricing
//! or storing it, so an upstream cost calculation cannot silently become our
//! subscription ledger.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use crate::pricing::{PriceQuote, PricingEngine, PricingStatus};

pub const TOKSCALE_REVISION: &str = "d4d1c751856e25913bce97bfbd7b254308863239";

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
    /// What the provider billed for this row, in millionths of a dollar. Set only where no list
    /// price can exist (Cursor's Grok Bot rows name a task type, not a model); priced as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_cost_micros: Option<i64>,
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

    // The date window is applied after enrichment below: until then an AGY turn may still carry
    // its session's start date, and filtering here would drop a long-running session whole.
    let parsed = tokscale_core::parse_local_clients(tokscale_core::LocalParseOptions {
        home_dir: options
            .home_dir
            .map(|path| path.to_string_lossy().into_owned()),
        use_env_roots: true,
        clients,
        since: None,
        until: None,
        year: options.year,
        scanner_settings: tokscale_core::scanner::ScannerSettings::default(),
    })
    .map_err(|error| format!("tokscale local parse failed: {error}"))?;

    let mut records: Vec<UsageRecord> = parsed
        .messages
        .into_iter()
        .map(|message| {
            let is_commandcode = message.client == "commandcode";
            UsageRecord {
                client: message.client,
                model_id: message.model_id,
                provider_id: message.provider_id,
                session_id: message.session_id,
                date: message.date,
                timestamp: message.timestamp,
                tokens: {
                    let input = if is_commandcode {
                        message.input.saturating_sub(message.cache_read).max(0)
                    } else {
                        message.input.max(0)
                    };
                    UsageTokens {
                        input,
                        output: message.output.max(0),
                        cache_read: message.cache_read.max(0),
                        cache_write: message.cache_write.max(0),
                        reasoning: message.reasoning.max(0),
                    }
                },
                message_count: message.message_count.max(0),
                reported_cost_micros: None,
            }
        })
        .collect();

    collect_commandcode_v3_records(&mut records, options.since.as_deref());
    collect_cursor_cache_records(&mut records, options.since.as_deref());
    let inherited = rebuild_forked_antigravity_sessions(&mut records);
    enrich_antigravity_records(&mut records, &inherited);
    tag_antigravity_account_records(&mut records);
    // Same string comparison tokscale applies to `date`.
    records.retain(|r| {
        options.since.as_deref().is_none_or(|s| r.date.as_str() >= s)
            && options.until.as_deref().is_none_or(|u| r.date.as_str() <= u)
    });

    Ok(UsageSnapshot {
        records,
        processing_time_ms: parsed.processing_time_ms,
        tokscale_revision: TOKSCALE_REVISION.to_owned(),
    })
}

fn infer_cursor_model_provider(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude")
        || lower.contains("sonnet")
        || lower.contains("opus")
        || lower.contains("haiku")
    {
        "anthropic"
    } else if lower.contains("gpt")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
    {
        "openai"
    } else if lower.contains("gemini") {
        "google"
    } else if lower.contains("grok") {
        "xai"
    } else if lower.contains("deepseek") {
        "deepseek"
    } else {
        "cursor"
    }
}

fn parse_cursor_json(content: &str, records: &mut Vec<UsageRecord>, since: Option<&str>) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };
    let Some(events) = val.get("usageEventsDisplay").and_then(|v| v.as_array()) else {
        return;
    };

    // Pre-scan: Identify conversation IDs that belong to Grok Bot.
    // Any session containing "grok-bot-*" models is an agent conversation on the Sand cloud machine.
    let mut grok_bot_sessions = std::collections::HashSet::new();
    for event in events {
        if let Some(m) = event.get("model").and_then(|v| v.as_str()) {
            if m.starts_with("grok-bot") {
                if let Some(cid) = event.get("conversationId").and_then(|v| v.as_str()) {
                    let trimmed = cid.trim();
                    if !trimmed.is_empty() {
                        grok_bot_sessions.insert(trimmed.to_string());
                    }
                }
            }
        }
    }

    for event in events {
        let model = event
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if model.is_empty() {
            continue;
        }

        let timestamp: i64 = match event.get("timestamp") {
            Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        if timestamp <= 0 {
            continue;
        }

        let date = chrono::DateTime::from_timestamp_millis(timestamp)
            .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();

        if let Some(s) = since {
            if !date.is_empty() && date.as_str() < s {
                continue;
            }
        }

        let provider_id = infer_cursor_model_provider(model).to_string();

        let raw_cid = event
            .get("conversationId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let session_id = if !raw_cid.is_empty() {
            format!("cursor-{}", raw_cid)
        } else {
            format!("cursor-{}", timestamp)
        };

        let tokens = if let Some(tu) = event.get("tokenUsage").and_then(|u| u.as_object()) {
            UsageTokens {
                input: tu.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
                output: tu.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
                cache_read: tu.get("cacheReadTokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
                cache_write: tu.get("cacheWriteTokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
                reasoning: 0,
            }
        } else {
            UsageTokens::default()
        };

        // Grok Bot has its own weekly pool, so it gets its own ledger row.
        // Attribute to grokbot if the model is a grok-bot task type OR if the conversation belongs to Grok Bot.
        let is_grok_bot = model.starts_with("grok-bot")
            || (!raw_cid.is_empty() && grok_bot_sessions.contains(raw_cid));

        let reported_cost_micros = model
            .starts_with("grok-bot")
            .then(|| event.get("chargedCents").and_then(|v| v.as_f64()))
            .flatten()
            .map(|cents| (cents * 10_000.0).round() as i64);

        records.push(UsageRecord {
            client: if is_grok_bot { "grokbot" } else { "cursor" }.into(),
            model_id: model.to_string(),
            provider_id,
            session_id,
            date,
            timestamp,
            tokens,
            message_count: 1,
            reported_cost_micros,
        });
    }
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

    let mut cursor_records = Vec::new();
    let usage_json = cursor_cache.join("usage.json");
    if usage_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&usage_json) {
            parse_cursor_json(&content, &mut cursor_records, since);
        }
    }

    let mut seen: std::collections::HashSet<(i64, String)> = cursor_records
        .iter()
        .map(|r| (r.timestamp, r.model_id.clone()))
        .collect();

    let Ok(entries) = std::fs::read_dir(cursor_cache) else {
        records.extend(cursor_records);
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
            if !seen.insert((m.timestamp, m.model_id.clone())) {
                continue;
            }
            cursor_records.push(UsageRecord {
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
                reported_cost_micros: None,
            });
        }
    }

    records.extend(cursor_records);
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
                    let raw_input = u.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    let cache_read = u
                        .get("cacheReadTokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let cache_write = u
                        .get("cacheWriteTokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let output = u.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    // In Command Code, `inputTokens` is total prompt tokens, which subsumes `cacheReadTokens`.
                    // We must deduct cached tokens so that only uncached tokens are billed at full input rates.
                    let uncached_input = raw_input.saturating_sub(cache_read).max(0);
                    UsageTokens {
                        input: uncached_input,
                        output,
                        cache_read,
                        cache_write,
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
                    reported_cost_micros: None,
                });
            }
        }
    }
}

/// AGY log lines that mean "this process is driving that conversation": the user sent a
/// message or approved a tool, or the process created, forked, or print-ran it.
const AGY_DRIVER_MARKERS: [&str; 5] = [
    "Forwarding user message to conversation ",
    "Tool confirmation for conversation ",
    "Created conversation ",
    "Forked conversation ",
    "Print mode: conversation=",
];
/// A call can land in the conversation DB shortly after its process's last log line.
const AGY_LOG_SLACK_MS: i64 = 120_000;

/// One AGY CLI process, parsed from its own log `~/.gemini/antigravity-cli/log/cli-*.log`.
#[derive(Debug, Default)]
struct AgyProcessLog {
    email: Option<String>,
    last_ms: i64,
    /// (timestamp, conversation id) of every driver line.
    events: Vec<(i64, String)>,
}

fn parse_agy_process_log(year: i32, content: &str) -> AgyProcessLog {
    let stamp = |line: &str| glog_timestamp_ms(year, line);
    let mut log = AgyProcessLog {
        last_ms: content.lines().rev().find_map(stamp).unwrap_or_default(),
        ..AgyProcessLog::default()
    };
    for line in lines_containing(content, "applyAuthResult: email=") {
        if let Some((_, rest)) = line.split_once("applyAuthResult: email=") {
            if stamp(line).is_some() {
                log.email = rest.split([',', ' ']).next().map(str::to_owned);
            }
        }
    }
    // Every driver marker names a conversation.
    for line in lines_containing(content, "conversation") {
        let Some(ms) = stamp(line) else {
            continue;
        };
        for marker in AGY_DRIVER_MARKERS {
            if let Some((_, rest)) = line.split_once(marker) {
                let id = rest.get(..36).unwrap_or_default();
                if id.len() == 36 && id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()) {
                    log.events.push((ms, id.to_owned()));
                }
                break;
            }
        }
    }
    log
}

/// Each line of `content` that contains `needle`, in order. One scan of the whole text: a
/// search per line took a second over the ~800k lines of AGY's process logs.
fn lines_containing<'a>(content: &'a str, needle: &'a str) -> impl Iterator<Item = &'a str> {
    let mut line_end = 0;
    content.match_indices(needle).filter_map(move |(at, _)| {
        if at < line_end {
            return None; // another hit on the line just yielded
        }
        let start = content[..at].rfind('\n').map_or(0, |i| i + 1);
        let line = content[start..].lines().next().unwrap_or_default();
        line_end = start + line.len();
        Some(line)
    })
}

/// glog prefix `I0924 18:08:14.490117`, local time; the year comes from the log file name.
// ponytail: a process log that spans New Year gets January lines in the old year.
fn glog_timestamp_ms(year: i32, line: &str) -> Option<i64> {
    if !matches!(line.as_bytes().first(), Some(b'I' | b'W' | b'E' | b'F')) {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<u32> { line.get(range)?.parse().ok() };
    let time = chrono::NaiveDate::from_ymd_opt(year, num(1..3)?, num(3..5)?)?.and_hms_micro_opt(
        num(6..8)?,
        num(9..11)?,
        num(12..14)?,
        num(15..21)?,
    )?;
    Some(
        time.and_local_timezone(chrono::Local)
            .earliest()?
            .timestamp_millis(),
    )
}

/// Research subagents run inside their parent's process; AGY's summary index links them.
fn agy_parent_conversations(db: &std::path::Path) -> HashMap<String, String> {
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return HashMap::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT conversation_id, parent_conversation_id FROM conversation_summaries WHERE parent_conversation_id != ''",
    ) else {
        return HashMap::new();
    };
    let parents = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    parents
}

/// Tags each Antigravity CLI record with the Google account (email) that made the call. AGY's own
/// process logs are the ground truth: each logs its signed-in email and every conversation it
/// drives. A call belongs to the live process that last drove its conversation (or, for a
/// research subagent, its parent conversation).
fn tag_antigravity_account_records(records: &mut [UsageRecord]) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let cli_dir = home.join(".gemini/antigravity-cli");
    // ponytail: AGY keeps only ~100 process logs (about two weeks here), so older calls stay
    // untagged. Persist the parsed logs if long per-account history matters.
    let mut logs = Vec::new();
    for entry in std::fs::read_dir(cli_dir.join("log"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(year) = name
            .strip_prefix("cli-")
            .and_then(|rest| rest.get(..4))
            .and_then(|y| y.parse().ok())
        else {
            continue;
        };
        if let Ok(bytes) = std::fs::read(entry.path()) {
            logs.push(parse_agy_process_log(
                year,
                &String::from_utf8_lossy(&bytes),
            ));
        }
    }
    let parents = agy_parent_conversations(&cli_dir.join("conversation_summaries.db"));
    tag_antigravity_records_from_logs(records, &logs, &parents);
}

fn tag_antigravity_records_from_logs(
    records: &mut [UsageRecord],
    logs: &[AgyProcessLog],
    parents: &HashMap<String, String>,
) {
    let mut drivers: HashMap<&str, Vec<(i64, usize)>> = HashMap::new();
    for (i, log) in logs.iter().enumerate() {
        for (ms, conversation) in &log.events {
            drivers
                .entry(conversation.as_str())
                .or_default()
                .push((*ms, i));
        }
    }
    for events in drivers.values_mut() {
        events.sort_unstable();
    }
    for record in records.iter_mut().filter(|r| r.client == "antigravity-cli") {
        let mut conversation = record.session_id.as_str();
        for _ in 0..8 {
            match parents.get(conversation) {
                Some(parent) if !drivers.contains_key(conversation) => conversation = parent,
                _ => break,
            }
        }
        let Some(events) = drivers.get(conversation) else {
            continue;
        };
        let ts = record.timestamp;
        let before = events.partition_point(|&(ms, _)| ms <= ts);
        let email = events[..before]
            .iter()
            .rev()
            .map(|&(_, i)| &logs[i])
            .find(|log| ts <= log.last_ms + AGY_LOG_SLACK_MS)
            .and_then(|log| log.email.as_deref());
        if let Some(email) = email {
            record.client = format!("antigravity-cli ({email})");
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum ProtoWire<'a> {
    Varint(u64),
    Fixed64(u64),
    Len(&'a [u8]),
    Fixed32,
}

struct ProtoWireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoWireReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn next_field(&mut self) -> Option<(u64, ProtoWire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let field = tag >> 3;
        let wire = match tag & 0x7 {
            0 => ProtoWire::Varint(self.read_varint()?),
            1 => {
                let end = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?;
                let bytes: [u8; 8] = self.buf[self.pos..end].try_into().ok()?;
                self.pos = end;
                ProtoWire::Fixed64(u64::from_le_bytes(bytes))
            }
            2 => {
                let len = self.read_varint()? as usize;
                let end = self.pos.checked_add(len).filter(|&p| p <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                ProtoWire::Len(bytes)
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?;
                ProtoWire::Fixed32
            }
            _ => return None,
        };
        Some((field, wire))
    }
}

fn extract_proto_timestamp_ms(buf: &[u8]) -> Option<i64> {
    let mut reader = ProtoWireReader::new(buf);
    let mut seconds = None;
    let mut nanos = 0i64;
    while let Some((field, wire)) = reader.next_field() {
        match (field, wire) {
            (1, ProtoWire::Varint(s)) => seconds = Some(s as i64),
            (2, ProtoWire::Varint(n)) => nanos = n as i64,
            _ => {}
        }
    }
    let sec = seconds?;
    if !(1_000_000_000..=3_000_000_000).contains(&sec) {
        return None;
    }
    Some(sec.saturating_mul(1000).saturating_add(nanos / 1_000_000))
}

fn extract_step_timestamp_ms(metadata: &[u8]) -> Option<i64> {
    let mut reader = ProtoWireReader::new(metadata);
    while let Some((field, wire)) = reader.next_field() {
        if field == 1 || field == 6 || field == 7 {
            if let ProtoWire::Len(bytes) = wire {
                if let Some(ts) = extract_proto_timestamp_ms(bytes) {
                    return Some(ts);
                }
            }
        }
    }
    None
}

fn parse_gen_metadata_turn(blob: &[u8]) -> Option<(Option<String>, Vec<u8>, Vec<i64>)> {
    let mut reader = ProtoWireReader::new(blob);
    let mut chat_model_bytes = None;
    let mut step_bytes = None;
    while let Some((field, wire)) = reader.next_field() {
        match (field, wire) {
            (1, ProtoWire::Len(b)) => chat_model_bytes = Some(b),
            (2, ProtoWire::Len(b)) => step_bytes = Some(b),
            _ => {}
        }
    }
    let cm = chat_model_bytes?;
    let mut cm_reader = ProtoWireReader::new(cm);
    let mut usage_bytes = None;
    while let Some((field, wire)) = cm_reader.next_field() {
        if field == 4 {
            if let ProtoWire::Len(b) = wire {
                usage_bytes = Some(b);
                break;
            }
        }
    }
    let u = usage_bytes?;
    let mut u_reader = ProtoWireReader::new(u);
    let mut inp1 = 0u64;
    let mut inp2 = 0u64;
    let mut out = 0u64;
    let mut cch = 0u64;
    let mut rea = 0u64;
    let mut dedup_id = None;
    while let Some((field, wire)) = u_reader.next_field() {
        match (field, wire) {
            (1, ProtoWire::Varint(v)) => inp1 = v,
            (2, ProtoWire::Varint(v)) => inp2 = v,
            (5, ProtoWire::Varint(v)) => cch = v,
            (9, ProtoWire::Varint(v)) => out = v,
            (10, ProtoWire::Varint(v)) => rea = v,
            (11, ProtoWire::Len(b)) => {
                if let Ok(s) = std::str::from_utf8(b) {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        dedup_id = Some(trimmed.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if inp1 == 0 && inp2 == 0 && out == 0 && cch == 0 && rea == 0 {
        return None;
    }
    let mut step_indices = Vec::new();
    if let Some(sb) = step_bytes {
        let mut s_reader = ProtoWireReader::new(sb);
        while let Some(idx) = s_reader.read_varint() {
            step_indices.push(idx as i64);
        }
    }
    Some((dedup_id, u.to_vec(), step_indices))
}

/// One `gen_metadata` generation: its identity (the response id, else its usage bytes) and the
/// step indices it cites.
type GenTurn = (Vec<u8>, Vec<i64>);

/// The generations in an AGY database's `gen_metadata`, in order. Rows tokscale skips (no usage, a
/// repeated response id) are skipped too, so they line up 1:1 with its records for the same file.
fn read_gen_turns(path: &std::path::Path) -> Option<Vec<GenTurn>> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let mut stmt = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx").ok()?;
    let mut rows = stmt.query([]).ok()?;
    let mut seen_ids = std::collections::HashSet::new();
    let mut turns = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
            continue;
        };
        let Some((dedup_id, usage, steps)) = parse_gen_metadata_turn(&blob) else {
            continue;
        };
        let key = match dedup_id {
            Some(id) => {
                if !seen_ids.insert(id.clone()) {
                    continue;
                }
                id.into_bytes()
            }
            None => [b"usage:".as_slice(), &usage].concat(),
        };
        turns.push((key, steps));
    }
    Some(turns)
}

/// A session's generations in order: everything `agy-dehydrate` archived (the history up to its
/// last run), then the hot rows the archive lacks, which are newer. AGY >= 1.2.15 overwrites the
/// hot rows that cite steps past the kept ones when it resumes a dehydrated session, so only the
/// archive still has them.
fn gen_history<'a>(archived: &'a [GenTurn], hot: &'a [GenTurn]) -> Vec<&'a GenTurn> {
    let seen: std::collections::HashSet<&[u8]> =
        archived.iter().map(|(key, _)| key.as_slice()).collect();
    archived
        .iter()
        .chain(hot.iter().filter(|(key, _)| !seen.contains(key.as_slice())))
        .collect()
}

fn set_record_time(record: &mut UsageRecord, ms: i64) {
    record.timestamp = ms;
    if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
        record.date = dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
    }
}

/// The session start tokscale gives every AGY row it could not date: the timestamp most records
/// share, since a row's own generation time is unique. None when no two records share one.
fn fallback_timestamp(timestamps: impl Iterator<Item = i64>) -> Option<i64> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for ts in timestamps {
        *counts.entry(ts).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|&(_, n)| n > 1)
        .max_by_key(|&(ts, n)| (n, std::cmp::Reverse(ts)))
        .map(|(ts, _)| ts)
}

/// A record for one AGY generation as tokscale parsed it from a single database file.
fn agy_record(
    m: tokscale_core::sessions::UnifiedMessage,
    client: &str,
    session_id: &str,
) -> UsageRecord {
    UsageRecord {
        client: client.to_owned(),
        model_id: m.model_id,
        provider_id: m.provider_id,
        session_id: session_id.to_owned(),
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
        reported_cost_micros: None,
    }
}

/// The conversation a `parent_references` row's trajectory segment came from (field 6).
fn parent_reference_conversation(blob: &[u8]) -> Option<&str> {
    let mut reader = ProtoWireReader::new(blob);
    while let Some((field, wire)) = reader.next_field() {
        if let (6, ProtoWire::Len(id)) = (field, wire) {
            return std::str::from_utf8(id).ok();
        }
    }
    None
}

/// The conversations a `/fork` copied this one's history from. `parent_references` lists the
/// trajectory segments a conversation is made of; a fork's first segments name its parent.
fn agy_ancestors(db: &std::path::Path, session_id: &str) -> Vec<String> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT data FROM parent_references ORDER BY idx") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) else {
        return Vec::new();
    };
    let mut ancestors: Vec<String> = Vec::new();
    for blob in rows.flatten() {
        if let Some(id) = parent_reference_conversation(&blob) {
            if id != session_id && !ancestors.iter().any(|a| a == id) {
                ancestors.push(id.to_owned());
            }
        }
    }
    ancestors
}

/// Per fork, the generations its ancestors also hold: the history it copied, which belongs to the
/// ancestor that made it.
fn agy_inherited_keys(
    lineage: &HashMap<String, Vec<String>>,
    held: &HashMap<String, HashSet<Vec<u8>>>,
) -> HashMap<String, HashSet<Vec<u8>>> {
    lineage
        .iter()
        .map(|(fork, ancestors)| {
            let inherited = ancestors
                .iter()
                .filter_map(|a| held.get(a))
                .flatten()
                .cloned()
                .collect();
            (fork.clone(), inherited)
        })
        .collect()
}

/// AGY's `/fork` copies the parent's history (steps and `gen_metadata` rows, response ids included)
/// into the new conversation, and tokscale keeps the first copy of a response id in file-path order.
/// A fork whose id sorts first so took over its parent's whole history, dated at the fork and driven
/// by no process (2026-10-27: 3,131 rows of [Immigration] showed up as 442M untagged tokens on the
/// day its fork [Job Hunting] was made), while the parent, left with only exactly-dated rows, had no
/// fallback to date its archive by and lost 775 archived rows outright. Rebuilds every fork and its
/// ancestors from their own
/// databases so each generation stays with the conversation that made it. Returns each fork's
/// inherited keys, which its own rows skip.
fn rebuild_forked_antigravity_sessions(
    records: &mut Vec<UsageRecord>,
) -> HashMap<String, HashSet<Vec<u8>>> {
    use rayon::prelude::*;

    let Some(home) = dirs::home_dir() else {
        return HashMap::new();
    };
    // ponytail: the desktop app and IDE extension share response ids with the CLI, and tokscale's
    // cross-surface dedup can't be redone from here; their users keep tokscale's attribution.
    if records
        .iter()
        .any(|r| r.client == "antigravity" || r.client == "antigravity-extension")
    {
        return HashMap::new();
    }
    let conv_dir = home.join(".gemini/antigravity-cli/conversations");
    let archive_dir = home.join(".gemini/antigravity-cli/archive");
    let db = |id: &str| conv_dir.join(format!("{id}.db"));

    let sessions: HashSet<&str> = records
        .iter()
        .filter(|r| r.client == "antigravity-cli")
        .map(|r| r.session_id.as_str())
        .collect();
    let lineage: HashMap<String, Vec<String>> = sessions
        .par_iter()
        .filter_map(|&id| {
            let ancestors: Vec<String> = agy_ancestors(&db(id), id)
                .into_iter()
                .filter(|a| db(a).exists())
                .collect();
            (!ancestors.is_empty()).then(|| (id.to_owned(), ancestors))
        })
        .collect();
    if lineage.is_empty() {
        return HashMap::new();
    }

    // Each member's rows straight from its own file: tokscale's per-file parse lines up 1:1 with
    // `read_gen_turns`.
    let members: BTreeSet<String> = lineage
        .iter()
        .flat_map(|(fork, ancestors)| std::iter::once(fork).chain(ancestors))
        .cloned()
        .collect();
    let mut parsed = BTreeMap::new();
    let mut held: HashMap<String, HashSet<Vec<u8>>> = HashMap::new();
    for id in &members {
        let path = db(id);
        let messages = tokscale_core::sessions::antigravity_cli::parse_antigravity_cli_file(&path);
        let Some(turns) = read_gen_turns(&path).filter(|t| t.len() == messages.len()) else {
            return HashMap::new(); // can't line them up: leave tokscale's records as they are
        };
        let archived = read_gen_turns(&archive_dir.join(format!("{id}.cold.db")));
        let keys: Vec<Vec<u8>> = turns.into_iter().map(|(key, _)| key).collect();
        let mut all_keys: HashSet<Vec<u8>> = keys.iter().cloned().collect();
        all_keys.extend(archived.unwrap_or_default().into_iter().map(|(key, _)| key));
        held.insert(id.clone(), all_keys);
        let rows: Vec<_> = keys.into_iter().zip(messages).collect();
        parsed.insert(id.clone(), rows);
    }
    let inherited = agy_inherited_keys(&lineage, &held);

    records.retain(|r| !(r.client == "antigravity-cli" && members.contains(&r.session_id)));
    for (id, rows) in parsed {
        let skip = inherited.get(&id);
        records.extend(
            rows.into_iter()
                .filter(|(key, _)| skip.is_none_or(|s| !s.contains(key)))
                .map(|(_, m)| agy_record(m, "antigravity-cli", &id)),
        );
    }
    inherited
}

/// Dates AGY records from AGY's transcript, and adds the usage that only `agy-dehydrate`'s archive
/// still holds. A fork's records skip the `inherited` history, which stays with its ancestor.
fn enrich_antigravity_records(
    records: &mut Vec<UsageRecord>,
    inherited: &HashMap<String, HashSet<Vec<u8>>>,
) {
    use rayon::prelude::*;

    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let conv_dir = home.join(".gemini/antigravity-cli/conversations");
    if !conv_dir.exists() {
        return;
    }
    let archive_dir = home.join(".gemini/antigravity-cli/archive");

    let mut session_map: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, record) in records.iter().enumerate() {
        if record.client.starts_with("antigravity-cli") {
            session_map.entry(record.session_id.clone()).or_default().push(idx);
        }
    }

    struct SessionEnrichment {
        time_updates: Vec<(usize, i64)>,
        archived_records: Vec<UsageRecord>,
    }

    let records_ref = records.as_slice();

    let results: Vec<SessionEnrichment> = session_map
        .into_par_iter()
        .filter_map(|(session_id, record_indices)| {
            if record_indices.len() <= 1 {
                return None;
            }
            // Only the records tokscale could not date get re-dated: those all sit on the session's
            // start. A session that spans an AGY upgrade also has rows dated by their own
            // generation time (agy <= 1.1.17 wrote one), and skipping it whole left 6baa4b1b's
            // October usage on 08-30.
            let first_ts =
                fallback_timestamp(record_indices.iter().map(|&i| records_ref[i].timestamp))?;
            let first_date = record_indices
                .iter()
                .map(|&i| &records_ref[i])
                .find(|r| r.timestamp == first_ts)
                .map(|r| r.date.clone())
                .unwrap_or_default();

            let db_path = conv_dir.join(format!("{session_id}.db"));
            if !db_path.exists() {
                return None;
            }

            let conn = rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            ).ok()?;

            // Extract step timestamps
            let mut step_timestamps: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
            if let Ok(mut stmt) = conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL") {
                if let Ok(mut rows) = stmt.query([]) {
                    while let Ok(Some(row)) = rows.next() {
                        let idx: i64 = match row.get(0) {
                            Ok(i) => i,
                            Err(_) => continue,
                        };
                        let meta: Vec<u8> = match row.get(1) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        if let Some(ts) = extract_step_timestamp_ms(&meta) {
                            step_timestamps.insert(idx, ts);
                        }
                    }
                }
            }

            // `agy-dehydrate` renumbers the hot `steps` but leaves `gen_metadata` on the old indices,
            // so after a dehydration the hot steps date older turns wrongly or not at all. AGY's own
            // append-only transcript keeps every step it ever logged, so it wins when present.
            let logs_dir = home
                .join(".gemini/antigravity-cli/brain")
                .join(&session_id)
                .join(".system_generated/logs");
            let compact_path = logs_dir.join("transcript.jsonl");
            let full_path = logs_dir.join("transcript_full.jsonl");
            let transcript_path = if compact_path.exists() {
                &compact_path
            } else {
                &full_path
            };
            let transcript = read_agy_transcript(transcript_path);
            if transcript.is_none() && step_timestamps.is_empty() {
                return None;
            }

            let hot = read_gen_turns(&db_path)?;
            let skip = inherited.get(&session_id);
            let own: Vec<&GenTurn> = hot
                .iter()
                .filter(|(key, _)| skip.is_none_or(|s| !s.contains(key)))
                .collect();
            let mut time_updates = Vec::new();

            let Some(lines) = &transcript else {
                for (&r_idx, (_, steps)) in record_indices.iter().zip(own.iter().copied()) {
                    if records_ref[r_idx].timestamp != first_ts {
                        continue;
                    }
                    if let Some(ts) = steps.iter().find_map(|s| step_timestamps.get(s).copied()) {
                        time_updates.push((r_idx, ts));
                    }
                }
                return Some(SessionEnrichment {
                    time_updates,
                    archived_records: Vec::new(),
                });
            };

            let archive_path = archive_dir.join(format!("{session_id}.cold.db"));
            let archived = read_gen_turns(&archive_path).unwrap_or_default();
            let history = gen_history(&archived, &hot);
            let steps: Vec<Vec<i64>> = history.iter().map(|(_, s)| s.clone()).collect();
            let dated: HashMap<&[u8], i64> = history
                .iter()
                .zip(date_turns_from_transcript(&steps, lines))
                .filter_map(|((key, _), ts)| Some((key.as_slice(), ts?)))
                .collect();

            for (&r_idx, (key, _)) in record_indices.iter().zip(own.iter().copied()) {
                if records_ref[r_idx].timestamp != first_ts {
                    continue;
                }
                if let Some(&ts) = dated.get(key.as_slice()) {
                    time_updates.push((r_idx, ts));
                }
            }

            // Usage only the archive still holds. tokscale parses the archive like any AGY database,
            // so its records line up 1:1 with `archived`.
            let in_hot: std::collections::HashSet<&[u8]> =
                hot.iter().map(|(key, _)| key.as_slice()).collect();
            let mut session_archived = Vec::new();
            if !archived.iter().all(|(key, _)| in_hot.contains(key.as_slice())) {
                let client = records_ref[record_indices[0]].client.clone();
                let parsed =
                    tokscale_core::sessions::antigravity_cli::parse_antigravity_cli_file(&archive_path);
                for ((key, _), m) in archived.iter().zip(parsed) {
                    if in_hot.contains(key.as_slice()) || skip.is_some_and(|s| s.contains(key)) {
                        continue;
                    }
                    let mut record = agy_record(m, &client, &session_id);
                    record.date = first_date.clone();
                    record.timestamp = first_ts;
                    if let Some(&ts) = dated.get(key.as_slice()) {
                        set_record_time(&mut record, ts);
                    }
                    session_archived.push(record);
                }
            }

            Some(SessionEnrichment {
                time_updates,
                archived_records: session_archived,
            })
        })
        .collect();

    for res in results {
        for (r_idx, ts) in res.time_updates {
            set_record_time(&mut records[r_idx], ts);
        }
        records.extend(res.archived_records);
    }
}

/// `(step_index, created_at ms)` for each line of AGY's transcript, in write order.
fn read_agy_transcript(path: &std::path::Path) -> Option<Vec<(i64, i64)>> {
    #[derive(Deserialize)]
    struct Line {
        step_index: i64,
        created_at: String,
    }
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut lines = Vec::new();
    for line_res in reader.lines() {
        let line_str = line_res.ok()?;
        if let Ok(line) = serde_json::from_str::<Line>(&line_str) {
            if let Ok(at) = chrono::DateTime::parse_from_rfc3339(&line.created_at) {
                lines.push((line.step_index, at.timestamp_millis()));
            }
        }
    }
    (!lines.is_empty()).then_some(lines)
}

/// Dates each turn (its step indices, in `gen_metadata` order) from the transcript. Every
/// dehydration restarts the step indices, so one index can appear once per era. Both logs are
/// append-only, so walking back from the newest turn, each turn's latest line at or before the
/// next turn's line is in its own era; the turn is dated by the earliest of its lines next to it.
///
/// The walk goes backward because AGY truncates the tail of `gen_metadata` when a session rewinds
/// (a retry after an error) and, since 1.2.15, when it resumes a dehydrated session. Walking
/// forward across that gap dated every newer turn from the discarded lines (2026-10-02: 8979be53
/// lost 47 h). Anchoring on the latest line, not the earliest, keeps a turn's reference to a
/// renumbered kept step, whose index is logged only in an older era, from dragging it back.
fn date_turns_from_transcript(turns: &[Vec<i64>], lines: &[(i64, i64)]) -> Vec<Option<i64>> {
    // ponytail: a turn's own steps are logged within a few lines of each other; any hit further
    // back than this is an older era. Results are identical for 4–64 on 59,612 real turns.
    const SAME_TURN: usize = 16;
    let mut positions: HashMap<i64, Vec<usize>> = HashMap::new();
    for (pos, &(step, _)) in lines.iter().enumerate() {
        positions.entry(step).or_default().push(pos);
    }
    let mut cursor = usize::MAX;
    let mut dated: Vec<Option<i64>> = turns
        .iter()
        .rev()
        .map(|steps| {
            let hits: Vec<usize> = steps
                .iter()
                .filter_map(|step| {
                    let at = positions.get(step)?;
                    let n = at.partition_point(|&pos| pos <= cursor);
                    n.checked_sub(1).map(|i| at[i])
                })
                .collect();
            let anchor = *hits.iter().max()?;
            cursor = hits.into_iter().filter(|&h| anchor - h <= SAME_TURN).min()?;
            Some(lines[cursor].1)
        })
        .collect();
    dated.reverse();
    dated
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
                    reported_cost_micros: None,
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
                    reported_cost_micros: None,
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
                reported_cost_micros: None,
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

    #[test]
    fn extract_proto_timestamp_decodes_seconds_and_nanos() {
        // Wire encoded {1: 1788640000, 2: 500000000}
        // tag 1 (field 1, varint): (1 << 3) | 0 = 0x08
        // 1788640000 in varint
        // tag 2 (field 2, varint): (2 << 3) | 0 = 0x10
        let mut buf = Vec::new();
        buf.push(0x08);
        let mut s = 1788640000u64;
        while s >= 0x80 {
            buf.push((s as u8 & 0x7f) | 0x80);
            s >>= 7;
        }
        buf.push(s as u8);
        buf.push(0x10);
        let mut n = 500_000_000u64;
        while n >= 0x80 {
            buf.push((n as u8 & 0x7f) | 0x80);
            n >>= 7;
        }
        buf.push(n as u8);

        let ms = extract_proto_timestamp_ms(&buf);
        assert_eq!(ms, Some(1788640000500));
    }

    #[test]
    fn antigravity_rows_on_the_session_start_are_the_undated_ones() {
        // Rows from agy <= 1.1.17 carry their own time; later ones all get the session start.
        assert_eq!(fallback_timestamp([5, 7, 1, 9, 1, 1].into_iter()), Some(1));
        assert_eq!(fallback_timestamp([1, 1, 1].into_iter()), Some(1));
        assert_eq!(fallback_timestamp([5, 7, 9].into_iter()), None);
    }

    #[test]
    fn antigravity_account_tagging_follows_agy_process_logs() {
        let conv = "aaaaaaaa-0000-4000-8000-000000000001";
        let log = parse_agy_process_log(
            2026,
            &format!(
                "I0924 18:08:14.490117       1 common.go:391] Resuming conversation {conv}\n\
                 I0924 18:08:14.532061     315 server_oauth.go:196] applyAuthResult: email=two@example.com, authMethod=consumer, quotaProject=\n\
                 I0924 18:08:15.990117     921 conversation_manager.go:699] Forwarding user message to conversation {conv} (items=1, media=0)\n"
            ),
        );
        assert_eq!(log.email.as_deref(), Some("two@example.com"));
        let stamp = |line| glog_timestamp_ms(2026, line).unwrap();
        assert_eq!(
            stamp("I0924 18:08:15.990117") - stamp("I0924 18:08:14.490117"),
            1_500
        );
        // Only the user message drives the conversation; resuming it just opens it.
        assert_eq!(log.events, vec![(log.last_ms, conv.to_owned())]);
        // The parser only looks at lines that mention a conversation.
        assert!(AGY_DRIVER_MARKERS
            .iter()
            .all(|m| m.contains("conversation")));

        // Two panes on different accounts share `conv`; the one that last drove it owns the call.
        let logs = vec![
            AgyProcessLog {
                email: Some("two@example.com".into()),
                last_ms: 1_000_000,
                events: vec![(10, conv.into())],
            },
            AgyProcessLog {
                email: Some("three@example.com".into()),
                last_ms: 1_000_000,
                events: vec![(200, conv.into())],
            },
        ];
        let parents = HashMap::from([("research-sub".to_owned(), conv.to_owned())]);
        let mut records: Vec<UsageRecord> = [
            (conv, 5),
            (conv, 50),
            (conv, 300),
            ("research-sub", 250),
            (conv, 2_000_000),
        ]
        .into_iter()
        .map(|(session, timestamp)| UsageRecord {
            client: "antigravity-cli".into(),
            model_id: "gemini-3.8-flash".into(),
            provider_id: "google".into(),
            session_id: session.into(),
            date: "2026-09-24".into(),
            timestamp,
            tokens: UsageTokens::default(),
            message_count: 1,
            reported_cost_micros: None,
        })
        .collect();

        tag_antigravity_records_from_logs(&mut records, &logs, &parents);

        let clients: Vec<&str> = records.iter().map(|r| r.client.as_str()).collect();
        assert_eq!(
            clients,
            [
                "antigravity-cli",                     // before any driver line: unknown
                "antigravity-cli (two@example.com)",   // pane 2 sent the last message
                "antigravity-cli (three@example.com)", // pane 3 took over
                "antigravity-cli (three@example.com)", // subagent follows its parent
                "antigravity-cli", // both processes long gone: unknown, not guessed
            ]
        );
    }

    #[test]
    fn forked_antigravity_history_stays_with_its_ancestor() {
        // A `parent_references` row: field 1 the trajectory, field 6 the conversation it came from.
        let row = |conversation: &str| {
            let mut blob = vec![0x0a, 4];
            blob.extend(b"traj");
            blob.extend([0x32, conversation.len() as u8]);
            blob.extend(conversation.as_bytes());
            blob
        };
        let parent = row("parent");
        assert_eq!(parent_reference_conversation(&parent), Some("parent"));

        let keys = |ids: &[&str]| -> HashSet<Vec<u8>> {
            ids.iter().map(|id| id.as_bytes().to_vec()).collect()
        };
        let ids = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let lineage = HashMap::from([
            ("fork".to_owned(), ids(&["parent"])),
            ("fork-of-fork".to_owned(), ids(&["parent", "fork"])),
        ]);
        let held = HashMap::from([
            ("parent".to_owned(), keys(&["a", "b"])),
            ("fork".to_owned(), keys(&["a", "b", "f"])),
            ("fork-of-fork".to_owned(), keys(&["a", "b", "f", "g"])),
        ]);
        let inherited = agy_inherited_keys(&lineage, &held);
        assert_eq!(inherited["fork"], keys(&["a", "b"])); // the parent's history, not its own "f"
        assert_eq!(inherited["fork-of-fork"], keys(&["a", "b", "f"]));
        assert!(!inherited.contains_key("parent")); // an ancestor keeps all of its rows
    }

    #[test]
    fn antigravity_turns_keep_their_era_after_dehydration() {
        // Era 0 logged steps 0..=5. A dehydration kept two of them, renumbered 0..=1, and the
        // resumed session logged steps 2 and 3 again. Each turn names steps of its own era.
        let lines = [(0, 10), (1, 11), (2, 12), (3, 13), (4, 14), (5, 15), (2, 30), (3, 31)];
        let turns = [vec![1], vec![3], vec![4, 5], vec![2], vec![3], vec![9]];
        assert_eq!(
            date_turns_from_transcript(&turns, &lines),
            [Some(11), Some(13), Some(14), Some(30), Some(31), None]
        );
    }

    #[test]
    fn antigravity_turns_survive_an_overwritten_metadata_tail() {
        // Era 0 logged steps 0..=5; the dehydration kept 0..=2 and the resumed session logged 3..=5
        // again. AGY overwrote era 0's turns for steps 3..=5, so `gen_metadata` jumps from era 0's
        // step 2 straight to era 1's step 3, which also exists in era 0.
        let lines = [(0, 10), (1, 11), (2, 12), (3, 13), (4, 14), (5, 15), (3, 30), (4, 31), (5, 32)];
        let turns = [vec![1], vec![2], vec![3], vec![4, 5]];
        assert_eq!(
            date_turns_from_transcript(&turns, &lines),
            [Some(11), Some(12), Some(30), Some(31)]
        );
    }

    #[test]
    fn antigravity_archive_keeps_the_usage_agy_overwrote() {
        // Era 0 logged steps 0..=3 and generations a, b, c; agy-dehydrate archived all three and
        // kept two steps. On resume AGY overwrote b and c, and generation d cites the new step 2.
        let lines = [(0, 10), (1, 11), (2, 12), (3, 13), (2, 30)];
        let turn = |key: &str, steps: &[i64]| (key.as_bytes().to_vec(), steps.to_vec());
        let archived = [turn("a", &[1]), turn("b", &[2]), turn("c", &[3])];
        let hot = [turn("a", &[1]), turn("d", &[2])];
        let history = gen_history(&archived, &hot);
        let keys: Vec<&[u8]> = history.iter().map(|(key, _)| key.as_slice()).collect();
        assert_eq!(keys, [b"a".as_slice(), b"b", b"c", b"d"]);
        let steps: Vec<Vec<i64>> = history.iter().map(|(_, s)| s.clone()).collect();
        assert_eq!(
            date_turns_from_transcript(&steps, &lines),
            [Some(11), Some(12), Some(13), Some(30)]
        );
    }

    #[test]
    fn antigravity_turn_citing_a_kept_step_stays_in_its_era() {
        // Era 0 logged steps 0..=19 (t = 100..=119); the dehydration kept two, renumbered 0..=1,
        // and the resumed session logged 2 and 3. Its first turn also cites kept step 0, which the
        // transcript holds only as era 0's step 0.
        let mut lines: Vec<(i64, i64)> = (0..20).map(|s| (s, 100 + s)).collect();
        lines.extend([(2, 300), (3, 301)]);
        let turns = [vec![18], vec![0, 2], vec![3]];
        assert_eq!(
            date_turns_from_transcript(&turns, &lines),
            [Some(118), Some(300), Some(301)]
        );
    }

    #[test]
    fn grok_bot_rows_get_their_own_client_and_cursors_charge() {
        let sample = serde_json::json!({
            "usageEventsDisplay": [
                {"model": "grok-bot-default", "timestamp": "1790289578354", "chargedCents": 19.70219993591309,
                 "tokenUsage": {"inputTokens": 42412, "cacheWriteTokens": 1280}},
                {"model": "grok-4.7-xhigh", "timestamp": "1790289578355", "chargedCents": 3.5}
            ]
        })
        .to_string();
        let mut records = Vec::new();
        parse_cursor_json(&sample, &mut records, None);
        assert_eq!(records[0].client, "grokbot");
        assert_eq!(records[0].reported_cost_micros, Some(197_022));
        assert_eq!(records[0].tokens.cache_write, 1280);
        assert_eq!(records[1].client, "cursor");
        assert_eq!(records[1].reported_cost_micros, None);
    }

    #[test]
    fn grok_bot_session_aware_attribution_for_auxiliary_models() {
        let sample = serde_json::json!({
            "usageEventsDisplay": [
                {
                    "conversationId": "grok-session-1",
                    "model": "grok-bot-default",
                    "timestamp": "1790289578354",
                    "chargedCents": 19.70219993591309
                },
                {
                    "conversationId": "grok-session-1",
                    "model": "gemini-2.5-flash",
                    "timestamp": "1790289578355",
                    "chargedCents": 5.8
                },
                {
                    "conversationId": "cursor-local-session-2",
                    "model": "gemini-2.5-flash",
                    "timestamp": "1790289578356",
                    "chargedCents": 5.8
                }
            ]
        })
        .to_string();
        let mut records = Vec::new();
        parse_cursor_json(&sample, &mut records, None);
        assert_eq!(records[0].client, "grokbot");
        assert_eq!(records[1].client, "grokbot");
        assert_eq!(records[2].client, "cursor");
    }

    #[test]
    fn cursor_json_parsing_recovers_tokens_and_models() {
        let sample = serde_json::json!({
            "usageEventsDisplay": [
                {
                    "model": "grok-4.7-xhigh",
                    "timestamp": "1790865010602",
                    "conversationId": "7494881e-291a-438c-b981-790d2e1ed309",
                    "tokenUsage": {
                        "inputTokens": 100550,
                        "outputTokens": 857,
                        "cacheReadTokens": 141312
                    }
                },
                {
                    "model": "claude-3.5-sonnet",
                    "timestamp": "1726694846401",
                    "conversationId": null
                }
            ]
        })
        .to_string();

        let mut records = Vec::new();
        parse_cursor_json(&sample, &mut records, None);
        assert_eq!(records.len(), 2);

        assert_eq!(records[0].client, "cursor");
        assert_eq!(records[0].model_id, "grok-4.7-xhigh");
        assert_eq!(records[0].provider_id, "xai");
        assert_eq!(records[0].session_id, "cursor-7494881e-291a-438c-b981-790d2e1ed309");
        assert_eq!(records[0].tokens.input, 100550);
        assert_eq!(records[0].tokens.output, 857);
        assert_eq!(records[0].tokens.cache_read, 141312);

        assert_eq!(records[1].client, "cursor");
        assert_eq!(records[1].model_id, "claude-3.5-sonnet");
        assert_eq!(records[1].provider_id, "anthropic");
        assert_eq!(records[1].session_id, "cursor-1726694846401");
        assert_eq!(records[1].tokens.input, 0);

        // Test since filter
        let mut filtered = Vec::new();
        parse_cursor_json(&sample, &mut filtered, Some("2026-01-01"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].model_id, "grok-4.7-xhigh");
    }

    #[test]
    fn commandcode_prompt_tokens_deduct_cache_read() {
        let raw = serde_json::json!({
            "inputTokens": 23057,
            "outputTokens": 209,
            "cacheReadTokens": 6016,
            "cacheWriteTokens": 0,
            "costUsd": 0.002699598
        });
        let raw_input = raw.get("inputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cache_read = raw.get("cacheReadTokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cache_write = raw.get("cacheWriteTokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let output = raw.get("outputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let uncached_input = raw_input.saturating_sub(cache_read).max(0);
        let tokens = UsageTokens {
            input: uncached_input,
            output,
            cache_read,
            cache_write,
            reasoning: 0,
        };
        assert_eq!(tokens.input, 17041);
        assert_eq!(tokens.cache_read, 6016);
        assert_eq!(tokens.output, 209);
        assert_eq!(tokens.reported_total_without_reasoning(), 23266);
    }

    #[test]
    #[ignore = "live local database audit"]
    fn audit_live_antigravity_attributions() {
        let snapshot = match collect_local_usage(UsageOptions::default()) {
            Ok(s) => s,
            Err(e) => {
                println!("Skipping live audit: {}", e);
                return;
            }
        };

        let now_ms = chrono::Utc::now().timestamp_millis();
        let d7_ms = now_ms - 7 * 86400 * 1000;

        let mut counts = std::collections::HashMap::new();
        let mut tokens_map = std::collections::HashMap::new();

        for r in snapshot.records.iter() {
            if r.timestamp >= d7_ms && r.client.starts_with("antigravity-cli") {
                *counts.entry(r.client.clone()).or_insert(0) += 1;
                *tokens_map.entry(r.client.clone()).or_insert(0i64) += r.tokens.reported_total_without_reasoning();
            }
        }

        println!("=== LIVE LAST 7D BREAKDOWN (CURRENT) ===");
        let mut clients: Vec<_> = counts.keys().cloned().collect();
        clients.sort();
        for c in clients {
            let cnt = counts.get(&c).copied().unwrap_or(0);
            let toks = tokens_map.get(&c).copied().unwrap_or(0);
            println!("{:<30} | turns: {:<6} | tokens: {:>12} ({:.2}M)", c, cnt, toks, toks as f64 / 1_000_000.0);
        }
    }
}


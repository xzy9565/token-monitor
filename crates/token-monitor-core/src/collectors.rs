//! Read-only live balance collectors for the first native migration slice.
//!
//! Every collector returns normalized `ProviderSnapshot` rows. Credentials are
//! read from the existing environment/config locations and are never included
//! in a snapshot, error, or log message.

use crate::{Availability, LimitWindow, ProviderSnapshot, SourceHealth, WindowKind, WindowMetric};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

// Modal/Vast CLIs can take several seconds to cold-start their Python/runtime
// layer. Keep the deadline generous enough for a real refresh while the TUI
// remains responsive because these calls run off the render thread.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
pub const DEFAULT_MODAL_GRANT_USD: f64 = 30.0;

#[derive(Clone, Debug, Default)]
pub struct CollectorOptions {
    pub providers: Option<HashSet<String>>,
    pub modal_profiles: Option<Vec<String>>,
    pub modal_config: Option<PathBuf>,
    pub modal_credit_grant_usd: Option<f64>,
    pub timeout: Option<Duration>,
}

impl CollectorOptions {
    pub fn includes(&self, provider: &str) -> bool {
        self.providers
            .as_ref()
            .is_none_or(|selected| selected.contains("all") || selected.contains(provider))
    }

    fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn clean_secret(value: Option<&String>) -> Option<String> {
    let mut value = value?.trim().to_owned();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value = value[1..value.len() - 1].trim().to_owned();
    }
    (!value.is_empty()).then_some(value)
}

fn parse_shell_secrets(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let assignment = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, raw_value)) = assignment.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            continue;
        }
        let value = raw_value.trim();
        if value.contains("$(") || value.contains('`') {
            // Never evaluate shell expressions while reading credentials.
            continue;
        }
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if !value.trim().is_empty() {
            values.insert(name.to_owned(), value.trim().to_owned());
        }
    }
    values
}

fn shell_secrets() -> &'static HashMap<String, String> {
    static CACHE: OnceLock<HashMap<String, String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        for path in [home.join(".zsh_secrets"), home.join(".zsh-secrets")] {
            if let Ok(text) = fs::read_to_string(path) {
                let values = parse_shell_secrets(&text);
                if !values.is_empty() {
                    return values;
                }
            }
        }
        HashMap::new()
    })
}

fn env_secret(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| {
            clean_secret(
                std::env::var_os(name)
                    .and_then(|value| value.into_string().ok())
                    .as_ref(),
            )
        })
        .or_else(|| names.iter().find_map(|name| crate::credentials::get(name)))
        .or_else(|| {
            names.iter().find_map(|name| {
                shell_secrets()
                    .get(*name)
                    .and_then(|value| clean_secret(Some(value)))
            })
        })
}

fn account_key(prefix: &str, secret: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(secret.as_bytes());
    let short = digest
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}:{short}")
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn connected_snapshot(
    provider_id: &str,
    account_key: String,
    account_label: String,
    plan: String,
    source: &str,
    windows: Vec<LimitWindow>,
    hue: u8,
) -> ProviderSnapshot {
    let availability = if windows.iter().any(|window| {
        (window.metric.is_credit() && window.effectively_exhausted())
            || (!window.metric.is_credit()
                && window.kind.durable()
                && window.effectively_exhausted())
    }) {
        Availability::Exhausted
    } else {
        Availability::Available
    };
    ProviderSnapshot {
        provider_id: provider_id.to_owned(),
        account_key,
        account_label,
        plan,
        source: source.to_owned(),
        collected_at_ms: now_ms(),
        source_health: SourceHealth::Connected,
        availability,
        windows,
        diagnostics: vec![],
        hue,
    }
}

fn unavailable_snapshot(
    provider_id: &str,
    account_key: String,
    account_label: String,
    source: &str,
    health: SourceHealth,
    detail: &str,
    hue: u8,
) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: provider_id.to_owned(),
        account_key,
        account_label,
        plan: String::new(),
        source: source.to_owned(),
        collected_at_ms: now_ms(),
        source_health: health,
        availability: Availability::Unknown,
        windows: vec![],
        diagnostics: vec![detail.to_owned()],
        hue,
    }
}

fn status_for_http(status: StatusCode) -> SourceHealth {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        SourceHealth::Unauthorized
    } else {
        SourceHealth::Unavailable
    }
}

async fn request_json(
    client: &Client,
    url: &str,
    token: &str,
) -> Result<Value, (SourceHealth, String)> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("HTTP-Referer", "https://github.com/Javis603/token-monitor")
        .header("X-OpenRouter-Title", "Token Monitor")
        .send()
        .await
        .map_err(|_| (SourceHealth::Unavailable, "request failed".to_owned()))?;
    let status = response.status();
    if !status.is_success() {
        return Err((status_for_http(status), format!("HTTP {}", status.as_u16())));
    }
    response.json::<Value>().await.map_err(|_| {
        (
            SourceHealth::Unavailable,
            "invalid JSON response".to_owned(),
        )
    })
}

async fn request_cursor_json(
    client: &Client,
    url: &str,
    session_token: &str,
    method: reqwest::Method,
) -> Result<Value, (SourceHealth, String)> {
    let response = client
        .request(method, url)
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://cursor.com/dashboard")
        .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
        .header(
            "Cookie",
            format!("WorkosCursorSessionToken={session_token}"),
        )
        .send()
        .await
        .map_err(|_| {
            (
                SourceHealth::Unavailable,
                "Cursor request failed".to_owned(),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err((status_for_http(status), format!("HTTP {}", status.as_u16())));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| (SourceHealth::Unavailable, "Invalid Cursor JSON".to_owned()))
}

fn cursor_accounts() -> Vec<(String, String)> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let path = home.join(".config/tokscale/cursor-credentials.json");
    let payload = match fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    {
        Some(payload) => payload,
        None => return vec![],
    };
    let active = payload
        .get("activeAccountId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(accounts) = payload.get("accounts").and_then(Value::as_object) else {
        return vec![];
    };
    let mut rows = accounts
        .iter()
        .filter_map(|(id, value)| {
            let token = value.get("sessionToken").and_then(Value::as_str)?.trim();
            if token.is_empty() {
                return None;
            }
            let label = value
                .get("label")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(id);
            Some((
                id.to_owned(),
                token.to_owned(),
                label.to_owned(),
                id == active,
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|(_, _, _, is_active)| !*is_active);
    rows.into_iter()
        .map(|(_, token, label, _)| (token, label))
        .collect()
}

fn cursor_number(value: Option<&Value>) -> Option<f64> {
    number(value)
}

fn cursor_remaining_percent(used: Option<f64>) -> Option<f64> {
    used.map(|value| (100.0 - value).clamp(0.0, 100.0))
}

fn cursor_window(
    label: &str,
    remaining_percent: Option<f64>,
    reset: Option<String>,
    reset_ms: Option<i64>,
) -> Option<LimitWindow> {
    remaining_percent.map(|percent| LimitWindow {
        label: label.to_owned(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Quota,
        remaining_percent: Some(percent),
        remaining_amount: None,
        currency: None,
        resets_at_ms: reset_ms,
        reset_text: reset,
        estimated: false,
    })
}

fn cursor_reset(summary: &Value) -> (Option<i64>, Option<String>) {
    let value = summary
        .get("billingCycleEnd")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let reset_ms = value
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|date| date.timestamp_millis());
    (reset_ms, value.filter(|_| reset_ms.is_none()))
}

fn cursor_agent_blocked_in(root: &Path, reset_ms: Option<i64>) -> bool {
    if reset_ms.is_some_and(|reset| reset <= now_ms()) {
        return false;
    }
    fn collect_logs(
        path: &Path,
        files: &mut Vec<(PathBuf, std::time::SystemTime)>,
        depth: usize,
    ) {
        if depth > 5 || files.len() >= 64 {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                collect_logs(&path, files, depth + 1);
                continue;
            }
            if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "log" | "txt" | "json") {
                continue;
            }
            let recent = metadata
                .modified()
                .ok()
                .and_then(|value| value.elapsed().ok())
                .map(|age| age <= Duration::from_secs(7 * 24 * 3600))
                .unwrap_or(false);
            if recent {
                if let Ok(mtime) = metadata.modified() {
                    files.push((path, mtime));
                }
            }
        }
    }
    let mut files = Vec::new();
    collect_logs(root, &mut files, 0);
    files.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (path, _) in files.iter().take(8) {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let lower = text.to_ascii_lowercase();
        if lower.contains("you've hit your usage limit")
            || lower.contains("you've hit your free requests limit")
            || lower.contains("free requests limit")
            || lower.contains("agent blocked")
        {
            return true;
        }
    }
    false
}

fn cursor_agent_blocked(reset_ms: Option<i64>) -> bool {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    cursor_agent_blocked_in(&home.join("Library/Application Support/Cursor/logs"), reset_ms)
}

pub async fn collect_cursor(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("cursor") {
        return vec![];
    }
    let accounts = cursor_accounts();
    if accounts.is_empty() {
        return vec![unavailable_snapshot(
            "cursor",
            "".into(),
            "Free".into(),
            "web",
            SourceHealth::Unavailable,
            "Cursor session credentials not found",
            75,
        )];
    }
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "cursor",
                "".into(),
                "Free".into(),
                "web",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let mut rows = Vec::with_capacity(accounts.len());
    for (token, label) in accounts {
        let key = account_key("cursor", &token);
        let usage_result = request_cursor_json(
            &client,
            "https://cursor.com/api/usage-summary",
            &token,
            reqwest::Method::GET,
        )
        .await;
        let usage = match usage_result {
            Ok(value) => value,
            Err((health, detail)) => {
                rows.push(unavailable_snapshot(
                    "cursor", key, label, "web", health, &detail, 75,
                ));
                continue;
            }
        };
        let user = request_cursor_json(
            &client,
            "https://cursor.com/api/auth/me",
            &token,
            reqwest::Method::GET,
        )
        .await
        .ok();
        let user_email = user
            .as_ref()
            .and_then(|value| value.get("email"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let summary = usage
            .get("individualUsage")
            .and_then(|value| value.get("plan"))
            .unwrap_or(&Value::Null);
        let (reset_ms, reset_text) = cursor_reset(&usage);
        let mut windows = vec![];
        let auto_used = cursor_number(summary.get("autoPercentUsed"));
        let api_used = cursor_number(summary.get("apiPercentUsed"));
        if let Some(window) = cursor_window(
            "Cursor Models",
            cursor_remaining_percent(auto_used),
            reset_text.clone(),
            reset_ms,
        ) {
            windows.push(window);
        }
        if let Some(window) = cursor_window(
            "Other Models",
            cursor_remaining_percent(api_used),
            reset_text.clone(),
            reset_ms,
        ) {
            windows.push(window);
        }
        if windows.is_empty() {
            let used = cursor_number(summary.get("totalPercentUsed"));
            if let Some(window) = cursor_window(
                "Overall",
                cursor_remaining_percent(used),
                reset_text.clone(),
                reset_ms,
            ) {
                windows.push(window);
            }
        }
        let membership = usage
            .get("membershipType")
            .and_then(Value::as_str)
            .unwrap_or("Free");
        let account_label = user_email.unwrap_or(label);
        let mut provider = connected_snapshot(
            "cursor",
            key,
            account_label,
            membership.into(),
            "web",
            windows,
            75,
        );
        if cursor_agent_blocked(reset_ms) {
            provider.availability = Availability::AgentBlocked;
            provider
                .diagnostics
                .push("Cursor Agent blocked by a provider usage gate".into());
        }
        rows.push(provider);
    }
    rows
}

async fn request_bearer_json(
    client: &Client,
    url: &str,
    token: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Value, (SourceHealth, String)> {
    let mut request = client
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("User-Agent", "token-monitor-rust");
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| (SourceHealth::Unavailable, "request failed".to_owned()))?;
    let status = response.status();
    if !status.is_success() {
        return Err((status_for_http(status), format!("HTTP {}", status.as_u16())));
    }
    response.json::<Value>().await.map_err(|_| {
        (
            SourceHealth::Unavailable,
            "invalid JSON response".to_owned(),
        )
    })
}

async fn request_cookie_json(
    client: &Client,
    url: &str,
    cookie: &str,
) -> Result<Value, (SourceHealth, String)> {
    let response = client
        .get(url)
        .header("Cookie", cookie)
        .header("Accept", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://claude.ai/")
        .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
        .send()
        .await
        .map_err(|_| {
            (
                SourceHealth::Unavailable,
                "Claude web request failed".to_owned(),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err((
            status_for_http(status),
            format!("Claude web HTTP {}", status.as_u16()),
        ));
    }
    response.json::<Value>().await.map_err(|_| {
        (
            SourceHealth::Unavailable,
            "Invalid Claude web JSON".to_owned(),
        )
    })
}

fn claude_credentials() -> Option<String> {
    // A value entered through the TUI's Claude field takes precedence over an
    // older shell-exported token. Cookie-shaped values stay on the web path.
    if let Some(value) = env_secret(&["TOKEN_MONITOR_CLAUDE_WEB_COOKIE", "CLAUDE_WEB_COOKIE"])
        .filter(|value| !looks_like_cookie_header(value))
    {
        return Some(value);
    }
    if let Some(token) = env_secret(&[
        "TOKEN_MONITOR_CLAUDE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_PERSONAL",
        "ANTHROPIC_OAUTH_TOKEN",
    ]) {
        return Some(token);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let candidates = [
        home.join(".claude/.credentials.json"),
        home.join(".claude/credentials.json"),
    ];
    for path in candidates {
        let value = fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let token = value
            .as_ref()
            .and_then(|value| {
                value
                    .get("claudeAiOauth")
                    .or_else(|| value.get("oauth"))
                    .or(Some(value))
            })
            .and_then(|value| value.get("accessToken"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        if let Some(token) = token {
            return Some(token.to_owned());
        }
    }
    None
}

fn normalize_claude_web_cookie(value: &str) -> Option<String> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    let stripped = raw
        .strip_prefix("Cookie:")
        .or_else(|| raw.strip_prefix("cookie:"))
        .unwrap_or(raw)
        .trim();
    if stripped.to_ascii_lowercase().starts_with("sessionkey=") {
        return Some(stripped.to_owned());
    }
    if stripped.starts_with("sk-ant-sid") {
        return Some(format!("sessionKey={stripped}"));
    }
    if stripped.contains('=')
        && (stripped.contains(';')
            || stripped.to_ascii_lowercase().contains("sessionkey"))
    {
        return Some(stripped.to_owned());
    }
    None
}

fn looks_like_cookie_header(value: &str) -> bool {
    normalize_claude_web_cookie(value).is_some()
}

fn claude_web_cookie() -> Option<String> {
    env_secret(&["TOKEN_MONITOR_CLAUDE_WEB_COOKIE", "CLAUDE_WEB_COOKIE"])
        .and_then(|value| normalize_claude_web_cookie(&value))
}

fn claude_used_percent(value: &Value) -> Option<f64> {
    number(
        value
            .get("usedPercent")
            .or_else(|| value.get("used_percent"))
            .or_else(|| value.get("utilization"))
            .or_else(|| value.get("percent")),
    )
}

fn claude_remaining_window(label: &str, value: &Value) -> Option<LimitWindow> {
    let used = claude_used_percent(value)?;
    Some(LimitWindow {
        label: label.into(),
        kind: if label == "5h" {
            WindowKind::Session
        } else {
            WindowKind::Weekly
        },
        metric: WindowMetric::Quota,
        remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
        remaining_amount: None,
        currency: None,
        resets_at_ms: reset_timestamp(value.get("resets_at").or_else(|| value.get("resetsAt"))),
        reset_text: value
            .get("resets_at")
            .or_else(|| value.get("resetsAt"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        estimated: false,
    })
}

fn claude_money(value: Option<&Value>) -> Option<(f64, Option<String>)> {
    let value = value?;
    let minor = number(
        value
            .get("amount_minor")
            .or_else(|| value.get("amountMinor")),
    )?;
    let exponent = number(value.get("exponent")).unwrap_or(2.0);
    let currency = value
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((minor / 10_f64.powf(exponent.max(0.0)), currency))
}

fn claude_spend_window(usage: &Value) -> Option<LimitWindow> {
    let spend = usage.get("spend")?;
    if spend.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let used = claude_money(spend.get("used"));
    let limit = claude_money(spend.get("limit"));
    let (used, currency) = used.or_else(|| {
        let extra = usage.get("extra_usage")?;
        let places = number(extra.get("decimal_places")).unwrap_or(2.0).max(0.0);
        let amount = number(extra.get("used_credits"))? / 10_f64.powf(places);
        Some((
            amount,
            extra
                .get("currency")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ))
    })?;
    let limit_currency = limit.as_ref().and_then(|(_, currency)| currency.clone());
    let limit_amount = limit.as_ref().map(|(amount, _)| *amount).or_else(|| {
        let extra = usage.get("extra_usage")?;
        let places = number(extra.get("decimal_places")).unwrap_or(2.0).max(0.0);
        Some(number(extra.get("monthly_limit"))? / 10_f64.powf(places))
    });
    Some(LimitWindow {
        label: "Usage credits".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Spend,
        remaining_percent: limit_amount
            .map(|limit| ((limit - used).max(0.0) / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: limit_amount.map(|limit| (limit - used).max(0.0)),
        currency: currency.or(limit_currency).or_else(|| Some("USD".into())),
        resets_at_ms: None,
        reset_text: None,
        estimated: false,
    })
}

fn claude_web_organization_id(value: &Value) -> Option<String> {
    let organizations = value
        .as_array()
        .or_else(|| value.get("organizations").and_then(Value::as_array))
        .or_else(|| value.get("data").and_then(Value::as_array))?;
    organizations.iter().find_map(|organization| {
        organization
            .get("uuid")
            .or_else(|| organization.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned)
    })
}

fn claude_web_plan(account: &Value) -> String {
    let tiers = account
        .get("memberships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|membership| membership.get("organization"))
        .filter_map(|organization| {
            organization
                .get("rate_limit_tier")
                .or_else(|| organization.get("rateLimitTier"))
                .and_then(Value::as_str)
        })
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if tiers.iter().any(|tier| tier.contains("enterprise")) {
        "Enterprise".into()
    } else if tiers.iter().any(|tier| tier.contains("team")) {
        "Team".into()
    } else if tiers.iter().any(|tier| {
        tier.contains("max")
            || tier.contains("pro")
            || tier.contains("claude_ai")
            || tier.contains("trust_tier")
    }) {
        "Pro".into()
    } else {
        "Claude".into()
    }
}

fn claude_web_balance_window(balance: &Value) -> Option<LimitWindow> {
    let amount = number(balance.get("amount").or_else(|| balance.get("balance")))?;
    let currency = balance
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("USD")
        .to_owned();
    Some(LimitWindow {
        label: "Balance".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Credits,
        remaining_percent: None,
        // Claude's `amount` is a minor-unit integer (7227 = $72.27).
        remaining_amount: Some((amount / 100.0).max(0.0)),
        currency: Some(currency),
        resets_at_ms: reset_timestamp(
            balance
                .get("next_expires_at")
                .or_else(|| balance.get("nextExpiresAt")),
        ),
        reset_text: None,
        estimated: false,
    })
}

async fn collect_claude_web(
    options: &CollectorOptions,
    cookie: &str,
) -> Result<ProviderSnapshot, (SourceHealth, String)> {
    let client = Client::builder()
        .timeout(options.timeout())
        .build()
        .map_err(|_| (SourceHealth::Unavailable, "HTTP client unavailable".into()))?;
    let organizations =
        request_cookie_json(&client, "https://claude.ai/api/organizations", cookie).await?;
    let organization_id = claude_web_organization_id(&organizations).ok_or((
        SourceHealth::Unavailable,
        "Claude web response has no organization".into(),
    ))?;
    let usage = request_cookie_json(
        &client,
        &format!("https://claude.ai/api/organizations/{organization_id}/usage"),
        cookie,
    )
    .await?;
    let mut windows = vec![];
    for (label, keys) in [
        ("5h", ["five_hour", "fiveHour"].as_slice()),
        ("7d", ["seven_day", "sevenDay"].as_slice()),
    ] {
        if let Some(value) = keys
            .iter()
            .find_map(|key| usage.get(*key))
            .and_then(|value| claude_remaining_window(label, value))
        {
            windows.push(value);
        }
    }
    if let Some(value) = claude_spend_window(&usage) {
        windows.push(value);
    }
    let account = request_cookie_json(&client, "https://claude.ai/api/account", cookie)
        .await
        .unwrap_or(Value::Null);
    let email = account
        .get("email_address")
        .or_else(|| account.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("Claude");
    let plan = account
        .get("subscription_type")
        .or_else(|| account.get("subscriptionType"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|plan| !plan.trim().is_empty())
        .unwrap_or_else(|| claude_web_plan(&account));
    // Prepaid balance is optional. A failure here must not erase the usage
    // windows that already succeeded.
    if let Ok(balance) = request_cookie_json(
        &client,
        &format!("https://claude.ai/api/organizations/{organization_id}/prepaid/credits"),
        cookie,
    )
    .await
    {
        if let Some(window) = claude_web_balance_window(&balance) {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err((
            SourceHealth::Unavailable,
            "Claude web response has no quota windows".into(),
        ));
    }
    Ok(connected_snapshot(
        "claude",
        account_key("claude-web", cookie),
        email.into(),
        plan,
        "web",
        windows,
        209,
    ))
}

pub async fn collect_claude(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("claude") {
        return vec![];
    }
    if let Some(cookie) = claude_web_cookie() {
        match collect_claude_web(options, &cookie).await {
            Ok(provider) => return vec![provider],
            Err((health, detail)) if claude_credentials().is_none() => {
                return vec![unavailable_snapshot(
                    "claude",
                    account_key("claude-web", &cookie),
                    "Claude".into(),
                    "web",
                    health,
                    &detail,
                    209,
                )]
            }
            Err(_) => {}
        }
    }
    let token = match claude_credentials() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "claude",
                "".into(),
                "Claude".into(),
                "oauth",
                SourceHealth::Unavailable,
                "Claude OAuth credentials not found",
                209,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "claude",
                account_key("claude", &token),
                "Claude".into(),
                "oauth",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                209,
            )]
        }
    };
    let (usage_result, profile_result) = tokio::join!(
        request_bearer_json(
            &client,
            "https://api.anthropic.com/api/oauth/usage",
            &token,
            &[("anthropic-beta", "oauth-2025-04-20")]
        ),
        request_bearer_json(
            &client,
            "https://api.anthropic.com/api/oauth/profile",
            &token,
            &[]
        ),
    );
    let usage = match usage_result {
        Ok(value) => value,
        Err((health, detail)) => {
            return vec![unavailable_snapshot(
                "claude",
                account_key("claude", &token),
                "Claude".into(),
                "oauth",
                health,
                &detail,
                209,
            )]
        }
    };
    let mut windows = vec![];
    if let Some(value) = usage
        .get("five_hour")
        .or_else(|| usage.get("fiveHour"))
        .and_then(|value| claude_remaining_window("5h", value))
    {
        windows.push(value);
    }
    if let Some(value) = usage
        .get("seven_day")
        .or_else(|| usage.get("sevenDay"))
        .and_then(|value| claude_remaining_window("7d", value))
    {
        windows.push(value);
    }
    if let Some(value) = claude_spend_window(&usage) {
        windows.push(value);
    }
    let profile = profile_result.ok().unwrap_or(Value::Null);
    let account = profile.get("account").unwrap_or(&profile);
    let email = account
        .get("email")
        .or_else(|| account.get("email_address"))
        .and_then(Value::as_str)
        .unwrap_or("Claude");
    let plan = account
        .get("subscriptionType")
        .or_else(|| account.get("subscription_type"))
        .and_then(Value::as_str)
        .unwrap_or("Claude");
    vec![connected_snapshot(
        "claude",
        account_key("claude", &token),
        email.into(),
        plan.into(),
        "oauth",
        windows,
        209,
    )]
}

fn openrouter_key_window(data: &Value) -> Option<LimitWindow> {
    let limit = number(data.get("limit"))?;
    if limit <= 0.0 {
        return None;
    }
    let usage = number(data.get("usage"));
    let provided_remaining = number(data.get("limit_remaining"));
    let used = usage.unwrap_or_else(|| (limit - provided_remaining.unwrap_or(0.0)).max(0.0));
    let remaining = provided_remaining.unwrap_or_else(|| (limit - used).max(0.0));
    let reset = data
        .get("limit_reset")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let (kind, label) = match reset.as_str() {
        "daily" => (WindowKind::Session, "Daily limit"),
        "weekly" => (WindowKind::Weekly, "Weekly limit"),
        "monthly" => (WindowKind::Monthly, "Monthly limit"),
        _ => (WindowKind::Billing, "API key limit"),
    };
    Some(LimitWindow {
        label: label.into(),
        kind,
        metric: WindowMetric::Quota,
        remaining_percent: Some((remaining / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: Some(remaining),
        currency: Some("USD".into()),
        resets_at_ms: None,
        reset_text: (!reset.is_empty()).then_some(reset),
        estimated: false,
    })
}

fn openrouter_credit_window(data: &Value) -> Option<LimitWindow> {
    let total = number(data.get("total_credits"))?;
    let used = number(data.get("total_usage"))?;
    if total < 0.0 || used < 0.0 {
        return None;
    }
    let remaining = (total - used).max(0.0);
    Some(LimitWindow {
        label: "Credits".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Credits,
        remaining_percent: Some(if total > 0.0 {
            remaining / total * 100.0
        } else {
            100.0
        }),
        remaining_amount: Some(remaining),
        currency: Some("USD".into()),
        resets_at_ms: None,
        reset_text: None,
        estimated: false,
    })
}

pub async fn collect_openrouter(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("openrouter") {
        return vec![];
    }
    let token = match env_secret(&["TOKEN_MONITOR_OPENROUTER_API_KEY", "OPENROUTER_API_KEY"]) {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "openrouter",
                "".into(),
                "environment".into(),
                "api",
                SourceHealth::Unavailable,
                "API key not configured",
                141,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "openrouter",
                account_key("openrouter", &token),
                "environment".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                141,
            )]
        }
    };
    let (key_result, credits_result) = tokio::join!(
        request_json(&client, "https://openrouter.ai/api/v1/key", &token),
        request_json(&client, "https://openrouter.ai/api/v1/credits", &token),
    );
    let key_data = key_result
        .as_ref()
        .ok()
        .and_then(|value| value.get("data"))
        .unwrap_or(&Value::Null);
    let credits_data = credits_result
        .as_ref()
        .ok()
        .and_then(|value| value.get("data"))
        .unwrap_or(&Value::Null);
    if key_result.is_err() && credits_result.is_err() {
        let health = key_result
            .err()
            .map(|error| error.0)
            .or_else(|| credits_result.err().map(|error| error.0))
            .unwrap_or(SourceHealth::Unavailable);
        return vec![unavailable_snapshot(
            "openrouter",
            account_key("openrouter", &token),
            "environment".into(),
            "api",
            health,
            "OpenRouter credentials or API unavailable",
            141,
        )];
    }
    let mut windows = vec![
        openrouter_key_window(key_data),
        openrouter_credit_window(credits_data),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if windows.is_empty() {
        return vec![unavailable_snapshot(
            "openrouter",
            account_key("openrouter", &token),
            "environment".into(),
            "api",
            SourceHealth::Unavailable,
            "No recognized credit or key-limit fields",
            141,
        )];
    }
    let plan = if key_data.get("is_free_tier").and_then(Value::as_bool) == Some(true) {
        "Free"
    } else if key_data.get("is_free_tier").and_then(Value::as_bool) == Some(false) {
        "Pay-as-you-go"
    } else if key_data.get("is_management_key").and_then(Value::as_bool) == Some(true) {
        "Management"
    } else {
        ""
    };
    windows.shrink_to_fit();
    vec![connected_snapshot(
        "openrouter",
        account_key("openrouter", &token),
        "environment".into(),
        plan.into(),
        "api",
        windows,
        141,
    )]
}

fn select_deepseek_balance(data: &Value) -> Option<(&Value, f64)> {
    let rows = data.get("balance_infos")?.as_array()?;
    rows.iter()
        .filter_map(|row| {
            Some((
                row,
                number(row.get("total_balance").or_else(|| row.get("balance")))?,
            ))
        })
        .max_by(|(_, left), (_, right)| {
            left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
        })
}

pub async fn collect_deepseek(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("deepseek") {
        return vec![];
    }
    let token = match env_secret(&["TOKEN_MONITOR_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"]) {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "deepseek",
                "".into(),
                "Pay-as-you-go".into(),
                "api",
                SourceHealth::Unavailable,
                "API key not configured",
                75,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "deepseek",
                account_key("deepseek", &token),
                "Pay-as-you-go".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let response = client
        .get("https://api.deepseek.com/user/balance")
        .bearer_auth(&token)
        .header("Accept", "application/json")
        .send()
        .await;
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "deepseek",
                account_key("deepseek", &token),
                "Pay-as-you-go".into(),
                "api",
                status_for_http(response.status()),
                "DeepSeek balance unavailable",
                75,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "deepseek",
                account_key("deepseek", &token),
                "Pay-as-you-go".into(),
                "api",
                SourceHealth::Unavailable,
                "DeepSeek request failed",
                75,
            )]
        }
    };
    let data = match response.json::<Value>().await {
        Ok(data) => data,
        Err(_) => {
            return vec![unavailable_snapshot(
                "deepseek",
                account_key("deepseek", &token),
                "Pay-as-you-go".into(),
                "api",
                SourceHealth::Unavailable,
                "Invalid DeepSeek JSON",
                75,
            )]
        }
    };
    let (row, amount) = match select_deepseek_balance(&data) {
        Some(value) => value,
        None => {
            return vec![unavailable_snapshot(
                "deepseek",
                account_key("deepseek", &token),
                "Pay-as-you-go".into(),
                "api",
                SourceHealth::Unavailable,
                "No DeepSeek balance row",
                75,
            )]
        }
    };
    let currency = row.get("currency").and_then(Value::as_str).unwrap_or("USD");
    vec![connected_snapshot(
        "deepseek",
        account_key("deepseek", &token),
        "Pay-as-you-go".into(),
        "Pay-as-you-go".into(),
        "api",
        vec![LimitWindow {
            label: "Balance".into(),
            kind: WindowKind::Billing,
            metric: WindowMetric::Credits,
            remaining_percent: None,
            remaining_amount: Some(amount),
            currency: Some(currency.into()),
            resets_at_ms: None,
            reset_text: None,
            estimated: false,
        }],
        75,
    )]
}

fn modal_profiles(options: &CollectorOptions) -> Vec<String> {
    if let Some(profiles) = &options.modal_profiles {
        return profiles.clone();
    }
    let path = options
        .modal_config
        .clone()
        .or_else(|| std::env::var_os("TOKEN_MONITOR_MODAL_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".modal.toml")
        });
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return vec![],
    };
    let mut profiles = text
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
        })
        .map(str::trim)
        .filter(|profile| !profile.is_empty() && !profile.contains('.'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    profiles.sort();
    profiles.dedup();
    profiles
}

async fn run_json_command(
    executable: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Value, String> {
    let mut command = tokio::process::Command::new(executable);
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| "command timeout".to_owned())?
        .map_err(|_| "command unavailable".to_owned())?;
    if !output.status.success() {
        return Err("command failed".to_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "invalid command JSON".to_owned())
}

async fn run_text_command(
    command: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let mut process = tokio::process::Command::new(command);
    process.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, process.output())
        .await
        .map_err(|_| "command timeout".to_owned())?
        .map_err(|_| "command unavailable".to_owned())?;
    if !output.status.success() {
        return Err("command failed".to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| "command returned invalid text".to_owned())
}

fn executable(env_name: &str, default: &Path) -> String {
    std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if default.is_file() {
                default.to_string_lossy().into_owned()
            } else {
                default
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .into()
            }
        })
}

fn next_utc_month_start_ms() -> i64 {
    use chrono::{Datelike, TimeZone};
    let now = chrono::Utc::now();
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    chrono::Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

pub async fn collect_modal(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("modal") {
        return vec![];
    }
    let profiles = modal_profiles(options);
    if profiles.is_empty() {
        return vec![unavailable_snapshot(
            "modal",
            "".into(),
            "".into(),
            "cli",
            SourceHealth::Unavailable,
            "No Modal profile",
            214,
        )];
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let executable = executable("TOKEN_MONITOR_MODAL_BIN", &home.join(".local/bin/modal"));
    let grant = options
        .modal_credit_grant_usd
        .or_else(|| {
            std::env::var("TOKEN_MONITOR_MODAL_CREDIT_GRANT")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(DEFAULT_MODAL_GRANT_USD);
    let tasks = profiles.into_iter().map(|profile| {
        let executable = executable.clone();
        async move {
            let result = run_json_command(
                &executable,
                &[
                    "billing",
                    "summary",
                    "--json",
                    "--for",
                    "this month",
                    "--profile",
                    &profile,
                ],
                options.timeout(),
            )
            .await;
            let payload = match result {
                Ok(payload) => payload,
                Err(detail) => {
                    return unavailable_snapshot(
                        "modal",
                        account_key("modal", &profile),
                        profile,
                        "cli",
                        SourceHealth::Unavailable,
                        &detail,
                        214,
                    )
                }
            };
            let adjustment = match number(
                payload
                    .get("adjustments")
                    .and_then(|value| value.get("credits")),
            ) {
                Some(value) => value,
                None => {
                    return unavailable_snapshot(
                        "modal",
                        account_key("modal", &profile),
                        profile,
                        "cli",
                        SourceHealth::Unavailable,
                        "No Modal credits adjustment",
                        214,
                    )
                }
            };
            let applied = (-adjustment).max(0.0);
            let remaining = (grant - applied).max(0.0);
            connected_snapshot(
                "modal",
                account_key("modal", &profile),
                profile,
                "Credits ~".into(),
                "cli",
                vec![LimitWindow {
                    label: "Credits ~".into(),
                    kind: WindowKind::Billing,
                    metric: WindowMetric::Credits,
                    remaining_percent: Some((remaining / grant * 100.0).clamp(0.0, 100.0)),
                    remaining_amount: Some(remaining),
                    currency: Some("USD".into()),
                    resets_at_ms: Some(next_utc_month_start_ms()),
                    reset_text: Some("monthly grant reset".into()),
                    estimated: true,
                }],
                214,
            )
        }
    });
    futures::future::join_all(tasks).await
}

pub async fn collect_vast(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("vast") {
        return vec![];
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let executable = executable("TOKEN_MONITOR_VASTAI_BIN", &home.join(".local/bin/vastai"));
    let result = run_json_command(&executable, &["--raw", "show", "user"], options.timeout()).await;
    let payload = match result {
        Ok(payload) => payload,
        Err(detail) => {
            return vec![unavailable_snapshot(
                "vast",
                "vast:account".into(),
                "Cloud GPU".into(),
                "cli",
                SourceHealth::Unavailable,
                &detail,
                220,
            )]
        }
    };
    let amount = number(payload.get("credit")).or_else(|| number(payload.get("balance")));
    let Some(amount) = amount else {
        return vec![unavailable_snapshot(
            "vast",
            "vast:account".into(),
            "Cloud GPU".into(),
            "cli",
            SourceHealth::Unavailable,
            "No Vast.ai credit field",
            220,
        )];
    };
    vec![connected_snapshot(
        "vast",
        "vast:account".into(),
        "Cloud GPU".into(),
        "Cloud GPU".into(),
        "cli",
        vec![LimitWindow {
            label: "Credit".into(),
            kind: WindowKind::Billing,
            metric: WindowMetric::Credits,
            remaining_percent: None,
            remaining_amount: Some(amount),
            currency: Some("USD".into()),
            resets_at_ms: None,
            reset_text: None,
            estimated: false,
        }],
        220,
    )]
}

fn codex_credentials() -> Option<(String, Option<String>)> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let value = fs::read_to_string(codex_home.join("auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    let token = value
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .or_else(|| value.get("access_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?
        .to_owned();
    let account_id = value
        .get("tokens")
        .and_then(|tokens| tokens.get("account_id"))
        .or_else(|| value.get("account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((token, account_id))
}

fn codex_window_kind(window: &Value, key: &str) -> WindowKind {
    let minutes = number(
        window
            .get("windowDurationMins")
            .or_else(|| window.get("window_duration_mins")),
    )
    .unwrap_or_else(|| {
        number(
            window
                .get("limitWindowSeconds")
                .or_else(|| window.get("limit_window_seconds")),
        )
        .unwrap_or(0.0)
            / 60.0
    });
    if minutes >= 30.0 * 24.0 * 60.0 {
        WindowKind::Billing
    } else if minutes >= 7.0 * 24.0 * 60.0 || key == "secondary" {
        WindowKind::Weekly
    } else if minutes >= 24.0 * 60.0 {
        WindowKind::Daily
    } else {
        WindowKind::Session
    }
}

fn codex_reset(value: Option<&Value>) -> (Option<i64>, Option<String>) {
    let Some(value) = value else {
        return (None, None);
    };
    if let Some(text) = value.as_str() {
        if let Ok(seconds) = text.parse::<i64>() {
            if seconds > 1_000_000_000 {
                return (
                    Some(seconds.saturating_mul(1000)),
                    Some(reset_duration_text(seconds.saturating_mul(1000))),
                );
            }
        }
        return (None, Some(text.to_owned()));
    }
    let Some(timestamp) = value.as_i64() else {
        return (None, None);
    };
    if timestamp > 1_000_000_000 {
        return (
            Some(timestamp.saturating_mul(1000)),
            Some(reset_duration_text(timestamp.saturating_mul(1000))),
        );
    }
    (None, None)
}

fn reset_duration_text(timestamp_ms: i64) -> String {
    let remaining = (timestamp_ms - now_ms()).max(0) / 1000;
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn codex_window(label: &str, window: &Value, key: &str) -> Option<LimitWindow> {
    let used = number(
        window
            .get("usedPercent")
            .or_else(|| window.get("used_percent")),
    )?;
    let (resets_at_ms, reset_text) = codex_reset(
        window
            .get("resetsAt")
            .or_else(|| window.get("resets_at"))
            .or_else(|| window.get("resetAt"))
            .or_else(|| window.get("reset_at")),
    );
    Some(LimitWindow {
        label: label.to_owned(),
        kind: codex_window_kind(window, key),
        metric: WindowMetric::Quota,
        remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
        remaining_amount: None,
        currency: None,
        resets_at_ms,
        reset_text,
        estimated: false,
    })
}

fn codex_rate_limits(payload: &Value) -> &Value {
    payload
        .get("rateLimit")
        .or_else(|| payload.get("rate_limit"))
        .or_else(|| payload.get("rateLimits"))
        .or_else(|| payload.get("rate_limits"))
        .unwrap_or(&Value::Null)
}

fn codex_plan(payload: &Value) -> String {
    let value = payload
        .get("planType")
        .or_else(|| payload.get("plan_type"))
        .or_else(|| {
            payload
                .get("account")
                .and_then(|account| account.get("planType"))
        })
        .or_else(|| {
            payload
                .get("account")
                .and_then(|account| account.get("plan_type"))
        })
        .or_else(|| {
            payload
                .get("account")
                .and_then(|account| account.get("plan"))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    match value.to_ascii_lowercase().as_str() {
        "pro" => "Pro".into(),
        "plus" => "Plus".into(),
        "free" => "Free".into(),
        "team" | "teams" => "Team".into(),
        "enterprise" => "Enterprise".into(),
        _ => value.into(),
    }
}

pub async fn collect_codex(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("codex") {
        return vec![];
    }
    let (token, account_id) = match codex_credentials() {
        Some(value) => value,
        None => {
            return vec![unavailable_snapshot(
                "codex",
                "".into(),
                "Codex".into(),
                "oauth",
                SourceHealth::Unavailable,
                "Codex auth.json not found",
                43,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "codex",
                account_key("codex", &token),
                "Codex".into(),
                "oauth",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                43,
            )]
        }
    };
    let mut request = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .bearer_auth(&token)
        .header("Accept", "application/json")
        .header("User-Agent", "token-monitor-rust");
    if let Some(account_id) = &account_id {
        request = request.header("chatgpt-account-id", account_id);
    }
    let response = match request.send().await {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "codex",
                account_key("codex", &token),
                "Codex".into(),
                "oauth",
                status_for_http(response.status()),
                "Codex usage unavailable",
                43,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "codex",
                account_key("codex", &token),
                "Codex".into(),
                "oauth",
                SourceHealth::Unavailable,
                "Codex request failed",
                43,
            )]
        }
    };
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return vec![unavailable_snapshot(
                "codex",
                account_key("codex", &token),
                "Codex".into(),
                "oauth",
                SourceHealth::Unavailable,
                "Invalid Codex JSON",
                43,
            )]
        }
    };
    let rates = codex_rate_limits(&payload);
    let mut windows = vec![];
    if let Some(window) = rates
        .get("primaryWindow")
        .or_else(|| rates.get("primary_window"))
        .or_else(|| rates.get("primary"))
        .and_then(|window| codex_window("5h", window, "primary"))
    {
        windows.push(window);
    }
    if let Some(window) = rates
        .get("secondaryWindow")
        .or_else(|| rates.get("secondary_window"))
        .or_else(|| rates.get("secondary"))
        .and_then(|window| codex_window("7d", window, "secondary"))
    {
        windows.push(window);
    }
    let email = payload
        .get("account")
        .and_then(|account| account.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("Codex");
    let reset_credits = payload
        .get("resetCredits")
        .or_else(|| payload.get("reset_credits"))
        .and_then(|rc| rc.get("availableCount").or_else(|| rc.get("available_count")))
        .and_then(Value::as_i64);
    let mut snapshot = connected_snapshot(
        "codex",
        account_key("codex", &token),
        email.into(),
        codex_plan(&payload),
        "oauth",
        windows,
        43,
    );
    if let Some(credits) = reset_credits.filter(|c| *c > 0) {
        snapshot.diagnostics.push(format!("{credits} reset credit available"));
    }
    vec![snapshot]
}

#[derive(Clone, Debug)]
struct AntigravityServer {
    pid: i64,
    csrf_token: String,
}

fn is_antigravity_command(command: &str) -> Option<Option<String>> {
    let lower = command.to_ascii_lowercase();
    let bin = lower.split_whitespace().next().unwrap_or("");
    let path = std::path::Path::new(bin);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or(bin);
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    if stem == "agy" || stem == "antigravity-cli" || stem == "antigravity_cli" {
        let csrf = extract_flag(command, "--csrf_token");
        return Some(csrf);
    }
    if (lower.contains("antigravity") || lower.contains("language_server") || lower.contains("language-server"))
        && (lower.contains("language") || lower.contains("--csrf_token") || lower.contains("app_data_dir"))
    {
        if let Some(csrf) = extract_flag(command, "--csrf_token") {
            return Some(Some(csrf));
        }
    }
    None
}

fn antigravity_servers(ps_output: &str) -> Vec<AntigravityServer> {
    let mut servers = vec![];
    for line in ps_output.lines() {
        let trimmed = line.trim();
        let Some((pid_text, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_text.trim().parse::<i64>() else {
            continue;
        };
        let Some(csrf) = is_antigravity_command(command) else {
            continue;
        };
        let csrf_token = csrf.unwrap_or_default();
        servers.push(AntigravityServer { pid, csrf_token });
    }
    servers.sort_by_key(|server| server.pid);
    servers.dedup_by_key(|server| server.pid);
    servers
}

fn extract_flag(command: &str, flag: &str) -> Option<String> {
    let needle = format!("{flag}=");
    if let Some(value) = command
        .split(&needle)
        .nth(1)
        .and_then(|value| value.split_whitespace().next())
    {
        if !value.is_empty() {
            return Some(value.trim_matches('"').to_owned());
        }
    }
    let mut parts = command.split_whitespace();
    while let Some(part) = parts.next() {
        if part == flag {
            return parts.next().map(|value| value.trim_matches('"').to_owned());
        }
    }
    None
}

async fn antigravity_ports(pid: i64, timeout: Duration) -> Vec<u16> {
    let output = match run_text_command(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pid.to_string()],
        timeout,
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return vec![],
    };
    let mut ports = output
        .split_whitespace()
        .filter_map(|part| {
            part.rsplit_once(':')
                .and_then(|(_, number)| number.trim_end_matches("(LISTEN)").parse::<u16>().ok())
        })
        .collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn reset_timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(value) => {
            let raw = value.as_i64()?;
            Some(if raw < 20_000_000_000 {
                raw.saturating_mul(1000)
            } else {
                raw
            })
        }
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .map(|raw| {
                if raw < 20_000_000_000 {
                    raw.saturating_mul(1000)
                } else {
                    raw
                }
            })
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|date| date.timestamp_millis())
            }),
        _ => None,
    }
}

fn antigravity_group_name(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("gemini") {
        "Gemini".into()
    } else if lower.contains("claude") || lower.contains("gpt") {
        "Claude/GPT".into()
    } else {
        value.trim().to_owned()
    }
}

fn antigravity_bucket_kind(bucket: &Value) -> Option<WindowKind> {
    for key in ["window", "bucketId", "displayName"] {
        let value = bucket
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .replace('_', "-");
        if value == "weekly" || value.ends_with("-weekly") {
            return Some(WindowKind::Weekly);
        }
        if ["session", "5h", "5-hour", "five-hour"]
            .iter()
            .any(|alias| value == *alias || value.ends_with(&format!("-{alias}")))
        {
            return Some(WindowKind::Session);
        }
    }
    None
}

fn antigravity_remaining_fraction(bucket: &Value) -> Option<f64> {
    number(bucket.get("remainingFraction")).or_else(|| {
        number(
            bucket
                .get("remaining")
                .and_then(|value| value.get("remainingFraction")),
        )
    })
}

fn antigravity_windows(payload: &Value) -> Vec<LimitWindow> {
    let summary = payload
        .get("response")
        .or_else(|| payload.get("summary"))
        .unwrap_or(payload);
    let groups = summary
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut windows = vec![];
    for group in groups {
        let group_name = antigravity_group_name(
            group
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Quota"),
        );
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(kind) = antigravity_bucket_kind(&bucket) else {
                continue;
            };
            let disabled = bucket
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let remaining = if disabled {
                None
            } else {
                antigravity_remaining_fraction(&bucket)
            };
            let label = format!(
                "{} {}",
                group_name,
                if kind == WindowKind::Session {
                    "5h"
                } else {
                    "7d"
                }
            );
            windows.push(LimitWindow {
                label,
                kind,
                metric: WindowMetric::Quota,
                remaining_percent: remaining.map(|fraction| (fraction * 100.0).clamp(0.0, 100.0)),
                remaining_amount: None,
                currency: None,
                resets_at_ms: reset_timestamp(bucket.get("resetTime")),
                reset_text: None,
                estimated: false,
            });
        }
    }
    windows.sort_by_key(|window| {
        (
            if window.label.starts_with("Gemini") {
                0
            } else {
                1
            },
            if window.kind == WindowKind::Session {
                0
            } else {
                1
            },
        )
    });
    windows
}

async fn antigravity_call(
    client: &Client,
    scheme: &str,
    port: u16,
    csrf: &str,
    method: &str,
    body: Value,
) -> Result<Value, String> {
    let url = format!(
        "{scheme}://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/{method}"
    );
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .header("User-Agent", "token-monitor-rust");
    if !csrf.is_empty() {
        request = request.header("x-codeium-csrf-token", csrf);
    }
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|_| "Antigravity RPC request failed".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "Antigravity RPC HTTP {}",
            response.status().as_u16()
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "Invalid Antigravity RPC JSON".to_owned())
}

pub async fn collect_antigravity(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("antigravity") {
        return vec![];
    }
    let ps = match run_text_command("ps", &["-axo", "pid=,command="], options.timeout()).await {
        Ok(output) => output,
        Err(_) => {
            return vec![unavailable_snapshot(
                "antigravity",
                "".into(),
                "Pro".into(),
                "rpc",
                SourceHealth::Unavailable,
                "Process list unavailable",
                141,
            )]
        }
    };
    let servers = antigravity_servers(&ps);
    if servers.is_empty() {
        return vec![unavailable_snapshot(
            "antigravity",
            "".into(),
            "Pro".into(),
            "rpc",
            SourceHealth::Unavailable,
            "Antigravity language server not running",
            141,
        )];
    }
    let client = match Client::builder()
        .timeout(options.timeout())
        .danger_accept_invalid_certs(true)
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "antigravity",
                "".into(),
                "Pro".into(),
                "rpc",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                141,
            )]
        }
    };
    for server in servers {
        let ports = antigravity_ports(server.pid, options.timeout()).await;
        for port in ports {
            for scheme in ["http", "https"] {
                let result = antigravity_call(
                    &client,
                    scheme,
                    port,
                    &server.csrf_token,
                    "RetrieveUserQuotaSummary",
                    serde_json::json!({"forceRefresh": true}),
                )
                .await;
                let Ok(payload) = result else {
                    continue;
                };
                let windows = antigravity_windows(&payload);
                if windows.is_empty() {
                    continue;
                }
                let identity = antigravity_call(
                    &client,
                    scheme,
                    port,
                    &server.csrf_token,
                    "GetUserStatus",
                    serde_json::json!({"metadata": {"ide": "antigravity", "locale": "en"}}),
                )
                .await
                .ok();
                let email = identity
                    .as_ref()
                    .and_then(|value| value.get("userStatus"))
                    .and_then(|value| value.get("email"))
                    .and_then(Value::as_str)
                    .unwrap_or("Antigravity");
                let plan = identity
                    .as_ref()
                    .and_then(|value| value.get("userStatus"))
                    .and_then(|value| value.get("userTier"))
                    .and_then(|value| value.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Pro");
                return vec![connected_snapshot(
                    "antigravity",
                    account_key("antigravity", email),
                    email.into(),
                    plan.into(),
                    "rpc",
                    windows,
                    141,
                )];
            }
        }
    }
    vec![unavailable_snapshot(
        "antigravity",
        "".into(),
        "Pro".into(),
        "rpc",
        SourceHealth::Unavailable,
        "Antigravity quota RPC unavailable",
        141,
    )]
}

async fn grok_rpc_billing(options: &CollectorOptions) -> Result<Value, String> {
    let command = std::env::var("GROK_CLI_PATH").unwrap_or_else(|_| "grok".into());
    let mut process = tokio::process::Command::new(command);
    process
        .args(["agent", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .map_err(|_| "Grok CLI unavailable".to_owned())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Grok CLI stdin unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Grok CLI stdout unavailable".to_owned())?;
    let mut reader = tokio::io::BufReader::new(stdout).lines();
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"1\",\"clientCapabilities\":{\"fs\":{\"readTextFile\":false,\"writeTextFile\":false},\"terminal\":false}}}\n")
        .await
        .map_err(|_| "Grok CLI initialize failed".to_owned())?;
    stdin
        .flush()
        .await
        .map_err(|_| "Grok CLI initialize failed".to_owned())?;
    let mut initialized = false;
    loop {
        let line = tokio::time::timeout(options.timeout(), reader.next_line())
            .await
            .map_err(|_| "Grok RPC timeout".to_owned())?
            .map_err(|_| "Grok RPC read failed".to_owned())?
            .ok_or_else(|| "Grok RPC exited before billing response".to_owned())?;
        let message = serde_json::from_str::<Value>(&line)
            .map_err(|_| "Grok RPC returned invalid JSON".to_owned())?;
        if message.get("error").is_some() {
            return Err("Grok RPC rejected request".to_owned());
        }
        let id = message.get("id").and_then(Value::as_i64);
        if id == Some(1) && !initialized {
            initialized = true;
            stdin
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"x.ai/billing\",\"params\":{}}\n",
                )
                .await
                .map_err(|_| "Grok CLI billing request failed".to_owned())?;
            stdin
                .flush()
                .await
                .map_err(|_| "Grok CLI billing request failed".to_owned())?;
        } else if id == Some(2) {
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            let _ = child.kill().await;
            return if result.is_object() {
                Ok(result)
            } else {
                Err("Grok billing response missing result".to_owned())
            };
        }
    }
}

fn grok_billing_window(payload: &Value) -> Option<LimitWindow> {
    let config = payload
        .get("config")
        .filter(|value| value.is_object())
        .unwrap_or(payload);
    let used_percent = number(
        config
            .get("creditUsagePercent")
            .or_else(|| config.get("credit_usage_percent")),
    )
    .or_else(|| {
        number(
            config
                .get("usedPercent")
                .or_else(|| config.get("used_percent")),
        )
    });
    let reset = config
        .get("currentPeriod")
        .or_else(|| config.get("current_period"))
        .and_then(|period| {
            period
                .get("end")
                .or_else(|| period.get("billingPeriodEnd"))
                .or_else(|| period.get("billing_period_end"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(used) = used_percent {
        return Some(LimitWindow {
            label: "Weekly".into(),
            kind: WindowKind::Weekly,
            metric: WindowMetric::Quota,
            remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
            remaining_amount: None,
            currency: None,
            resets_at_ms: None,
            reset_text: reset,
            estimated: false,
        });
    }
    let limit = number(config.get("monthlyLimit"));
    let used = number(config.get("used").or_else(|| config.get("totalUsed"))).unwrap_or(0.0);
    limit.filter(|value| *value > 0.0).map(|limit| LimitWindow {
        label: "Monthly".into(),
        kind: WindowKind::Monthly,
        metric: WindowMetric::Quota,
        remaining_percent: Some(((limit - used).max(0.0) / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: None,
        currency: None,
        resets_at_ms: None,
        reset_text: reset,
        estimated: false,
    })
}

fn grok_auth_identity() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let value = fs::read_to_string(home.join(".grok/auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    value.as_object()?.values().find_map(|entry| {
        entry
            .get("email")
            .and_then(Value::as_str)
            .filter(|email| !email.trim().is_empty())
            .map(str::to_owned)
    })
}

fn grok_auth_token() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let value = fs::read_to_string(home.join(".grok/auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    value.as_object()?.values().find_map(|entry| {
        entry
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.trim().is_empty())
            .map(str::to_owned)
    })
}

#[derive(Default)]
struct ProtoScan {
    fixed32: Vec<(Vec<u32>, f32, usize)>,
    varints: Vec<(Vec<u32>, u64, usize)>,
    order: usize,
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while *offset < bytes.len() && shift <= 63 {
        let byte = bytes[*offset];
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn scan_proto(bytes: &[u8], depth: u8, path: &[u32], scan: &mut ProtoScan) {
    if depth > 8 {
        return;
    }
    let mut offset = 0usize;
    while offset < bytes.len() {
        let key = match read_varint(bytes, &mut offset) {
            Some(value) => value,
            None => return,
        };
        let field = (key >> 3) as u32;
        let wire = key & 7;
        if field == 0 {
            return;
        }
        let mut next_path = path.to_vec();
        next_path.push(field);
        match wire {
            0 => {
                let value = match read_varint(bytes, &mut offset) {
                    Some(value) => value,
                    None => return,
                };
                let order = scan.order;
                scan.order += 1;
                scan.varints.push((next_path, value, order));
            }
            1 => {
                if offset.checked_add(8).is_none_or(|end| end > bytes.len()) {
                    return;
                }
                offset += 8;
            }
            2 => {
                let length = match read_varint(bytes, &mut offset)
                    .and_then(|value| usize::try_from(value).ok())
                {
                    Some(length) => length,
                    None => return,
                };
                let end = match offset.checked_add(length) {
                    Some(end) if end <= bytes.len() => end,
                    _ => return,
                };
                if length > 0 {
                    scan_proto(&bytes[offset..end], depth + 1, &next_path, scan);
                }
                offset = end;
            }
            5 => {
                let end = match offset.checked_add(4) {
                    Some(end) if end <= bytes.len() => end,
                    _ => return,
                };
                let value = f32::from_le_bytes(bytes[offset..end].try_into().unwrap());
                let order = scan.order;
                scan.order += 1;
                scan.fixed32.push((next_path, value, order));
                offset = end;
            }
            _ => return,
        }
    }
}

fn grpc_data_frames(bytes: &[u8]) -> Vec<&[u8]> {
    let mut frames = vec![];
    let mut offset = 0usize;
    while offset + 5 <= bytes.len() {
        let flags = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let start = offset + 5;
        let end = match start.checked_add(length) {
            Some(end) if end <= bytes.len() => end,
            _ => return vec![],
        };
        if flags & 0x80 == 0 {
            frames.push(&bytes[start..end]);
        }
        offset = end;
    }
    frames
}

fn grpc_trailer_status(bytes: &[u8]) -> Option<String> {
    let mut offset = 0usize;
    while offset + 5 <= bytes.len() {
        let flags = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let start = offset + 5;
        let end = start.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if flags & 0x80 != 0 {
            let text = String::from_utf8_lossy(&bytes[start..end]);
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("grpc-status:") {
                    return Some(value.trim().to_owned());
                }
            }
        }
        offset = end;
    }
    None
}

fn parse_grok_grpc_billing(bytes: &[u8]) -> Option<LimitWindow> {
    let frames = grpc_data_frames(bytes);
    if frames.is_empty() {
        return None;
    }
    let mut scan = ProtoScan::default();
    for frame in frames {
        scan_proto(frame, 0, &[], &mut scan);
    }
    let now = now_ms();
    let percent = scan
        .fixed32
        .iter()
        .filter(|(path, value, _)| {
            path.last() == Some(&1) && value.is_finite() && (0.0..=100.0).contains(value)
        })
        .min_by_key(|(path, _, order)| (path.len(), *order))
        .map(|(_, value, _)| f64::from(*value));
    let reset = scan
        .varints
        .iter()
        .filter_map(|(path, value, order)| {
            if (1_700_000_000..=2_100_000_000).contains(value) {
                let millis = (*value as i64).saturating_mul(1000);
                Some((path, millis, *order))
            } else {
                None
            }
        })
        .filter(|(_, millis, _)| *millis > now)
        .min_by_key(|(_, millis, _)| *millis)
        .map(|(_, millis, _)| millis);
    let used = percent?;
    Some(LimitWindow {
        label: "Weekly".into(),
        kind: WindowKind::Weekly,
        metric: WindowMetric::Quota,
        remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
        remaining_amount: None,
        currency: None,
        resets_at_ms: reset,
        reset_text: None,
        estimated: false,
    })
}

async fn grok_web_billing(options: &CollectorOptions, token: &str) -> Result<LimitWindow, String> {
    let client = Client::builder()
        .timeout(options.timeout())
        .build()
        .map_err(|_| "HTTP client unavailable".to_owned())?;
    let response = client
        .post("https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("Accept", "*/*")
        .header("Content-Type", "application/grpc-web+proto")
        .header("User-Agent", "Grok Build")
        .header("Origin", "https://grok.com")
        .header("Referer", "https://grok.com/?_s=usage")
        .header("x-grpc-web", "1")
        .header("x-user-agent", "connect-es/2.1.1")
        .body(vec![0, 0, 0, 0, 0])
        .send()
        .await
        .map_err(|_| "Grok web billing request failed".to_owned())?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err("Grok web billing credentials rejected".into());
    }
    if !response.status().is_success() {
        return Err("Grok web billing unavailable".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "Grok web billing body unavailable".to_owned())?;
    if grpc_trailer_status(&bytes)
        .as_deref()
        .is_some_and(|status| status != "0")
    {
        return Err("Grok web billing RPC rejected".into());
    }
    parse_grok_grpc_billing(&bytes).ok_or_else(|| "Could not parse Grok web billing usage".into())
}

pub async fn collect_grok(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("grok") {
        return vec![];
    }
    let email = grok_auth_identity().unwrap_or_else(|| "SuperGrok".into());
    let rpc_result = grok_rpc_billing(options).await.and_then(|payload| {
        grok_billing_window(&payload).ok_or_else(|| "Grok billing response has no quota".to_owned())
    });
    let result = match rpc_result {
        Ok(window) => Ok(window),
        Err(_) => match grok_auth_token() {
            Some(token) => grok_web_billing(options, &token).await,
            None => Err("Grok credentials not configured".to_owned()),
        },
    };
    match result {
        Ok(window) => vec![connected_snapshot(
            "grok",
            account_key("grok", &email),
            email,
            "SuperGrok".into(),
            "web",
            vec![window],
            75,
        )],
        Err(detail) => vec![unavailable_snapshot(
            "grok",
            account_key("grok", &email),
            email,
            "rpc",
            SourceHealth::Unavailable,
            &detail,
            75,
        )],
    }
}

fn commandcode_cookie() -> Option<String> {
    let raw = env_secret(&["TOKEN_MONITOR_COMMANDCODE_COOKIE", "COMMANDCODE_COOKIE"])?;
    let mut forwarded = vec![];
    let mut has_session = false;
    for pair in raw.trim_start_matches("Cookie:").split(';') {
        let (name, value) = pair.trim().split_once('=')?;
        let lower = name.trim().to_ascii_lowercase();
        let is_session = matches!(
            lower.as_str(),
            "__secure-commandcode_prod_.session_token"
                | "__host-commandcode_prod_.session_token"
                | "commandcode_prod_.session_token"
        );
        let is_data = matches!(
            lower.as_str(),
            "__secure-commandcode_prod_.session_data"
                | "__host-commandcode_prod_.session_data"
                | "commandcode_prod_.session_data"
        );
        if is_session || is_data {
            has_session |= is_session;
            forwarded.push(format!("{}={}", name.trim(), value.trim()));
        }
    }
    has_session.then(|| forwarded.join("; "))
}

fn commandcode_plan(plan_id: &str) -> Option<(&'static str, f64, f64, f64)> {
    match plan_id.to_ascii_lowercase().as_str() {
        "individual-go" => Some(("Go", 10.0, 3.0, 6.0)),
        "individual-goat" => Some(("GOAT", 70.0, 14.0, 35.0)),
        "individual-pro" => Some(("Pro", 80.0, 16.0, 40.0)),
        "individual-max" => Some(("Max 10x", 150.0, 45.0, 90.0)),
        "individual-ultra" => Some(("Max 20x", 300.0, 90.0, 180.0)),
        _ => None,
    }
}

fn commandcode_rolling_window(
    label: &str,
    kind: WindowKind,
    raw: Option<&Value>,
    cap: Option<f64>,
) -> Option<LimitWindow> {
    let raw = raw?;
    let limit = number(raw.get("cap").or_else(|| raw.get("limit"))).or(cap)?;
    if limit <= 0.0 {
        return None;
    }
    let used = number(raw.get("used")).unwrap_or(0.0).max(0.0);
    Some(LimitWindow {
        label: label.into(),
        kind,
        metric: WindowMetric::Quota,
        remaining_percent: Some(((limit - used).max(0.0) / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: Some((limit - used).max(0.0)),
        currency: Some("USD".into()),
        resets_at_ms: reset_timestamp(raw.get("resetAt").or_else(|| raw.get("reset_at"))),
        reset_text: None,
        estimated: false,
    })
}

fn commandcode_cap(raw: Option<&Value>) -> Option<f64> {
    let raw = raw?;
    number(raw.get("cap").or_else(|| raw.get("limit"))).filter(|value| *value > 0.0)
}

pub async fn collect_commandcode(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("commandcode") {
        return vec![];
    }
    let cookie = match commandcode_cookie() {
        Some(cookie) => cookie,
        None => {
            return vec![unavailable_snapshot(
                "commandcode",
                "".into(),
                "Go".into(),
                "web",
                SourceHealth::Unavailable,
                "Command Code session cookie not configured",
                220,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "commandcode",
                account_key("commandcode", &cookie),
                "Go".into(),
                "web",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                220,
            )]
        }
    };
    let headers = |request: reqwest::RequestBuilder| {
        request
            .header("Cookie", &cookie)
            .header("Accept", "application/json, text/plain, */*")
            .header("Origin", "https://commandcode.ai")
            .header("Referer", "https://commandcode.ai/")
            .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
    };
    let (credits_response, subscription_response) = tokio::join!(
        headers(client.get("https://api.commandcode.ai/internal/billing/credits")).send(),
        headers(client.get("https://api.commandcode.ai/internal/billing/subscriptions")).send(),
    );
    let credits_response = match credits_response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "commandcode",
                account_key("commandcode", &cookie),
                "Go".into(),
                "web",
                status_for_http(response.status()),
                "Command Code credits unavailable",
                220,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "commandcode",
                account_key("commandcode", &cookie),
                "Go".into(),
                "web",
                SourceHealth::Unavailable,
                "Command Code request failed",
                220,
            )]
        }
    };
    let credits = match credits_response.json::<Value>().await {
        Ok(value) => value,
        Err(_) => {
            return vec![unavailable_snapshot(
                "commandcode",
                account_key("commandcode", &cookie),
                "Go".into(),
                "web",
                SourceHealth::Unavailable,
                "Invalid Command Code credits JSON",
                220,
            )]
        }
    };
    let subscriptions = subscription_response
        .ok()
        .filter(|response| response.status().is_success());
    let subscription = match subscriptions {
        Some(response) => response.json::<Value>().await.ok(),
        None => None,
    };
    let plan_id = subscription
        .as_ref()
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get("planId").or_else(|| value.get("plan_id")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let plan = commandcode_plan(plan_id);
    let credit_body = credits.get("credits").unwrap_or(&credits);
    let monthly = number(
        credit_body
            .get("monthlyCredits")
            .or_else(|| credit_body.get("monthly_credits")),
    );
    let window_limits = credit_body
        .get("windowLimits")
        .or_else(|| credit_body.get("window_limits"))
        .or_else(|| credits.get("windowLimits"))
        .or_else(|| credits.get("window_limits"));
    let five_hour_raw =
        window_limits.and_then(|value| value.get("fiveHour").or_else(|| value.get("five_hour")));
    let weekly_raw = window_limits.and_then(|value| value.get("weekly"));
    let mut windows = vec![];
    if let Some(window) = commandcode_rolling_window(
        "5h",
        WindowKind::Session,
        five_hour_raw,
        plan.map(|value| value.2),
    ) {
        windows.push(window);
    }
    if let Some(window) = commandcode_rolling_window(
        "7d",
        WindowKind::Weekly,
        weekly_raw,
        plan.map(|value| value.3),
    ) {
        windows.push(window);
    }
    if let Some(remaining) = monthly {
        let trusted_limit = plan.and_then(|(_, allowance, five_hour, weekly)| {
            (remaining <= allowance
                && commandcode_cap(five_hour_raw)
                    .is_some_and(|cap| (cap - five_hour).abs() < f64::EPSILON)
                && commandcode_cap(weekly_raw)
                    .is_some_and(|cap| (cap - weekly).abs() < f64::EPSILON))
            .then_some(allowance)
        });
        windows.push(LimitWindow {
            label: "Monthly".into(),
            kind: WindowKind::Monthly,
            metric: WindowMetric::Credits,
            remaining_percent: trusted_limit
                .map(|limit| (remaining / limit * 100.0).clamp(0.0, 100.0)),
            remaining_amount: Some(remaining.max(0.0)),
            currency: Some("USD".into()),
            resets_at_ms: subscription
                .as_ref()
                .and_then(|value| value.get("data"))
                .and_then(|value| {
                    reset_timestamp(
                        value
                            .get("currentPeriodEnd")
                            .or_else(|| value.get("current_period_end")),
                    )
                }),
            reset_text: None,
            estimated: trusted_limit.is_none(),
        });
    }
    let account_id = subscription
        .as_ref()
        .and_then(|value| value.get("data"))
        .and_then(|value| {
            value
                .get("userId")
                .or_else(|| value.get("user_id"))
                .or_else(|| value.get("id"))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    let account_key = account_key(
        "commandcode",
        if account_id.is_empty() {
            &cookie
        } else {
            account_id
        },
    );
    vec![connected_snapshot(
        "commandcode",
        account_key,
        plan.map(|value| value.0).unwrap_or("Command Code").into(),
        plan.map(|value| value.0).unwrap_or("").into(),
        "web",
        windows,
        220,
    )]
}

fn minimax_token() -> Option<String> {
    env_secret(&["TOKEN_MONITOR_MINIMAX_API_KEY", "MINIMAX_CODING_API_KEY"])
}

fn minimax_windows(payload: &Value) -> Vec<LimitWindow> {
    let rows = payload
        .get("data")
        .and_then(|value| value.get("model_remains"))
        .or_else(|| payload.get("model_remains"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(row) = rows
        .iter()
        .find(|row| row.get("model_name").and_then(Value::as_str) == Some("general"))
    else {
        return vec![];
    };
    let mut windows = vec![];
    if row
        .get("current_interval_status")
        .and_then(|value| number(Some(value)))
        .unwrap_or(0.0)
        != 3.0
    {
        if let Some(percent) = number(row.get("current_interval_remaining_percent")) {
            windows.push(LimitWindow {
                label: "5h".into(),
                kind: WindowKind::Session,
                metric: WindowMetric::Quota,
                remaining_percent: Some(percent.clamp(0.0, 100.0)),
                remaining_amount: None,
                currency: None,
                resets_at_ms: reset_timestamp(row.get("end_time")),
                reset_text: None,
                estimated: false,
            });
        }
    }
    if row
        .get("current_weekly_status")
        .and_then(|value| number(Some(value)))
        .unwrap_or(0.0)
        != 3.0
    {
        if let Some(percent) = number(row.get("current_weekly_remaining_percent")) {
            windows.push(LimitWindow {
                label: "Weekly".into(),
                kind: WindowKind::Weekly,
                metric: WindowMetric::Quota,
                remaining_percent: Some(percent.clamp(0.0, 100.0)),
                remaining_amount: None,
                currency: None,
                resets_at_ms: reset_timestamp(row.get("weekly_end_time")),
                reset_text: None,
                estimated: false,
            });
        }
    }
    windows
}

pub async fn collect_minimax(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("minimax") {
        return vec![];
    }
    let token = match minimax_token() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "minimax",
                "".into(),
                "Token Plan".into(),
                "api",
                SourceHealth::Unavailable,
                "MiniMax API key not configured",
                214,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "minimax",
                account_key("minimax", &token),
                "Token Plan".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                214,
            )]
        }
    };
    let urls = [
        "https://api.minimax.io/v1/token_plan/remains",
        "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
        "https://api.minimaxi.com/v1/token_plan/remains",
        "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains",
    ];
    let mut last_health = SourceHealth::Unavailable;
    for url in urls {
        let response = match client
            .get(url)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => continue,
        };
        if !response.status().is_success() {
            last_health = status_for_http(response.status());
            if matches!(last_health, SourceHealth::Unauthorized) {
                continue;
            }
            break;
        }
        let payload = match response.json::<Value>().await {
            Ok(payload) => payload,
            Err(_) => continue,
        };
        if let Some(status) = number(
            payload
                .get("base_resp")
                .and_then(|value| value.get("status_code")),
        ) {
            if status != 0.0 {
                last_health = SourceHealth::Unauthorized;
                continue;
            }
        }
        let windows = minimax_windows(&payload);
        if !windows.is_empty() {
            return vec![connected_snapshot(
                "minimax",
                account_key("minimax", &token),
                "Token Plan".into(),
                "Token Plan".into(),
                "api",
                windows,
                214,
            )];
        }
    }
    vec![unavailable_snapshot(
        "minimax",
        account_key("minimax", &token),
        "Token Plan".into(),
        "api",
        last_health,
        "MiniMax quota response unavailable",
        214,
    )]
}

fn zai_token() -> Option<String> {
    env_secret(&[
        "TOKEN_MONITOR_ZAI_API_KEY",
        "ZAI_API_KEY",
        "Z_AI_API_KEY",
        "GLM_API_KEY",
        "ZHIPU_API_KEY",
    ])
}

fn zai_region() -> &'static str {
    let value = std::env::var("TOKEN_MONITOR_ZAI_API_REGION")
        .or_else(|_| std::env::var("ZAI_API_REGION"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(value.as_str(), "cn" | "china" | "bigmodel" | "bigmodel-cn")
        || value.contains("bigmodel")
    {
        "cn"
    } else {
        "global"
    }
}

fn zai_used_percent(limit: &Value) -> Option<f64> {
    let total = number(limit.get("usage"));
    let remaining = number(limit.get("remaining"));
    let current = number(
        limit
            .get("currentValue")
            .or_else(|| limit.get("current_value")),
    );
    if let Some(total) = total.filter(|total| *total > 0.0) {
        let used = remaining.map(|value| total - value).or(current)?;
        return Some((used.max(0.0).min(total) / total * 100.0).clamp(0.0, 100.0));
    }
    number(
        limit
            .get("percentage")
            .or_else(|| limit.get("usedPercent"))
            .or_else(|| limit.get("used_percent")),
    )
}

fn zai_window_minutes(limit: &Value) -> f64 {
    let unit = number(limit.get("unit")).unwrap_or(0.0);
    let amount = number(limit.get("number")).unwrap_or(0.0);
    match unit {
        5.0 => amount,
        3.0 => amount * 60.0,
        1.0 => amount * 24.0 * 60.0,
        6.0 => amount * 7.0 * 24.0 * 60.0,
        _ => f64::MAX,
    }
}

fn zai_window(
    limit: &Value,
    kind: WindowKind,
    label: &str,
    fallback_reset: Option<i64>,
) -> Option<LimitWindow> {
    let used = zai_used_percent(limit)?;
    Some(LimitWindow {
        label: label.into(),
        kind,
        metric: WindowMetric::Quota,
        remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
        remaining_amount: None,
        currency: None,
        resets_at_ms: reset_timestamp(
            limit
                .get("nextResetTime")
                .or_else(|| limit.get("next_reset_time")),
        )
        .or(fallback_reset),
        reset_text: None,
        estimated: false,
    })
}

fn zai_windows(quota: &Value, subscription: Option<&Value>) -> Vec<LimitWindow> {
    let fallback_reset = subscription
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|row| {
            reset_timestamp(
                row.get("next_renew_time")
                    .or_else(|| row.get("nextRenewTime")),
            )
        });
    let limits = quota
        .get("data")
        .and_then(|value| value.get("limits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut token_limits = vec![];
    let mut time_limit = None;
    for limit in &limits {
        let kind = limit
            .get("type")
            .or_else(|| limit.get("limit_type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(kind.as_str(), "TOKENS_LIMIT" | "CREDIT_LIMIT")
            && zai_used_percent(limit).is_some()
        {
            token_limits.push(limit);
        }
        if kind == "TIME_LIMIT" && zai_used_percent(limit).is_some() {
            time_limit = Some(limit);
        }
    }
    token_limits.sort_by(|left, right| {
        let left_minutes = zai_window_minutes(left);
        let right_minutes = zai_window_minutes(right);
        left_minutes
            .partial_cmp(&right_minutes)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut windows = vec![];
    if let Some(limit) = token_limits.first() {
        if let Some(window) = zai_window(limit, WindowKind::Session, "5h", None) {
            windows.push(window);
        }
    }
    if let Some(limit) = token_limits.last().filter(|_| token_limits.len() > 1) {
        if let Some(window) = zai_window(limit, WindowKind::Weekly, "Weekly", None) {
            windows.push(window);
        }
    }
    if let Some(limit) = time_limit {
        if let Some(mut window) = zai_window(limit, WindowKind::Billing, "MCP", fallback_reset) {
            window.metric = WindowMetric::Credits;
            window.remaining_amount = number(limit.get("remaining"));
            window.reset_text = Some("Monthly".into());
            windows.push(window);
        }
    }
    windows
}

fn zai_plan(subscription: Option<&Value>, quota: &Value) -> String {
    subscription
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|row| {
            row.get("product_name")
                .or_else(|| row.get("productName"))
                .or_else(|| row.get("plan_name"))
                .or_else(|| row.get("planName"))
        })
        .and_then(Value::as_str)
        .or_else(|| {
            quota
                .get("data")
                .and_then(|value| value.get("planName"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Z.ai")
        .to_owned()
}

pub async fn collect_zai(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("zai") {
        return vec![];
    }
    let token = match zai_token() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "zai",
                "".into(),
                "Z.ai".into(),
                "api",
                SourceHealth::Unavailable,
                "Z.ai API key not configured",
                75,
            )]
        }
    };
    let base = if zai_region() == "cn" {
        "https://open.bigmodel.cn"
    } else {
        "https://api.z.ai"
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "zai",
                account_key("zai", &token),
                "Z.ai".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let quota = request_bearer_json(
        &client,
        &format!("{base}/api/monitor/usage/quota/limit"),
        &token,
        &[],
    )
    .await;
    let quota = match quota {
        Ok(value) => {
            if let Some(code) = value.get("code").and_then(Value::as_i64) {
                if code != 200 && code != 0 {
                    let msg = value.get("msg").and_then(Value::as_str).unwrap_or("No coding plan");
                    return vec![unavailable_snapshot(
                        "zai",
                        account_key("zai", &token),
                        "Z.ai".into(),
                        "api",
                        SourceHealth::Unavailable,
                        &format!("Z.ai: {msg}"),
                        75,
                    )];
                }
            }
            value
        }
        Err((health, detail)) => {
            return vec![unavailable_snapshot(
                "zai",
                account_key("zai", &token),
                "Z.ai".into(),
                "api",
                health,
                &detail,
                75,
            )]
        }
    };
    let subscription = request_bearer_json(
        &client,
        &format!("{base}/api/biz/subscription/list"),
        &token,
        &[],
    )
    .await
    .ok();
    let windows = zai_windows(&quota, subscription.as_ref());
    if windows.is_empty() {
        return vec![unavailable_snapshot(
            "zai",
            account_key("zai", &token),
            "Z.ai".into(),
            "api",
            SourceHealth::Unavailable,
            "Z.ai quota response has no windows",
            75,
        )];
    }
    vec![connected_snapshot(
        "zai",
        account_key("zai", &token),
        zai_plan(subscription.as_ref(), &quota),
        zai_plan(subscription.as_ref(), &quota),
        "api",
        windows,
        75,
    )]
}

fn zai_team_token() -> Option<String> {
    env_secret(&[
        "TOKEN_MONITOR_ZAI_TEAM_API_KEY",
        "ZAI_TEAM_API_KEY",
        "BIGMODEL_TEAM_API_KEY",
    ])
}

pub async fn collect_zai_team(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("zaiteam") {
        return vec![];
    }
    let token = match zai_team_token() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "zaiteam",
                "".into(),
                "Z.ai Team".into(),
                "api",
                SourceHealth::Unavailable,
                "Z.ai team API key not configured",
                75,
            )]
        }
    };
    let organization = std::env::var("ZAI_TEAM_ORGANIZATION_ID").unwrap_or_default();
    let project = std::env::var("ZAI_TEAM_PROJECT_ID").unwrap_or_default();
    if organization.trim().is_empty() || project.trim().is_empty() {
        return vec![unavailable_snapshot(
            "zaiteam",
            account_key("zaiteam", &token),
            "Z.ai Team".into(),
            "api",
            SourceHealth::Unavailable,
            "Z.ai team organization/project not configured",
            75,
        )];
    }
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "zaiteam",
                account_key("zaiteam", &organization),
                "Z.ai Team".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let response = client
        .get("https://open.bigmodel.cn/api/monitor/usage/quota/limit?type=2")
        .bearer_auth(&token)
        .header("bigmodel-organization", &organization)
        .header("bigmodel-project", &project)
        .header("Accept", "application/json")
        .send()
        .await;
    let quota = match response {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        Ok(response) => {
            return vec![unavailable_snapshot(
                "zaiteam",
                account_key("zaiteam", &organization),
                "Z.ai Team".into(),
                "api",
                status_for_http(response.status()),
                "Z.ai team quota unavailable",
                75,
            )]
        }
        Err(_) => None,
    };
    let Some(quota) = quota else {
        return vec![unavailable_snapshot(
            "zaiteam",
            account_key("zaiteam", &organization),
            "Z.ai Team".into(),
            "api",
            SourceHealth::Unavailable,
            "Invalid Z.ai team JSON",
            75,
        )];
    };
    let windows = zai_windows(&quota, None);
    if windows.is_empty() {
        return vec![unavailable_snapshot(
            "zaiteam",
            account_key("zaiteam", &organization),
            "Z.ai Team".into(),
            "api",
            SourceHealth::Unavailable,
            "Z.ai team response has no windows",
            75,
        )];
    }
    vec![connected_snapshot(
        "zaiteam",
        account_key("zaiteam", &organization),
        "Team".into(),
        "Team".into(),
        "api",
        windows,
        75,
    )]
}

fn copilot_token() -> Option<String> {
    env_secret(&[
        "TOKEN_MONITOR_COPILOT_API_TOKEN",
        "COPILOT_API_TOKEN",
        "GITHUB_COPILOT_TOKEN",
    ])
}

fn copilot_quota_window(
    label: &str,
    raw: Option<&Value>,
    reset: Option<i64>,
) -> Option<LimitWindow> {
    let raw = raw?;
    if raw.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return Some(LimitWindow {
            label: label.into(),
            kind: WindowKind::Billing,
            metric: WindowMetric::Quota,
            remaining_percent: Some(100.0),
            remaining_amount: None,
            currency: None,
            resets_at_ms: None,
            reset_text: None,
            estimated: false,
        });
    }
    let entitlement = number(raw.get("entitlement"));
    let remaining = number(raw.get("remaining"));
    let percent = number(
        raw.get("percent_remaining")
            .or_else(|| raw.get("percentRemaining")),
    )
    .or_else(|| {
        entitlement
            .zip(remaining)
            .filter(|(total, _)| *total > 0.0)
            .map(|(total, remaining)| remaining / total * 100.0)
    })?;
    Some(LimitWindow {
        label: label.into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Quota,
        remaining_percent: Some(percent.clamp(0.0, 100.0)),
        remaining_amount: remaining,
        currency: None,
        resets_at_ms: reset,
        reset_text: None,
        estimated: false,
    })
}

fn copilot_reset(value: Option<&Value>) -> Option<i64> {
    let value = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.timestamp_millis())
}

pub async fn collect_copilot(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("copilot") {
        return vec![];
    }
    let token = match copilot_token() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "copilot",
                "".into(),
                "Copilot".into(),
                "api",
                SourceHealth::Unavailable,
                "Copilot API token not configured",
                75,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "copilot",
                account_key("copilot", &token),
                "Copilot".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let response = match client
        .get("https://api.github.com/copilot_internal/user")
        .header("Accept", "application/json")
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "token-monitor-rust")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "copilot",
                account_key("copilot", &token),
                "Copilot".into(),
                "api",
                status_for_http(response.status()),
                "Copilot usage unavailable",
                75,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "copilot",
                account_key("copilot", &token),
                "Copilot".into(),
                "api",
                SourceHealth::Unavailable,
                "Copilot request failed",
                75,
            )]
        }
    };
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return vec![unavailable_snapshot(
                "copilot",
                account_key("copilot", &token),
                "Copilot".into(),
                "api",
                SourceHealth::Unavailable,
                "Invalid Copilot JSON",
                75,
            )]
        }
    };
    let reset = copilot_reset(
        payload
            .get("quota_reset_date")
            .or_else(|| payload.get("quotaResetDate")),
    );
    let snapshots = payload
        .get("quota_snapshots")
        .or_else(|| payload.get("quotaSnapshots"));
    let mut windows = vec![];
    if let Some(window) = copilot_quota_window(
        "Premium",
        snapshots.and_then(|value| {
            value
                .get("premium_interactions")
                .or_else(|| value.get("premiumInteractions"))
        }),
        reset,
    ) {
        windows.push(window);
    }
    if let Some(window) =
        copilot_quota_window("Chat", snapshots.and_then(|value| value.get("chat")), reset)
    {
        windows.push(window);
    }
    if windows.is_empty() {
        let monthly = payload
            .get("monthly_quotas")
            .or_else(|| payload.get("monthlyQuotas"));
        let limited = payload
            .get("limited_user_quotas")
            .or_else(|| payload.get("limitedUserQuotas"));
        for (label, key) in [("Premium", "completions"), ("Chat", "chat")] {
            let entitlement = monthly.and_then(|value| number(value.get(key)));
            let remaining = limited.and_then(|value| number(value.get(key)));
            if let (Some(entitlement), Some(remaining)) = (entitlement, remaining) {
                windows.push(LimitWindow {
                    label: label.into(),
                    kind: WindowKind::Billing,
                    metric: WindowMetric::Quota,
                    remaining_percent: Some((remaining / entitlement * 100.0).clamp(0.0, 100.0)),
                    remaining_amount: Some(remaining),
                    currency: None,
                    resets_at_ms: reset,
                    reset_text: None,
                    estimated: false,
                });
            }
        }
    }
    if windows.is_empty() {
        return vec![unavailable_snapshot(
            "copilot",
            account_key("copilot", &token),
            "Copilot".into(),
            "api",
            SourceHealth::Unavailable,
            "Copilot response has no usable quota",
            75,
        )];
    }
    let login = match client
        .get("https://api.github.com/user")
        .header("Accept", "application/json")
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "token-monitor-rust")
        .send()
        .await
    {
        Ok(response) => response.json::<Value>().await.ok(),
        Err(_) => None,
    };
    let account = login
        .as_ref()
        .and_then(|value| value.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("Copilot");
    let plan = payload
        .get("copilot_plan")
        .or_else(|| payload.get("copilotPlan"))
        .and_then(Value::as_str)
        .unwrap_or("Copilot");
    vec![connected_snapshot(
        "copilot",
        account_key("copilot", &token),
        account.into(),
        plan.into(),
        "api",
        windows,
        75,
    )]
}

fn qoder_cookie() -> Option<String> {
    env_secret(&["TOKEN_MONITOR_QODER_COOKIE", "QODER_COOKIE"])
}

fn qoder_site() -> &'static str {
    let value = std::env::var("TOKEN_MONITOR_QODER_SITE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(value.as_str(), "cn" | "china") || value.contains("qoder.com.cn") {
        "cn"
    } else {
        "global"
    }
}

fn qoder_usage_summary(value: Option<&Value>) -> Option<(f64, f64, f64)> {
    let value = value?;
    let used = number(value.get("usedValue").or_else(|| value.get("used_value")))?;
    let limit = number(value.get("limitValue").or_else(|| value.get("limit_value")))?;
    if used < 0.0 || limit < 0.0 {
        return None;
    }
    let remaining = number(
        value
            .get("remainingValue")
            .or_else(|| value.get("remaining_value")),
    )
    .unwrap_or((limit - used).max(0.0));
    Some((used, limit, remaining.max(0.0)))
}

fn qoder_window(payload: &Value) -> Option<LimitWindow> {
    let data = payload.get("data").unwrap_or(payload);
    let total = data.get("totalQuota").or_else(|| data.get("total_quota"));
    let total = total
        .and_then(|value| {
            value
                .get("quotaSummary")
                .or_else(|| value.get("quota_summary"))
        })
        .and_then(|value| qoder_usage_summary(Some(value)))?;
    let shared = data.get("sharedQuota").or_else(|| data.get("shared_quota"));
    let shared = shared
        .and_then(|value| {
            value
                .get("quotaSummary")
                .or_else(|| value.get("quota_summary"))
        })
        .and_then(|value| qoder_usage_summary(Some(value)));
    let limit = total.1 + shared.map(|value| value.1).unwrap_or(0.0);
    let remaining = total.2 + shared.map(|value| value.2).unwrap_or(0.0);
    Some(LimitWindow {
        label: "Credits".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Credits,
        remaining_percent: (limit > 0.0).then_some((remaining / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: Some(remaining),
        currency: Some("USD".into()),
        resets_at_ms: reset_timestamp(
            data.get("nextResetAt")
                .or_else(|| data.get("next_reset_at")),
        ),
        reset_text: None,
        estimated: false,
    })
}

pub async fn collect_qoder(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("qoder") {
        return vec![];
    }
    let cookie = match qoder_cookie() {
        Some(cookie) => cookie,
        None => {
            return vec![unavailable_snapshot(
                "qoder",
                "".into(),
                "Qoder".into(),
                "web",
                SourceHealth::Unavailable,
                "Qoder session cookie not configured",
                141,
            )]
        }
    };
    let origin = if qoder_site() == "cn" {
        "https://qoder.com.cn"
    } else {
        "https://qoder.com"
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "qoder",
                account_key("qoder", &cookie),
                "Qoder".into(),
                "web",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                141,
            )]
        }
    };
    let response = client
        .get(format!("{origin}/api/v2/me/usages/big_model_credits"))
        .header("Cookie", &cookie)
        .header("Accept", "application/json")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/account/usage"))
        .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
        .send()
        .await;
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "qoder",
                account_key("qoder", &cookie),
                "Qoder".into(),
                "web",
                status_for_http(response.status()),
                "Qoder usage unavailable",
                141,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "qoder",
                account_key("qoder", &cookie),
                "Qoder".into(),
                "web",
                SourceHealth::Unavailable,
                "Qoder request failed",
                141,
            )]
        }
    };
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return vec![unavailable_snapshot(
                "qoder",
                account_key("qoder", &cookie),
                "Qoder".into(),
                "web",
                SourceHealth::Unavailable,
                "Invalid Qoder JSON",
                141,
            )]
        }
    };
    let Some(window) = qoder_window(&payload) else {
        return vec![unavailable_snapshot(
            "qoder",
            account_key("qoder", &cookie),
            "Qoder".into(),
            "web",
            SourceHealth::Unavailable,
            "Qoder response has no credits",
            141,
        )];
    };
    let plan = match client
        .get(format!("{origin}/api/v1/me/userplan"))
        .header("Cookie", &cookie)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(response) => response.json::<Value>().await.ok(),
        Err(_) => None,
    };
    let plan = plan
        .as_ref()
        .and_then(|value| value.get("data").or(Some(value)))
        .and_then(|value| {
            value
                .get("plan_tier")
                .or_else(|| value.get("planTier"))
                .or_else(|| value.get("plan"))
                .or_else(|| value.get("tier"))
        })
        .and_then(Value::as_str)
        .unwrap_or("Qoder");
    vec![connected_snapshot(
        "qoder",
        account_key("qoder", &cookie),
        plan.into(),
        plan.into(),
        "web",
        vec![window],
        141,
    )]
}

fn trae_access_token() -> Option<String> {
    let mut token = env_secret(&["TOKEN_MONITOR_TRAE_ACCESS_TOKEN", "TRAE_ACCESS_TOKEN"])?;
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("authorization:") {
        token = token
            .split_once(':')
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or(token);
    }
    if token.to_ascii_lowercase().starts_with("cloud-ide-jwt ") {
        token = token["cloud-ide-jwt ".len()..].trim().to_owned();
    }
    (!token.is_empty()).then_some(token)
}

pub async fn collect_trae(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("trae") {
        return vec![];
    }
    let token = match trae_access_token() {
        Some(token) => token,
        None => {
            return vec![unavailable_snapshot(
                "trae",
                "".into(),
                "Trae".into(),
                "api",
                SourceHealth::Unavailable,
                "Trae access token not configured",
                220,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "trae",
                account_key("trae", &token),
                "Trae".into(),
                "api",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                220,
            )]
        }
    };
    let response = client
        .post("https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage")
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Cloud-IDE-JWT {token}"))
        .header("X-User-Region", "CN")
        .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
        .body("{}")
        .send()
        .await;
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "trae",
                account_key("trae", &token),
                "Trae".into(),
                "api",
                status_for_http(response.status()),
                "Trae credits unavailable",
                220,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "trae",
                account_key("trae", &token),
                "Trae".into(),
                "api",
                SourceHealth::Unavailable,
                "Trae request failed",
                220,
            )]
        }
    };
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return vec![unavailable_snapshot(
                "trae",
                account_key("trae", &token),
                "Trae".into(),
                "api",
                SourceHealth::Unavailable,
                "Invalid Trae JSON",
                220,
            )]
        }
    };
    let packs = payload
        .get("user_entitlement_pack_list")
        .or_else(|| payload.get("userEntitlementPackList"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut limit = 0.0;
    let mut used = 0.0;
    for pack in packs {
        let quota = pack
            .get("entitlement_base_info")
            .and_then(|value| value.get("quota"))
            .or_else(|| {
                pack.get("entitlementBaseInfo")
                    .and_then(|value| value.get("quota"))
            });
        let Some(pack_limit) = number(quota.and_then(|value| {
            value
                .get("credits_limit")
                .or_else(|| value.get("creditsLimit"))
        })) else {
            continue;
        };
        let pack_used = number(pack.get("usage").and_then(|value| {
            value
                .get("credits_amount")
                .or_else(|| value.get("creditsAmount"))
        }))
        .unwrap_or(0.0);
        if pack_limit > 0.0 {
            limit += pack_limit;
            used += pack_used.max(0.0);
        }
    }
    if limit <= 0.0 {
        return vec![unavailable_snapshot(
            "trae",
            account_key("trae", &token),
            "Trae".into(),
            "api",
            SourceHealth::Unavailable,
            "Trae response has no entitlement credits",
            220,
        )];
    }
    let remaining = (limit - used).max(0.0);
    let window = LimitWindow {
        label: "Credits".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Credits,
        remaining_percent: Some((remaining / limit * 100.0).clamp(0.0, 100.0)),
        remaining_amount: Some(remaining),
        currency: Some("CREDITS".into()),
        resets_at_ms: None,
        reset_text: None,
        estimated: false,
    };
    vec![connected_snapshot(
        "trae",
        account_key("trae", &token),
        "Trae".into(),
        "Trae".into(),
        "api",
        vec![window],
        220,
    )]
}

#[allow(clippy::while_let_on_iterator)]
fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            while let Some(next) = chars.next() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if chars.peek() == Some(&']') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '\u{7}' {
                    break;
                }
                if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
        } else {
            let _ = chars.next();
        }
    }
    output
}

fn kiro_usage_text(text: &str) -> Result<(String, f64, f64, Option<i64>, bool), String> {
    let text = strip_ansi(text);
    let lower = text.to_ascii_lowercase();
    if lower.contains("not logged in")
        || lower.contains("login required")
        || lower.contains("kiro-cli login")
        || lower.contains("oauth error")
    {
        return Err("Kiro CLI is not logged in".into());
    }
    if lower.trim().is_empty() || lower.contains("could not retrieve usage information") {
        return Err("Kiro CLI returned no usage".into());
    }
    let plan = text
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .find("plan:")
                .map(|index| line[index + 5..].trim().to_owned())
        })
        .or_else(|| {
            text.lines()
                .find(|line| line.to_ascii_uppercase().contains("KIRO "))
                .map(|line| line.trim().to_owned())
        })
        .unwrap_or_else(|| "Kiro".into());
    let percent = text.split('%').find_map(|prefix| {
        let digits = prefix
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let digits = digits
            .trim()
            .trim_start_matches(|character: char| !character.is_ascii_digit() && character != '.');
        digits.parse::<f64>().ok()
    });
    let lower_text = text.to_ascii_lowercase();
    let covered = lower_text.split_whitespace().collect::<Vec<_>>();
    let covered = covered.windows(3).find_map(|words| {
        if words[1] != "of" {
            return None;
        }
        let used = words[0]
            .trim_matches(|character: char| !character.is_ascii_digit() && character != '.')
            .parse::<f64>()
            .ok()?;
        let total = words[2]
            .trim_matches(|character: char| !character.is_ascii_digit() && character != '.')
            .parse::<f64>()
            .ok()?;
        Some((used, total))
    });
    let (used, total) = covered.unwrap_or((0.0, 50.0));
    let remaining_percent = percent
        .map(|used_percent| (100.0 - used_percent).clamp(0.0, 100.0))
        .unwrap_or_else(|| ((total - used).max(0.0) / total.max(1.0) * 100.0).clamp(0.0, 100.0));
    let reset = text.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .find("resets on ")
            .and_then(|index| {
                let raw = line[index + 10..].split_whitespace().next()?;
                chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|date| date.and_utc().timestamp_millis())
            })
    });
    let managed = lower.contains("managed by admin") || lower.contains("managed by organization");
    Ok((
        plan,
        remaining_percent,
        total.max(0.0) - used.max(0.0),
        reset,
        managed,
    ))
}

async fn run_kiro_usage(timeout: Duration) -> Result<String, String> {
    let command = std::env::var("TOKEN_MONITOR_KIRO_COMMAND").unwrap_or_else(|_| "kiro-cli".into());
    let mut process = tokio::process::Command::new(command);
    process
        .args(["chat", "--no-interactive", "/usage"])
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, process.output())
        .await
        .map_err(|_| "Kiro CLI timeout".to_owned())?
        .map_err(|_| "Kiro CLI unavailable".to_owned())?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !stdout.is_empty() {
        return Ok(stdout);
    }
    Ok(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

pub async fn collect_kiro(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("kiro") {
        return vec![];
    }
    let text = match run_kiro_usage(options.timeout()).await {
        Ok(text) => text,
        Err(detail) => {
            return vec![unavailable_snapshot(
                "kiro",
                "kiro:default".into(),
                "Kiro".into(),
                "cli",
                SourceHealth::Unavailable,
                &detail,
                214,
            )]
        }
    };
    let (plan, remaining_percent, remaining_amount, reset, managed) = match kiro_usage_text(&text) {
        Ok(value) => value,
        Err(detail) => {
            return vec![unavailable_snapshot(
                "kiro",
                "kiro:default".into(),
                "Kiro".into(),
                "cli",
                SourceHealth::Unavailable,
                &detail,
                214,
            )]
        }
    };
    let window = LimitWindow {
        label: "Credits".into(),
        kind: WindowKind::Billing,
        metric: WindowMetric::Credits,
        remaining_percent: if managed {
            None
        } else {
            Some(remaining_percent)
        },
        remaining_amount: if managed {
            None
        } else {
            Some(remaining_amount.max(0.0))
        },
        currency: Some("credits".into()),
        resets_at_ms: reset,
        reset_text: None,
        estimated: false,
    };
    vec![connected_snapshot(
        "kiro",
        "kiro:default".into(),
        plan.clone(),
        plan,
        "cli",
        vec![window],
        214,
    )]
}

fn ollama_cookie() -> Option<String> {
    env_secret(&["TOKEN_MONITOR_OLLAMA_COOKIE", "OLLAMA_COOKIE"])
}

fn ollama_percent(block: &str) -> Option<f64> {
    let index = block.to_ascii_lowercase().find('%')?;
    let mut digits = String::new();
    for character in block[..index].chars().rev() {
        if character.is_ascii_digit() || character == '.' {
            digits.push(character);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.chars().rev().collect::<String>().parse::<f64>().ok()
}

fn ollama_html_windows(html: &str) -> Vec<LimitWindow> {
    let lower = html.to_ascii_lowercase();
    let markers = ["session usage", "hourly usage", "weekly usage"];
    let mut positions = markers
        .iter()
        .filter_map(|marker| lower.find(marker).map(|index| (index, *marker)))
        .collect::<Vec<_>>();
    positions.sort_by_key(|(index, _)| *index);
    let mut windows = vec![];
    for (position, marker) in positions {
        let end = markers
            .iter()
            .filter_map(|candidate| {
                lower[position + marker.len()..]
                    .find(candidate)
                    .map(|offset| position + marker.len() + offset)
            })
            .min()
            .unwrap_or(html.len());
        let block = &html[position..end.min(html.len())];
        let Some(used) = ollama_percent(block) else {
            continue;
        };
        let kind = if marker == "weekly usage" {
            WindowKind::Weekly
        } else {
            WindowKind::Session
        };
        let label = if marker == "weekly usage" {
            "Weekly"
        } else if marker == "hourly usage" {
            "Hourly"
        } else {
            "Session"
        };
        let reset = block
            .to_ascii_lowercase()
            .find("data-time=")
            .and_then(|index| {
                let rest = &block[index + "data-time=".len()..];
                let quote = rest.chars().next()?;
                let value = rest[quote.len_utf8()..].split(quote).next()?;
                reset_timestamp(Some(&Value::String(value.to_owned())))
            });
        windows.push(LimitWindow {
            label: label.into(),
            kind,
            metric: WindowMetric::Quota,
            remaining_percent: Some((100.0 - used).clamp(0.0, 100.0)),
            remaining_amount: None,
            currency: None,
            resets_at_ms: reset,
            reset_text: None,
            estimated: false,
        });
    }
    windows
}

pub async fn collect_ollama(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    if !options.includes("ollama") {
        return vec![];
    }
    let cookie = match ollama_cookie() {
        Some(cookie) => cookie,
        None => {
            return vec![unavailable_snapshot(
                "ollama",
                "".into(),
                "Ollama".into(),
                "web",
                SourceHealth::Unavailable,
                "Ollama session cookie not configured",
                75,
            )]
        }
    };
    let client = match Client::builder().timeout(options.timeout()).build() {
        Ok(client) => client,
        Err(_) => {
            return vec![unavailable_snapshot(
                "ollama",
                account_key("ollama", &cookie),
                "Ollama".into(),
                "web",
                SourceHealth::Unavailable,
                "HTTP client unavailable",
                75,
            )]
        }
    };
    let response = match client
        .get("https://ollama.com/settings")
        .header("Cookie", &cookie)
        .header("Accept", "text/html,application/xhtml+xml")
        .header("User-Agent", "Mozilla/5.0 Token-Monitor-Rust")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            return vec![unavailable_snapshot(
                "ollama",
                account_key("ollama", &cookie),
                "Ollama".into(),
                "web",
                status_for_http(response.status()),
                "Ollama settings unavailable",
                75,
            )]
        }
        Err(_) => {
            return vec![unavailable_snapshot(
                "ollama",
                account_key("ollama", &cookie),
                "Ollama".into(),
                "web",
                SourceHealth::Unavailable,
                "Ollama request failed",
                75,
            )]
        }
    };
    let html = match response.text().await {
        Ok(html) => html,
        Err(_) => {
            return vec![unavailable_snapshot(
                "ollama",
                account_key("ollama", &cookie),
                "Ollama".into(),
                "web",
                SourceHealth::Unavailable,
                "Invalid Ollama settings body",
                75,
            )]
        }
    };
    let windows = ollama_html_windows(&html);
    if windows.is_empty() {
        return vec![unavailable_snapshot(
            "ollama",
            account_key("ollama", &cookie),
            "Ollama".into(),
            "web",
            SourceHealth::Unavailable,
            "Ollama settings page has no usage meters",
            75,
        )];
    }
    vec![connected_snapshot(
        "ollama",
        account_key("ollama", &cookie),
        "Ollama".into(),
        "Ollama".into(),
        "web",
        windows,
        75,
    )]
}

pub async fn collect_live_limits(options: &CollectorOptions) -> Vec<ProviderSnapshot> {
    let explicit_selection = options.providers.is_some();
    let (
        openrouter,
        deepseek,
        modal,
        vast,
        cursor,
        claude,
        codex,
        antigravity,
        grok,
        commandcode,
        minimax,
        zai,
        zaiteam,
        copilot,
        qoder,
        trae,
        kiro,
        ollama,
    ) = tokio::join!(
        collect_openrouter(options),
        collect_deepseek(options),
        collect_modal(options),
        collect_vast(options),
        collect_cursor(options),
        collect_claude(options),
        collect_codex(options),
        collect_antigravity(options),
        collect_grok(options),
        collect_commandcode(options),
        collect_minimax(options),
        collect_zai(options),
        collect_zai_team(options),
        collect_copilot(options),
        collect_qoder(options),
        collect_trae(options),
        collect_kiro(options),
        collect_ollama(options),
    );
    let mut rows = [
        openrouter,
        deepseek,
        modal,
        vast,
        cursor,
        claude,
        codex,
        antigravity,
        grok,
        commandcode,
        minimax,
        zai,
        zaiteam,
        copilot,
        qoder,
        trae,
        kiro,
        ollama,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if options
        .providers
        .as_ref()
        .is_some_and(|selected| selected.contains("all"))
    {
        for provider in crate::provider_registry::ALL_PROVIDER_IDS {
            if crate::provider_registry::is_native(provider) {
                continue;
            }
            rows.push(unavailable_snapshot(
                provider,
                "".into(),
                crate::provider_registry::display_name(provider).into(),
                "registry",
                SourceHealth::Unavailable,
                "Native adapter pending",
                245,
            ));
        }
    } else if let Some(selected) = &options.providers {
        for provider in selected {
            if provider == "all"
                || crate::provider_registry::is_native(provider)
                || !crate::provider_registry::ALL_PROVIDER_IDS.contains(&provider.as_str())
            {
                continue;
            }
            rows.push(unavailable_snapshot(
                provider,
                "".into(),
                crate::provider_registry::display_name(provider).into(),
                "registry",
                SourceHealth::Unavailable,
                "Native adapter pending",
                245,
            ));
        }
    }
    if explicit_selection {
        rows
    } else {
        rows.into_iter()
            .filter(ProviderSnapshot::visible_by_default)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_credit_math_preserves_decimal_remainder() {
        let value = serde_json::json!({"total_credits": 10.0, "total_usage": 0.02234472});
        let window = openrouter_credit_window(&value).unwrap();
        assert_eq!(window.remaining_amount, Some(9.97765528));
        assert!((window.remaining_percent.unwrap() - 99.7765528).abs() < 0.000001);
    }

    #[test]
    fn zero_credit_wallet_is_not_advertised_as_available() {
        let snapshot = connected_snapshot(
            "modal",
            "modal:test".into(),
            "test".into(),
            "Pro".into(),
            "modal",
            vec![LimitWindow {
                label: "credit~".into(),
                kind: WindowKind::Billing,
                metric: WindowMetric::Credits,
                remaining_percent: Some(0.0),
                remaining_amount: Some(0.0),
                currency: Some("USD".into()),
                resets_at_ms: None,
                reset_text: None,
                estimated: true,
            }],
            214,
        );
        assert_eq!(snapshot.availability, Availability::Exhausted);
    }

    #[test]
    fn shell_secret_parser_never_evaluates_commands() {
        let values = parse_shell_secrets(
            "export OPENROUTER_API_KEY='abc'\nDEEPSEEK_API_KEY=def\nBAD=$(cat secret)\n# ignored=comment\n",
        );
        assert_eq!(values.get("OPENROUTER_API_KEY"), Some(&"abc".to_owned()));
        assert_eq!(values.get("DEEPSEEK_API_KEY"), Some(&"def".to_owned()));
        assert!(!values.contains_key("BAD"));
    }

    #[test]
    fn modal_profiles_ignore_nested_tables_and_deduplicate() {
        let path =
            std::env::temp_dir().join(format!("token-monitor-modal-{}.toml", std::process::id()));
        std::fs::write(&path, "[first]\nkey='x'\n[first.extra]\nx=1\n[second]\n").unwrap();
        let options = CollectorOptions {
            modal_config: Some(path.clone()),
            ..CollectorOptions::default()
        };
        assert_eq!(modal_profiles(&options), vec!["first", "second"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cursor_used_percent_is_inverted_to_remaining_percent() {
        assert_eq!(cursor_remaining_percent(Some(3.0)), Some(97.0));
        assert_eq!(cursor_remaining_percent(Some(100.0)), Some(0.0));
        let (reset_ms, reset_text) =
            cursor_reset(&serde_json::json!({"billingCycleEnd":"2026-09-02T00:00:00Z"}));
        assert!(reset_ms.is_some());
        assert!(reset_text.is_none());
    }

    #[test]
    fn cursor_agent_block_is_detected_from_recent_local_log() {
        let root =
            std::env::temp_dir().join(format!("token-monitor-cursor-log-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("agent.log"),
            "Error: You've hit your free requests limit",
        )
        .unwrap();
        assert!(cursor_agent_blocked_in(&root, None));
        assert!(!cursor_agent_blocked_in(&root, Some(1000)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claude_usage_percent_is_inverted_and_spend_stays_separate() {
        let window =
            claude_remaining_window("7d", &serde_json::json!({"utilization": 34.0})).unwrap();
        assert_eq!(window.remaining_percent, Some(66.0));
        let spend = claude_spend_window(&serde_json::json!({
            "spend": {"enabled": true, "used": {"amount_minor": 250, "exponent": 2, "currency": "USD"}, "limit": {"amount_minor": 1000, "exponent": 2, "currency": "USD"}}
        })).unwrap();
        assert_eq!(spend.remaining_amount, Some(7.5));
        assert_eq!(spend.metric, WindowMetric::Spend);
    }

    #[test]
    fn claude_web_organization_id_accepts_array_and_data_envelopes() {
        assert_eq!(
            claude_web_organization_id(&serde_json::json!([{"uuid":"org-1"}])),
            Some("org-1".into())
        );
        assert_eq!(
            claude_web_organization_id(&serde_json::json!({"data":[{"id":"org-2"}]})),
            Some("org-2".into())
        );
    }

    #[test]
    fn claude_web_plan_maps_individual_tiers_to_pro() {
        let account = serde_json::json!({
            "memberships": [{"organization": {"rate_limit_tier": "default_claude_ai"}}]
        });
        assert_eq!(claude_web_plan(&account), "Pro");
    }

    #[test]
    fn claude_web_balance_converts_minor_units_and_expiry() {
        let window = claude_web_balance_window(&serde_json::json!({
            "amount": 7227,
            "currency": "USD",
            "next_expires_at": "2026-09-19T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(window.remaining_amount, Some(72.27));
        assert_eq!(window.currency.as_deref(), Some("USD"));
        assert!(window.resets_at_ms.is_some());
    }

    #[test]
    fn claude_credential_shape_distinguishes_cookie_from_oauth() {
        assert!(looks_like_cookie_header("sessionKey=abc; other=def"));
        assert!(looks_like_cookie_header("Cookie: sessionKey=abc"));
        assert!(!looks_like_cookie_header("sk-ant-oat01-example"));
    }

    #[test]
    fn codex_windows_invert_used_percent_and_select_weekly_kind() {
        let window = codex_window(
            "7d",
            &serde_json::json!({"used_percent": 23, "window_duration_mins": 10080}),
            "secondary",
        )
        .unwrap();
        assert_eq!(window.remaining_percent, Some(77.0));
        assert_eq!(window.kind, WindowKind::Weekly);
    }

    #[test]
    fn antigravity_groups_and_fractions_become_named_remaining_windows() {
        let payload = serde_json::json!({"groups":[{"displayName":"Gemini","buckets":[{"window":"weekly","remainingFraction":0.001}]}]});
        let windows = antigravity_windows(&payload);
        assert_eq!(windows[0].label, "Gemini 7d");
        assert!((windows[0].remaining_percent.unwrap() - 0.1).abs() < 0.0001);
    }

    #[test]
    fn antigravity_process_parser_requires_csrf_for_desktop_server() {
        let output = "123 /Applications/Antigravity.app/language_server --csrf_token=secret\n124 /Applications/Antigravity.app/language_server\n";
        let servers = antigravity_servers(output);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].pid, 123);
        assert_eq!(servers[0].csrf_token, "secret");
    }

    #[test]
    fn grok_billing_percent_becomes_remaining_quota() {
        let window = grok_billing_window(&serde_json::json!({"creditUsagePercent": 25.0})).unwrap();
        assert_eq!(window.remaining_percent, Some(75.0));
        assert_eq!(window.kind, WindowKind::Weekly);
    }

    #[test]
    fn minimax_general_bucket_preserves_remaining_percent_and_reset() {
        let payload = serde_json::json!({"data":{"model_remains":[{"model_name":"general","current_interval_remaining_percent":"88.5","current_weekly_remaining_percent":"75","end_time":1788257925,"weekly_end_time":1788748040}]}});
        let windows = minimax_windows(&payload);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].remaining_percent, Some(88.5));
        assert_eq!(windows[1].remaining_percent, Some(75.0));
        assert_eq!(windows[0].resets_at_ms, Some(1_788_257_925_000));
    }

    #[test]
    fn zai_usage_converts_used_percentage_to_remaining() {
        let quota = serde_json::json!({"data":{"limits":[{"type":"TOKENS_LIMIT","usage":100,"remaining":75,"unit":6,"number":1},{"type":"TOKENS_LIMIT","usage":100,"remaining":50,"unit":5,"number":5}]}});
        let windows = zai_windows(&quota, None);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, Some(50.0));
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].remaining_percent, Some(75.0));
    }

    #[test]
    fn copilot_quota_prefers_explicit_remaining_percent() {
        let window = copilot_quota_window(
            "Premium",
            Some(&serde_json::json!({"entitlement":100,"remaining":5,"percent_remaining":7})),
            None,
        )
        .unwrap();
        assert_eq!(window.remaining_percent, Some(7.0));
    }

    #[test]
    fn kiro_usage_parser_handles_ansi_plan_and_reset() {
        let text =
            "\u{1b}[32mPlan: Kiro Pro\u{1b}[0m\nUsage (10 of 50 covered)\nresets on 2026-09-15";
        let (plan, remaining, amount, reset, managed) = kiro_usage_text(text).unwrap();
        assert_eq!(plan, "Kiro Pro");
        assert_eq!(remaining, 80.0);
        assert_eq!(amount, 40.0);
        assert!(reset.is_some());
        assert!(!managed);
    }

    #[test]
    fn ollama_html_parser_keeps_session_and_weekly_windows() {
        let html = r#"<span>Session usage</span><div>20% used</div><div data-time="2026-09-02T00:00:00Z"></div><span>Weekly usage</span><div>50% used</div>"#;
        let windows = ollama_html_windows(html);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].remaining_percent, Some(80.0));
        assert_eq!(windows[1].kind, WindowKind::Weekly);
    }

    #[test]
    fn commandcode_rolling_windows_keep_plan_cap_and_percent() {
        let plan = commandcode_plan("individual-go").unwrap();
        assert_eq!(plan.0, "Go");
        let window = commandcode_rolling_window(
            "5h",
            WindowKind::Session,
            Some(&serde_json::json!({"cap": 3.0, "used": 1.0})),
            Some(plan.2),
        )
        .unwrap();
        assert!((window.remaining_percent.unwrap() - 66.66666666666667).abs() < 1e-9);
        assert_eq!(window.remaining_amount, Some(2.0));
    }
}

//! UI-neutral domain model for the native Token Monitor.
//!
//! The first native milestone deliberately has no network or credential code.
//! Keeping the quota model independent from the renderer lets provider
//! collectors and the TUI evolve without recreating the third-party GUI.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub mod collectors;
pub mod credentials;
pub mod legacy;
pub mod pricing;
pub mod provider_registry;
pub mod storage;
pub mod usage;

pub const EFFECTIVE_EXHAUSTION_PERCENT: f64 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceHealth {
    Connected,
    Stale,
    Unauthorized,
    Unavailable,
    Error,
}

impl SourceHealth {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Stale => "stale",
            Self::Unauthorized => "unauthorized",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Available,
    Exhausted,
    AgentBlocked,
    Unknown,
}

impl Availability {
    pub fn dimmed(self) -> bool {
        !matches!(self, Self::Available)
    }

    pub fn marker(self) -> char {
        match self {
            Self::Available => '●',
            Self::Exhausted => '✕',
            Self::AgentBlocked => '▲',
            Self::Unknown => '·',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "connected",
            Self::Exhausted => "exhausted",
            Self::AgentBlocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowKind {
    Session,
    Daily,
    Weekly,
    Monthly,
    Billing,
}

impl WindowKind {
    pub fn durable(self) -> bool {
        !matches!(self, Self::Session)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowMetric {
    Quota,
    Credits,
    Spend,
}

impl WindowMetric {
    pub fn is_credit(self) -> bool {
        matches!(self, Self::Credits | Self::Spend)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub label: String,
    pub kind: WindowKind,
    pub metric: WindowMetric,
    pub remaining_percent: Option<f64>,
    pub remaining_amount: Option<f64>,
    pub currency: Option<String>,
    pub resets_at_ms: Option<i64>,
    pub reset_text: Option<String>,
    pub estimated: bool,
}

impl LimitWindow {
    pub fn effectively_exhausted(&self) -> bool {
        if self
            .remaining_amount
            .is_some_and(|amount| self.metric.is_credit() && amount <= 0.0)
        {
            return true;
        }
        self.remaining_percent.is_some_and(|percent| {
            if self.metric.is_credit() {
                return false;
            }
            // The UI shows sub-10% values to one decimal place. Use that same
            // visible value for requestability, so a raw 0.10334% remainder
            // displayed as 0.1% is not advertised as usable.
            let displayed = (percent * 10.0).round() / 10.0;
            displayed <= EFFECTIVE_EXHAUSTION_PERCENT
        })
    }

    pub fn spendable(&self) -> bool {
        if self.effectively_exhausted() {
            return false;
        }
        self.remaining_percent
            .is_some_and(|percent| percent > EFFECTIVE_EXHAUSTION_PERCENT)
            || self
                .remaining_amount
                .is_some_and(|amount| self.metric.is_credit() && amount > 0.0)
    }

    pub fn deadline_ms(&self, now_ms: i64) -> Option<i64> {
        if self.kind.durable() || self.metric.is_credit() {
            return self.resets_at_ms.map(|value| value.max(now_ms));
        }
        if self.effectively_exhausted() {
            return Some(now_ms);
        }
        self.resets_at_ms
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    pub provider_id: String,
    pub account_key: String,
    pub account_label: String,
    pub plan: String,
    pub source: String,
    pub collected_at_ms: i64,
    pub source_health: SourceHealth,
    pub availability: Availability,
    pub windows: Vec<LimitWindow>,
    pub diagnostics: Vec<String>,
    pub hue: u8,
}

impl ProviderSnapshot {
    pub fn has_credits(&self) -> bool {
        self.windows.iter().any(|window| window.metric.is_credit())
    }

    pub fn lowest_remaining_percent(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
    }

    pub fn is_exhausted(&self) -> bool {
        self.availability == Availability::Exhausted
            || self
                .windows
                .iter()
                .any(|w| (w.kind.durable() || w.metric.is_credit()) && w.effectively_exhausted())
    }

    pub fn earliest_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        if self.is_exhausted() {
            return self
                .windows
                .iter()
                .filter(|w| w.kind.durable() || w.metric.is_credit())
                .filter_map(|w| w.deadline_ms(now_ms))
                .min()
                .or_else(|| {
                    self.windows
                        .iter()
                        .filter_map(|w| w.deadline_ms(now_ms))
                        .min()
                });
        }
        self.windows
            .iter()
            .filter(|window| window.spendable())
            .filter_map(|window| window.deadline_ms(now_ms))
            .min()
    }

    pub fn payg(&self) -> bool {
        let id = self.provider_id.to_ascii_lowercase();
        id == "deepseek"
            || id == "openrouter"
            || id == "vast"
            || id == "vastai"
            || self.plan.to_ascii_lowercase().contains("pay-as-you-go")
    }

    /// Whether this row represents a configured/observed source rather than a
    /// collector's empty "not configured" placeholder. The default TUI hides
    /// the latter; explicit `--providers` and `--providers all` still expose
    /// them for diagnostics.
    pub fn visible_by_default(&self) -> bool {
        let diagnostics = self.diagnostics.join(" ").to_ascii_lowercase();
        let not_configured = [
            "not configured",
            "credentials not found",
            "session credentials not found",
            "auth.json not found",
            "cookie not configured",
            "api key not configured",
            "access token not configured",
            "oauth credentials not found",
            "cli unavailable",
            "no modal profile",
            "no coding plan",
            "不存在coding plan",
        ]
        .iter()
        .any(|marker| diagnostics.contains(marker));
        if not_configured {
            return false;
        }
        !self.windows.is_empty()
            || !self.account_key.trim().is_empty()
            || self.source_health == SourceHealth::Unauthorized
            || self.source_health == SourceHealth::Connected
    }
}

/// Default burn-first order. Durable reset deadlines outrank short session
/// resets, while PAYG wallets remain a separate lane at the bottom.
pub fn sort_burn_first(providers: &mut [ProviderSnapshot], now_ms: i64) {
    providers.sort_by(|left, right| {
        let payg_order = left.payg().cmp(&right.payg());
        if payg_order != Ordering::Equal {
            return payg_order;
        }

        let exhausted_order = left.is_exhausted().cmp(&right.is_exhausted());
        if exhausted_order != Ordering::Equal {
            return exhausted_order;
        }

        let left_deadline = left.earliest_deadline_ms(now_ms);
        let right_deadline = right.earliest_deadline_ms(now_ms);
        match (left_deadline, right_deadline) {
            (Some(a), Some(b)) if a != b => return a.cmp(&b),
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            _ => {}
        }

        let left_remaining = left.lowest_remaining_percent().unwrap_or(101.0);
        let right_remaining = right.lowest_remaining_percent().unwrap_or(101.0);
        left_remaining
            .partial_cmp(&right_remaining)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.account_label.cmp(&right.account_label))
    });
}

/// Merge a fresh collector pass over the last-good snapshot. A transient HTTP
/// 429/5xx, expired local process, or CLI timeout must not erase a useful quota
/// row and replace it with an empty "unavailable" placeholder. The failed
/// source is marked stale and its diagnostic is retained; a genuinely fresh
/// row always wins.
pub fn merge_provider_snapshots(
    previous: &[ProviderSnapshot],
    fresh: Vec<ProviderSnapshot>,
) -> Vec<ProviderSnapshot> {
    fresh
        .into_iter()
        .map(|row| {
            if !row.windows.is_empty() || row.source_health == SourceHealth::Connected {
                return row;
            }
            let old = previous.iter().find(|candidate| {
                candidate.provider_id == row.provider_id
                    && ((!row.account_key.is_empty() && candidate.account_key == row.account_key)
                        || row.account_key.is_empty())
                    && !candidate.windows.is_empty()
            });
            let Some(old) = old else {
                return row;
            };
            let mut retained = old.clone();
            retained.source_health = SourceHealth::Stale;
            retained.collected_at_ms = old.collected_at_ms;
            retained.diagnostics = row.diagnostics;
            if retained.diagnostics.is_empty() {
                retained.diagnostics.push(format!(
                    "{} refresh returned no quota data",
                    row.source_health.label()
                ));
            }
            retained
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, kind: WindowKind, percent: f64, reset: i64) -> LimitWindow {
        LimitWindow {
            label: label.into(),
            kind,
            metric: WindowMetric::Quota,
            remaining_percent: Some(percent),
            remaining_amount: None,
            currency: None,
            resets_at_ms: Some(reset),
            reset_text: None,
            estimated: false,
        }
    }

    fn provider(id: &str, plan: &str, windows: Vec<LimitWindow>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: id.into(),
            account_key: id.into(),
            account_label: id.into(),
            plan: plan.into(),
            source: "fixture".into(),
            collected_at_ms: 0,
            source_health: SourceHealth::Connected,
            availability: Availability::Available,
            windows,
            diagnostics: vec![],
            hue: 45,
        }
    }

    #[test]
    fn effective_floor_only_applies_to_quota_windows() {
        assert!(window("G7d", WindowKind::Weekly, 0.1, 100).effectively_exhausted());
        assert!(window("G7d", WindowKind::Weekly, 0.103, 100).effectively_exhausted());
        assert!(!window("G7d", WindowKind::Weekly, 1.0, 100).effectively_exhausted());
        let credit = LimitWindow {
            label: "credit".into(),
            kind: WindowKind::Billing,
            metric: WindowMetric::Credits,
            remaining_percent: Some(0.01),
            remaining_amount: Some(0.01),
            currency: Some("USD".into()),
            resets_at_ms: None,
            reset_text: None,
            estimated: false,
        };
        assert!(!credit.effectively_exhausted());
    }

    #[test]
    fn durable_window_beats_short_session_and_payg() {
        let now = 1_000;
        let mut rows = vec![
            provider("openrouter", "Pay-as-you-go", vec![]),
            provider(
                "codex",
                "Plus",
                vec![window("5h", WindowKind::Session, 90.0, 1_100)],
            ),
            provider(
                "cursor",
                "Free",
                vec![window("Credit", WindowKind::Billing, 100.0, 1_050)],
            ),
        ];
        sort_burn_first(&mut rows, now);
        assert_eq!(
            rows.iter()
                .map(|row| row.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["cursor", "codex", "openrouter"]
        );
    }

    #[test]
    fn exhausted_subscriptions_rank_to_bottom_above_payg() {
        let now = 1_000;
        let mut rows = vec![
            provider("openrouter", "Pay-as-you-go", vec![]),
            provider(
                "codex",
                "Plus",
                vec![
                    window("5h", WindowKind::Session, 100.0, 1_100),
                    window("7d", WindowKind::Weekly, 0.0, 1_500),
                ],
            ),
            provider(
                "claude",
                "Pro",
                vec![window("7d", WindowKind::Weekly, 95.0, 2_000)],
            ),
            provider(
                "commandcode",
                "Go",
                vec![window("7d", WindowKind::Weekly, 88.0, 1_200)],
            ),
            provider(
                "grok",
                "SuperGrok",
                vec![window("7d", WindowKind::Weekly, 0.0, 1_300)],
            ),
        ];
        sort_burn_first(&mut rows, now);
        assert_eq!(
            rows.iter()
                .map(|row| row.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["commandcode", "claude", "grok", "codex", "openrouter"]
        );
    }

    #[test]
    fn default_visibility_hides_unconfigured_placeholders_but_keeps_failures() {
        let mut placeholder = provider("copilot", "", vec![]);
        placeholder.diagnostics = vec!["Copilot API token not configured".into()];
        assert!(!placeholder.visible_by_default());

        let mut failed = provider("claude", "", vec![]);
        failed.account_key = "claude:hashed".into();
        failed.source_health = SourceHealth::Unavailable;
        failed.diagnostics = vec!["HTTP 429".into()];
        assert!(failed.visible_by_default());
    }

    #[test]
    fn transient_empty_refresh_keeps_last_good_windows_as_stale() {
        let old = provider(
            "codex",
            "Plus",
            vec![window("5h", WindowKind::Session, 80.0, 2_000)],
        );
        let mut failed = old.clone();
        failed.windows.clear();
        failed.source_health = SourceHealth::Unavailable;
        failed.diagnostics = vec!["HTTP 429".into()];
        let merged = merge_provider_snapshots(&[old], vec![failed]);
        assert_eq!(merged[0].source_health, SourceHealth::Stale);
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].diagnostics, vec!["HTTP 429"]);
    }
}

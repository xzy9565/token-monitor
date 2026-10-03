//! UI-neutral domain model for the native Token Monitor.
//!
//! The first native milestone deliberately has no network or credential code.
//! Keeping the quota model independent from the renderer lets provider
//! collectors and the TUI evolve without recreating the third-party GUI.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cmp::Ordering;

pub mod collectors;
pub mod credentials;
pub mod legacy;
pub mod pricing;
pub mod provider_registry;
pub mod storage;
pub mod usage;

/// Remaining % at or below which a quota window counts as used up. AGY's weekly 429s all came at
/// or below 1% (2026-09-23→10-01 process logs), but once spent its meter keeps reading up to 1.35%
/// for hours (agy1/agy3, 09-29→10-01), so the floor sits above that bounce.
// ponytail: one floor for every provider; make it per-provider if another meter needs a lower one.
pub const EFFECTIVE_EXHAUSTION_PERCENT: f64 = 2.0;

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
        if self.remaining_amount.is_some_and(|amount| {
            self.metric.is_credit() && (amount <= 0.0 || (amount * 100.0).round() <= 0.0)
        }) {
            return true;
        }
        self.remaining_percent.is_some_and(|percent| {
            if self.metric.is_credit() {
                return percent <= 0.0;
            }
            // The UI shows sub-10% values to one decimal place. Use that same
            // visible value for requestability, so a raw 0.10334% remainder
            // displayed as 0.1% is not advertised as usable.
            let displayed = (percent * 10.0).round() / 10.0;
            displayed <= EFFECTIVE_EXHAUSTION_PERCENT || percent.round() <= 0.0
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
        if self.availability == Availability::Exhausted {
            return true;
        }
        let pid = self.provider_id.to_ascii_lowercase();
        if pid == "antigravity" {
            let gemini_capped = self.windows.iter().any(|w| {
                let l = w.label.to_ascii_lowercase();
                l.contains("gemini") && w.effectively_exhausted()
            });
            let claude_capped = self.windows.iter().any(|w| {
                let l = w.label.to_ascii_lowercase();
                (l.contains("claude") || l.contains("gpt")) && w.effectively_exhausted()
            });
            let has_claude = self.windows.iter().any(|w| {
                let l = w.label.to_ascii_lowercase();
                l.contains("claude") || l.contains("gpt")
            });
            return gemini_capped && (claude_capped || !has_claude);
        }
        if pid == "cursor" {
            let cursor_models = self.windows.iter().find(|w| w.label.to_ascii_lowercase().contains("cursor"));
            let other_models = self.windows.iter().find(|w| w.label.to_ascii_lowercase().contains("other"));
            let c_capped = cursor_models.is_some_and(|w| w.effectively_exhausted());
            let o_capped = other_models.is_some_and(|w| w.effectively_exhausted());
            return match (cursor_models.is_some(), other_models.is_some()) {
                (true, true) => c_capped && o_capped,
                (true, false) => c_capped,
                (false, true) => o_capped,
                _ => {
                    if self.windows.iter().any(|w| !w.metric.is_credit()) {
                        self.windows
                            .iter()
                            .any(|w| !w.metric.is_credit() && w.kind.durable() && w.effectively_exhausted())
                    } else {
                        self.windows.iter().all(|w| w.effectively_exhausted())
                    }
                }
            };
        }
        if self.windows.is_empty() {
            false
        } else if self.windows.iter().any(|w| !w.metric.is_credit()) {
            self.windows
                .iter()
                .any(|w| !w.metric.is_credit() && w.kind.durable() && w.effectively_exhausted())
        } else {
            self.windows.iter().all(|w| w.effectively_exhausted())
        }
    }

    /// Whether this provider is temporarily blocked by a session (5h) cooldown
    /// but still has durable (7d / monthly / billing) capacity remaining.
    /// These should rank below currently-usable providers in burn-first order.
    pub fn is_session_capped(&self) -> bool {
        if self.is_exhausted() {
            return false; // fully exhausted is a separate, lower tier
        }
        let has_session = self.windows.iter().any(|w| {
            w.kind == WindowKind::Session
                || w.label.to_ascii_lowercase().contains("5h")
        });
        if !has_session {
            return false;
        }
        // Every session window must be exhausted for the provider to be capped.
        self.windows
            .iter()
            .filter(|w| {
                w.kind == WindowKind::Session
                    || w.label.to_ascii_lowercase().contains("5h")
            })
            .all(|w| w.effectively_exhausted())
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
            .filter(|window| window.spendable() && (window.kind.durable() || window.metric.is_credit()))
            .filter_map(|window| window.deadline_ms(now_ms))
            .min()
            .or_else(|| {
                self.windows
                    .iter()
                    .filter(|window| window.spendable())
                    .filter_map(|window| window.deadline_ms(now_ms))
                    .min()
            })
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
        let (left, right) = (burn_view(left), burn_view(right));
        let (left, right) = (left.as_ref(), right.as_ref());
        let payg_order = left.payg().cmp(&right.payg());
        if payg_order != Ordering::Equal {
            return payg_order;
        }

        let exhausted_order = left.is_exhausted().cmp(&right.is_exhausted());
        if exhausted_order != Ordering::Equal {
            return exhausted_order;
        }

        // Session-capped providers (5h cooldown, durable pools still open)
        // rank below currently-usable providers but above exhausted ones.
        let capped_order = left.is_session_capped().cmp(&right.is_session_capped());
        if capped_order != Ordering::Equal {
            return capped_order;
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

/// AGY's Claude/GPT pool is about a tenth of its Gemini pool (2026-09-25 meters), so
/// burn-first ranks an AGY account on its Gemini windows alone. Display still sees both pools.
pub fn burn_view(provider: &ProviderSnapshot) -> Cow<'_, ProviderSnapshot> {
    let is_gemini = |w: &LimitWindow| w.label.to_ascii_lowercase().contains("gemini");
    if !provider.provider_id.eq_ignore_ascii_case("antigravity")
        || !provider.windows.iter().any(is_gemini)
    {
        return Cow::Borrowed(provider);
    }
    let mut gemini = provider.clone();
    gemini.windows.retain(is_gemini);
    Cow::Owned(gemini)
}

/// Merge a fresh collector pass over the last-good snapshot. A transient HTTP
/// 429/5xx, expired local process, or CLI timeout must not erase a useful quota
/// row and replace it with an empty "unavailable" placeholder. The failed
pub fn supports_multiple_accounts(provider_id: &str) -> bool {
    matches!(provider_id.to_ascii_lowercase().as_str(), "antigravity" | "modal")
}

/// source is marked stale and its diagnostic is retained; a genuinely fresh
/// row always wins.
pub fn merge_provider_snapshots(
    previous: &[ProviderSnapshot],
    fresh: Vec<ProviderSnapshot>,
) -> Vec<ProviderSnapshot> {
    if fresh.is_empty() {
        return previous
            .iter()
            .cloned()
            .map(|mut row| {
                if row.source_health == SourceHealth::Connected {
                    row.source_health = SourceHealth::Stale;
                }
                row
            })
            .collect();
    }

    let mut merged: Vec<ProviderSnapshot> = fresh
        .into_iter()
        .map(|row| {
            if !row.windows.is_empty() || row.source_health == SourceHealth::Connected {
                return row;
            }
            if !row.visible_by_default() {
                return row;
            }
            let old = previous.iter().find(|candidate| {
                if candidate.provider_id != row.provider_id {
                    return false;
                }
                let accounts_match = if supports_multiple_accounts(&row.provider_id) {
                    (!row.account_key.is_empty() && candidate.account_key == row.account_key)
                        || row.account_key.is_empty()
                } else {
                    true
                };
                accounts_match && !candidate.windows.is_empty()
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
        .collect();

    // Retain any previous provider that was not returned in the fresh batch
    for old in previous {
        let present = merged.iter().any(|candidate| {
            if candidate.provider_id != old.provider_id {
                return false;
            }
            if supports_multiple_accounts(&candidate.provider_id) {
                (!old.account_key.is_empty() && candidate.account_key == old.account_key)
                    || (!old.account_label.is_empty() && candidate.account_label == old.account_label)
            } else {
                // For single-account providers, if candidate already exists in merged, old is NOT retained.
                true
            }
        });
        if !present {
            let mut retained = old.clone();
            if retained.source_health == SourceHealth::Connected {
                retained.source_health = SourceHealth::Stale;
            }
            merged.push(retained);
        }
    }

    merged
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
        assert!(window("G7d", WindowKind::Weekly, 0.4, 100).effectively_exhausted());
        assert!(window("G7d", WindowKind::Weekly, 0.56, 100).effectively_exhausted());
        assert!(window("G7d", WindowKind::Weekly, 1.35, 100).effectively_exhausted());
        assert!(!window("G7d", WindowKind::Weekly, 2.1, 100).effectively_exhausted());
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
    fn durable_cycle_reset_beats_sooner_session_reset() {
        let now = 1_000;
        let mut rows = vec![
            provider(
                "claude",
                "Pro",
                vec![
                    window("5h", WindowKind::Session, 72.0, now + 3_600_000),
                    window("7d", WindowKind::Weekly, 77.0, now + 500_000_000),
                ],
            ),
            provider(
                "commandcode",
                "Go",
                vec![
                    window("5h", WindowKind::Session, 47.0, now + 7_200_000),
                    window("Cycle", WindowKind::Monthly, 11.0, now + 80_000_000),
                ],
            ),
        ];
        sort_burn_first(&mut rows, now);
        assert_eq!(
            rows.iter()
                .map(|row| row.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["commandcode", "claude"]
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
    fn antigravity_ranks_on_gemini_pool_alone() {
        let now = 1_000;
        let agy = |label: &str, gemini_7d: f64, claude_reset: i64| {
            let mut row = provider(
                "antigravity",
                "Google AI Pro",
                vec![
                    window("Gemini 7d", WindowKind::Weekly, gemini_7d, now + 500_000_000),
                    window("Claude/GPT 7d", WindowKind::Weekly, 86.0, claude_reset),
                ],
            );
            row.account_label = label.into();
            row
        };
        // agy1 sits at the Gemini floor but holds the soonest Claude reset; agy3's Gemini is open.
        let mut rows = vec![
            agy("agy1", 0.98, now + 100_000),
            agy("agy3", 91.0, now + 600_000_000),
        ];
        sort_burn_first(&mut rows, now);
        assert_eq!(
            rows.iter()
                .map(|row| row.account_label.as_str())
                .collect::<Vec<_>>(),
            ["agy3", "agy1"]
        );
        assert!(!rows[1].is_exhausted(), "agy1's Claude pool is still usable");
    }

    #[test]
    fn session_capped_ranks_below_usable_above_exhausted() {
        let now = 1_000;
        // Claude: 5h at 0% (capped), 7d at 87% (still has durable capacity).
        // Its 5h resets soonest, but you can't use it right now.
        let claude = provider(
            "claude",
            "Pro",
            vec![
                window("5h", WindowKind::Session, 0.0, now + 2_000_000),
                window("7d", WindowKind::Weekly, 87.0, now + 500_000_000),
            ],
        );
        // Codex: 5h at 55%, 7d at 90%. Currently usable.
        let codex = provider(
            "codex",
            "Plus",
            vec![
                window("5h", WindowKind::Session, 55.0, now + 3_000_000),
                window("7d", WindowKind::Weekly, 90.0, now + 600_000_000),
            ],
        );
        // Grok: 7d at 0%. Fully exhausted.
        let grok = provider(
            "grok",
            "SuperGrok",
            vec![window("7d", WindowKind::Weekly, 0.0, now + 400_000_000)],
        );
        let mut rows = vec![claude, codex, grok];
        sort_burn_first(&mut rows, now);
        assert_eq!(
            rows.iter()
                .map(|row| row.provider_id.as_str())
                .collect::<Vec<_>>(),
            // Usable first, then session-capped, then exhausted.
            ["codex", "claude", "grok"]
        );
        assert!(rows[1].is_session_capped());
        assert!(!rows[1].is_exhausted());
        assert!(rows[2].is_exhausted());
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

    #[test]
    fn empty_or_partial_fresh_keeps_previous_snapshots_as_stale() {
        let codex = provider("codex", "Plus", vec![window("5h", WindowKind::Session, 80.0, 2_000)]);
        let claude = provider("claude", "Pro", vec![window("7d", WindowKind::Weekly, 60.0, 5_000)]);

        // 1. Completely empty fresh list (e.g. timeout / network down)
        let merged_empty = merge_provider_snapshots(&[codex.clone(), claude.clone()], vec![]);
        assert_eq!(merged_empty.len(), 2, "Never wipe out providers on empty refresh");
        assert_eq!(merged_empty[0].source_health, SourceHealth::Stale);
        assert_eq!(merged_empty[1].source_health, SourceHealth::Stale);

        // 2. Partial fresh list (e.g. only codex succeeded, claude missing)
        let mut fresh_codex = codex.clone();
        fresh_codex.windows[0].remaining_percent = Some(85.0);
        fresh_codex.source_health = SourceHealth::Connected;

        let merged_partial = merge_provider_snapshots(&[codex.clone(), claude.clone()], vec![fresh_codex]);
        assert_eq!(merged_partial.len(), 2, "Retain missing providers from previous snapshots");
        assert_eq!(merged_partial[0].provider_id, "codex");
        assert_eq!(merged_partial[0].windows[0].remaining_percent, Some(85.0));
        assert_eq!(merged_partial[1].provider_id, "claude");
        assert_eq!(merged_partial[1].source_health, SourceHealth::Stale);
    }

    #[test]
    fn single_account_provider_does_not_duplicate_when_account_key_rotates() {
        let mut old_codex = provider("codex", "Plus", vec![window("5h", WindowKind::Session, 80.0, 2_000)]);
        old_codex.account_key = "codex:old_token_hash".into();

        let mut fresh_codex = provider("codex", "Plus", vec![window("5h", WindowKind::Session, 85.0, 3_000)]);
        fresh_codex.account_key = "codex:new_token_hash".into();

        let merged = merge_provider_snapshots(&[old_codex], vec![fresh_codex.clone()]);
        assert_eq!(merged.len(), 1, "Must never duplicate single-account providers on token rotation");
        assert_eq!(merged[0].account_key, "codex:new_token_hash");
    }

    #[test]
    fn multi_account_providers_retain_distinct_accounts() {
        let mut ag_acc1 = provider("antigravity", "Pro", vec![window("5h", WindowKind::Session, 80.0, 2_000)]);
        ag_acc1.account_key = "antigravity:user1".into();
        ag_acc1.account_label = "user1@gmail.com".into();

        let mut ag_acc2 = provider("antigravity", "Pro", vec![window("5h", WindowKind::Session, 90.0, 2_000)]);
        ag_acc2.account_key = "antigravity:user2".into();
        ag_acc2.account_label = "user2@gmail.com".into();

        // fresh only returns acc1
        let merged = merge_provider_snapshots(&[ag_acc1.clone(), ag_acc2.clone()], vec![ag_acc1.clone()]);
        assert_eq!(merged.len(), 2, "Must retain missing account of multi-account provider");
        assert_eq!(merged[0].account_label, "user1@gmail.com");
        assert_eq!(merged[1].account_label, "user2@gmail.com");
        assert_eq!(merged[1].source_health, SourceHealth::Stale);
    }
}

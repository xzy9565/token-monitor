//! Stable provider registry shared by CLI, collectors, and future parity work.

pub const ALL_PROVIDER_IDS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "cursor",
    "antigravity",
    "kimi",
    "grok",
    "copilot",
    "commandcode",
    "mimo",
    "zai",
    "zaiteam",
    "kiro",
    "workbuddy",
    "qoder",
    "deepseek",
    "openrouter",
    "minimax",
    "volcengine",
    "ollama",
    "trae",
    "thirdparty",
    "modal",
    "vast",
];

pub const NATIVE_PROVIDER_IDS: &[&str] = &[
    "claude",
    "codex",
    "cursor",
    "antigravity",
    "grok",
    "copilot",
    "commandcode",
    "minimax",
    "zai",
    "zaiteam",
    "qoder",
    "trae",
    "kiro",
    "ollama",
    "deepseek",
    "openrouter",
    "modal",
    "vast",
];

pub fn display_name(id: &str) -> &str {
    match id {
        "antigravity" => "Antigravity",
        "claude" => "Claude",
        "commandcode" => "Command Code",
        "codex" => "Codex",
        "copilot" => "Copilot",
        "cursor" => "Cursor",
        "deepseek" => "DeepSeek",
        "grok" => "Grok",
        "kimi" => "Kimi",
        "kiro" => "Kiro",
        "minimax" => "MiniMax",
        "mimo" => "Mimo",
        "modal" => "Modal",
        "ollama" => "Ollama",
        "openrouter" => "OpenRouter",
        "opencode" => "OpenCode",
        "qoder" => "Qoder",
        "thirdparty" => "Third-party",
        "trae" => "Trae",
        "vast" => "Vast.ai",
        "volcengine" => "Volcengine",
        "workbuddy" => "WorkBuddy",
        "zai" => "Z.ai",
        "zaiteam" => "Z.ai Team",
        other => other,
    }
}

pub fn is_native(id: &str) -> bool {
    NATIVE_PROVIDER_IDS.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_legacy_provider_ids_once() {
        let mut values = ALL_PROVIDER_IDS.to_vec();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), ALL_PROVIDER_IDS.len());
        assert!(is_native("antigravity"));
        assert!(!is_native("kimi"));
    }
}

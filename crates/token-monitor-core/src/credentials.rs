//! Token Monitor-owned interactive credential store.
//!
//! The TUI settings screen writes only this file. It never edits `.zshrc`, the
//! Electron app's credential store, or provider configuration. Values are
//! stored with owner-only permissions and are never included in diagnostics.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const CREDENTIALS_FILE: &str = "credentials.json";

fn default_path() -> Option<PathBuf> {
    crate::storage::StoragePaths::discover()
        .ok()
        .map(|paths| paths.config_dir.join(CREDENTIALS_FILE))
}

fn load_from(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    value
        .get("credentials")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(name, value)| {
                    let value = value.as_str()?.trim();
                    (!name.is_empty() && !value.is_empty())
                        .then(|| (name.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn write_to(path: &Path, values: &BTreeMap<String, String>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "credential path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("create credential directory: {error}"))?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let payload = serde_json::json!({
        "version": 1,
        "credentials": values,
    });
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("encode credentials: {error}"))?;
    fs::write(&temporary, bytes).map_err(|error| format!("write credentials: {error}"))?;
    set_private_permissions(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| format!("commit credentials: {error}"))?;
    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect credentials: {error}"))?;
    }
    Ok(())
}

fn load_app_support_fallback() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let home = match dirs::home_dir() {
        Some(home) => home,
        None => return map,
    };
    let path = home.join("Library/Application Support/Token Monitor/credentials.json");
    let Ok(text) = fs::read_to_string(path) else {
        return map;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    if let Some(providers) = value
        .get("credentials")
        .and_then(|c| c.get("providers"))
        .and_then(Value::as_object)
    {
        for (provider, creds) in providers {
            if let Some(creds_obj) = creds.as_object() {
                for (field, val) in creds_obj {
                    if let Some(s) = val.as_str().filter(|s| !s.trim().is_empty()) {
                        let lower_provider = provider.to_ascii_lowercase();
                        let lower_field = field.to_ascii_lowercase();
                        if lower_provider == "commandcode" && lower_field.contains("cookie") {
                            map.insert(
                                "TOKEN_MONITOR_COMMANDCODE_COOKIE".into(),
                                s.trim().to_owned(),
                            );
                        } else if lower_provider == "claude"
                            && (lower_field.contains("cookie") || lower_field.contains("session"))
                        {
                            map.insert(
                                "TOKEN_MONITOR_CLAUDE_WEB_COOKIE".into(),
                                s.trim().to_owned(),
                            );
                        } else if lower_provider == "openrouter" && lower_field.contains("key") {
                            map.insert(
                                "TOKEN_MONITOR_OPENROUTER_API_KEY".into(),
                                s.trim().to_owned(),
                            );
                        } else if lower_provider == "deepseek" && lower_field.contains("key") {
                            map.insert(
                                "TOKEN_MONITOR_DEEPSEEK_API_KEY".into(),
                                s.trim().to_owned(),
                            );
                        }
                    }
                }
            }
        }
    }
    map
}

pub fn path() -> Option<PathBuf> {
    default_path()
}

pub fn get(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(path) = default_path() {
        if let Some(val) = load_from(&path).get(name).cloned() {
            return Some(val);
        }
    }
    load_app_support_fallback().get(name).cloned()
}

pub fn has(name: &str) -> bool {
    get(name).is_some()
}

pub fn set(name: &str, secret: &str) -> Result<(), String> {
    let name = name.trim();
    let secret = secret.trim();
    if name.is_empty() {
        return Err("credential name is empty".into());
    }
    if secret.is_empty() {
        return remove(name);
    }
    let path = default_path().ok_or_else(|| "config directory not found".to_owned())?;
    let mut values = load_from(&path);
    values.insert(name.to_owned(), secret.to_owned());
    write_to(&path, &values)
}

pub fn remove(name: &str) -> Result<(), String> {
    let Some(path) = default_path() else {
        return Err("config directory not found".into());
    };
    let mut values = load_from(&path);
    values.remove(name.trim());
    if values.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove credentials: {error}")),
        }
    } else {
        write_to(&path, &values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_non_empty_string_credentials() {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-credentials-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{"version":1,"credentials":{"OPENROUTER_API_KEY":"secret","EMPTY":"","NUMBER":4}}"#,
        )
        .unwrap();
        let values = load_from(&path);
        assert_eq!(values.get("OPENROUTER_API_KEY"), Some(&"secret".to_owned()));
        assert!(!values.contains_key("EMPTY"));
        assert!(!values.contains_key("NUMBER"));
        let _ = fs::remove_file(path);
    }
}

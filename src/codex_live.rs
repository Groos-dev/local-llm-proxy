//! Codex live-config takeover aligned with cc-switch local proxy routing.
//!
//! On apply: backup current values, point `base_url` at the local proxy, force
//! `wire_api = "responses"` (Codex always speaks Responses to the proxy), and
//! set auth `OPENAI_API_KEY` to a placeholder (real key stays in proxy config).
//! On restore: write the backed-up values back. Does not touch MCP or unrelated
//! Codex settings. Upstream Chat/Anthropic conversion is decided by the proxy
//! active provider's `api_format`, not by Codex `wire_api`.

use serde_json::{Value, json};
use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs,
    path::{Path, PathBuf},
};

pub const PROXY_TOKEN_PLACEHOLDER: &str = "PROXY_MANAGED";

#[derive(Debug)]
pub struct CodexLiveError(String);

impl CodexLiveError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for CodexLiveError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CodexLiveError {}

fn codex_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(dir);
    }
    dirs_next_home()
        .map(|h| h.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn default_backup_path(run_dir: &Path) -> PathBuf {
    run_dir.join("codex-live-backup.json")
}

/// Locate the active `[model_providers.<name>]` section from top-level
/// `model_provider = "..."`.
fn active_provider_section(config_text: &str) -> Result<String, CodexLiveError> {
    for line in config_text.lines() {
        let trimmed = line.trim();
        let Some((key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "model_provider" {
            continue;
        }
        let value = raw_value.trim().trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    Err(CodexLiveError::new(
        "codex config.toml missing model_provider",
    ))
}

fn read_toml_quoted_field(section_body: &str, field: &str) -> Option<String> {
    let needle = format!("{field}");
    for line in section_body.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&needle) {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        if trimmed[..eq].trim() != field {
            continue;
        }
        let value = trimmed[eq + 1..].trim();
        if let Some(v) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            return Some(v.to_string());
        }
        if let Some(v) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            return Some(v.to_string());
        }
    }
    None
}

fn extract_provider_section<'a>(
    config_text: &'a str,
    provider: &str,
) -> Result<&'a str, CodexLiveError> {
    let header = format!("[model_providers.{provider}]");
    let start = config_text
        .find(&header)
        .ok_or_else(|| CodexLiveError::new(format!("missing section {header}")))?;
    let after = start + header.len();
    let rest = &config_text[after..];
    let end_rel = rest.find("\n[").map(|i| i + 1).unwrap_or(rest.len());
    Ok(&config_text[start..after + end_rel])
}

fn set_toml_quoted_field(section: &str, field: &str, value: &str) -> String {
    let mut replaced = false;
    let mut out = String::new();
    for line in section.lines() {
        let trimmed = line.trim();
        let is_field = trimmed
            .split_once('=')
            .map(|(k, _)| k.trim() == field)
            .unwrap_or(false);
        if is_field {
            out.push_str(&format!("{field} = \"{value}\"\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        // Insert after the section header line.
        let mut lines = out.lines();
        let mut rebuilt = String::new();
        if let Some(first) = lines.next() {
            rebuilt.push_str(first);
            rebuilt.push('\n');
            rebuilt.push_str(&format!("{field} = \"{value}\"\n"));
        }
        for line in lines {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
        return rebuilt;
    }
    out
}

fn remove_toml_field(section: &str, field: &str) -> String {
    let mut out = String::new();
    for line in section.lines() {
        let is_field = line
            .trim()
            .split_once('=')
            .map(|(k, _)| k.trim() == field)
            .unwrap_or(false);
        if !is_field {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn rewrite_provider_fields(
    section: &str,
    base_url: Option<&str>,
    wire_api: Option<&str>,
    remove_wire_api: bool,
) -> String {
    let mut next = match base_url {
        Some(url) => set_toml_quoted_field(section, "base_url", url),
        None => remove_toml_field(section, "base_url"),
    };
    if remove_wire_api {
        next = remove_toml_field(&next, "wire_api");
    } else if let Some(wire) = wire_api {
        next = set_toml_quoted_field(&next, "wire_api", wire);
    }
    next
}

fn replace_section(config_text: &str, old_section: &str, new_section: &str) -> String {
    // Keep a trailing newline so the next TOML table header does not glue onto
    // the last field value (e.g. `token = "x"[projects]`).
    let mut replacement = new_section.to_string();
    if !replacement.ends_with('\n') {
        replacement.push('\n');
    }
    config_text.replacen(old_section, &replacement, 1)
}

fn read_auth_api_key(auth_path: &Path) -> Result<Option<String>, CodexLiveError> {
    if !auth_path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(auth_path)
        .map_err(|e| CodexLiveError::new(format!("read {}: {e}", auth_path.display())))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| CodexLiveError::new(format!("parse auth.json: {e}")))?;
    let key = value
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(key)
}

fn write_auth_api_key(auth_path: &Path, key: Option<&str>) -> Result<(), CodexLiveError> {
    let mut value = if auth_path.exists() {
        let text = fs::read_to_string(auth_path)
            .map_err(|e| CodexLiveError::new(format!("read {}: {e}", auth_path.display())))?;
        serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| CodexLiveError::new("auth.json root must be an object"))?;
    match key {
        Some(k) => {
            obj.insert("OPENAI_API_KEY".to_string(), json!(k));
        }
        None => {
            obj.remove("OPENAI_API_KEY");
        }
    }
    if let Some(parent) = auth_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| CodexLiveError::new(format!("create {}: {e}", parent.display())))?;
    }
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|e| CodexLiveError::new(format!("serialize auth.json: {e}")))?;
    fs::write(auth_path, pretty + "\n")
        .map_err(|e| CodexLiveError::new(format!("write {}: {e}", auth_path.display())))?;
    Ok(())
}

pub fn apply_takeover(proxy_base_url: &str, backup_path: &Path) -> Result<(), CodexLiveError> {
    let home = codex_home();
    let config_path = home.join("config.toml");
    let auth_path = home.join("auth.json");
    let config_text = fs::read_to_string(&config_path)
        .map_err(|e| CodexLiveError::new(format!("read {}: {e}", config_path.display())))?;
    let provider = active_provider_section(&config_text)?;
    let section = extract_provider_section(&config_text, &provider)?;
    let mut previous_base_url = read_toml_quoted_field(section, "base_url");
    let mut previous_wire_api = read_toml_quoted_field(section, "wire_api");
    let had_wire_api = previous_wire_api.is_some();
    let mut auth_existed = auth_path.exists();
    let raw_auth_key = read_auth_api_key(&auth_path)?;
    // Avoid nested-apply poisoning: never record the placeholder / proxy URL as
    // the restore value. Prefer an existing backup when re-applying.
    let mut previous_auth_api_key = raw_auth_key.filter(|k| k != PROXY_TOKEN_PLACEHOLDER);
    let looking_poisoned = previous_base_url
        .as_deref()
        .is_some_and(|u| u.contains("127.0.0.1") || u.contains("localhost"))
        || previous_wire_api.as_deref() == Some("responses")
            && previous_base_url
                .as_deref()
                .is_some_and(|u| u.contains("127.0.0.1") || u.contains("localhost"))
        || previous_auth_api_key.is_none();
    if looking_poisoned {
        if let Ok(existing) = fs::read_to_string(backup_path) {
            if let Ok(value) = serde_json::from_str::<Value>(&existing) {
                if previous_base_url
                    .as_deref()
                    .is_some_and(|u| u.contains("127.0.0.1") || u.contains("localhost"))
                {
                    previous_base_url = value
                        .get("previous_base_url")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                if previous_wire_api.as_deref() == Some("responses")
                    && value.get("previous_wire_api").is_some()
                {
                    previous_wire_api = value
                        .get("previous_wire_api")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                if previous_auth_api_key.is_none() {
                    previous_auth_api_key = value
                        .get("previous_auth_api_key")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    auth_existed = value
                        .get("auth_existed")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(auth_existed);
                }
            }
        }
    }

    // Codex must speak Responses to the local proxy; upstream conversion is
    // decided by the proxy provider's api_format (same as cc-switch).
    let new_section =
        rewrite_provider_fields(section, Some(proxy_base_url), Some("responses"), false);
    let new_config = replace_section(&config_text, section, &new_section);
    fs::write(&config_path, new_config)
        .map_err(|e| CodexLiveError::new(format!("write {}: {e}", config_path.display())))?;
    write_auth_api_key(&auth_path, Some(PROXY_TOKEN_PLACEHOLDER))?;

    let backup = json!({
        "config_path": config_path,
        "auth_path": auth_path,
        "provider_section": provider,
        "previous_base_url": previous_base_url,
        "previous_wire_api": previous_wire_api,
        "had_wire_api": had_wire_api,
        "previous_auth_api_key": previous_auth_api_key,
        "auth_existed": auth_existed,
    });
    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| CodexLiveError::new(format!("create {}: {e}", parent.display())))?;
    }
    fs::write(
        backup_path,
        serde_json::to_string_pretty(&backup).map_err(|e| CodexLiveError::new(e.to_string()))?,
    )
    .map_err(|e| CodexLiveError::new(format!("write {}: {e}", backup_path.display())))?;
    Ok(())
}

pub fn restore_takeover(backup_path: &Path) -> Result<(), CodexLiveError> {
    if !backup_path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(backup_path)
        .map_err(|e| CodexLiveError::new(format!("read {}: {e}", backup_path.display())))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| CodexLiveError::new(format!("parse backup: {e}")))?;

    let config_path = PathBuf::from(
        value
            .get("config_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CodexLiveError::new("backup missing config_path"))?,
    );
    let auth_path = PathBuf::from(
        value
            .get("auth_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CodexLiveError::new("backup missing auth_path"))?,
    );
    let provider = value
        .get("provider_section")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CodexLiveError::new("backup missing provider_section"))?;
    let previous_base_url = value
        .get("previous_base_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let previous_wire_api = value
        .get("previous_wire_api")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let had_wire_api = value
        .get("had_wire_api")
        .and_then(|v| v.as_bool())
        .unwrap_or(previous_wire_api.is_some());
    let previous_auth_api_key = value
        .get("previous_auth_api_key")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let auth_existed = value
        .get("auth_existed")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if config_path.exists() {
        let config_text = fs::read_to_string(&config_path)
            .map_err(|e| CodexLiveError::new(format!("read {}: {e}", config_path.display())))?;
        if let Ok(section) = extract_provider_section(&config_text, provider) {
            let new_section = rewrite_provider_fields(
                section,
                previous_base_url.as_deref(),
                previous_wire_api.as_deref(),
                !had_wire_api,
            );
            let new_config = replace_section(&config_text, section, &new_section);
            fs::write(&config_path, new_config).map_err(|e| {
                CodexLiveError::new(format!("write {}: {e}", config_path.display()))
            })?;
        }
    }

    if auth_existed {
        write_auth_api_key(&auth_path, previous_auth_api_key.as_deref())?;
    } else if let Err(err) = fs::remove_file(&auth_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(CodexLiveError::new(format!(
            "remove {}: {err}",
            auth_path.display()
        )));
    }
    let _ = fs::remove_file(backup_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static CODEX_HOME_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("llpx-codex-live-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apply_and_restore_round_trip() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = temp_dir();
        let config_path = dir.join("config.toml");
        let auth_path = dir.join("auth.json");
        fs::write(
            &config_path,
            r#"model_provider = "acme"
model = "gpt-5.4"

[model_providers.acme]
name = "acme"
base_url = "https://upstream.example/v1"
wire_api = "chat"
"#,
        )
        .unwrap();
        fs::write(
            &auth_path,
            r#"{
  "OPENAI_API_KEY": "sk-real"
}
"#,
        )
        .unwrap();

        // Point CODEX_HOME at temp dir for this test.
        // Safety: test-only env mutation.
        unsafe {
            std::env::set_var("CODEX_HOME", &dir);
        }

        let backup = dir.join("backup.json");
        apply_takeover("http://127.0.0.1:8787/v1", &backup).unwrap();

        let config_after = fs::read_to_string(&config_path).unwrap();
        assert!(config_after.contains("base_url = \"http://127.0.0.1:8787/v1\""));
        assert!(config_after.contains("wire_api = \"responses\""));
        assert!(!config_after.contains("wire_api = \"chat\""));
        assert!(config_after.contains("model = \"gpt-5.4\""));
        let auth_after: Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth_after["OPENAI_API_KEY"], PROXY_TOKEN_PLACEHOLDER);

        restore_takeover(&backup).unwrap();
        let config_restored = fs::read_to_string(&config_path).unwrap();
        assert!(config_restored.contains("base_url = \"https://upstream.example/v1\""));
        assert!(config_restored.contains("wire_api = \"chat\""));
        let auth_restored: Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth_restored["OPENAI_API_KEY"], "sk-real");
        assert!(!backup.exists());

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_provider_parser_does_not_match_similar_keys() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let config = r#"
model_providers = "not-a-provider"
model_provider = "actual"
"#;
        assert_eq!(active_provider_section(config).unwrap(), "actual");
    }

    #[test]
    fn restore_removes_placeholder_when_auth_had_no_api_key() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = temp_dir();
        fs::write(
            dir.join("config.toml"),
            r#"model_provider = "acme"

[model_providers.acme]
base_url = "https://upstream.example/v1"
"#,
        )
        .unwrap();
        fs::write(dir.join("auth.json"), r#"{"other": true}"#).unwrap();
        unsafe {
            std::env::set_var("CODEX_HOME", &dir);
        }

        let backup = dir.join("backup.json");
        apply_takeover("http://127.0.0.1:8787/v1", &backup).unwrap();
        restore_takeover(&backup).unwrap();

        let auth: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert_eq!(auth["other"], true);

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_removes_auth_file_when_it_did_not_exist() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = temp_dir();
        fs::write(
            dir.join("config.toml"),
            r#"model_provider = "acme"

[model_providers.acme]
base_url = "https://upstream.example/v1"
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("CODEX_HOME", &dir);
        }

        let backup = dir.join("backup.json");
        apply_takeover("http://127.0.0.1:8787/v1", &backup).unwrap();
        assert!(dir.join("auth.json").exists());
        restore_takeover(&backup).unwrap();
        assert!(!dir.join("auth.json").exists());

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_auth_does_not_partially_apply_takeover() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = temp_dir();
        let config = dir.join("config.toml");
        let auth = dir.join("auth.json");
        let original_config = r#"model_provider = "acme"

[model_providers.acme]
base_url = "https://upstream.example/v1"
"#;
        fs::write(&config, original_config).unwrap();
        fs::write(&auth, "not json").unwrap();
        unsafe {
            std::env::set_var("CODEX_HOME", &dir);
        }

        let backup = dir.join("backup.json");
        assert!(apply_takeover("http://127.0.0.1:8787/v1", &backup).is_err());
        assert_eq!(fs::read_to_string(config).unwrap(), original_config);
        assert_eq!(fs::read_to_string(auth).unwrap(), "not json");
        assert!(!backup.exists());

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

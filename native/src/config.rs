//! Persisted user configuration: `$UG_HOME/config.json` (`~/.ug/config.json`).
//!
//! Sits one tier below env vars in the precedence chain every command
//! resolves:
//!
//!   CLI flag  >  env var  >  config file  >  built-in default
//!
//! `ug config set/get/unset/list` manage the file; `resolve_pref_cfg`
//! is the shared lookup used by the embedder/chat builders. When a
//! flag or env var overrides a value the user persisted, we print a
//! one-time stderr notice so the override never happens silently.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use serde_json::Value;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

use crate::project::ug_home;
use crate::PrefSource;

/// Value type a config key accepts. `set` validates against this so a
/// typo like `ug config set chat.temperature warm` fails at write time,
/// not at first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Str,
    F32,
    U32,
    U64,
}

/// One persistable setting: its dotted CLI name, where it lives in the
/// JSON file (`section` + camelCase `field`), the env var and flag that
/// outrank it, and how to validate it.
pub(crate) struct ConfigKey {
    pub name: &'static str,
    pub section: &'static str,
    pub field: &'static str,
    pub env: Option<&'static str>,
    pub flag: &'static str,
    pub kind: Kind,
    pub secret: bool,
    pub desc: &'static str,
}

/// Every key `ug config` accepts. Adding a row here is all it takes to
/// make a new setting persistable — the list/get/set/unset commands and
/// the resolver are all registry-driven.
pub(crate) const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey { name: "chat.model", section: "chat", field: "model", env: Some("UG_CHAT_MODEL"), flag: "--chat-model", kind: Kind::Str, secret: false, desc: "chat completion model (ug chat / POST /api/chat)" },
    ConfigKey { name: "chat.base_url", section: "chat", field: "baseUrl", env: Some("UG_CHAT_BASE_URL"), flag: "--chat-base-url", kind: Kind::Str, secret: false, desc: "OpenAI-compatible chat endpoint base URL" },
    ConfigKey { name: "chat.api_key", section: "chat", field: "apiKey", env: Some("UG_CHAT_API_KEY"), flag: "--chat-api-key", kind: Kind::Str, secret: true, desc: "API key for the chat endpoint" },
    ConfigKey { name: "chat.temperature", section: "chat", field: "temperature", env: None, flag: "--temperature", kind: Kind::F32, secret: false, desc: "chat sampling temperature" },
    ConfigKey { name: "chat.max_tokens", section: "chat", field: "maxTokens", env: None, flag: "--max-tokens", kind: Kind::U32, secret: false, desc: "chat completion max tokens" },
    ConfigKey { name: "chat.timeout_secs", section: "chat", field: "timeoutSecs", env: None, flag: "--chat-timeout", kind: Kind::U64, secret: false, desc: "chat request timeout (seconds)" },
    ConfigKey { name: "embed.model", section: "embed", field: "model", env: Some("UG_EMBED_MODEL"), flag: "--model", kind: Kind::Str, secret: false, desc: "embedding model (local alias or remote model name)" },
    ConfigKey { name: "embed.base_url", section: "embed", field: "baseUrl", env: Some("UG_EMBED_BASE_URL"), flag: "--base-url", kind: Kind::Str, secret: false, desc: "remote /v1/embeddings base URL (unset = local in-process)" },
    ConfigKey { name: "embed.api_key", section: "embed", field: "apiKey", env: Some("UG_EMBED_API_KEY"), flag: "--api-key", kind: Kind::Str, secret: true, desc: "API key for the embeddings endpoint" },
    ConfigKey { name: "embed.dim", section: "embed", field: "dim", env: None, flag: "--embedding-dim", kind: Kind::U32, secret: false, desc: "embedding dimension override (normally auto-probed)" },
];

/// Look up a registry entry by dotted name. Accepts `-` for `_` and is
/// case-insensitive so `chat.base-url` and `Chat.Base_URL` both work.
pub(crate) fn find_key(name: &str) -> Option<&'static ConfigKey> {
    let norm = name.trim().to_ascii_lowercase().replace('-', "_");
    CONFIG_KEYS.iter().find(|k| k.name == norm)
}

pub(crate) fn config_path() -> PathBuf {
    ug_home().join("config.json")
}

/// Parse a config file into a JSON tree. Missing file → empty object.
/// A malformed file is an error — `set` must not silently clobber a
/// file the user hand-edited into invalid JSON.
pub(crate) fn read_config_file(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| format!("{} isn't valid JSON — fix or remove it ({})", path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(format!("failed to read {}: {}", path.display(), e)),
    }
}

/// Write the config file with owner-only permissions (it may hold API
/// keys).
pub(crate) fn write_config_file(path: &Path, cfg: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
    }
    let json = serde_json::to_string_pretty(cfg).expect("config Value serializes") + "\n";
    std::fs::write(path, json).map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Read one key out of a parsed config tree as a string. Numbers are
/// stringified so callers can share one `Option<String>` code path with
/// flags/env vars; blank strings count as unset.
pub(crate) fn value_get(cfg: &Value, key: &ConfigKey) -> Option<String> {
    let v = cfg.get(key.section)?.get(key.field)?;
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Validate `raw` against the key's kind and store it (typed) in the
/// tree.
pub(crate) fn value_set(cfg: &mut Value, key: &ConfigKey, raw: &str) -> Result<(), String> {
    let parsed = match key.kind {
        Kind::Str => Value::String(raw.to_string()),
        Kind::F32 => {
            // Parse as f64 so round decimals like 0.7 don't pick up f32
            // widening noise in the stored JSON; consumers narrow to f32.
            let f: f64 = raw.parse().map_err(|_| format!("{} expects a number, got '{}'", key.name, raw))?;
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| format!("{} must be a finite number", key.name))?
        }
        Kind::U32 => {
            let n: u32 = raw.parse().map_err(|_| format!("{} expects a non-negative integer, got '{}'", key.name, raw))?;
            Value::Number(n.into())
        }
        Kind::U64 => {
            let n: u64 = raw.parse().map_err(|_| format!("{} expects a non-negative integer, got '{}'", key.name, raw))?;
            Value::Number(n.into())
        }
    };
    if !cfg.is_object() {
        *cfg = Value::Object(Default::default());
    }
    let section = cfg
        .as_object_mut()
        .unwrap()
        .entry(key.section)
        .or_insert_with(|| Value::Object(Default::default()));
    if !section.is_object() {
        *section = Value::Object(Default::default());
    }
    section
        .as_object_mut()
        .unwrap()
        .insert(key.field.to_string(), parsed);
    Ok(())
}

/// Remove one key; returns whether it was present. Empty sections are
/// pruned so a fully-unset file goes back to `{}`.
pub(crate) fn value_unset(cfg: &mut Value, key: &ConfigKey) -> bool {
    let Some(section) = cfg.get_mut(key.section).and_then(|s| s.as_object_mut()) else {
        return false;
    };
    let existed = section.remove(key.field).is_some();
    if section.is_empty() {
        if let Some(obj) = cfg.as_object_mut() {
            obj.remove(key.section);
        }
    }
    existed
}

/// The parsed config file, loaded once per process and reloadable —
/// `ug serve` rewrites the file through `POST /api/config` and calls
/// `reload()` so the change applies without a restart. An unreadable or
/// malformed file degrades to "no persisted config" for resolution —
/// commands shouldn't die because of it — but we warn so the user knows
/// their saved settings aren't being applied.
fn cache() -> &'static RwLock<Value> {
    static CACHE: OnceLock<RwLock<Value>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(load_from_disk()))
}

fn load_from_disk() -> Value {
    let path = config_path();
    match read_config_file(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{C_YELLOW}▸ warning:{C_RESET} ignoring persisted config: {}", e);
            Value::Object(Default::default())
        }
    }
}

/// Re-read the config file into the process cache after an in-process
/// write (the `ug config` CLI exits after writing, so only long-lived
/// callers like `ug serve` need this).
pub(crate) fn reload() {
    *cache().write().expect("config cache poisoned") = load_from_disk();
}

fn with_loaded<T>(f: impl FnOnce(&Value) -> T) -> T {
    f(&cache().read().expect("config cache poisoned"))
}

/// Persisted value for a dotted key name, if the file has one.
pub(crate) fn get(name: &str) -> Option<String> {
    let key = find_key(name)?;
    with_loaded(|cfg| value_get(cfg, key))
}

/// Built-in default used when no tier overrides the key, for display in
/// `ug config`-adjacent UIs. `None` where "unset" is itself the default
/// (API keys; `embed.base_url`, where unset means local in-process;
/// `embed.dim`, which is auto-probed).
pub(crate) fn default_for(key: &ConfigKey) -> Option<String> {
    match key.name {
        "chat.model" => Some(crate::chat::DEFAULT_CHAT_MODEL.to_string()),
        "chat.base_url" => Some(crate::chat::DEFAULT_CHAT_BASE_URL.to_string()),
        "chat.temperature" => Some(crate::chat::DEFAULT_TEMPERATURE.to_string()),
        "chat.max_tokens" => Some(crate::chat::DEFAULT_MAX_TOKENS.to_string()),
        "chat.timeout_secs" => Some(crate::chat::DEFAULT_TIMEOUT_SECS.to_string()),
        "embed.model" => Some(ultragraph::storage::DEFAULT_MODEL.to_string()),
        _ => None,
    }
}

/// Mask secrets for display: keep a short prefix, elide the rest.
pub(crate) fn display_value(key: &ConfigKey, val: &str) -> String {
    if !key.secret {
        return val.to_string();
    }
    let prefix: String = val.chars().take(4).collect();
    format!("{}… ({} chars)", prefix, val.chars().count())
}

/// Four-tier precedence: flag > env > config file > default. Same
/// contract as `resolve_pref`, plus the persisted tier. When a flag or
/// env var outranks a *different* value the user saved with `ug config
/// set`, print a one-time stderr notice — the override still wins, the
/// user just gets told.
pub(crate) fn resolve_pref_cfg(
    flag: Option<String>,
    cfg_name: &'static str,
) -> (Option<String>, PrefSource) {
    let key = find_key(cfg_name).unwrap_or_else(|| panic!("unknown config key: {}", cfg_name));
    let saved = with_loaded(|cfg| value_get(cfg, key));
    let (resolved, src) = match key.env {
        Some(env_key) => crate::resolve_pref(flag, env_key),
        None => match flag {
            Some(v) => (Some(v), PrefSource::Flag),
            None => (None, PrefSource::Default),
        },
    };
    match (&resolved, src) {
        (Some(v), PrefSource::Flag) => {
            if let Some(s) = &saved {
                if s != v {
                    notice_override(key, "CLI flag", key.flag, s);
                }
            }
            (resolved, src)
        }
        (Some(v), PrefSource::Env(env_key)) => {
            if let Some(s) = &saved {
                if s != v {
                    notice_override(key, "env var", env_key, s);
                }
            }
            (resolved, src)
        }
        _ => match saved {
            Some(s) => (Some(s), PrefSource::Config(key.name)),
            None => (None, PrefSource::Default),
        },
    }
}

/// Stderr note that an explicit flag/env var beat a persisted value.
/// Deduped per key so REPL-ish commands that resolve config more than
/// once don't repeat themselves.
fn notice_override(key: &ConfigKey, tier: &str, source: &str, saved: &str) {
    static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if !seen.lock().map(|mut s| s.insert(key.name)).unwrap_or(false) {
        return;
    }
    eprintln!(
        "{C_CYAN}▸ note:{C_RESET} {tier} {C_BOLD}{source}{C_RESET} overrides saved config {C_BOLD}{}{C_RESET} = {} {C_DIM}({}){C_RESET}",
        key.name,
        display_value(key, saved),
        config_path().display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> &'static ConfigKey {
        find_key(name).unwrap()
    }

    #[test]
    fn find_key_normalizes_name() {
        assert!(find_key("chat.model").is_some());
        assert!(find_key("chat.base-url").is_some());
        assert!(find_key("CHAT.MAX_TOKENS").is_some());
        assert!(find_key("chat.nope").is_none());
    }

    #[test]
    fn set_get_unset_roundtrip() {
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("chat.model"), "qwen3").unwrap();
        value_set(&mut cfg, key("chat.temperature"), "0.7").unwrap();
        assert_eq!(value_get(&cfg, key("chat.model")).as_deref(), Some("qwen3"));
        assert_eq!(value_get(&cfg, key("chat.temperature")).as_deref(), Some("0.7"));
        // Stored under the camelCase field in the right section.
        assert!(cfg["chat"]["temperature"].is_number());
        assert!(value_unset(&mut cfg, key("chat.model")));
        assert!(!value_unset(&mut cfg, key("chat.model")));
        assert_eq!(value_get(&cfg, key("chat.model")), None);
    }

    #[test]
    fn unset_prunes_empty_section() {
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("embed.dim"), "768").unwrap();
        assert!(value_unset(&mut cfg, key("embed.dim")));
        assert!(cfg.get("embed").is_none());
    }

    #[test]
    fn numeric_keys_reject_garbage() {
        let mut cfg = Value::Object(Default::default());
        assert!(value_set(&mut cfg, key("chat.temperature"), "warm").is_err());
        assert!(value_set(&mut cfg, key("chat.max_tokens"), "-5").is_err());
        assert!(value_set(&mut cfg, key("embed.dim"), "3.5").is_err());
    }

    #[test]
    fn blank_string_counts_as_unset() {
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("chat.model"), "   ").unwrap();
        assert_eq!(value_get(&cfg, key("chat.model")), None);
    }

    #[test]
    fn read_missing_file_is_empty() {
        let path = std::env::temp_dir().join(format!("ug-cfg-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = read_config_file(&path).unwrap();
        assert!(cfg.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_malformed_file_errors() {
        let path = std::env::temp_dir().join(format!("ug-cfg-bad-{}.json", std::process::id()));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(read_config_file(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ug-cfg-rw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("chat.model"), "m1").unwrap();
        write_config_file(&path, &cfg).unwrap();
        let back = read_config_file(&path).unwrap();
        assert_eq!(value_get(&back, key("chat.model")).as_deref(), Some("m1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_display_is_masked() {
        let k = key("chat.api_key");
        let shown = display_value(k, "sk-abcdef123456");
        assert!(shown.starts_with("sk-a"));
        assert!(!shown.contains("abcdef123456"));
    }
}

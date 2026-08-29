//! Persisted user configuration: `$UG_HOME/config.json` (`~/.ug/config.json`).
//!
//! Sits below CLI flags in the precedence chain:
//!
//!   CLI flag  >  config file  >  built-in default
//!
//! `ug config set/get/unset/list` manage the file; `resolve_pref_cfg`
//! is the shared lookup used by the embedder/chat builders. When a
//! flag overrides a value the user persisted, we print a one-time
//! stderr notice so the override never happens silently.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use serde_json::Value;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

use crate::project::ug_home;
use crate::cli::embed::PrefSource;

/// Value type a config key accepts. `set` validates against this so a
/// typo like `ug config set chat.temperature warm` fails at write time,
/// not at first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Str,
    F32,
    U32,
    U64,
    /// One of a fixed set of spellings, validated and normalized to the
    /// canonical form (e.g. `vis.renderer` accepts only auto/three/cosmos).
    Enum(&'static [&'static str]),
}

/// One persistable setting: its dotted CLI name, where it lives in the
/// JSON file (`section` + camelCase `field`), the flag that overrides it,
/// and how to validate it. `min` is the inclusive lower bound for the
/// numeric kinds — the same bound the settings panel enforces client-side,
/// so a value the UI would reject never reaches the file.
pub(crate) struct ConfigKey {
    pub name: &'static str,
    pub section: &'static str,
    pub field: &'static str,
    pub flag: &'static str,
    pub kind: Kind,
    pub min: u64,
    pub secret: bool,
    pub desc: &'static str,
}

/// Every key `ug config` accepts. Adding a row here is all it takes to
/// make a new setting persistable — the list/get/set/unset commands and
/// the resolver are all registry-driven.
pub(crate) const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey { name: "chat.model", section: "chat", field: "model", flag: "--chat-model", kind: Kind::Str, min: 0, secret: false, desc: "chat completion model (ug chat / POST /api/chat)" },
    ConfigKey { name: "chat.base_url", section: "chat", field: "baseUrl", flag: "--chat-base-url", kind: Kind::Str, min: 0, secret: false, desc: "OpenAI-compatible chat endpoint base URL" },
    ConfigKey { name: "chat.api_key", section: "chat", field: "apiKey", flag: "--chat-api-key", kind: Kind::Str, min: 0, secret: true, desc: "API key for the chat endpoint" },
    ConfigKey { name: "chat.temperature", section: "chat", field: "temperature", flag: "--temperature", kind: Kind::F32, min: 0, secret: false, desc: "chat sampling temperature" },
    ConfigKey { name: "chat.max_tokens", section: "chat", field: "maxTokens", flag: "--max-tokens", kind: Kind::U32, min: 1, secret: false, desc: "chat completion max tokens" },
    ConfigKey { name: "chat.timeout_secs", section: "chat", field: "timeoutSecs", flag: "--chat-timeout", kind: Kind::U64, min: 1, secret: false, desc: "chat request timeout (seconds)" },
    ConfigKey { name: "embed.model", section: "embed", field: "model", flag: "--model", kind: Kind::Str, min: 0, secret: false, desc: "embedding model (local alias or remote model name)" },
    ConfigKey { name: "embed.base_url", section: "embed", field: "baseUrl", flag: "--base-url", kind: Kind::Str, min: 0, secret: false, desc: "remote /v1/embeddings base URL (unset = local in-process)" },
    ConfigKey { name: "embed.api_key", section: "embed", field: "apiKey", flag: "--api-key", kind: Kind::Str, min: 0, secret: true, desc: "API key for the embeddings endpoint" },
    ConfigKey { name: "embed.dim", section: "embed", field: "dim", flag: "--embedding-dim", kind: Kind::U32, min: 1, secret: false, desc: "embedding dimension override (normally auto-probed)" },
    ConfigKey { name: "embed.section_cap", section: "embed", field: "sectionCap", flag: "--section-cap", kind: Kind::U32, min: 1, secret: false, desc: "chars of a node's description to embed (default: derived from the model's token window)" },
    ConfigKey { name: "vis.renderer", section: "vis", field: "renderer", flag: "", kind: Kind::Enum(&["auto", "three", "cosmos"]), min: 0, secret: false, desc: "preferred rendering engine: auto (three below three_d_max_elements, cosmos above), three, or cosmos" },
    ConfigKey { name: "vis.three_d_max_elements", section: "vis", field: "threeDMaxElements", flag: "", kind: Kind::U32, min: 100, secret: false, desc: "max nodes/edges the 3D engine draws whole; above it auto switches to the 2D engine and 3D solo-passes neighbourhoods" },
    ConfigKey { name: "vis.solo_threshold", section: "vis", field: "soloThreshold", flag: "", kind: Kind::U32, min: 1, secret: false, desc: "nodes/edges past which the page opens in solo mode (the 2D engine's ceiling)" },
    ConfigKey { name: "vis.link_blending", section: "vis", field: "linkBlending", flag: "", kind: Kind::Enum(&["on", "off"]), min: 0, secret: false, desc: "additive blending for links in the 2D engine: richer where strands overlap, but the single biggest per-frame cost at high resolution (off is ~2.3x cheaper at 3400x2000)" },
    ConfigKey { name: "graph.server_mode_bytes", section: "graph", field: "serverModeBytes", flag: "", kind: Kind::U64, min: 1024, secret: false, desc: "graph.json bytes at/above which the browser is served the slim node index instead of the whole file (server mode)" },
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
            if (n as u64) < key.min {
                return Err(format!("{} must be at least {}", key.name, key.min));
            }
            Value::Number(n.into())
        }
        Kind::U64 => {
            let n: u64 = raw.parse().map_err(|_| format!("{} expects a non-negative integer, got '{}'", key.name, raw))?;
            if n < key.min {
                return Err(format!("{} must be at least {}", key.name, key.min));
            }
            Value::Number(n.into())
        }
        Kind::Enum(allowed) => {
            // Case-insensitive so `vis.renderer THREE` works, but stored
            // under the canonical spelling so the UI dropdown matches.
            let canon = raw.trim().to_ascii_lowercase();
            let Some(&hit) = allowed.iter().find(|a| **a == canon) else {
                return Err(format!(
                    "{} must be one of: {}",
                    key.name,
                    allowed.join(", ")
                ));
            };
            Value::String(hit.to_string())
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
        "vis.renderer" => Some("auto".to_string()),
        // Mirrors THREE_D_MAX_ELEMENTS in native/src/vis/js/10-render-core.js —
        // display values only; the page falls back to those constants when unset.
        "vis.three_d_max_elements" => Some("3000".to_string()),
        // Mirrors SOLO_THRESHOLD in native/src/vis/js/16-solo-view.js.
        "vis.solo_threshold" => Some("200000".to_string()),
        // Mirrors LINK_BLENDING_DEFAULT in native/src/vis/js/12-render-cosmos.js.
        "vis.link_blending" => Some("on".to_string()),
        // Mirrors GRAPH_SERVER_MODE_BYTES in native/src/serve.rs.
        "graph.server_mode_bytes" => Some("52428800".to_string()),
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

/// Three-tier precedence: flag > config file > default. When a flag
/// outranks a *different* value the user saved with `ug config set`,
/// print a one-time stderr notice — the flag still wins, the user just
/// gets told.
pub(crate) fn resolve_pref_cfg(
    flag: Option<String>,
    cfg_name: &'static str,
) -> (Option<String>, PrefSource) {
    let key = find_key(cfg_name).unwrap_or_else(|| panic!("unknown config key: {}", cfg_name));
    let saved = with_loaded(|cfg| value_get(cfg, key));
    match flag {
        Some(v) => {
            if let Some(s) = &saved {
                if s != &v {
                    notice_override(key, "CLI flag", key.flag, s);
                }
            }
            (Some(v), PrefSource::Flag)
        }
        None => match saved {
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
    fn enum_keys_validate_and_normalize() {
        let mut cfg = Value::Object(Default::default());
        // Canonical value saved as-is.
        value_set(&mut cfg, key("vis.renderer"), "cosmos").unwrap();
        assert_eq!(value_get(&cfg, key("vis.renderer")).as_deref(), Some("cosmos"));
        // Case-insensitive on input, canonical on output.
        value_set(&mut cfg, key("vis.renderer"), "THREE").unwrap();
        assert_eq!(value_get(&cfg, key("vis.renderer")).as_deref(), Some("three"));
        // Anything outside the allowed set is rejected.
        let err = value_set(&mut cfg, key("vis.renderer"), "vulkan").unwrap_err();
        assert!(err.contains("one of"), "{err}");
        assert!(value_set(&mut cfg, key("vis.renderer"), "").is_err());
    }

    #[test]
    fn solo_threshold_is_a_positive_integer() {
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("vis.solo_threshold"), "50000").unwrap();
        assert!(cfg["vis"]["soloThreshold"].is_number());
        assert!(value_set(&mut cfg, key("vis.solo_threshold"), "zero").is_err());
        assert!(value_set(&mut cfg, key("vis.solo_threshold"), "-1").is_err());
        assert!(value_set(&mut cfg, key("vis.solo_threshold"), "0").is_err());
    }

    #[test]
    fn server_mode_bytes_is_a_positive_integer() {
        let mut cfg = Value::Object(Default::default());
        value_set(&mut cfg, key("graph.server_mode_bytes"), "104857600").unwrap();
        assert!(cfg["graph"]["serverModeBytes"].is_number());
        assert_eq!(value_get(&cfg, key("graph.server_mode_bytes")).as_deref(), Some("104857600"));
        assert!(value_set(&mut cfg, key("graph.server_mode_bytes"), "lots").is_err());
        assert!(value_set(&mut cfg, key("graph.server_mode_bytes"), "-1").is_err());
        assert!(value_set(&mut cfg, key("graph.server_mode_bytes"), "3.5").is_err());
        // Below the 1 KB floor — a threshold that small would put every
        // graph, however tiny, into server mode by accident.
        assert!(value_set(&mut cfg, key("graph.server_mode_bytes"), "1023").is_err());
        assert!(value_set(&mut cfg, key("graph.server_mode_bytes"), "1024").is_ok());
    }

    #[test]
    fn numeric_keys_enforce_their_minimums() {
        let mut cfg = Value::Object(Default::default());
        assert!(value_set(&mut cfg, key("vis.three_d_max_elements"), "99").is_err());
        assert!(value_set(&mut cfg, key("vis.three_d_max_elements"), "100").is_ok());
        assert!(value_set(&mut cfg, key("chat.max_tokens"), "0").is_err());
        assert!(value_set(&mut cfg, key("chat.timeout_secs"), "0").is_err());
        assert!(value_set(&mut cfg, key("embed.dim"), "0").is_err());
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

//! `ug mcp install` / `ug mcp uninstall`: write (or strip) the `ultragraph`
//! server entry in an MCP client's config file, and drop the agent skill/rule
//! file next to it. Ported from the `MCP_INSTALL_TARGETS` / `SKILL_TARGETS`
//! tables in the old `node/cli.mjs`.
//!
//! Three config formats are handled: JSON (most clients), TOML (Codex) and
//! YAML (Hermes). JSON goes through `serde_json` (key order preserved via the
//! `preserve_order` feature). TOML is spliced by text range so unrelated
//! tables/comments survive. YAML round-trips through `serde_yaml` — note this
//! does not preserve comments, unlike the old JS path.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::project::derive_project_name;
use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

const SKILL_MD: &str = include_str!("ug-skill.md");

/// Config file layout for a target — how a `{ command, args, env }` server
/// entry is grafted into that client's own JSON/TOML/YAML shape.
#[derive(Clone, Copy)]
enum Format {
    /// `mcpServers.<name> = { command, args, env }` (Claude, Cursor, …).
    JsonMcpServers,
    /// `servers.<name> = { type: "stdio", command, args, env }` (VS Code).
    JsonVscode,
    /// `mcp.<name> = { type: "local", command: [...], environment, enabled }`
    /// (opencode).
    JsonOpencode,
    Toml,
    Yaml,
}

struct Target {
    key: &'static str,
    label: &'static str,
    format: Format,
    project_path: Option<fn() -> PathBuf>,
    global_path: Option<fn() -> PathBuf>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn claude_desktop_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/Claude/claude_desktop_config.json")
    } else if cfg!(target_os = "windows") {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join("AppData/Roaming"));
        appdata.join("Claude/claude_desktop_config.json")
    } else {
        home().join(".config/Claude/claude_desktop_config.json")
    }
}

fn vscode_global_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/Code/User/mcp.json")
    } else if cfg!(target_os = "windows") {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join("AppData/Roaming"));
        appdata.join("Code/User/mcp.json")
    } else {
        home().join(".config/Code/User/mcp.json")
    }
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            key: "claude",
            label: "Claude Code",
            format: Format::JsonMcpServers,
            project_path: Some(|| cwd().join(".mcp.json")),
            global_path: Some(|| home().join(".claude.json")),
        },
        Target {
            key: "claude-desk",
            label: "Claude Desktop",
            format: Format::JsonMcpServers,
            project_path: None,
            global_path: Some(claude_desktop_path),
        },
        Target {
            key: "cursor",
            label: "Cursor",
            format: Format::JsonMcpServers,
            project_path: Some(|| cwd().join(".cursor/mcp.json")),
            global_path: Some(|| home().join(".cursor/mcp.json")),
        },
        Target {
            key: "windsurf",
            label: "Windsurf",
            format: Format::JsonMcpServers,
            project_path: None,
            global_path: Some(|| home().join(".codeium/windsurf/mcp_config.json")),
        },
        Target {
            key: "vscode",
            label: "VS Code",
            format: Format::JsonVscode,
            project_path: Some(|| cwd().join(".vscode/mcp.json")),
            global_path: Some(vscode_global_path),
        },
        Target {
            key: "gemini",
            label: "Gemini CLI",
            format: Format::JsonMcpServers,
            project_path: Some(|| cwd().join(".gemini/settings.json")),
            global_path: Some(|| home().join(".gemini/settings.json")),
        },
        Target {
            key: "codex",
            label: "Codex CLI",
            format: Format::Toml,
            project_path: None,
            global_path: Some(|| home().join(".codex/config.toml")),
        },
        Target {
            key: "hermes",
            label: "Hermes Agent",
            format: Format::Yaml,
            project_path: None,
            global_path: Some(|| home().join(".hermes/config.yaml")),
        },
        Target {
            key: "opencode",
            label: "opencode",
            format: Format::JsonOpencode,
            project_path: Some(|| cwd().join("opencode.json")),
            global_path: Some(|| home().join(".config/opencode/opencode.json")),
        },
    ]
}

/// Back-compat spellings for renamed targets.
fn resolve_alias(target: &str) -> &str {
    match target {
        "claude-code" => "claude",
        "claude-desktop" => "claude-desk",
        other => other,
    }
}

fn find_target(key: &str) -> Result<Target, String> {
    let key = resolve_alias(key);
    targets()
        .into_iter()
        .find(|t| t.key == key)
        .ok_or_else(|| {
            let all = targets()
                .iter()
                .map(|t| t.key)
                .collect::<Vec<_>>()
                .join(", ");
            format!("Unknown MCP target '{}' (expected: {})", key, all)
        })
}

impl Target {
    fn path_for(&self, scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::Project => self.project_path.map(|f| f()),
            Scope::Global => self.global_path.map(|f| f()),
        }
    }
    fn scopes(&self) -> Vec<Scope> {
        let mut out = Vec::new();
        if self.project_path.is_some() {
            out.push(Scope::Project);
        }
        if self.global_path.is_some() {
            out.push(Scope::Global);
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Project,
    Global,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

/// Which project the installed server should serve, and why.
///
/// The MCP server is launched by an editor from whatever directory it
/// happens to be in, so deriving the project from the *installer's* cwd
/// produced a name that often didn't exist (`native`, `src`, …). Resolve
/// it the same way the rest of `ug` does instead: the active project
/// (`ug active`), else the only sensible default — the first indexed
/// project — and say so, since the value is baked into a config file the
/// user won't look at again.
fn resolve_ug_project() -> (String, &'static str) {
    let first = crate::project::list_projects()
        .first()
        .and_then(|(path, _)| path.file_name().map(|n| n.to_string_lossy().into_owned()));
    pick_ug_project(
        crate::project::get_active_project(),
        first,
        derive_project_name("."),
    )
}

/// The precedence itself, separated from the filesystem so it can be
/// tested without racing other tests over `$UG_HOME`.
fn pick_ug_project(
    active: Option<String>,
    first_indexed: Option<String>,
    cwd_name: String,
) -> (String, &'static str) {
    if let Some(name) = active {
        return (name, "active project");
    }
    if let Some(name) = first_indexed {
        return (name, "first indexed project — no active project set");
    }
    // Nothing indexed yet: fall back to the cwd name so the entry is at
    // least a plausible target once the user runs `ug gen` here.
    (cwd_name, "current folder — no indexed projects found")
}

/// The command clients should launch for the MCP server: the resolved path to
/// this very `ug` binary (via `UG_BIN` when the launcher set it, else
/// `current_exe`), falling back to a bare `ug` on PATH.
fn server_entry() -> Value {
    let bin = std::env::var("UG_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::current_exe().ok().map(|p| {
                std::fs::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_else(|| "ug".to_string());
    let (project, _why) = resolve_ug_project();
    json!({
        "command": bin,
        "args": ["mcp"],
        "env": { "UG_PROJECT": project },
    })
}

// ── JSON read/write ────────────────────────────────────────────────────────

fn read_json(path: &PathBuf) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{} exists but isn't valid JSON — fix or remove it, then retry ({})",
            path.display(),
            e
        )
    })
}

fn write_json(path: &PathBuf, cfg: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
    }
    let mut out = serde_json::to_string_pretty(cfg).expect("config serializes");
    out.push('\n');
    std::fs::write(path, out).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn ensure_object<'a>(parent: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !parent[key].is_object() {
        parent[key] = Value::Object(Map::new());
    }
    parent[key].as_object_mut().unwrap()
}

fn apply_json(cfg: &mut Value, format: Format, server: &Value) {
    if !cfg.is_object() {
        *cfg = Value::Object(Map::new());
    }
    match format {
        Format::JsonMcpServers => {
            ensure_object(cfg, "mcpServers").insert("ultragraph".into(), server.clone());
        }
        Format::JsonVscode => {
            let mut entry = server.clone();
            entry["type"] = json!("stdio");
            ensure_object(cfg, "servers").insert("ultragraph".into(), entry);
        }
        Format::JsonOpencode => {
            if cfg.get("$schema").is_none() {
                cfg["$schema"] = json!("https://opencode.ai/config.json");
            }
            let command: Vec<Value> = std::iter::once(server["command"].clone())
                .chain(server["args"].as_array().cloned().unwrap_or_default())
                .collect();
            let entry = json!({
                "type": "local",
                "command": command,
                "environment": server["env"].clone(),
                "enabled": true,
            });
            ensure_object(cfg, "mcp").insert("ultragraph".into(), entry);
        }
        Format::Toml | Format::Yaml => unreachable!("non-JSON format in apply_json"),
    }
}

/// Returns whether an `ultragraph` entry existed (and removes it), so callers
/// can tell "removed" from "already absent".
fn remove_json(cfg: &mut Value, format: Format) -> bool {
    let container = match format {
        Format::JsonMcpServers => "mcpServers",
        Format::JsonVscode => "servers",
        Format::JsonOpencode => "mcp",
        Format::Toml | Format::Yaml => unreachable!("non-JSON format in remove_json"),
    };
    cfg.get_mut(container)
        .and_then(|c| c.as_object_mut())
        .map(|m| m.remove("ultragraph").is_some())
        .unwrap_or(false)
}

// ── TOML (Codex) — text-range splice, preserving unrelated tables ──────────

fn remove_toml_block(content: &str, name: &str) -> String {
    let header = format!("[mcp_servers.{}]", name);
    let env_header = format!("[mcp_servers.{}.env]", name);
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        let trimmed = line.trim();
        let is_own = trimmed == header || trimmed == env_header;
        let is_other_header =
            trimmed.starts_with('[') && trimmed.ends_with(']') && !is_own;
        if skipping {
            if is_other_header {
                skipping = false;
            } else {
                continue;
            }
        }
        if is_own {
            skipping = true;
            continue;
        }
        out.push(line);
    }
    // Collapse 3+ blank lines and trim trailing whitespace, matching the JS.
    let joined = out.join("\n");
    let mut collapsed = String::with_capacity(joined.len());
    let mut blank_run = 0;
    for line in joined.split('\n') {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run >= 2 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed.trim_end().to_string()
}

fn upsert_toml_server(content: &str, name: &str, server: &Value) -> String {
    let header = format!("[mcp_servers.{}]", name);
    let env_header = format!("[mcp_servers.{}.env]", name);
    let command = server["command"].as_str().unwrap_or("ug");
    let args = server["args"].as_array().cloned().unwrap_or_default();
    let args_toml = serde_json::to_string(&args).unwrap_or_else(|_| "[]".into());

    let mut block = vec![
        header,
        format!("command = {}", json!(command)),
        format!("args = {}", args_toml),
    ];
    if let Some(env) = server["env"].as_object() {
        if !env.is_empty() {
            block.push(String::new());
            block.push(env_header);
            for (k, v) in env {
                block.push(format!("{} = {}", k, v));
            }
        }
    }
    let block = block.join("\n");

    let remainder = remove_toml_block(content, name);
    if remainder.is_empty() {
        format!("{}\n", block)
    } else {
        format!("{}\n\n{}\n", remainder, block)
    }
}

// ── YAML (Hermes) ──────────────────────────────────────────────────────────

fn upsert_yaml_server(content: &str, name: &str, server: &Value) -> Result<String, String> {
    let mut doc: serde_yaml::Value = if content.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(content).map_err(|e| format!("invalid YAML: {}", e))?
    };
    if !doc.is_mapping() {
        doc = serde_yaml::Value::Mapping(Default::default());
    }
    let map = doc.as_mapping_mut().unwrap();
    let servers_key = serde_yaml::Value::String("mcp_servers".into());
    if !map.get(&servers_key).map(|v| v.is_mapping()).unwrap_or(false) {
        map.insert(
            servers_key.clone(),
            serde_yaml::Value::Mapping(Default::default()),
        );
    }
    let servers = map.get_mut(&servers_key).unwrap().as_mapping_mut().unwrap();
    let server_yaml: serde_yaml::Value = serde_yaml::to_value(server)
        .map_err(|e| format!("failed to encode server entry: {}", e))?;
    servers.insert(serde_yaml::Value::String(name.into()), server_yaml);
    serde_yaml::to_string(&doc).map_err(|e| format!("failed to write YAML: {}", e))
}

fn remove_yaml_server(content: &str, name: &str) -> Result<(String, bool), String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(content).map_err(|e| format!("invalid YAML: {}", e))?;
    let removed = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut("mcp_servers"))
        .and_then(|s| s.as_mapping_mut())
        .map(|s| s.remove(name).is_some())
        .unwrap_or(false);
    if !removed {
        return Ok((content.to_string(), false));
    }
    let out = serde_yaml::to_string(&doc).map_err(|e| format!("failed to write YAML: {}", e))?;
    Ok((out, true))
}

// ── Agent skill / rule file ────────────────────────────────────────────────

fn skill_body() -> String {
    // Strip the SKILL.md frontmatter block (`---\n…\n---\n`), keeping the body.
    let text = SKILL_MD;
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---\n") {
            return rest[idx + 5..].trim().to_string();
        }
    }
    text.trim().to_string()
}

/// One-line description for targets whose format carries its own
/// frontmatter (Cursor, Windsurf) rather than the skill's. Quoted for YAML.
const GUIDE_BLURB: &str = "\"UltraGraph guide — answer codebase questions (where is X, who calls X, blast radius, repo statistics) with the ug CLI instead of grepping\"";

/// How a target wants the guide written.
enum SkillKind {
    /// An agent *skill*: a directory holding `SKILL.md`, whose own
    /// frontmatter (name + description) is what makes the agent load it.
    /// Stripping that frontmatter is what makes a skill invisible.
    Skill,
    /// A rule file: body only, under the target's own frontmatter.
    Rule(String),
}

/// Where the guide goes for each target, and in which form.
fn skill_target(target: &str, scope: Scope) -> Option<(PathBuf, SkillKind)> {
    let root = if scope == Scope::Global && target != "windsurf" {
        home()
    } else {
        cwd()
    };
    match target {
        // Claude Code discovers skills as `<root>/.claude/skills/<name>/SKILL.md`
        // — global under $HOME, or per-project next to the repo.
        "claude" => Some((root.join(".claude/skills/ug/SKILL.md"), SkillKind::Skill)),
        "cursor" => {
            let fm = format!("description: {}\nalwaysApply: false", GUIDE_BLURB);
            Some((root.join(".cursor/rules/ug.mdc"), SkillKind::Rule(fm)))
        }
        "windsurf" => {
            // Windsurf rules always live in the project dir.
            let fm = format!("trigger: model_decision\ndescription: {}", GUIDE_BLURB);
            Some((cwd().join(".windsurf/rules/ug.md"), SkillKind::Rule(fm)))
        }
        "opencode" => Some((root.join(".agents/skills/ug/SKILL.md"), SkillKind::Skill)),
        _ => None,
    }
}

/// Where older versions wrote the guide. Cleaned up on install and
/// uninstall so a stale copy can't shadow or duplicate the real one.
fn legacy_skill_paths(target: &str, scope: Scope) -> Vec<PathBuf> {
    let root = if scope == Scope::Global { home() } else { cwd() };
    // The guide was named `ug-mcp` while it documented the MCP tools; it now
    // teaches the CLI and is named `ug`. Both spellings are removed, or the
    // old one keeps loading alongside the new.
    match target {
        "claude" => vec![
            root.join(".claude/rules/ug-mcp.md"),
            root.join(".claude/skills/ug-mcp/SKILL.md"),
        ],
        "cursor" => vec![root.join(".cursor/rules/ug-mcp.mdc")],
        "windsurf" => vec![cwd().join(".windsurf/rules/ug-mcp.md")],
        "opencode" => vec![root.join(".agents/skills/ug-mcp/SKILL.md")],
        _ => Vec::new(),
    }
}

/// Delete one guide file, and the skill directory holding it if that leaves
/// it empty — a bare `ug-mcp/` or `ug/` shell looks like an installed skill.
fn remove_guide_file(path: &PathBuf) {
    if !path.exists() {
        return;
    }
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let is_skill_dir = dir
            .file_name()
            .map(|n| n == "ug" || n == "ug-mcp")
            .unwrap_or(false);
        if is_skill_dir {
            // Fails harmlessly when the directory still has contents.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn install_skill_file(target: &str, scope: Scope) -> Option<PathBuf> {
    let (path, kind) = skill_target(target, scope)?;
    let content = match kind {
        // Hand the skill over intact: its frontmatter is the part the
        // agent reads to decide the skill exists at all.
        SkillKind::Skill => {
            let text = SKILL_MD.trim_end();
            format!("{}\n", text)
        }
        SkillKind::Rule(fm) => format!("---\n{}\n---\n\n{}\n", fm, skill_body()),
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("{C_YELLOW}!{C_RESET} could not create {}: {}", parent.display(), e);
            return None;
        }
    }
    match std::fs::write(&path, content) {
        Ok(()) => {
            for old in legacy_skill_paths(target, scope) {
                remove_guide_file(&old);
            }
            Some(path)
        }
        Err(e) => {
            eprintln!("{C_YELLOW}!{C_RESET} could not write {}: {}", path.display(), e);
            None
        }
    }
}

fn uninstall_skill_file(target: &str, scope: Scope) {
    let mut paths: Vec<PathBuf> = legacy_skill_paths(target, scope);
    if let Some((path, _)) = skill_target(target, scope) {
        paths.push(path);
    }
    for path in paths {
        remove_guide_file(&path);
    }
}

// ── install / uninstall drivers ────────────────────────────────────────────

fn install_config(target: &Target, scope: Scope) -> Result<PathBuf, String> {
    let path = target.path_for(scope).ok_or_else(|| {
        format!(
            "Target '{}' has no {} config (supported: {})",
            target.key,
            scope.name(),
            target.scopes().iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
        )
    })?;
    let server = server_entry();

    match target.format {
        Format::Toml => {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
            }
            std::fs::write(&path, upsert_toml_server(&existing, "ultragraph", &server))
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        }
        Format::Yaml => {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
            }
            std::fs::write(&path, upsert_yaml_server(&existing, "ultragraph", &server)?)
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        }
        json_format => {
            let mut cfg = read_json(&path)?;
            apply_json(&mut cfg, json_format, &server);
            write_json(&path, &cfg)?;
        }
    }
    Ok(path)
}

/// Strip the `ultragraph` server entry, leaving the rest of the config alone.
///
/// Does **not** touch the skill: `do_uninstall` owns that, and a `--cli`
/// install calls this to remove a stale server entry *after* writing the
/// skill — a skill deletion hidden in here would erase what it just wrote.
fn uninstall_config(target: &Target, scope: Scope) -> Result<(PathBuf, bool), String> {
    let path = target.path_for(scope).ok_or_else(|| {
        format!("Target '{}' has no {} config", target.key, scope.name())
    })?;
    if !path.exists() {
        return Ok((path, false));
    }
    let removed = match target.format {
        Format::Toml => {
            let existing = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
            let remainder = remove_toml_block(&existing, "ultragraph");
            let changed = remainder != existing.trim_end();
            if changed {
                let out = if remainder.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", remainder)
                };
                std::fs::write(&path, out)
                    .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
            }
            changed
        }
        Format::Yaml => {
            let existing = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
            let (out, changed) = remove_yaml_server(&existing, "ultragraph")?;
            if changed {
                std::fs::write(&path, out)
                    .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
            }
            changed
        }
        json_format => {
            let mut cfg = read_json(&path)?;
            let changed = remove_json(&mut cfg, json_format);
            if changed {
                write_json(&path, &cfg)?;
            }
            changed
        }
    };
    Ok((path, removed))
}

// ── stdin picker ───────────────────────────────────────────────────────────

fn prompt_choice(
    title: &str,
    choices: &[(String, String)],
    non_interactive_hint: &str,
) -> Result<String, String> {
    if !std::io::stdin().is_terminal() {
        return Err(non_interactive_hint.to_string());
    }
    println!("{C_BOLD}{}{C_RESET}", title);
    for (i, (name, hint)) in choices.iter().enumerate() {
        println!("  {C_CYAN}{:>2}{C_RESET}) {:<14} {}", i + 1, name, hint);
    }
    loop {
        print!("Select [1-{}]: ", choices.len());
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Err(non_interactive_hint.to_string());
        }
        let answer = line.trim();
        if let Ok(idx) = answer.parse::<usize>() {
            if idx >= 1 && idx <= choices.len() {
                return Ok(choices[idx - 1].0.clone());
            }
        }
        if let Some((name, _)) = choices.iter().find(|(n, _)| n == answer) {
            return Ok(name.clone());
        }
        println!(
            "{C_YELLOW}Enter a number between 1 and {} (Ctrl+C to abort).{C_RESET}",
            choices.len()
        );
    }
}

/// Which of the two ways to reach `ug` this install wires up.
///
/// They are alternatives, not layers. Both work, and installing both means
/// the agent picks — and it tends to pick the MCP tools, because a connected
/// tool is more visible to it than a CLI it has to be told about. The skill
/// says to prefer the CLI, but the cleanest way to get the CLI is to not
/// install the competing path at all.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// The agent skill only: the agent runs `ug` as a shell command.
    Cli,
    /// The MCP server entry only: the agent calls tools over the protocol.
    Mcp,
    /// Both — the agent decides per question.
    Both,
}

impl Mode {
    fn installs_skill(self) -> bool {
        self != Mode::Mcp
    }
    fn installs_server(self) -> bool {
        self != Mode::Cli
    }
}

/// Parse `--cli` / `--mcp` / `--both`, erroring if more than one is given.
fn mode_flag(args: &[String]) -> Result<Option<Mode>, String> {
    let picked: Vec<Mode> = args
        .iter()
        .filter_map(|a| match a.as_str() {
            "--cli" | "--skill-only" => Some(Mode::Cli),
            "--mcp" | "--mcp-only" => Some(Mode::Mcp),
            "--both" => Some(Mode::Both),
            _ => None,
        })
        .collect();
    match picked.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => Err("Pass at most one of --cli / --mcp / --both.".to_string()),
    }
}

/// Parse `--global`/`-g` / `--project` out of the args, erroring if both.
fn scope_flag(args: &[String]) -> Result<Option<Scope>, String> {
    let wants_global = args.iter().any(|a| a == "--global" || a == "-g");
    let wants_project = args.iter().any(|a| a == "--project");
    if wants_global && wants_project {
        return Err("Pass at most one of --global / --project.".to_string());
    }
    Ok(if wants_global {
        Some(Scope::Global)
    } else if wants_project {
        Some(Scope::Project)
    } else {
        None
    })
}

fn resolve_target_arg(args: &[String], action: &str) -> Result<Target, String> {
    let named = args.iter().find(|a| !a.starts_with('-')).cloned();
    let key = match named {
        Some(t) => t,
        None => {
            let choices: Vec<(String, String)> = targets()
                .iter()
                .map(|t| (t.key.to_string(), t.label.to_string()))
                .collect();
            let hint = format!(
                "Usage: mcp {} <{}> [--global|--project]",
                action,
                targets().iter().map(|t| t.key).collect::<Vec<_>>().join("|")
            );
            prompt_choice(
                &format!(
                    "{} the UltraGraph MCP server for which client?",
                    if action == "install" { "Install" } else { "Uninstall" }
                ),
                &choices,
                &hint,
            )?
        }
    };
    find_target(&key)
}

pub fn run_mcp_install(args: &[String]) {
    if let Err(e) = do_install(args) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn do_install(args: &[String]) -> Result<(), String> {
    let flag_scope = scope_flag(args)?;
    let mode = resolve_mode(args)?;
    let target = resolve_target_arg(args, "install")?;
    let scopes = target.scopes();

    if let Some(fs) = flag_scope {
        if !scopes.contains(&fs) {
            return Err(format!(
                "{} has no {} config — it only supports: {}",
                target.label,
                fs.name(),
                scopes.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
            ));
        }
    }

    let scope = match flag_scope {
        Some(s) => s,
        None if scopes.len() == 1 => scopes[0],
        None => {
            let describe = |s: Scope| -> String {
                let p = target.path_for(s).map(|p| p.display().to_string()).unwrap_or_default();
                match s {
                    Scope::Project => format!("{}  — this directory only", p),
                    Scope::Global => format!("{}  — all projects", p),
                }
            };
            let choices: Vec<(String, String)> =
                scopes.iter().map(|s| (s.name().to_string(), describe(*s))).collect();
            let hint = format!(
                "'{}' supports both a project and a global config — re-run with --project or --global.",
                target.key
            );
            let subject = if mode.installs_server() { "the MCP server" } else { "the skill" };
            let picked = prompt_choice(
                &format!("Where should {} pick up {}?", target.label, subject),
                &choices,
                &hint,
            )?;
            if picked == "global" {
                Scope::Global
            } else {
                Scope::Project
            }
        }
    };

    if mode.installs_skill() {
        match install_skill_file(target.key, scope) {
            Some(skill_path) => println!(
                "{C_GREEN}✓{C_RESET} Installed agent skill to {}",
                skill_path.display()
            ),
            // Only reachable for a target with no skill/rule location, and
            // then `--cli` has written nothing at all — say so rather than
            // reporting a success that did not happen.
            None if !mode.installs_server() => {
                return Err(format!(
                    "{} has no agent-skill location, so there is nothing for --cli to install. \
                     Re-run with --mcp to wire up the MCP server instead.",
                    target.label
                ))
            }
            None => {}
        }
    } else {
        // A stale skill would keep teaching the CLI path the user just opted
        // out of, and would be the thing the agent reads first.
        uninstall_skill_file(target.key, scope);
    }

    if mode.installs_server() {
        let path = install_config(&target, scope)?;
        println!("{C_GREEN}✓{C_RESET} Wrote MCP config to {}", path.display());
    } else {
        // Same reasoning in reverse: leaving the server entry behind is what
        // makes the agent choose MCP over the CLI the user asked for.
        let (path, removed) = uninstall_config(&target, scope)?;
        if removed {
            println!(
                "{C_GREEN}✓{C_RESET} Removed the MCP server entry from {} {}(--cli){C_RESET}",
                path.display(),
                C_DIM
            );
        }
    }

    // Freshness is the other half of connecting an agent: the tools it just
    // gained answer from the graph, and an agent that edits does not think to
    // refresh it. `--hooks` hands that job to git.
    install_hooks_if_asked(args);

    // Whichever path was wired, it answers about *this* project — baked into
    // the MCP config as UG_PROJECT, and the CLI's active project otherwise.
    let (project, why) = resolve_ug_project();
    println!(
        "{C_CYAN}▸{C_RESET} It will serve project {C_BOLD}{}{C_RESET} {}({}){C_RESET}",
        project, C_DIM, why
    );
    println!(
        "{C_DIM}  Change it with `ug active <name>`{}.{C_RESET}",
        if mode.installs_server() { " then re-run this, or edit UG_PROJECT in the config" } else { "" }
    );
    println!("{C_CYAN}Restart {} to pick it up.{C_RESET}", target.label);
    match mode {
        Mode::Cli => println!(
            "{C_DIM}  The skill teaches your agent to answer codebase questions by running the ug CLI.{C_RESET}"
        ),
        Mode::Mcp => println!(
            "{C_DIM}  Your agent will call ug over MCP. Add the CLI guide too with `ug connect {} --both`.{C_RESET}",
            target.key
        ),
        Mode::Both => println!(
            "{C_DIM}  Both paths are wired. The skill tells your agent to prefer the CLI, but a\n  \
             connected tool is the more visible option — use --cli if you want only that.{C_RESET}"
        ),
    }
    Ok(())
}

/// `--hooks`: also install the git hooks that keep the graph in step with the
/// repo. Opt-in, because writing into someone's `.git/hooks` is not something
/// to do as a side effect of connecting an agent.
///
/// Failures here are reported, not fatal: the agent wiring above already
/// landed, and "not in a git repo" is a perfectly ordinary reason.
fn install_hooks_if_asked(args: &[String]) {
    if !args.iter().any(|a| a == "--hooks") {
        println!(
            "{C_DIM}  Tip: `ug hook install` adds git hooks that refresh the graph after every\n  \
             commit, merge and rebase — so its answers never lag your edits.{C_RESET}"
        );
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match crate::cli::hook::install(args, &cwd) {
        Ok(()) => {}
        Err(e) => eprintln!("{C_YELLOW}⚠{C_RESET}  Git hooks not installed — {}", e),
    }
}

/// Which paths to wire: the flag if given, otherwise ask.
///
/// Non-interactive with no flag keeps installing both, because that is what
/// existing scripted installs expect; the hint names the flags so the choice
/// can be made explicit.
fn resolve_mode(args: &[String]) -> Result<Mode, String> {
    if let Some(m) = mode_flag(args)? {
        return Ok(m);
    }
    if !std::io::stdin().is_terminal() {
        println!(
            "{C_DIM}▸ Installing both the CLI skill and the MCP server (no --cli / --mcp / --both given).{C_RESET}"
        );
        return Ok(Mode::Both);
    }
    let choices = vec![
        (
            "cli".to_string(),
            format!("{}the agent runs `ug` directly — recommended{}", C_BOLD, C_RESET),
        ),
        ("mcp".to_string(), "MCP server only — the agent calls ug over the protocol".to_string()),
        ("both".to_string(), "both, and let the agent choose".to_string()),
    ];
    let picked = prompt_choice(
        "How should your agent reach ug?",
        &choices,
        "Pass one of --cli / --mcp / --both.",
    )?;
    Ok(match picked.as_str() {
        "mcp" => Mode::Mcp,
        "both" => Mode::Both,
        _ => Mode::Cli,
    })
}

pub fn run_mcp_uninstall(args: &[String]) {
    if let Err(e) = do_uninstall(args) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn do_uninstall(args: &[String]) -> Result<(), String> {
    let flag_scope = scope_flag(args)?;
    let target = resolve_target_arg(args, "uninstall")?;

    // A scope flag narrows it; with no flag, sweep every scope the target
    // supports — removal is precise (just the `ultragraph` key), so no prompt.
    let scopes = match flag_scope {
        Some(s) => vec![s],
        None => target.scopes(),
    };
    let mut removed_any = false;
    for scope in scopes {
        let (path, removed) = uninstall_config(&target, scope)?;
        if removed {
            removed_any = true;
            println!("{C_GREEN}✓{C_RESET} Removed ultragraph from {}", path.display());
        }
        // Always, and regardless of whether a server entry was there: a
        // `--cli` install writes only the skill, so keying skill removal off
        // the config left that install with no way to undo itself.
        if let Some((skill, _)) = skill_target(target.key, scope) {
            if skill.exists() {
                removed_any = true;
                println!("{C_GREEN}✓{C_RESET} Removed agent skill {}", skill.display());
            }
        }
        uninstall_skill_file(target.key, scope);
    }
    if removed_any {
        println!("{C_CYAN}Restart {} to pick it up.{C_RESET}", target.label);
    } else {
        println!(
            "{C_YELLOW}•{C_RESET} No ultragraph entry or skill found for {} — nothing to do.",
            target.label
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Value {
        json!({ "command": "/bin/ug", "args": ["mcp"], "env": { "UG_PROJECT": "demo" } })
    }

    #[test]
    fn toml_upsert_preserves_other_tables_and_roundtrips() {
        let existing = "[model]\nname = \"gpt\"\n\n[mcp_servers.other]\ncommand = \"x\"\nargs = []\n";
        let out = upsert_toml_server(existing, "ultragraph", &server());
        assert!(out.contains("[model]"));
        assert!(out.contains("[mcp_servers.other]"));
        assert!(out.contains("[mcp_servers.ultragraph]"));
        assert!(out.contains("[mcp_servers.ultragraph.env]"));
        assert!(out.contains("UG_PROJECT = \"demo\""));

        // Removing ours leaves the unrelated tables untouched.
        let after = remove_toml_block(&out, "ultragraph");
        assert!(after.contains("[model]"));
        assert!(after.contains("[mcp_servers.other]"));
        assert!(!after.contains("ultragraph"));
    }

    #[test]
    fn toml_upsert_replaces_existing_entry() {
        let first = upsert_toml_server("", "ultragraph", &server());
        let second = upsert_toml_server(&first, "ultragraph", &server());
        // Exactly one ultragraph table, not two.
        assert_eq!(second.matches("[mcp_servers.ultragraph]").count(), 1);
    }

    #[test]
    fn yaml_upsert_and_remove_roundtrip() {
        let existing = "mcp_servers:\n  other:\n    command: x\n";
        let out = upsert_yaml_server(existing, "ultragraph", &server()).unwrap();
        assert!(out.contains("ultragraph"));
        assert!(out.contains("other"));
        let (removed, changed) = remove_yaml_server(&out, "ultragraph").unwrap();
        assert!(changed);
        assert!(!removed.contains("ultragraph"));
        assert!(removed.contains("other"));
    }

    #[test]
    fn ug_project_prefers_the_active_project() {
        // The editor launches the server from wherever it likes, so the
        // installer's cwd is the last resort, not the first choice.
        let (name, why) = pick_ug_project(
            Some("beta".into()),
            Some("alpha".into()),
            "cwd-folder".into(),
        );
        assert_eq!((name.as_str(), why), ("beta", "active project"));

        let (name, why) = pick_ug_project(None, Some("alpha".into()), "cwd-folder".into());
        assert_eq!(name, "alpha");
        assert!(why.contains("first indexed"), "unexpected reason: {}", why);

        let (name, why) = pick_ug_project(None, None, "cwd-folder".into());
        assert_eq!(name, "cwd-folder");
        assert!(why.contains("no indexed projects"), "unexpected reason: {}", why);
    }

    #[test]
    fn claude_gets_a_skill_dir_with_frontmatter_intact() {
        // Claude Code discovers skills by their frontmatter; a rules file
        // with the frontmatter stripped is invisible to it.
        let (path, kind) = skill_target("claude", Scope::Global).expect("claude skill target");
        assert!(
            path.ends_with(".claude/skills/ug/SKILL.md"),
            "unexpected path: {}",
            path.display()
        );
        assert!(matches!(kind, SkillKind::Skill));
        assert!(SKILL_MD.starts_with("---\nname: ug\n"), "skill needs its frontmatter");
    }

    #[test]
    fn rule_targets_still_get_their_own_frontmatter() {
        let (path, kind) = skill_target("cursor", Scope::Project).expect("cursor target");
        assert!(path.ends_with(".cursor/rules/ug.mdc"));
        match kind {
            SkillKind::Rule(fm) => {
                assert!(fm.contains("alwaysApply"));
                // Unquoted, the em dash and parentheses would break YAML.
                assert!(fm.contains("description: \""), "blurb must stay quoted");
            }
            _ => panic!("cursor should get a rule file"),
        }
    }

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn install_mode_comes_from_one_flag() {
        assert_eq!(mode_flag(&argv("claude --cli")).unwrap(), Some(Mode::Cli));
        assert_eq!(mode_flag(&argv("claude --skill-only")).unwrap(), Some(Mode::Cli));
        assert_eq!(mode_flag(&argv("claude --mcp")).unwrap(), Some(Mode::Mcp));
        assert_eq!(mode_flag(&argv("claude --both")).unwrap(), Some(Mode::Both));
        // No flag means "ask", which is the caller's job, not this function's.
        assert_eq!(mode_flag(&argv("claude --global")).unwrap(), None);
        assert!(mode_flag(&argv("claude --cli --mcp")).is_err());
    }

    /// The point of the modes: each installs its own path and only its own.
    #[test]
    fn each_mode_wires_exactly_one_pair_of_paths() {
        assert!(Mode::Cli.installs_skill() && !Mode::Cli.installs_server());
        assert!(!Mode::Mcp.installs_skill() && Mode::Mcp.installs_server());
        assert!(Mode::Both.installs_skill() && Mode::Both.installs_server());
    }

    #[test]
    fn the_pre_rename_ug_mcp_guide_is_cleaned_up() {
        // Every target that ever wrote a `ug-mcp` guide must remove it, or
        // the stale MCP-era copy loads alongside the CLI one.
        for (target, stale) in [
            ("claude", ".claude/skills/ug-mcp/SKILL.md"),
            ("cursor", ".cursor/rules/ug-mcp.mdc"),
            ("windsurf", ".windsurf/rules/ug-mcp.md"),
            ("opencode", ".agents/skills/ug-mcp/SKILL.md"),
        ] {
            assert!(
                legacy_skill_paths(target, Scope::Global)
                    .iter()
                    .any(|p| p.ends_with(stale)),
                "{} should clean up {}",
                target,
                stale
            );
        }
        assert!(legacy_skill_paths("claude", Scope::Global)
            .iter()
            .any(|p| p.ends_with(".claude/rules/ug-mcp.md")));
    }

    #[test]
    fn skill_body_strips_frontmatter() {
        let body = skill_body();
        assert!(!body.starts_with("---"));
        assert!(body.contains("ug query"));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_alias("claude-code"), "claude");
        assert_eq!(resolve_alias("claude-desktop"), "claude-desk");
        assert!(find_target("cursor").is_ok());
        assert!(find_target("nope").is_err());
    }
}

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

const SKILL_MD: &str = include_str!("ug-mcp-skill.md");

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
        "claude" => Some((
            root.join(".claude/skills/ug-mcp/SKILL.md"),
            SkillKind::Skill,
        )),
        "cursor" => {
            let fm = "description: \"UltraGraph MCP tools guide — efficient codebase and knowledge-base search via a semantic knowledge graph\"\nalwaysApply: false";
            Some((
                root.join(".cursor/rules/ug-mcp.mdc"),
                SkillKind::Rule(fm.to_string()),
            ))
        }
        "windsurf" => {
            // Windsurf rules always live in the project dir.
            let fm = "trigger: model_decision\ndescription: \"UltraGraph MCP tools guide — efficient codebase and knowledge-base search via a semantic knowledge graph\"";
            Some((
                cwd().join(".windsurf/rules/ug-mcp.md"),
                SkillKind::Rule(fm.to_string()),
            ))
        }
        _ => None,
    }
}

/// Where older versions wrote the guide. Cleaned up on install and
/// uninstall so a stale copy can't shadow or duplicate the real one.
fn legacy_skill_paths(target: &str, scope: Scope) -> Vec<PathBuf> {
    let root = if scope == Scope::Global { home() } else { cwd() };
    match target {
        "claude" => vec![root.join(".claude/rules/ug-mcp.md")],
        _ => Vec::new(),
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
                if old.exists() {
                    let _ = std::fs::remove_file(&old);
                }
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
        if !path.exists() {
            continue;
        }
        let _ = std::fs::remove_file(&path);
        // A skill lives in its own directory; leave no empty shell behind.
        if let Some(dir) = path.parent() {
            if dir.file_name().map(|n| n == "ug-mcp").unwrap_or(false) {
                let _ = std::fs::remove_dir(dir);
            }
        }
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
            if let Some(skill) = install_skill_file(target.key, scope) {
                println!(
                    "{C_GREEN}✓{C_RESET} Installed the ug tool guide to {}",
                    skill.display()
                );
            }
        }
    }
    Ok(path)
}

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
                uninstall_skill_file(target.key, scope);
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
            let picked = prompt_choice(
                &format!("Where should {} pick up the server?", target.label),
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

    let path = install_config(&target, scope)?;
    println!("{C_GREEN}✓{C_RESET} Wrote MCP config to {}", path.display());

    // The server will answer questions about *this* project; that's baked
    // into the config as UG_PROJECT, so make it visible now rather than
    // leaving the user to wonder which graph they're querying.
    let (project, why) = resolve_ug_project();
    println!(
        "{C_CYAN}▸{C_RESET} It will serve project {C_BOLD}{}{C_RESET} {}({}){C_RESET}",
        project, C_DIM, why
    );
    println!(
        "{C_DIM}  Change it with `ug active <name>` then re-run this, or edit UG_PROJECT in the config.{C_RESET}"
    );
    println!("{C_CYAN}Restart {} to pick it up.{C_RESET}", target.label);
    Ok(())
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
    }
    if removed_any {
        println!("{C_CYAN}Restart {} to pick it up.{C_RESET}", target.label);
    } else {
        println!(
            "{C_YELLOW}•{C_RESET} No ultragraph entry found for {} — nothing to do.",
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
            path.ends_with(".claude/skills/ug-mcp/SKILL.md"),
            "unexpected path: {}",
            path.display()
        );
        assert!(matches!(kind, SkillKind::Skill));
        assert!(SKILL_MD.starts_with("---\nname: ug-mcp"), "skill needs its frontmatter");
    }

    #[test]
    fn rule_targets_still_get_their_own_frontmatter() {
        let (path, kind) = skill_target("cursor", Scope::Project).expect("cursor target");
        assert!(path.ends_with(".cursor/rules/ug-mcp.mdc"));
        match kind {
            SkillKind::Rule(fm) => assert!(fm.contains("alwaysApply")),
            _ => panic!("cursor should get a rule file"),
        }
    }

    #[test]
    fn legacy_claude_rule_path_is_cleaned_up() {
        assert!(legacy_skill_paths("claude", Scope::Global)
            .iter()
            .any(|p| p.ends_with(".claude/rules/ug-mcp.md")));
        assert!(legacy_skill_paths("cursor", Scope::Global).is_empty());
    }

    #[test]
    fn skill_body_strips_frontmatter() {
        let body = skill_body();
        assert!(!body.starts_with("---"));
        assert!(body.contains("UltraGraph MCP Tool Guide"));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_alias("claude-code"), "claude");
        assert_eq!(resolve_alias("claude-desktop"), "claude-desk");
        assert!(find_target("cursor").is_ok());
        assert!(find_target("nope").is_err());
    }
}

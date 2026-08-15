//! Managing the projects under `~/.ug`: listing them, selecting the
//! active one, removing one, and uninstalling ug entirely.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

use super::args::{first_positional, flag_value, has_flag, positionals};

/// One project's row, with everything the scans produced.
///
/// Assembled up front so the table can size its columns to the widest cell
/// rather than to a guess — project names and repo paths vary by an order of
/// magnitude, and a fixed `{:<24}` either truncates or wastes half the line.
struct Row {
    meta: project::ProjectMeta,
    dir: std::path::PathBuf,
    /// Bytes under the project dir. `None` when `--quick` skipped the walk.
    size: Option<u64>,
    /// `None` when `--quick` skipped the scan, or the project has no graph.
    staleness: Option<project::Staleness>,
    has_db: bool,
    is_active: bool,
    is_cwd: bool,
}

impl Row {
    /// The STATUS cell: the single most actionable thing about this project.
    ///
    /// Ordered by what blocks the user soonest — a missing repo or graph means
    /// nothing can be refreshed at all, a missing db means half the commands
    /// (`ug analyze`, `chat`, `search`) have nothing to read, and only then does
    /// drift matter. Returns the plain text and its colour separately so the
    /// column can be padded to width: ANSI escapes count toward `{:<n}` and
    /// would skew every row that carries them.
    fn status(&self) -> (String, &'static str) {
        match &self.staleness {
            None if self.dir.join("graph.json").exists() => ("?".to_string(), C_DIM),
            None => ("no graph".to_string(), C_YELLOW),
            Some(s) if s.repo_missing => ("repo gone".to_string(), C_YELLOW),
            Some(_) if !self.has_db => ("no db".to_string(), C_YELLOW),
            Some(s) if s.is_stale() => (
                format!("{} changed", s.changed + s.missing),
                C_YELLOW,
            ),
            Some(_) => ("fresh".to_string(), C_GREEN),
        }
    }
}

/// `ug list` — enumerate project data dirs under `~/.ug` (or `$UG_HOME`).
pub(crate) fn run_list(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_list_help();
        return;
    }
    let projects = project::list_projects();
    let root = project::ug_home();
    if projects.is_empty() {
        println!(
            "No projects found in {C_BOLD}{}{C_RESET}. Run {C_CYAN}ug gen{C_RESET} in a repo to create one.",
            root.display()
        );
        return;
    }

    // `--quick` skips both filesystem scans: the recursive size walk and the
    // one `stat` per indexed file. Both are fast on a normal project and
    // neither is on a network mount or a repo with a hundred thousand files,
    // and `ug list` is also what a script calls in a loop.
    let quick = has_flag(args, "--quick");
    let cwd_name = project::derive_project_name(".");
    let active = project::get_active_project();
    let rows: Vec<Row> = projects
        .into_iter()
        .map(|(dir, meta)| Row {
            size: (!quick).then(|| project::dir_size(&dir)),
            staleness: (!quick).then(|| project::staleness(&dir, &meta)).flatten(),
            has_db: dir.join("ugdb").exists(),
            is_active: active.as_deref() == Some(meta.name.as_str()),
            is_cwd: meta.name == cwd_name,
            meta,
            dir,
        })
        .collect();

    if has_flag(args, "--json") {
        println!("{}", list_json(&rows, &root));
        return;
    }

    println!(
        "{C_BOLD}Projects in {}{C_RESET} ({}):\n",
        root.display(),
        rows.len()
    );

    let w_name = rows.iter().map(|r| r.meta.name.len()).max().unwrap_or(4).max(4);
    let w_nodes = rows.iter().map(|r| commas(r.meta.nodes).len()).max().unwrap_or(5).max(5);
    let w_edges = rows.iter().map(|r| commas(r.meta.edges).len()).max().unwrap_or(5).max(5);
    let w_size = rows
        .iter()
        .map(|r| size_cell(r.size).len())
        .max()
        .unwrap_or(4)
        .max(4);
    let w_status = rows.iter().map(|r| r.status().0.len()).max().unwrap_or(6).max(6);

    println!(
        "  {C_BOLD}{:<w_name$}  {:>w_nodes$}  {:>w_edges$}  {:>w_size$}  {:<w_status$}  {:<19}  {}{C_RESET}",
        "NAME", "NODES", "EDGES", "SIZE", "STATUS", "UPDATED", "REPO"
    );

    for row in &rows {
        // `*` = matches the current directory; `← active` = the project
        // (`ug active`) that `ug mcp` and `ug serve` default to. When one
        // project is both, `*` wins the leading slot and the row still carries
        // the active tag.
        let marker = if row.is_cwd { "*" } else { " " };
        let tag = if row.is_active {
            format!("  {C_YELLOW}← active{C_RESET}")
        } else {
            String::new()
        };
        let (status, status_color) = row.status();
        let repo = if row.meta.repo_root.is_empty() {
            "(no repo recorded)".to_string()
        } else {
            row.meta.repo_root.clone()
        };
        println!(
            "{C_GREEN}{}{C_RESET} {C_CYAN}{:<w_name$}{C_RESET}  {:>w_nodes$}  {:>w_edges$}  {:>w_size$}  \
             {}{:<w_status$}{C_RESET}  {:<19}  {}{}",
            marker,
            row.meta.name,
            commas(row.meta.nodes),
            commas(row.meta.edges),
            size_cell(row.size),
            status_color,
            status,
            project::format_epoch(row.meta.updated_at),
            repo,
            tag,
        );
    }

    // Per-project follow-ups, below the table rather than crammed into it:
    // each names the command that resolves it, which a status cell has no
    // room for.
    println!();
    for row in &rows {
        if let Some(age) = project::pending_vectors_age(&row.dir) {
            println!(
                "  {C_YELLOW}·{C_RESET} {C_CYAN}{}{C_RESET} owes vectors ({} behind) — \
                 {C_CYAN}ug ingest -n {}{C_RESET} catches semantic search up.",
                row.meta.name,
                humanize(age),
                row.meta.name
            );
        }
        match &row.staleness {
            Some(s) if s.repo_missing => println!(
                "  {C_YELLOW}·{C_RESET} {C_CYAN}{}{C_RESET} indexes {}, which is gone — \
                 {C_CYAN}ug gen -i <path> -n {}{C_RESET} repoints it.",
                row.meta.name, row.meta.repo_root, row.meta.name
            ),
            Some(s) if s.is_stale() => println!(
                "  {C_YELLOW}·{C_RESET} {C_CYAN}{}{C_RESET} is {} of {} file(s) behind \
                 ({} changed, {} deleted) — {C_CYAN}ug gen -n {}{C_RESET} refreshes it.",
                row.meta.name,
                s.changed + s.missing,
                s.files,
                s.changed,
                s.missing,
                row.meta.name
            ),
            _ => {}
        }
        if !row.has_db {
            println!(
                "  {C_YELLOW}·{C_RESET} {C_CYAN}{}{C_RESET} has no db — {C_CYAN}ug analyze{C_RESET}, \
                 {C_CYAN}search{C_RESET} and {C_CYAN}chat{C_RESET} cannot read it. \
                 {C_CYAN}ug ingest -n {}{C_RESET} builds one.",
                row.meta.name, row.meta.name
            );
        }
    }

    if let Some(total) = rows.iter().map(|r| r.size).sum::<Option<u64>>() {
        println!("  {C_DIM}{} total in {}{C_RESET}", format_bytes(total), root.display());
    }
    println!(
        "\n{C_BOLD}*{C_RESET} matches the current directory; {C_YELLOW}← active{C_RESET} is the default for {C_CYAN}ug mcp{C_RESET} and {C_CYAN}ug serve{C_RESET} (set with {C_CYAN}ug active <name>{C_RESET})."
    );
}

/// `ug list --json` — the same rows, for anything that parses rather than
/// reads. Field names match `GET /api/projects/staleness` where they overlap.
fn list_json(rows: &[Row], root: &std::path::Path) -> String {
    let projects: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let (status, _) = r.status();
            serde_json::json!({
                "name": r.meta.name,
                "repoRoot": r.meta.repo_root,
                "dataDir": r.dir.to_string_lossy(),
                "nodes": r.meta.nodes,
                "edges": r.meta.edges,
                "sizeBytes": r.size,
                "status": status,
                "updatedAt": r.meta.updated_at,
                "createdAt": r.meta.created_at,
                "ugVersion": r.meta.ug_version,
                "hasDb": r.has_db,
                "active": r.is_active,
                "matchesCwd": r.is_cwd,
                "pendingVectorsSince": r.meta.pending_vectors_since,
                "files": r.staleness.as_ref().map(|s| s.files),
                "changed": r.staleness.as_ref().map(|s| s.changed),
                "missing": r.staleness.as_ref().map(|s| s.missing),
                "isStale": r.staleness.as_ref().map(|s| s.is_stale()),
                "repoMissing": r.staleness.as_ref().map(|s| s.repo_missing),
                "kbKind": r.staleness.as_ref().map(|s| s.kb_kind()),
            })
        })
        .collect();
    serde_json::json!({ "ugHome": root.to_string_lossy(), "projects": projects }).to_string()
}

/// A count with thousands separators — `ug list`'s two widest numeric columns
/// are otherwise an unreadable run of digits at repo scale.
fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn size_cell(size: Option<u64>) -> String {
    size.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

/// Bytes at human scale. Three significant figures below 10 units, so a
/// column of sizes stays comparable at a glance.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else if value < 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.0} {}", value, UNITS[unit])
    }
}

/// Coarse "how long ago" for the pending-vectors note — the useful
/// distinction is minutes vs. days, not seconds.
fn humanize(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=90 => "under a minute".to_string(),
        91..=5400 => format!("{}m", secs / 60),
        5401..=172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `ug active [<project>|--clear]` — view or set the persisted active
/// project. The active project is the default `ug mcp` resolves to when no
/// `UG_PROJECT` env var is set and the current directory isn't itself an
/// indexed project — so `ug mcp call <tool>` works from anywhere.
pub(crate) fn run_active(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug active{C_RESET} [<project> | --clear]");
        println!("  No args: show the current active project.");
        println!("  <project>: set it (must be an indexed project — see {C_CYAN}ug list{C_RESET}).");
        println!("  --clear: unset it.");
        return;
    }

    // Clear: `--clear`/`--unset`, or a bare `clear`/`none` positional.
    let positional = first_positional(args, &[]);
    let wants_clear = has_flag(args, "--clear")
        || has_flag(args, "--unset")
        || matches!(positional.as_deref(), Some("clear") | Some("none"));

    if wants_clear {
        match project::clear_active_project() {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Cleared the active project."),
            Err(e) => {
                eprintln!("Failed to clear active project: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    match positional {
        Some(name) => match project::set_active_project(&name) {
            Ok(set) => {
                println!("{C_GREEN}✓{C_RESET} Active project set to {C_CYAN}{}{C_RESET}.", set);
                if let Some(meta) = project::read_meta(&project::project_dir(&set)) {
                    if !meta.repo_root.is_empty() {
                        println!("  repo: {}", meta.repo_root);
                    }
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                eprintln!("Run {C_CYAN}ug list{C_RESET} to see available projects.");
                std::process::exit(1);
            }
        },
        None => match project::get_active_project() {
            Some(name) => {
                println!("Active project: {C_CYAN}{}{C_RESET}", name);
                if let Some(meta) = project::read_meta(&project::project_dir(&name)) {
                    if !meta.repo_root.is_empty() {
                        println!("  repo: {}", meta.repo_root);
                    }
                }
            }
            None => {
                println!("No active project set.");
                println!(
                    "Set one with {C_CYAN}ug active <name>{C_RESET} (see {C_CYAN}ug list{C_RESET})."
                );
            }
        },
    }
}

/// `ug rename <new>` (alias `rn`) — rename a project's data directory
/// under `~/.ug` (or `$UG_HOME`). With one argument it renames the
/// current project — the active one (`ug active`), falling back to the
/// current directory's basename — which is the common case. A second
/// positional (or `-n/--name`) renames some *other* project instead:
/// `ug rename old new`.
pub(crate) fn run_rename(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_rename_help();
        return;
    }

    let value_flags = ["-n", "--name"];
    let pos = positionals(args, &value_flags);
    let (old_name, new_name) = match pos.len() {
        0 => {
            eprintln!("{C_BOLD}ug rename{C_RESET} needs a new name.");
            eprintln!("Usage: {C_CYAN}ug rename <new-name>{C_RESET}  (or {C_CYAN}ug rename <old> <new>{C_RESET})");
            std::process::exit(1);
        }
        // `resolve_active_project_name` honors `-n/--name` first, so
        // `ug rename -n other new` targets `other`.
        1 => (
            project::resolve_active_project_name(args, "."),
            pos[0].clone(),
        ),
        _ => (project::sanitize_name(&pos[0]), pos[1].clone()),
    };

    let old_dir = project::project_dir(&old_name);
    if !old_dir.exists() {
        eprintln!(
            "No project named {C_BOLD}{}{C_RESET} found at {}.",
            old_name,
            old_dir.display()
        );
        eprintln!("Run {C_CYAN}ug list{C_RESET} to see available projects.");
        std::process::exit(1);
    }

    // Read before the move — both are used in the report below.
    let was_active = project::get_active_project().as_deref() == Some(old_name.as_str());
    let repo_root = project::read_meta(&old_dir)
        .map(|m| m.repo_root)
        .unwrap_or_default();

    let new_name = match project::rename_project(&old_name, &new_name) {
        Ok(name) => name,
        Err(e) => {
            eprintln!("Failed to rename {C_BOLD}{}{C_RESET}: {}", old_name, e);
            std::process::exit(1);
        }
    };

    println!(
        "{C_GREEN}✓{C_RESET} Renamed {C_BOLD}{}{C_RESET} → {C_CYAN}{}{C_RESET}",
        old_name, new_name
    );
    println!("  path: {}", project::project_dir(&new_name).display());
    if was_active {
        println!("  {C_YELLOW}← active{C_RESET} marker now points at {C_CYAN}{}{C_RESET}", new_name);
    }
    // The gotcha: commands that derive a project from the working
    // directory (`ug gen`, `ug index`, `ug graph`) still use the repo's
    // basename, so re-generating in that repo would build a *second*
    // project under the old name rather than updating this one.
    if !repo_root.is_empty() && project::derive_project_name(&repo_root) == old_name {
        println!();
        println!(
            "{C_YELLOW}Note:{C_RESET} {C_CYAN}ug gen{C_RESET} in {} still derives {C_BOLD}{}{C_RESET} from the directory name.",
            repo_root, old_name
        );
        println!(
            "  Pass {C_CYAN}-n {}{C_RESET} there to keep updating this project.",
            new_name
        );
    }
}

/// `ug rm [<project>]` — delete a project's data directory under
/// `~/.ug` (or `$UG_HOME`). Prompts for confirmation unless `-f/--force`
/// (or `-y/--yes`) is given; an empty/EOF answer (e.g. non-interactive
/// stdin) is treated as "no" so this fails closed by default.
pub(crate) fn run_rm(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug rm{C_RESET} [<project>] [-n, --name <project>] [-f, --force | -y, --yes]");
        println!("  Delete a project's data directory under ~/.ug (or $UG_HOME).");
        println!("  Project defaults to the current directory's basename if omitted.");
        return;
    }

    let value_flags = ["-n", "--name"];
    let name_flag = flag_value(args, &["-n", "--name"]);
    let positional = first_positional(args, &value_flags);
    let project_name = name_flag
        .or(positional)
        .map(|n| project::sanitize_name(&n))
        .unwrap_or_else(|| project::derive_project_name("."));

    let dir = project::project_dir(&project_name);
    if !dir.exists() {
        eprintln!(
            "No project named {C_BOLD}{}{C_RESET} found at {}.",
            project_name,
            dir.display()
        );
        eprintln!("Run {C_CYAN}ug list{C_RESET} to see available projects.");
        std::process::exit(1);
    }

    println!("About to remove project {C_BOLD}{}{C_RESET}", project_name);
    println!("  path:  {}", dir.display());
    if let Some(meta) = project::read_meta(&dir) {
        println!("  repo:  {}", meta.repo_root);
        println!("  nodes: {}, edges: {}", meta.nodes, meta.edges);
    }

    let force = has_flag(args, "-f")
        || has_flag(args, "--force")
        || has_flag(args, "-y")
        || has_flag(args, "--yes");
    if !force {
        use std::io::Write;
        print!("Delete this project directory? This cannot be undone. [y/N] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        let answer = input.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return;
        }
    }

    // Captured before removal: get_active_project() validates the project
    // still has data, so it must be read while the dir still exists.
    let was_active = project::get_active_project().as_deref() == Some(project_name.as_str());

    match project::remove_project_dir(&dir) {
        Ok(()) => {
            // Drop the now-dangling active-project marker.
            if was_active {
                let _ = project::clear_active_project();
            }
            println!(
                "{C_GREEN}✓{C_RESET} Removed {C_BOLD}{}{C_RESET} ({})",
                project_name,
                dir.display()
            );
        }
        Err(e) => {
            eprintln!("Failed to remove {}: {}", dir.display(), e);
            std::process::exit(1);
        }
    }
}

/// `ug uninstall` — deletes every indexed project under `ug_home()` (all
/// of `~/.ug` / `$UG_HOME`) and then removes the standalone install
/// itself: the `~/.local/share/ultragraph` dir the prebuilt installer
/// (see README's Install section, `curl ... install.sh`) unpacks into,
/// and the `~/.local/bin/ug` symlink it points at. The symlink is only
/// touched when it actually resolves into that install dir — never a
/// same-named file the user happens to have on their own PATH. A
/// from-source checkout has neither of those, so that half is silently
/// skipped and only project data is removed. Prompts for confirmation
/// unless `-f/--force` (or `-y/--yes`); empty/EOF input (e.g.
/// non-interactive stdin) reads as "no", same fail-closed default as `ug
/// rm`.
pub(crate) fn run_uninstall(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug uninstall{C_RESET} [-f, --force | -y, --yes]");
        println!(
            "  Delete ALL indexed projects under {} and uninstall ug itself",
            project::ug_home().display()
        );
        println!("  (the standalone install dir + `ug` symlink, if this is a prebuilt install).");
        return;
    }

    let home = dirs::home_dir();
    let install_dir = home
        .as_ref()
        .map(|h| h.join(".local").join("share").join("ultragraph"));
    let bin_symlink = home.as_ref().map(|h| h.join(".local").join("bin").join("ug"));

    let ug_home_dir = project::ug_home();
    let projects = project::list_projects();
    let install_dir_exists = install_dir.as_ref().is_some_and(|d| d.exists());
    // Resolve both sides before the prefix test. `read_link` returns the
    // symlink's literal target, which the installer may have written as a
    // relative path, and `install_dir` is built from `$HOME` without being
    // resolved — so an unresolved comparison reports "not ours" for a symlink
    // that is ours and silently leaves it behind on uninstall. See Agents.md §9a.
    let canon_install_dir = install_dir
        .as_ref()
        .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()));
    let bin_symlink_is_ours = bin_symlink.as_ref().is_some_and(|p| {
        p.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
            && std::fs::canonicalize(p)
                .ok()
                .and_then(|target| canon_install_dir.as_ref().map(|d| target.starts_with(d)))
                .unwrap_or(false)
    });

    println!("{C_BOLD}This will:{C_RESET}");
    if ug_home_dir.exists() {
        println!(
            "  - Delete {} indexed project(s) under {}",
            projects.len(),
            ug_home_dir.display()
        );
    }
    if install_dir_exists {
        println!(
            "  - Remove the installed app at {}",
            install_dir.as_ref().unwrap().display()
        );
    }
    if bin_symlink_is_ours {
        println!(
            "  - Remove the `ug` symlink at {}",
            bin_symlink.as_ref().unwrap().display()
        );
    }
    if !install_dir_exists && !bin_symlink_is_ours {
        println!(
            "  {C_YELLOW}(no standalone install found — looks like a from-source checkout, so only project data will be removed){C_RESET}"
        );
    }
    println!();
    println!("{C_BOLD}{C_YELLOW}This cannot be undone.{C_RESET}");

    let force = has_flag(args, "-f")
        || has_flag(args, "--force")
        || has_flag(args, "-y")
        || has_flag(args, "--yes");
    if !force {
        use std::io::Write;
        print!("Type 'yes' to confirm: ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        let answer = input.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return;
        }
    }

    if ug_home_dir.exists() {
        match std::fs::remove_dir_all(&ug_home_dir) {
            Ok(()) => println!(
                "{C_GREEN}✓{C_RESET} Removed project data at {}",
                ug_home_dir.display()
            ),
            Err(e) => eprintln!("Failed to remove {}: {}", ug_home_dir.display(), e),
        }
    }

    if bin_symlink_is_ours {
        let p = bin_symlink.unwrap();
        match std::fs::remove_file(&p) {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Removed symlink {}", p.display()),
            Err(e) => eprintln!("Failed to remove {}: {}", p.display(), e),
        }
    }

    if install_dir_exists {
        let d = install_dir.unwrap();
        match std::fs::remove_dir_all(&d) {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Removed {}", d.display()),
            Err(e) => eprintln!("Failed to remove {}: {}", d.display(), e),
        }
    }

    println!();
    println!("{C_BOLD}ug has been uninstalled.{C_RESET} Thanks for trying UltraGraph.");
}

fn print_rename_help() {
    println!("  {C_CYAN}ug rename{C_RESET}  {C_YELLOW}— rename a project{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug rename <new-name>          {C_DIM}(alias: rn){C_RESET}");
    println!("        ug rename <old-name> <new-name>");
    println!("        ug rename -n <old-name> <new-name>");
    println!();
    println!("  Renames the project's data directory under ~/.ug (or $UG_HOME) and");
    println!("  updates its project.json. With one argument it renames the current");
    println!("  project: the active one ({C_CYAN}ug active{C_RESET}), else this directory's basename.");
    println!("  The {C_YELLOW}← active{C_RESET} marker follows the rename; the graph and db are untouched.");
    println!();
    println!("  Names are sanitized the same way project names always are (chars");
    println!("  outside {C_BOLD}[A-Za-z0-9._-]{C_RESET} become {C_BOLD}-{C_RESET}). Renaming onto an existing project");
    println!("  is refused — remove it first with {C_CYAN}ug rm{C_RESET}.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug rename backend-api{C_RESET}          {C_YELLOW}# rename the active project{C_RESET}");
    println!("  {C_CYAN}ug rename ug-3 ug{C_RESET}              {C_YELLOW}# rename a specific project{C_RESET}");
}

fn print_list_help() {
    println!("  {C_BOLD}{C_GREEN}★ ug list{C_RESET}  {C_YELLOW}— list generated projects{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug list [--quick] [--json]   {C_DIM}(aliases: ls, list_projects — the MCP tool's name){C_RESET}");
    println!();
    println!("  Lists every project under ~/.ug (or $UG_HOME): node/edge counts, size on");
    println!("  disk, how far the index has drifted from the repo, and last-updated time.");
    println!("  The current directory's project is marked with {C_BOLD}*{C_RESET}, the {C_CYAN}ug active{C_RESET} one with {C_YELLOW}←{C_RESET}.");
    println!();
    println!("{C_BOLD}STATUS{C_RESET} is whatever blocks you soonest:");
    println!("  {C_GREEN}fresh{C_RESET}        every indexed file still matches the graph");
    println!("  {C_YELLOW}N changed{C_RESET}    N indexed files were edited or deleted — {C_CYAN}ug gen -n <name>{C_RESET}");
    println!("  {C_YELLOW}no db{C_RESET}        never ingested: {C_CYAN}query{C_RESET}/{C_CYAN}search{C_RESET}/{C_CYAN}chat{C_RESET} cannot read it");
    println!("  {C_YELLOW}repo gone{C_RESET}    the indexed tree has moved or been deleted");
    println!("  {C_YELLOW}no graph{C_RESET}     the project dir holds no graph.json");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--quick{C_RESET}   Skip the size walk and the staleness scan (SIZE and STATUS read {C_BOLD}-{C_RESET}/{C_BOLD}?{C_RESET}).");
    println!("  {C_CYAN}--json{C_RESET}    Machine-readable rows, including the per-file changed/missing counts.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dir: std::path::PathBuf, has_db: bool, staleness: Option<project::Staleness>) -> Row {
        Row {
            meta: project::ProjectMeta::new("demo", "/repo", 0, 0),
            dir,
            size: None,
            staleness,
            has_db,
            is_active: false,
            is_cwd: false,
        }
    }

    fn drift(changed: usize, missing: usize, repo_missing: bool) -> project::Staleness {
        project::Staleness {
            built_at: Some(1),
            files: 10,
            changed,
            missing,
            repo_missing,
            doc_nodes: 0,
            code_nodes: 10,
        }
    }

    /// STATUS shows whatever blocks the user soonest, not everything at once.
    /// A project with no db is *also* usually stale, and telling someone to
    /// re-generate a graph they cannot query yet is the wrong next step.
    #[test]
    fn status_reports_the_soonest_blocker() {
        let empty = tempfile::tempdir().expect("dir");
        let d = empty.path().to_path_buf();

        assert_eq!(row(d.clone(), true, None).status().0, "no graph");
        assert_eq!(row(d.clone(), true, Some(drift(0, 0, true))).status().0, "repo gone");
        assert_eq!(row(d.clone(), false, Some(drift(3, 0, false))).status().0, "no db");
        assert_eq!(row(d.clone(), true, Some(drift(3, 2, false))).status().0, "5 changed");
        assert_eq!(row(d.clone(), true, Some(drift(0, 0, false))).status().0, "fresh");

        // `--quick` skipped the scan on a project that *does* have a graph:
        // unknown, which is not the same claim as "fresh".
        std::fs::write(empty.path().join("graph.json"), "{}").expect("graph");
        assert_eq!(row(d, true, None).status().0, "?");
    }

    #[test]
    fn commas_group_digits() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(43_831), "43,831");
        assert_eq!(commas(1_234_567), "1,234,567");
    }

    #[test]
    fn bytes_render_at_human_scale() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(72_743_516), "69 MB");
        // The unit ladder stops at TB rather than overflowing off the end.
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024 * 1024), "5.0 TB");
    }
}

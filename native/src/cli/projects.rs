//! Managing the projects under `~/.ug`: listing them, selecting the
//! active one, removing one, and uninstalling ug entirely.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

use super::args::{first_positional, flag_value, has_flag, positionals};

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
    let cwd_name = project::derive_project_name(".");
    let active = project::get_active_project();
    println!(
        "{C_BOLD}Projects in {}{C_RESET} ({}):\n",
        root.display(),
        projects.len()
    );
    println!(
        "  {C_BOLD}{:<24} {:>8} {:>8}  {:<19}  {}{C_RESET}",
        "NAME", "NODES", "EDGES", "UPDATED", "REPO"
    );
    for (_dir, meta) in &projects {
        // `*` = matches the current directory; `→` = the active project
        // (`ug active`). When one project is both, `*` wins the leading slot
        // and the row still carries the active tag.
        let is_active = active.as_deref() == Some(meta.name.as_str());
        let marker = if meta.name == cwd_name { "*" } else { " " };
        let tag = if is_active {
            format!("  {C_YELLOW}← active{C_RESET}")
        } else {
            String::new()
        };
        let updated = format_epoch(meta.updated_at);
        println!(
            "{C_GREEN}{}{C_RESET} {C_CYAN}{:<24}{C_RESET} {:>8} {:>8}  {:<19}  {}{}",
            marker, meta.name, meta.nodes, meta.edges, updated, meta.repo_root, tag
        );
    }
    println!(
        "\n{C_BOLD}*{C_RESET} matches the current directory; {C_YELLOW}← active{C_RESET} is the default for {C_CYAN}ug mcp{C_RESET} and {C_CYAN}ug serve{C_RESET} (set with {C_CYAN}ug active <name>{C_RESET})."
    );
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
    let bin_symlink_is_ours = bin_symlink.as_ref().is_some_and(|p| {
        p.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
            && std::fs::read_link(p)
                .ok()
                .and_then(|target| install_dir.as_ref().map(|d| target.starts_with(d)))
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
    println!("{C_BOLD}Usage:{C_RESET}  ug list   {C_DIM}(aliases: ls, list_projects — the MCP tool's name){C_RESET}");
    println!();
    println!("  Lists every project under ~/.ug (or $UG_HOME), with node/edge counts");
    println!("  and last-updated time. The current directory's project is marked with {C_BOLD}*{C_RESET}.");
}

/// Render epoch seconds as local-naive `YYYY-MM-DD HH:MM:SS` (UTC).
fn format_epoch(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    // Days-from-civil algorithm (Howard Hinnant) — avoids a chrono dep.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

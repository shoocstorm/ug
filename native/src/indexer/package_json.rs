//! Project-level dependency extraction.
//!
//! Reads `package.json` from the indexed project root and surfaces every
//! declared dependency. Currently only the npm ecosystem is supported - when
//! adding Java (`pom.xml`, `build.gradle`) or Python (`pyproject.toml`),
//! introduce a sibling module and have the orchestrator merge their results.

use crate::types::Dependency;
use std::fs;
use std::path::Path;

/// Read `package.json` next to `path` (the project root) and return every
/// declared `dependencies`, `devDependencies`, and `optionalDependencies`
/// entry. Returns an empty list if the file is missing or unparseable - the
/// indexer should still produce a useful result for non-npm projects.
pub fn extract_package_json_dependencies(path: &str) -> Vec<Dependency> {
    let pkg_path = Path::new(path).join("package.json");
    if !pkg_path.exists() {
        return Vec::new();
    }

    let content = match fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let pkg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut deps = Vec::new();
    push_section(&pkg, &mut deps, "dependencies", false, false);
    push_section(&pkg, &mut deps, "devDependencies", true, false);
    push_section(&pkg, &mut deps, "optionalDependencies", false, true);
    deps
}

fn push_section(
    pkg: &serde_json::Value,
    deps: &mut Vec<Dependency>,
    key: &str,
    dev: bool,
    optional: bool,
) {
    let Some(obj) = pkg.get(key).and_then(|v| v.as_object()) else {
        return;
    };
    for (name, version) in obj {
        deps.push(Dependency {
            name: name.clone(),
            version: version.as_str().map(|s| s.to_string()),
            dev,
            optional,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Stage a `package.json` with `content` and extract from its directory.
    fn extract(content: &str) -> Vec<Dependency> {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("package.json"), content).expect("write");
        extract_package_json_dependencies(&dir.path().to_string_lossy())
    }

    fn by_name(deps: &[Dependency]) -> HashMap<&str, &Dependency> {
        deps.iter().map(|d| (d.name.as_str(), d)).collect()
    }

    #[test]
    fn all_three_sections_are_read_and_flagged() {
        let deps = extract(
            r#"{
                "dependencies":         { "react": "^18.0.0" },
                "devDependencies":      { "vitest": "^1.2.0" },
                "optionalDependencies": { "fsevents": "~2.3.2" }
            }"#,
        );
        assert_eq!(deps.len(), 3);
        let m = by_name(&deps);

        // The flags are the whole point of reading three sections rather
        // than one: they are what lets a consumer tell a shipped dependency
        // from a build-time one.
        assert_eq!(m["react"].version.as_deref(), Some("^18.0.0"));
        assert!(!m["react"].dev && !m["react"].optional);
        assert!(m["vitest"].dev && !m["vitest"].optional);
        assert!(m["fsevents"].optional && !m["fsevents"].dev);
    }

    #[test]
    fn a_missing_file_yields_no_dependencies() {
        // Every non-npm project takes this path, so it has to be a quiet
        // empty rather than an error.
        let dir = TempDir::new().expect("tempdir");
        assert!(extract_package_json_dependencies(&dir.path().to_string_lossy()).is_empty());
    }

    #[test]
    fn a_missing_directory_yields_no_dependencies() {
        assert!(extract_package_json_dependencies("/nonexistent/path/xyzzy").is_empty());
    }

    #[test]
    fn malformed_json_degrades_to_empty_rather_than_panicking() {
        // A trailing comma is the most common hand-edit mistake, and a
        // half-written file is what a watcher catches mid-save.
        assert!(extract(r#"{ "dependencies": { "a": "1", } }"#).is_empty());
        assert!(extract("not json at all").is_empty());
        assert!(extract("").is_empty());
    }

    #[test]
    fn a_package_json_with_no_dependency_sections_is_empty() {
        assert!(extract(r#"{ "name": "app", "version": "1.0.0" }"#).is_empty());
        assert!(extract("{}").is_empty());
    }

    #[test]
    fn a_section_of_the_wrong_shape_is_skipped_not_fatal() {
        // `dependencies` as an array is invalid npm, but it must not stop
        // the valid sections beside it from being read.
        let deps = extract(
            r#"{
                "dependencies": ["react"],
                "devDependencies": { "vitest": "^1.0.0" }
            }"#,
        );
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "vitest");
    }

    #[test]
    fn a_non_string_version_keeps_the_dependency_but_drops_the_version() {
        // The name is the useful half; losing the whole entry over an
        // unparseable version would lose a real edge in the graph.
        let deps = extract(r#"{ "dependencies": { "weird": 42, "fine": "^1.0.0" } }"#);
        let m = by_name(&deps);
        assert_eq!(deps.len(), 2);
        assert_eq!(m["weird"].version, None);
        assert_eq!(m["fine"].version.as_deref(), Some("^1.0.0"));
    }

    #[test]
    fn scoped_names_and_url_versions_survive_verbatim() {
        let deps = extract(
            r#"{
                "dependencies": {
                    "@scope/pkg": "workspace:*",
                    "from-git": "github:user/repo#v1.2.3",
                    "from-file": "file:../local"
                }
            }"#,
        );
        let m = by_name(&deps);
        assert_eq!(m["@scope/pkg"].version.as_deref(), Some("workspace:*"));
        assert_eq!(
            m["from-git"].version.as_deref(),
            Some("github:user/repo#v1.2.3")
        );
        assert_eq!(m["from-file"].version.as_deref(), Some("file:../local"));
    }

    #[test]
    fn the_same_name_in_two_sections_produces_two_entries() {
        // npm allows it (and it happens during migrations). Deduping here
        // would silently pick a winner; the caller can decide.
        let deps = extract(
            r#"{
                "dependencies":    { "typescript": "^5.0.0" },
                "devDependencies": { "typescript": "^4.9.0" }
            }"#,
        );
        assert_eq!(deps.len(), 2);
        assert_eq!(deps.iter().filter(|d| d.dev).count(), 1);
    }
}

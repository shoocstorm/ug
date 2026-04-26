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

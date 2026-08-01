//! Canonical string labels for OverGraph nodes and edges.
//!
//! OverGraph ≥ 0.17 keys nodes by `(label: &str, key: &str)` and labels
//! edges with a string, so the numeric `u32` id mapping this module used
//! to own is gone. What remains is still needed:
//!
//! 1. **Canonicalization.** Edge-type filters arrive from user input and
//!    tool arguments in whatever case the caller typed (`"calls"`,
//!    `"Calls"`, `"CALLS"`). Labels are matched exactly by the engine, so
//!    they have to be normalized to one spelling before they reach it.
//! 2. **The label inventory.** Callers that must sweep the whole store —
//!    `lookup_id`'s slow path, the ingest pruner — iterate
//!    [`ALL_NODE_LABELS`] rather than hardcoding a list, so adding a node
//!    type cannot leave a sweep silently missing it.
//!
//! Labels are persisted in OverGraph segments. Renaming one is a
//! breaking store change: bump `STORE_FORMAT_VERSION` in `db.rs` so old
//! databases are rejected rather than silently misread.

/// Every node label that can appear on disk, including the `Unknown`
/// fallback used by the JSON hydration path so an older graph does not
/// crash a newer build.
pub const ALL_NODE_LABELS: &[&str] = &[
    "File",
    "Folder",
    "Function",
    "Class",
    "Interface",
    "Concept",
    "Dependency",
    "Config",
    "Constant",
    "Route",
    "Unknown",
];

/// Every edge label that can appear on disk.
pub const ALL_EDGE_LABELS: &[&str] = &[
    "DependsOn",
    "Calls",
    "Extends",
    "Implements",
    "References",
    "Contains",
    "Imports",
    "Exports",
    "Requires",
    "Uses",
    "Overrides",
    "Instantiates",
    "Unknown",
];

/// Normalize a node type string (a `GraphNodeType` variant name in any
/// case) to its canonical stored label. Unrecognized input maps to
/// `"Unknown"` rather than being passed through, so a typo cannot mint a
/// new label in the engine's catalog.
pub fn node_label(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "file" => "File",
        "folder" => "Folder",
        "function" => "Function",
        "class" => "Class",
        "interface" => "Interface",
        "concept" => "Concept",
        "dependency" => "Dependency",
        "config" => "Config",
        "constant" => "Constant",
        "route" => "Route",
        _ => "Unknown",
    }
}

/// Normalize an edge type string to its canonical stored label. Accepts
/// both `DependsOn` and `depends_on` spellings.
pub fn edge_label(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "dependson" | "depends_on" => "DependsOn",
        "calls" => "Calls",
        "extends" => "Extends",
        "implements" => "Implements",
        "references" => "References",
        "contains" => "Contains",
        "imports" => "Imports",
        "exports" => "Exports",
        "requires" => "Requires",
        "uses" => "Uses",
        "overrides" => "Overrides",
        "instantiates" => "Instantiates",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_labels_are_canonical_and_idempotent() {
        for s in ALL_NODE_LABELS {
            if *s == "Unknown" {
                continue;
            }
            assert_eq!(node_label(s), *s, "{s} should map to itself");
            assert_eq!(
                node_label(&s.to_ascii_uppercase()),
                *s,
                "{s} should normalize case-insensitively"
            );
        }
    }

    #[test]
    fn edge_labels_are_canonical_and_idempotent() {
        for s in ALL_EDGE_LABELS {
            if *s == "Unknown" {
                continue;
            }
            assert_eq!(edge_label(s), *s, "{s} should map to itself");
            assert_eq!(
                edge_label(&s.to_ascii_lowercase()),
                *s,
                "{s} should normalize case-insensitively"
            );
        }
    }

    #[test]
    fn snake_case_edge_alias_resolves() {
        assert_eq!(edge_label("depends_on"), "DependsOn");
    }

    #[test]
    fn unknown_input_falls_back_rather_than_passing_through() {
        assert_eq!(node_label("MadeUpType"), "Unknown");
        assert_eq!(edge_label("MadeUpEdge"), "Unknown");
    }
}

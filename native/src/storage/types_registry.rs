//! Stable string ↔ u32 mapping for OverGraph type IDs.
//!
//! OverGraph keys nodes by `(type_id: u32, key: String)` and edges by
//! `(from_id, to_id, type_id: u32)`. The project uses string variant
//! names from `GraphNodeType` / `GraphEdgeType`. This module is the
//! single source of truth for the translation.
//!
//! IDs are persisted on disk in OverGraph segments. **Once assigned an
//! ID must never change** — renaming a constant would silently corrupt
//! every existing database. New types append at the end.

// ---- Node types ----
// Aligned with `GraphNodeType` in `crate::types`. IDs are reserved in
// blocks of 100 to leave room for future additions per category.
pub const NODE_TYPE_FILE: u32 = 1;
pub const NODE_TYPE_FOLDER: u32 = 2;
pub const NODE_TYPE_FUNCTION: u32 = 3;
pub const NODE_TYPE_CLASS: u32 = 4;
pub const NODE_TYPE_INTERFACE: u32 = 5;
pub const NODE_TYPE_CONCEPT: u32 = 6;
pub const NODE_TYPE_DEPENDENCY: u32 = 7;
pub const NODE_TYPE_CONFIG: u32 = 8;
// Generic catch-all for anything not modeled above; used by the JSON
// hydration path so older graphs don't crash a newer build.
pub const NODE_TYPE_UNKNOWN: u32 = 99;

// ---- Edge types ----
pub const EDGE_TYPE_DEPENDS_ON: u32 = 100;
pub const EDGE_TYPE_CALLS: u32 = 101;
pub const EDGE_TYPE_EXTENDS: u32 = 102;
pub const EDGE_TYPE_IMPLEMENTS: u32 = 103;
pub const EDGE_TYPE_REFERENCES: u32 = 104;
pub const EDGE_TYPE_CONTAINS: u32 = 105;
pub const EDGE_TYPE_IMPORTS: u32 = 106;
pub const EDGE_TYPE_EXPORTS: u32 = 107;
pub const EDGE_TYPE_REQUIRES: u32 = 108;
pub const EDGE_TYPE_USES: u32 = 109;
pub const EDGE_TYPE_UNKNOWN: u32 = 199;

/// Map a node type string (variant name from `GraphNodeType` debug
/// formatting, case-insensitive) to a stable u32 id.
pub fn node_type_to_id(s: &str) -> u32 {
    match s.to_ascii_lowercase().as_str() {
        "file" => NODE_TYPE_FILE,
        "folder" => NODE_TYPE_FOLDER,
        "function" => NODE_TYPE_FUNCTION,
        "class" => NODE_TYPE_CLASS,
        "interface" => NODE_TYPE_INTERFACE,
        "concept" => NODE_TYPE_CONCEPT,
        "dependency" => NODE_TYPE_DEPENDENCY,
        "config" => NODE_TYPE_CONFIG,
        _ => NODE_TYPE_UNKNOWN,
    }
}

pub fn node_type_from_id(id: u32) -> &'static str {
    match id {
        NODE_TYPE_FILE => "File",
        NODE_TYPE_FOLDER => "Folder",
        NODE_TYPE_FUNCTION => "Function",
        NODE_TYPE_CLASS => "Class",
        NODE_TYPE_INTERFACE => "Interface",
        NODE_TYPE_CONCEPT => "Concept",
        NODE_TYPE_DEPENDENCY => "Dependency",
        NODE_TYPE_CONFIG => "Config",
        _ => "Unknown",
    }
}

pub fn edge_type_to_id(s: &str) -> u32 {
    match s.to_ascii_lowercase().as_str() {
        "dependson" | "depends_on" => EDGE_TYPE_DEPENDS_ON,
        "calls" => EDGE_TYPE_CALLS,
        "extends" => EDGE_TYPE_EXTENDS,
        "implements" => EDGE_TYPE_IMPLEMENTS,
        "references" => EDGE_TYPE_REFERENCES,
        "contains" => EDGE_TYPE_CONTAINS,
        "imports" => EDGE_TYPE_IMPORTS,
        "exports" => EDGE_TYPE_EXPORTS,
        "requires" => EDGE_TYPE_REQUIRES,
        "uses" => EDGE_TYPE_USES,
        _ => EDGE_TYPE_UNKNOWN,
    }
}

pub fn edge_type_from_id(id: u32) -> &'static str {
    match id {
        EDGE_TYPE_DEPENDS_ON => "DependsOn",
        EDGE_TYPE_CALLS => "Calls",
        EDGE_TYPE_EXTENDS => "Extends",
        EDGE_TYPE_IMPLEMENTS => "Implements",
        EDGE_TYPE_REFERENCES => "References",
        EDGE_TYPE_CONTAINS => "Contains",
        EDGE_TYPE_IMPORTS => "Imports",
        EDGE_TYPE_EXPORTS => "Exports",
        EDGE_TYPE_REQUIRES => "Requires",
        EDGE_TYPE_USES => "Uses",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_type_roundtrip_known() {
        for s in [
            "File",
            "Folder",
            "Function",
            "Class",
            "Interface",
            "Concept",
            "Dependency",
            "Config",
        ] {
            let id = node_type_to_id(s);
            assert_ne!(id, NODE_TYPE_UNKNOWN, "{s} should be a known node type");
            assert_eq!(node_type_from_id(id), s);
        }
    }

    #[test]
    fn edge_type_roundtrip_known() {
        for s in [
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
        ] {
            let id = edge_type_to_id(s);
            assert_ne!(id, EDGE_TYPE_UNKNOWN, "{s} should be a known edge type");
            assert_eq!(edge_type_from_id(id), s);
        }
    }

    #[test]
    fn node_type_unknown_falls_back() {
        assert_eq!(node_type_to_id("MadeUpType"), NODE_TYPE_UNKNOWN);
        assert_eq!(node_type_from_id(99999), "Unknown");
    }
}

//! Node ids must survive edits elsewhere in the file.
//!
//! Ids used to embed the symbol's start line, so inserting anything above a
//! symbol renamed it — it re-embedded as a new node and orphaned its old row
//! in the store. These tests pin the property that actually matters: index a
//! file, index it again with unrelated lines prepended, and every symbol's id
//! must be unchanged.

use std::fs;
use tempfile::TempDir;
use ultragraph::{build_graph, index, types::GraphData};

fn graph_of(dir: &TempDir) -> GraphData {
    let json = build_graph(index(dir.path().to_string_lossy().to_string()));
    serde_json::from_str(&json).unwrap()
}

fn symbol_ids(g: &GraphData) -> Vec<String> {
    let mut ids: Vec<String> = g
        .nodes
        .iter()
        .filter(|n| n.start_line.is_some())
        .map(|n| n.id.clone())
        .collect();
    ids.sort();
    ids
}

const SRC: &str = r#"
export function alpha(): void { }
export function beta(): void { }
export class Gamma { }
"#;

#[test]
fn ids_survive_lines_inserted_above() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.ts"), SRC).unwrap();
    let before = symbol_ids(&graph_of(&dir));
    assert!(!before.is_empty(), "fixture produced no symbols");

    // Every symbol shifts down four lines; nothing else changes.
    fs::write(dir.path().join("a.ts"), format!("//\n//\n//\n//\n{SRC}")).unwrap();
    let after = symbol_ids(&graph_of(&dir));

    assert_eq!(before, after, "ids must not move when lines shift");
}

#[test]
fn ids_carry_no_line_number() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.ts"), SRC).unwrap();
    let g = graph_of(&dir);

    let alpha = g
        .nodes
        .iter()
        .find(|n| n.name == "alpha")
        .expect("alpha node");
    let line = alpha.start_line.expect("line still travels on the node");

    assert!(alpha.id.ends_with(":alpha"), "id ends with the name: {}", alpha.id);
    assert!(alpha.id.contains("a.ts"), "id is file-scoped: {}", alpha.id);
    assert!(
        !alpha.id.contains(&format!(":{line}:")),
        "id must not embed the start line ({line}): {}",
        alpha.id
    );
}

#[test]
fn same_name_twice_in_one_file_gets_an_ordinal() {
    let dir = TempDir::new().unwrap();
    // Two same-named methods in one file — the real-world case is a method
    // defined in both an inherent and a trait impl.
    fs::write(
        dir.path().join("dup.ts"),
        "export function handler(): void { }\nexport function handler(): void { }\n",
    )
    .unwrap();
    let g = graph_of(&dir);

    let ids: Vec<&str> = g
        .nodes
        .iter()
        .filter(|n| n.name == "handler")
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(ids.len(), 2, "both declarations should be nodes");
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), 2, "ids must stay unique: {ids:?}");
    assert!(
        ids.iter().any(|i| i.ends_with("#2")),
        "second occurrence takes an ordinal: {ids:?}"
    );
}

#[test]
fn same_name_in_different_files_stays_distinct() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.ts"), "export function shared(): void { }").unwrap();
    fs::write(dir.path().join("b.ts"), "export function shared(): void { }").unwrap();
    let g = graph_of(&dir);

    let ids: std::collections::HashSet<&str> = g
        .nodes
        .iter()
        .filter(|n| n.name == "shared")
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(ids.len(), 2, "file scoping keeps them apart: {ids:?}");
}

#[test]
fn every_id_in_a_graph_is_unique() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.ts"),
        "export function f(): void { }\nexport function f(): void { }\nexport class f { }\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.md"), "# Overview\n\ntext\n\n# Overview\n\nmore\n").unwrap();
    let g = graph_of(&dir);

    let mut seen = std::collections::HashSet::new();
    for n in &g.nodes {
        assert!(seen.insert(n.id.clone()), "duplicate node id: {}", n.id);
    }
}

#[test]
fn edges_still_resolve_to_the_new_ids() {
    // Pass 1 and pass 2 of build_graph compute ids independently; if they
    // ever disagreed, edges would point at ids no node carries.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.ts"),
        "export class Base { }\nexport class Child extends Base { }\n",
    )
    .unwrap();
    let g = graph_of(&dir);

    let ids: std::collections::HashSet<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(!g.edges.is_empty(), "fixture produced no edges");
    for e in &g.edges {
        assert!(ids.contains(e.source.as_str()), "dangling source: {}", e.source);
        assert!(ids.contains(e.target.as_str()), "dangling target: {}", e.target);
    }
}

#[test]
fn repeated_markdown_headings_stay_addressable() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("d.md"),
        "# Setup\n\na\n\n## Usage\n\nb\n\n# Setup\n\nc\n",
    )
    .unwrap();
    let before = symbol_ids(&graph_of(&dir));

    fs::write(
        dir.path().join("d.md"),
        "intro line\n\n# Setup\n\na\n\n## Usage\n\nb\n\n# Setup\n\nc\n",
    )
    .unwrap();
    let after = symbol_ids(&graph_of(&dir));

    assert_eq!(before, after, "heading ids must not move when lines shift");
    assert!(
        before.iter().any(|i| i.ends_with("#2")),
        "the repeated heading takes an ordinal: {before:?}"
    );
}

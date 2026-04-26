use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(rename = "startLine")]
    pub start_line: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    pub docstring: Option<String>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default)]
    pub imports: Vec<ImportInfo>,
    #[serde(default)]
    pub exports: Vec<ExportInfo>,
    #[serde(default)]
    pub extends: Vec<String>,
    #[serde(default)]
    pub implements: Vec<String>,
    #[serde(default)]
    pub calls: Vec<String>,

    #[serde(default)]
    pub metrics: Option<SymbolMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub params: Vec<Param>,
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub param_type: Option<String>,
    pub optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportInfo {
    pub path: String,
    pub imported: Vec<ImportedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedItem {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportInfo {
    pub name: String,
    pub alias: Option<String>,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub name: String,
    pub generic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolMetrics {
    pub loc: u32,
    pub params: u32,
    #[serde(rename = "maxNesting")]
    pub max_nesting: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub hash: String,
    pub language: String,
    pub classification: Option<FileClassification>,
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub imports: Vec<ImportInfo>,
    #[serde(default)]
    pub exports: Vec<ExportInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileClassification {
    Component,
    Page,
    Hook,
    Util,
    Service,
    Config,
    Type,
    Constant,
    Context,
    Reducer,
    Test,
    Asset,
    Documentation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub files: Vec<FileNode>,
    pub dependencies: Vec<Dependency>,
    pub stats: IndexStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub version: Option<String>,
    pub dev: bool,
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "cachedFiles")]
    pub cached_files: usize,
    #[serde(rename = "totalSymbols")]
    pub total_symbols: usize,
    #[serde(rename = "indexingTimeMs")]
    pub indexing_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GraphNodeType {
    File,
    Function,
    Class,
    Interface,
    Concept,
    Dependency,
    Config,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum GraphEdgeType {
    DependsOn,
    Calls,
    Extends,
    Implements,
    References,
    Contains,
     Imports,
     Exports,
     Requires,
     Uses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub node_type: GraphNodeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(rename = "startLine", skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(rename = "endLine", skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SymbolMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<GraphNodeSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<GraphNodeImport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<GraphNodeExport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,

}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeSignature {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    #[serde(rename = "returnType", skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeImport {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported: Vec<GraphImportedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphImportedItem {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeExport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphTypeRef {
    pub name: String,
    pub generic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub edge_type: GraphEdgeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BfsResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub distances: std::collections::HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathResult {
    pub path: Vec<String>,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentralityResult {
    pub degree_centrality: std::collections::HashMap<String, f64>,
    pub betweenness_centrality: std::collections::HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub has_cycles: bool,
    pub cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredEdgesResult {
    pub edges: Vec<GraphEdge>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub nodes: Vec<GraphNode>,
    pub count: usize,
}
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
    pub typed_as: Vec<TypeRef>,
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
    #[serde(rename = "type")]
    pub param_type: Option<String>,
    pub optional: bool,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportInfo {
    pub path: String,
    pub imported: Vec<ImportedItem>,
    #[serde(rename = "isExternal")]
    pub is_external: bool,
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
    TypedAs,
    Uses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub node_type: GraphNodeType,
    pub file: Option<String>,
    #[serde(rename = "startLine")]
    pub start_line: Option<u32>,
    #[serde(rename = "endLine")]
    pub end_line: Option<u32>,
    pub metrics: Option<SymbolMetrics>,
    pub signature: Option<GraphNodeSignature>,
    pub docstring: Option<String>,
    #[serde(default)]
    pub imports: Vec<GraphNodeImport>,
    #[serde(default)]
    pub exports: Vec<GraphNodeExport>,
    #[serde(default)]
    pub extends: Vec<String>,
    #[serde(default)]
    pub implements: Vec<String>,
    #[serde(default)]
    pub calls: Vec<String>,
    #[serde(default)]
    pub typed_as: Vec<GraphTypeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeSignature {
    pub params: Vec<Param>,
    #[serde(rename = "returnType")]
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeImport {
    pub path: String,
    pub imported: Vec<GraphImportedItem>,
    #[serde(rename = "isExternal")]
    pub is_external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphImportedItem {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeExport {
    pub name: String,
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
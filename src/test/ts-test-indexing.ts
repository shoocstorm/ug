import { readFileSync, existsSync, mkdirSync } from "fs";
import { join, dirname } from "path";
import { addNativeBinding, loadBinding } from "napi-builds";

let binding: any;

function getBinding() {
  if (!binding) {
    try {
      binding = loadBinding("ultragraph-kb", "index");
    } catch (e) {
      console.error("Failed to load native binding:", e);
      throw e;
    }
  }
  return binding;
}

export interface Symbol {
  id: string;
  name: string;
  kind: string;
  file: string;
  startLine: number;
  endLine: number;
  docstring: string | null;
}

export interface FileNode {
  path: string;
  hash: string;
  language: string;
  symbols: Symbol[];
}

export interface IndexStats {
  totalFiles: number;
  cachedFiles: number;
  totalSymbols: number;
  indexingTimeMs: number;
}

export interface IndexResult {
  files: FileNode[];
  stats: IndexStats;
}

export type GraphNodeType = "File" | "Function" | "Class" | "Interface" | "Concept" | "Dependency" | "Config";
export type GraphEdgeType = "DependsOn" | "Calls" | "Extends" | "Implements" | "References" | "Contains" | "Imports" | "Exports" | "Requires" | "Uses";

export interface GraphNode {
  id: string;
  name: string;
  node_type: GraphNodeType;
  file: string | null;
  startLine: number | null;
  endLine: number | null;
  metrics?: { loc: number; params: number; maxNesting: number };
  signature?: { params: Array<{ name: string; type?: string; optional: boolean; default?: string }>; returnType?: string };
  docstring?: string | null;
  imports?: Array<{ path: string; imported: Array<{ name: string; alias?: string }> }>;
  exports?: Array<{ name: string; alias?: string; isDefault: boolean }>;
  extends?: string[];
  implements?: string[];
  calls?: string[];
}

export interface GraphEdge {
  source: string;
  target: string;
  edge_type: GraphEdgeType;
}

export interface GraphData {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export interface BfsResult {
  nodes: GraphNode[];
  edges: GraphEdge[];
  distances: Record<string, number>;
}

/**
 * Index all the files in a directory
 * @param path - Path to the directory
 * @returns IndexResult
 */
export async function index(path: string): Promise<IndexResult> {
  const binding = getBinding();
  const result = binding.index(path);
  return JSON.parse(result) as IndexResult;
}

/**
 * Index all the files in a directory with cache
 * @param path - Path to the directory
 * @param cachePath - Path to the cache
 * @returns IndexResult
 */
export async function indexWithCache(path: string, cachePath: string): Promise<IndexResult> {
  const binding = getBinding();
  const result = binding.indexWithCache(path, cachePath);
  return JSON.parse(result) as IndexResult;
}

/**
 * Index all the files in a directory
 * @param path - Path to the directory
 * @returns IndexResult
 */
export function indexSync(path: string): IndexResult {
  const binding = getBinding();
  const result = binding.index(path);
  return JSON.parse(result) as IndexResult;
}

/**
 * Index all the files in a directory with cache
 * @param path - Path to the directory
 * @param cachePath - Path to the cache
 * @returns IndexResult
 */
export function indexWithCacheSync(path: string, cachePath: string): IndexResult {
  const binding = getBinding();
  const result = binding.indexWithCache(path, cachePath);
  return JSON.parse(result) as IndexResult;
}

/**
 * Build a graph from an index result
 * @param indexResult - Index result
 * @returns GraphData
 */
export function buildGraph(indexResult: IndexResult): GraphData {
  const binding = getBinding();
  const json = JSON.stringify(indexResult);
  const result = binding.buildGraph(json);
  return JSON.parse(result) as GraphData;
}

/**
 * Perform k-hop BFS on a graph
 * @param graph - Graph data
 * @param startNodeId - Starting node ID
 * @param k - Number of hops
 * @returns BfsResult
 */
export function kHopBfs(graph: GraphData, startNodeId: string, k: number): BfsResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.kHopBfs(json, startNodeId, k);
  return JSON.parse(result) as BfsResult;
}

export interface FilteredEdgesResult {
  edges: GraphEdge[];
  count: number;
}

export interface PathResult {
  path: string[];
  found: boolean;
  length: number | null;
}

export interface CentralityResult {
  degree_centrality: Record<string, number>;
  betweenness_centrality: Record<string, number>;
}

export interface CycleResult {
  has_cycles: boolean;
  cycles: string[][];
}

export function filterEdgesByType(graph: GraphData, edgeTypes: string[]): FilteredEdgesResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.filterEdgesByType(json, edgeTypes);
  return JSON.parse(result) as FilteredEdgesResult;
}

export function findShortestPath(graph: GraphData, sourceId: string, targetId: string): PathResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.findShortestPath(json, sourceId, targetId);
  return JSON.parse(result) as PathResult;
}

export function calculateCentrality(graph: GraphData): CentralityResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.calculateCentrality(json);
  return JSON.parse(result) as CentralityResult;
}

export function detectCycles(graph: GraphData): CycleResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.detectCycles(json);
  return JSON.parse(result) as CycleResult;
}

export interface GraphAnalysis {
  centrality: CentralityResult;
  cycles: CycleResult;
  edgeCounts: Record<string, number>;
}

export function analyzeGraph(graph: GraphData): GraphAnalysis {
  const binding = getBinding();
  const json = JSON.stringify(graph);

  const centralityJson = binding.calculateCentrality(json);
  const cyclesJson = binding.detectCycles(json);

  const edgeCounts: Record<string, number> = {};
  graph.edges.forEach(e => {
    const t = e.edge_type || 'unknown';
    edgeCounts[t] = (edgeCounts[t] || 0) + 1;
  });

  return {
    centrality: JSON.parse(centralityJson) as CentralityResult,
    cycles: JSON.parse(cyclesJson) as CycleResult,
    edgeCounts
  };
}
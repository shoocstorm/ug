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

export type GraphNodeType = "File" | "Function" | "Class" | "Interface" | "Concept";
export type GraphEdgeType = "DependsOn" | "Calls" | "Extends" | "References" | "Contains";

export interface GraphNode {
  id: string;
  name: string;
  node_type: GraphNodeType;
  file: string | null;
  startLine: number | null;
  endLine: number | null;
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

export async function index(path: string): Promise<IndexResult> {
  const binding = getBinding();
  const result = binding.index(path);
  return JSON.parse(result) as IndexResult;
}

export async function indexWithCache(path: string, cachePath: string): Promise<IndexResult> {
  const binding = getBinding();
  const result = binding.indexWithCache(path, cachePath);
  return JSON.parse(result) as IndexResult;
}

export function indexSync(path: string): IndexResult {
  const binding = getBinding();
  const result = binding.index(path);
  return JSON.parse(result) as IndexResult;
}

export function indexWithCacheSync(path: string, cachePath: string): IndexResult {
  const binding = getBinding();
  const result = binding.indexWithCache(path, cachePath);
  return JSON.parse(result) as IndexResult;
}

export function buildGraph(indexResult: IndexResult): GraphData {
  const binding = getBinding();
  const json = JSON.stringify(indexResult);
  const result = binding.buildGraph(json);
  return JSON.parse(result) as GraphData;
}

export function kHopBfs(graph: GraphData, startNodeId: string, k: number): BfsResult {
  const binding = getBinding();
  const json = JSON.stringify(graph);
  const result = binding.kHopBfs(json, startNodeId, k);
  return JSON.parse(result) as BfsResult;
}
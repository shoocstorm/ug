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
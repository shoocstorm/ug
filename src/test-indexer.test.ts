import { describe, it, expect, beforeEach } from 'bun:test';
import { join } from 'path';
import { mkdtempSync, writeFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';

const { index, indexWithCache, buildGraph, kHopBfs } = require('./src/index.ts');

describe('UltraGraph-KB Indexer', () => {
  let testDir: string;

  beforeEach(() => {
    testDir = mkdtempSync(join(tmpdir(), 'kb-test-'));
  });

  it('should index an empty directory', async () => {
    const result = await index(testDir);
    expect(result.stats.totalFiles).toBe(0);
    expect(result.stats.totalSymbols).toBe(0);
  });

  it('should index TypeScript files', async () => {
    writeFileSync(join(testDir, 'test.ts'), `
export function hello(name: string): string {
  return 'Hello, ' + name;
}

export class Calculator {
  add(a: number, b: number): number {
    return a + b;
  }
}

export interface Config {
  name: string;
  value: number;
}
`);

    const result = await index(testDir);
    
    expect(result.stats.totalFiles).toBe(1);
    expect(result.stats.totalSymbols).toBe(3);
    expect(result.files[0].symbols).toContainEqual({
      id: 'fn:2:hello',
      name: 'hello',
      kind: 'function_declaration',
      file: expect.any(String),
      startLine: 2,
      endLine: 4,
      docstring: null,
    });
  });

  it('should index Python files', async () => {
    writeFileSync(join(testDir, 'test.py'), `
def greet(name: str) -> str:
    return f"Hello, {name}"

class Math:
    def add(self, a: int, b: int) -> int:
        return a + b
`);

    const result = await index(testDir);
    
    expect(result.stats.totalFiles).toBe(1);
    expect(result.stats.totalSymbols).toBe(2);
    expect(result.files[0].language).toBe('python');
  });

  it('should use incremental caching', async () => {
    writeFileSync(join(testDir, 'test.ts'), `export function test(): void {}`);

    const result1 = await indexWithCache(testDir, join(testDir, '.cache'));
    expect(result1.stats.cachedFiles).toBe(0);
    expect(result1.stats.totalFiles).toBe(1);

    const result2 = await indexWithCache(testDir, join(testDir, '.cache'));
    expect(result2.stats.cachedFiles).toBe(1);
    expect(result2.stats.totalFiles).toBe(0);
  });

  it('should ignore node_modules and .git', async () => {
    writeFileSync(join(testDir, 'test.ts'), `export function test(): void {}`);
    writeFileSync(join(testDir, 'node_modules/test.ts'), `export function ignored(): void {}`);
    writeFileSync(join(testDir, '.git/test.ts'), `export function ignored(): void {}`);

    const result = await index(testDir);
    
    expect(result.stats.totalFiles).toBe(1);
  });
});

describe('UltraGraph-KB Graph', () => {
  let testDir: string;

  beforeEach(() => {
    testDir = mkdtempSync(join(tmpdir(), 'kb-graph-'));
  });

  it('should build graph from index result', async () => {
    writeFileSync(join(testDir, 'test.ts'), `
export function hello(name: string): string {
  return 'Hello, ' + name;
}

export class Calculator {
  add(a: number, b: number): number {
    return a + b;
  }
}
`);

    const idx = await index(testDir);
    const graph = buildGraph(idx);
    
    expect(graph.nodes.length).toBe(3);
    expect(graph.edges.length).toBe(2);
    
    const fileNode = graph.nodes.find(n => n.node_type === 'File');
    expect(fileNode).toBeDefined();
    expect(fileNode?.name).toContain('test.ts');
    
    const funcNode = graph.nodes.find(n => n.node_type === 'Function');
    expect(funcNode?.name).toBe('hello');
    
    const classNode = graph.nodes.find(n => n.node_type === 'Class');
    expect(classNode?.name).toBe('Calculator');
  });

  it('should perform K-hop BFS from file node', async () => {
    writeFileSync(join(testDir, 'test.ts'), `
export function hello(): string { return 'hi'; }
export class Calc { }
`);

    const idx = await index(testDir);
    const graph = buildGraph(idx);
    
    const fileNodeId = graph.nodes.find(n => n.node_type === 'File')?.id;
    const bfs = kHopBfs(graph, fileNodeId!, 1);
    
    expect(bfs.nodes.length).toBe(3);
    expect(bfs.distances[fileNodeId!]).toBe(0);
    expect(bfs.edges.length).toBeGreaterThan(0);
  });

  it('should perform K-hop BFS from symbol node', async () => {
    writeFileSync(join(testDir, 'test.ts'), `
export function hello(): string { return 'hi'; }
export class Calc { }
`);

    const idx = await index(testDir);
    const graph = buildGraph(idx);
    
    const funcNodeId = graph.nodes.find(n => n.name === 'hello')?.id;
    const bfs = kHopBfs(graph, funcNodeId!, 2);
    
    expect(bfs.distances[funcNodeId!]).toBe(0);
  });

  it('should return empty result for invalid start node', async () => {
    writeFileSync(join(testDir, 'test.ts'), `export function test(): void {}`);

    const idx = await index(testDir);
    const graph = buildGraph(idx);
    const bfs = kHopBfs(graph, 'invalid-node-id', 2);
    
    expect(bfs.nodes.length).toBe(0);
    expect(bfs.edges.length).toBe(0);
  });

  it('should respect K parameter in BFS', async () => {
    writeFileSync(join(testDir, 'test.ts'), `
export function a(): void { b(); }
export function b(): void { c(); }
export function c(): void { }
`);

    const idx = await index(testDir);
    const graph = buildGraph(idx);
    
    const nodeA = graph.nodes.find(n => n.name === 'a')?.id;
    
    const bfs0 = kHopBfs(graph, nodeA!, 0);
    expect(bfs0.nodes.length).toBe(1);
    
    const bfs1 = kHopBfs(graph, nodeA!, 1);
    expect(bfs1.nodes.length).toBe(2);
    
    const bfs2 = kHopBfs(graph, nodeA!, 2);
    expect(bfs2.nodes.length).toBe(3);
  });
});
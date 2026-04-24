import { describe, it, expect, beforeEach } from 'bun:test';
import { join } from 'path';
import { mkdtempSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';

const { index, indexWithCache } = require('./src/index.ts');

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
#!/usr/bin/env node

import { join, dirname, resolve, basename } from 'node:path';
import { homedir } from 'node:os';
import {
  readFileSync, existsSync, writeFileSync, mkdirSync, copyFileSync, realpathSync,
  readdirSync, statSync, rmSync,
} from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { createInterface } from 'node:readline/promises';
import chalk from 'chalk';
import { z } from 'zod';
import { parseDocument } from 'yaml';
import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from '@modelcontextprotocol/sdk/types.js';

chalk.level = 2;

// Minimal `.env` loader (mirrors native's `dotenvy::dotenv()`): reads
// KEY=VALUE lines from a `.env` in cwd, skipping blank lines/comments.
// Real env vars always win — only fills in names not already set.
function loadDotEnv() {
  const path = join(process.cwd(), '.env');
  if (!existsSync(path)) return;
  const lines = readFileSync(path, 'utf-8').split('\n');
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const eq = trimmed.indexOf('=');
    if (eq === -1) continue;
    const key = trimmed.slice(0, eq).trim();
    let value = trimmed.slice(eq + 1).trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    if (key && !(key in process.env)) process.env[key] = value;
  }
}
loadDotEnv();

const __dirname = dirname(fileURLToPath(import.meta.url));
// Named nodeRequire (not `require`) so bundlers don't mistake this dynamic,
// computed-path load for a statically resolvable module and try to inline it.
const nodeRequire = createRequire(import.meta.url);

const ug = nodeRequire(join(dirname(__dirname), '.ug', 'ug.node'));

// ---------------------------------------------------------------------------
// Project-folder resolution for the ~/.ug/<project> data layout.
// Mirrors native/src/project.rs — keep the two in sync. All project.json
// reads/writes go through here so the metadata backend can later be
// swapped for the project's own OverGraph db.
// ---------------------------------------------------------------------------

const UG_VERSION = '0.1.4';

function ugHome() {
  const env = process.env.UG_HOME;
  if (env && env.trim()) return env;
  return join(homedir(), '.ug');
}

// Chars outside [A-Za-z0-9._-] become '-'; leading '.'/'-' stripped;
// capped at 64 chars; empty or './..' fall back to "default".
function sanitizeName(raw) {
  const mapped = String(raw).trim().replace(/[^A-Za-z0-9._-]/g, '-');
  const stripped = mapped.replace(/^[.-]+/, '').slice(0, 64);
  if (!stripped || stripped === '.' || stripped === '..') return 'default';
  return stripped;
}

function deriveProjectName(inputPath) {
  let canon;
  try {
    canon = realpathSync(resolve(inputPath || '.'));
  } catch {
    canon = resolve(inputPath || '.');
  }
  const base = basename(canon);
  return base ? sanitizeName(base) : 'default';
}

function projectDir(name) {
  return join(ugHome(), sanitizeName(name));
}

function metaPath(dir) {
  return join(dir, 'project.json');
}

function readProjectMeta(dir) {
  try {
    return JSON.parse(readFileSync(metaPath(dir), 'utf-8'));
  } catch {
    return null;
  }
}

// Writes project.json, preserving createdAt from any existing file.
function writeProjectMeta(dir, meta) {
  const now = Math.floor(Date.now() / 1000);
  const existing = readProjectMeta(dir);
  const out = {
    name: meta.name,
    repoRoot: meta.repoRoot || '',
    createdAt: existing && existing.createdAt ? existing.createdAt : now,
    updatedAt: now,
    nodes: meta.nodes || 0,
    edges: meta.edges || 0,
    ugVersion: meta.ugVersion || UG_VERSION,
  };
  mkdirSync(dir, { recursive: true });
  writeFileSync(metaPath(dir), JSON.stringify(out, null, 2));
  return out;
}

// Subdirs of ugHome() containing project.json or graph.json, sorted by
// updatedAt descending. Synthesizes metadata when project.json is missing.
function listProjects() {
  const root = ugHome();
  if (!existsSync(root)) return [];
  const out = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const dir = join(root, entry.name);
    const meta = readProjectMeta(dir);
    if (meta) {
      out.push({ dir, meta });
      continue;
    }
    const graph = join(dir, 'graph.json');
    if (existsSync(graph)) {
      let mtime = 0;
      try {
        mtime = Math.floor(statSync(graph).mtimeMs / 1000);
      } catch {}
      out.push({
        dir,
        meta: {
          name: entry.name, repoRoot: '', createdAt: mtime, updatedAt: mtime,
          nodes: 0, edges: 0, ugVersion: '',
        },
      });
    }
  }
  out.sort((a, b) => (b.meta.updatedAt || 0) - (a.meta.updatedAt || 0));
  return out;
}

// Project name for an invocation: -n/--name flag wins, else derived
// from the given input path's basename.
function resolveProjectName(args, inputPath) {
  const flagged = extractFlag(args, '-n') || extractFlag(args, '--name');
  if (flagged) return sanitizeName(flagged);
  return deriveProjectName(inputPath || '.');
}

function extractArg(args, shortFlag, longFlag, defaultValue) {
  const shortIdx = args.indexOf(shortFlag);
  const longIdx = args.indexOf(longFlag);
  const idx = shortIdx >= 0 ? shortIdx : longIdx;
  if (idx < 0 || idx + 1 >= args.length) return defaultValue;
  const parsed = parseInt(args[idx + 1], 10);
  return isNaN(parsed) ? defaultValue : parsed;
}

function extractFlag(args, flag) {
  const idx = args.indexOf(flag);
  if (idx < 0 || idx + 1 >= args.length) return null;
  return args[idx + 1];
}

function extractMultiFlags(args, flag) {
  const results = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] === flag && i + 1 < args.length) {
      results.push(args[i + 1]);
      i++;
    }
  }
  return results;
}

function parseEmbedderOptions(args) {
  const baseUrl = extractFlag(args, '--base-url') || extractFlag(args, '-b');
  const apiKey = extractFlag(args, '--api-key') || extractFlag(args, '-a');
  const model = extractFlag(args, '--model') || extractFlag(args, '-m');
  const dimRaw = extractFlag(args, '--embedding-dim');
  if (!baseUrl && !apiKey && !model && !dimRaw) return null;
  const opts = {};
  if (baseUrl) opts.baseUrl = baseUrl;
  if (apiKey) opts.apiKey = apiKey;
  if (model) opts.model = model;
  if (dimRaw) {
    const dim = Number.parseInt(dimRaw, 10);
    if (!Number.isFinite(dim) || dim <= 0) {
      throw new Error(`--embedding-dim must be a positive integer, got: ${dimRaw}`);
    }
    opts.embeddingDim = dim;
  }
  return opts;
}

// ---------------------------------------------------------------------------
// MCP server — exposes UltraGraph's GraphRAG knowledge-base tools over
// stdio. Configuration via env vars:
//   UG_DB_PATH         - OverGraph directory (overrides everything)
//   UG_PROJECT         - project name under ~/.ug (or $UG_HOME); the db is
//                        ~/.ug/<project>/ugdb and the repo root comes from
//                        that project's project.json
//   UG_HOME            - override the ~/.ug root
//   UG_REPO_ROOT       - root for resolving snippet file paths (default:
//                        project.json repoRoot, else cwd)
//   (no env)           - ~/.ug/<cwd-basename>/ugdb if it exists, else ./ugdb
//   UG_EMBED_BASE_URL  - override embedding endpoint base URL
//   UG_EMBED_API_KEY   - override embedding API key
//   UG_EMBED_MODEL     - override embedding model name
//   (embed settings persisted with `ug config set embed.*` in
//    $UG_HOME/config.json are the fallback when the env vars are unset)
//
// Multi-destination (default backend: overgraph):
//   UG_DEST            - "overgraph" (default) or "neo4j"
//   UG_NEO4J_URI       - Neo4j Bolt URI (e.g. neo4j://localhost:7687)
//   UG_NEO4J_USER      - Neo4j username (default: neo4j)
//   UG_NEO4J_PASSWORD  - Neo4j password (required when UG_DEST=neo4j)
//   UG_NEO4J_DATABASE  - optional database name
// ---------------------------------------------------------------------------

// DB + repo-root resolution, most explicit first: UG_DB_PATH →
// UG_PROJECT → the ~/.ug project matching the cwd → legacy ./ugdb.
// UG_REPO_ROOT always wins over project.json's repoRoot.
function resolveDbAndRoot() {
  if (process.env.UG_DB_PATH) {
    return {
      dbPath: process.env.UG_DB_PATH,
      repoRoot: process.env.UG_REPO_ROOT || process.cwd(),
    };
  }
  const fromProject = (dir) => {
    const meta = readProjectMeta(dir);
    return {
      dbPath: join(dir, 'ugdb'),
      repoRoot: process.env.UG_REPO_ROOT || meta?.repoRoot || process.cwd(),
    };
  };
  if (process.env.UG_PROJECT) {
    return fromProject(projectDir(process.env.UG_PROJECT));
  }
  const derived = projectDir(deriveProjectName('.'));
  if (existsSync(join(derived, 'ugdb'))) {
    return fromProject(derived);
  }
  return {
    dbPath: './ugdb',
    repoRoot: process.env.UG_REPO_ROOT || process.cwd(),
  };
}

// Long snippets blow up the prompt. Cap each item but indicate truncation
// so the agent knows it can re-fetch the full slice via the file path.
const SNIPPET_PREVIEW_CHARS = 1200;

// Persisted user config ($UG_HOME/config.json, written by `ug config
// set`). Mirrors native/src/config.rs — env vars outrank it, it
// outranks built-in defaults. Malformed/missing file → {}.
function userConfig() {
  try {
    return JSON.parse(readFileSync(join(ugHome(), 'config.json'), 'utf-8'));
  } catch {
    return {};
  }
}

function embedderOptionsJson() {
  const saved = userConfig().embed || {};
  const o = {};
  const baseUrl = process.env.UG_EMBED_BASE_URL || saved.baseUrl;
  const apiKey = process.env.UG_EMBED_API_KEY || saved.apiKey;
  const model = process.env.UG_EMBED_MODEL || saved.model;
  if (baseUrl) o.baseUrl = baseUrl;
  if (apiKey) o.apiKey = apiKey;
  if (model) o.model = model;
  if (Number.isFinite(saved.dim) && saved.dim > 0) o.embeddingDim = saved.dim;
  return Object.keys(o).length ? JSON.stringify(o) : null;
}

// Build the destOptions JSON from env vars. Returns null when the
// caller wants the default (OverGraph at DB_PATH). When UG_DEST=neo4j
// is set, the URI + password are required and we throw early so the
// MCP transport surfaces a clean error instead of failing per-call.
function destOptionsJson() {
  const dest = (process.env.UG_DEST || 'overgraph').toLowerCase();
  if (dest === 'overgraph' || dest === 'og') return null;
  if (dest === 'neo4j' || dest === 'neo') {
    const uri = process.env.UG_NEO4J_URI;
    const password = process.env.UG_NEO4J_PASSWORD;
    if (!uri) {
      throw new Error('UG_DEST=neo4j requires UG_NEO4J_URI');
    }
    if (!password) {
      throw new Error('UG_DEST=neo4j requires UG_NEO4J_PASSWORD');
    }
    return JSON.stringify({
      kind: 'neo4j',
      uri,
      user: process.env.UG_NEO4J_USER || 'neo4j',
      password,
      database: process.env.UG_NEO4J_DATABASE || null,
    });
  }
  throw new Error(`Unknown UG_DEST value: ${dest} (expected: overgraph, neo4j)`);
}

const SearchKbInput = z.object({
  query: z.string().min(1),
  k: z.number().int().min(1).max(50).optional(),
  hops: z.number().int().min(0).max(5).optional(),
  edgeTypes: z.array(z.string()).optional(),
  direction: z.enum(['outbound', 'inbound', 'both']).optional(),
  maxChars: z.number().int().min(100).max(200000).optional(),
  mmrLambda: z.number().min(0).max(1).optional(),
  whereClause: z.string().optional(),
  includeSnippets: z.boolean().optional(),
  strategy: z.enum(['ppr', 'mmr']).optional(),
  pprRestartProb: z.number().min(0.01).max(0.99).optional(),
  pprMaxIter: z.number().int().min(1).max(200).optional(),
  pprSeedPool: z.number().int().min(1).max(200).optional(),
  pprEdgeWeights: z.record(z.string(), z.number().min(0)).optional(),
});

const SemanticSearchInput = z.object({
  query: z.string().min(1),
  k: z.number().int().min(1).max(100).optional(),
  whereClause: z.string().optional(),
});

// Batch cap for tools accepting multiple ids/names/files: keeps one
// response from blowing the agent's context while still collapsing a
// handful of lookups into one round trip.
const MAX_BATCH = 10;

// One-or-many: accept "x" or ["x", "y"], normalize to a capped array.
const oneOrMany = z
  .union([z.string().min(1), z.array(z.string().min(1)).min(1).max(MAX_BATCH)])
  .transform((v) => (Array.isArray(v) ? v : [v]));

// Canonical id parameter is `nodeId` (matching get_code / find_usages);
// `startNodeIds` is the legacy spelling, still accepted.
const TraverseInput = z
  .object({
    nodeId: oneOrMany.optional(),
    startNodeIds: z.array(z.string()).min(1).optional(),
    hops: z.number().int().min(1).max(5).default(2),
    edgeTypes: z.array(z.string()).optional(),
    direction: z.enum(['outbound', 'inbound', 'both']).default('outbound'),
  })
  .refine((v) => v.nodeId || v.startNodeIds, { message: 'Pass nodeId (one id or an array).' })
  .transform((v) => ({ ...v, nodeId: v.nodeId ?? v.startNodeIds }));

const FindUsagesInput = z.object({
  nodeId: oneOrMany,
  hops: z.number().int().min(1).max(3).default(1),
  edgeTypes: z.array(z.string()).optional(),
});

const FindSymbolsInput = z.object({
  nodeId: oneOrMany.optional(),
  name: oneOrMany.optional(),
  nodeTypes: z.array(z.string()).optional(),
  filePrefix: z.string().optional(),
  limit: z.number().int().min(1).max(100).optional(),
  includeDocs: z.boolean().optional(),
}).refine((v) => v.nodeId || v.name, {
  message: 'Pass nodeId (one id or an array) for direct lookup, or name (one or an array) for name search.',
});

const FileOutlineInput = z.object({
  nodeId: oneOrMany.optional(),
  file: oneOrMany.optional(),
}).refine((v) => v.nodeId || v.file, {
  message: 'Pass nodeId (one id or an array) for direct lookup, or file (one or an array) for file path lookup.',
});

const GetCodeInput = z
  .object({
    nodeId: oneOrMany.optional(),
    file: z.string().min(1).optional(),
    startLine: z.number().int().min(1).optional(),
    endLine: z.number().int().min(1).optional(),
    maxChars: z.number().int().min(200).max(200000).optional(),
  })
  .refine((v) => v.nodeId || v.file, {
    message: 'Pass nodeId (one id or an array), or file (optionally with startLine/endLine).',
  });

const ShortestPathInput = z.object({
  sourceId: z.string().min(1),
  targetId: z.string().min(1),
});

// Tool registry — JSON Schema is what MCP wants on the wire; we keep
// zod for runtime validation and a hand-written JSON Schema for the
// tool list response. Avoiding a zod-to-json-schema dep keeps the
// install footprint tiny.
const MCP_TOOLS = [
  {
    name: 'search',
    description:
      "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse / find_usages. " +
      "Trigger phrases include: 'how does X work', 'where is X', 'what is X', 'find / show me code for X', 'explain X', 'is there a function that...', 'how is X implemented', 'before I change X look up...', 'context on X', or any question whose answer likely lives in the repo. Prefer calling this once with a focused natural-language query over guessing file paths. " +
      'Internals: RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance. Pass strategy=\'mmr\' for the legacy diversity-first BFS+MMR cascade.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description:
            "Natural-language query. Be specific — name the concept, function, or behavior you're after (e.g. 'how does the embedder probe its dim' beats 'embedder').",
        },
        k: {
          type: 'integer',
          minimum: 1,
          maximum: 50,
          description:
            'How many context items to return (default 8). Bump to 15-20 when surveying a subsystem; keep 5-8 when answering a focused question.',
        },
        hops: {
          type: 'integer',
          minimum: 0,
          maximum: 5,
          description:
            'MMR-only: graph expansion radius from each seed (default 2). Ignored under PPR.',
        },
        edgeTypes: {
          type: 'array',
          items: { type: 'string' },
          description:
            'Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. Leave unset for the default mix.',
        },
        direction: {
          type: 'string',
          enum: ['outbound', 'inbound', 'both'],
          description:
            "Edge direction during the walk (default 'both'). Use 'inbound' when you care about who depends on the seed; 'outbound' for what the seed depends on.",
        },
        maxChars: {
          type: 'integer',
          minimum: 100,
          maximum: 200000,
          description:
            'Approximate character budget for assembled context (default ~16k). Lower it when you only need a sketch.',
        },
        mmrLambda: {
          type: 'number',
          minimum: 0,
          maximum: 1,
          description:
            "MMR balance (only when strategy='mmr'): 1 = max relevance, 0 = max diversity (default 0.6).",
        },
        whereClause: {
          type: 'string',
          description:
            "Optional SQL WHERE applied during seed search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\".",
        },
        includeSnippets: {
          type: 'boolean',
          description:
            'Read source slice for each item (default true). Set false when you only need IDs and locations for a follow-up traversal.',
        },
        strategy: {
          type: 'string',
          enum: ['ppr', 'mmr'],
          description:
            "Ranking strategy. 'ppr' (default) = Personalized PageRank seeded by RRF — best general-purpose. 'mmr' = legacy seed+BFS+MMR, prefer when you specifically want diversity over centrality.",
        },
        pprRestartProb: {
          type: 'number',
          minimum: 0.01,
          maximum: 0.99,
          description:
            'PPR teleport probability (default 0.15). Higher = stay closer to seeds; lower = let centrality dominate.',
        },
        pprMaxIter: {
          type: 'integer',
          minimum: 1,
          maximum: 200,
          description: 'PPR power-iteration cap (default 30).',
        },
        pprSeedPool: {
          type: 'integer',
          minimum: 1,
          maximum: 200,
          description:
            'How many RRF hits feed the personalization vector (default 16). Larger = more robust to a noisy top hit.',
        },
        pprEdgeWeights: {
          type: 'object',
          additionalProperties: { type: 'number', minimum: 0 },
          description:
            'Override edge-type weights, e.g. { calls: 1.0, imports: 0.7, contains: 0.3 }. Keys are case-insensitive.',
        },
      },
      required: ['query'],
    },
  },
  {
    name: 'semantic_search',
    description:
      'Lightweight pure-vector lookup over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance. Use this when search would be overkill: ' +
      "(a) quick disambiguation ('which node is the user talking about?'), " +
      '(b) candidate generation before a deeper traverse, ' +
      '(c) filtered lookups via whereClause (e.g. only Functions in a given folder). ' +
      'Cheaper and faster than search. Switch to search when you need actual code snippets or graph-aware ranking.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Natural-language query.',
        },
        k: {
          type: 'integer',
          minimum: 1,
          maximum: 100,
          description: 'How many candidate nodes to return (default 10).',
        },
        whereClause: {
          type: 'string',
          description:
            "Optional SQL WHERE filter applied to the vector search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\", \"node_type IN ('Class','Interface')\".",
        },
      },
      required: ['query'],
    },
  },
  {
    name: 'traverse',
    description:
      'Walk the graph N hops from given seed node ids. The natural follow-up to search / semantic_search: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. Filters by edge type and direction. ' +
      "Use 'outbound' to see what the seed depends on; 'inbound' to see who depends on the seed. Output groups edges by type so the structure is easy to scan.",
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            'Seed node id(s) — one id or an array of up to 10, typically copied from a prior search / find_symbols result. (`startNodeIds` is the deprecated legacy name for the same parameter.)',
        },
        hops: {
          type: 'integer',
          minimum: 1,
          maximum: 5,
          description: 'Hop radius (default 2). Use 1 for direct neighbors only.',
        },
        edgeTypes: {
          type: 'array',
          items: { type: 'string' },
          description:
            'Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. See graph_schema for what this graph has.',
        },
        direction: {
          type: 'string',
          enum: ['outbound', 'inbound', 'both'],
          description:
            "Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on; 'both' = either.",
        },
      },
      required: ['nodeId'],
    },
  },
  {
    name: 'find_usages',
    description:
      "Find inbound references to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node. Convenience wrapper over traverse with direction='inbound' and a sensible default edge-type set ['calls', 'references', 'imports', 'extends', 'implements']. " +
      "Use this when the user asks 'who uses X', 'what calls X', 'where is X imported', 'what would break if I change X', or before a refactor. Batch-friendly: pass an ARRAY of up to 10 nodeIds to check them all in one call (e.g. every symbol a refactor touches).",
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            'The node id (or an array of up to 10 ids — batch related lookups into ONE call instead of several) to look up usages for. Get ids from search or find_symbols.',
        },
        hops: {
          type: 'integer',
          minimum: 1,
          maximum: 3,
          description:
            'How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages.',
        },
        edgeTypes: {
          type: 'array',
          items: { type: 'string' },
          description:
            "Override the default ['calls', 'references', 'imports', 'extends', 'implements'] set if you only care about a subset (e.g. ['calls']).",
        },
      },
      required: ['nodeId'],
    },
  },
  {
    name: 'find_symbols',
    description:
      'EXACT-NAME symbol lookup — no embeddings, no fuzziness beyond substring. Use this instead of search whenever you already know (part of) an identifier: a function, class, interface, or file the user named, an id you saw in a stack trace, a symbol you are about to edit. ' +
      'Direct nodeId lookup is also supported: if you already have a nodeId from a prior search, pass it for O(1) direct access instead of re-searching. ' +
      'Matches case-insensitively against node names, ranked exact > prefix > substring. Returns id/type/file:line for each hit — feed the id straight into get_code (source), find_usages (callers), or traverse (dependencies). Cheaper and more precise than vector search for known names; fall back to search when you only know the concept, not the name. Batch-friendly: pass an ARRAY of up to 10 names/nodeIds to resolve them all in one call. ' +
      'Set includeDocs to also match docstring text — a keyword scan that finds symbols described by a word they do not contain in their name. Docstring hits rank below every name hit.',
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            "Direct node id lookup — O(1) access when you already have the id from a prior search. Use instead of 'name' to skip the search step.",
        },
        name: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            "Identifier to look up, e.g. 'resolveDbAndRoot' or a fragment like 'resolve'. Pass an array of up to 10 names to resolve several symbols in ONE call (e.g. every function you're about to edit).",
        },
        nodeTypes: {
          type: 'array',
          items: { type: 'string' },
          description: "Restrict to node types (case-insensitive). Common: Function, Class, Interface, File, Concept.",
        },
        filePrefix: {
          type: 'string',
          description: "Only symbols whose file path starts with this repo-relative prefix, e.g. 'src/auth/'.",
        },
        limit: {
          type: 'integer',
          minimum: 1,
          maximum: 100,
          description: 'Max hits to return (default 20).',
        },
        includeDocs: {
          type: 'boolean',
          description:
            'Also match docstrings, not just names (default false). Use when the concept may be described in prose rather than named — e.g. "cache invalidation" when the function is called `drop_stale`. Docstring hits rank below all name hits.',
        },
      },
    },
  },
  {
    name: 'file_outline',
    description:
      "List every indexed symbol in one file, in line order — a structural table of contents. Use before opening or editing a file to know what's in it, or to map a file the user mentioned. " +
      "Direct nodeId lookup is also supported: if you already have a File node id from a prior search, pass it for O(1) direct access. " +
      "Accepts a repo-relative path or a unique suffix (e.g. just the basename), a File node id ('file:native/src/main.rs'), or an ARRAY of up to 10 files/ids to outline them all in one call. Returns name/type/line-range/id per symbol; ids feed get_code / find_usages / traverse.",
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            "Direct File node id lookup — O(1) access when you already have the File node id from a prior search. Use instead of 'file' to skip the file lookup step.",
        },
        file: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            "Repo-relative path ('native/src/main.rs'), unique suffix ('main.rs'), or a File node id ('file:native/src/main.rs'). Pass an array of up to 10 files to outline several in ONE call.",
        },
      },
    },
  },
  {
    name: 'get_code',
    description:
      'Read the full source for a node id or an arbitrary file/line range from the indexed repo. THE follow-up to every other tool: search previews truncate at ~1200 chars and traverse/find_usages return no code at all — call this to see the real implementation before reasoning about it or editing it. ' +
      'Pass a nodeId from any prior result — or an ARRAY of up to 10 ids to read several symbols in one call instead of several calls — or file (+ optional startLine/endLine) for raw ranges. Reads from the indexed repo root, so it works even when you have no direct file access (e.g. Claude Desktop).',
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          oneOf: [
            { type: 'string' },
            { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 10 },
          ],
          description:
            "Node id from find_symbols / search / file_outline / traverse — reads exactly that symbol's line range. Pass an array of up to 10 ids to read several symbols in ONE call (per-symbol maxChars still applies).",
        },
        file: {
          type: 'string',
          description: 'Repo-relative file path. Used when nodeId is not given (or to read outside any symbol).',
        },
        startLine: { type: 'integer', minimum: 1, description: '1-based first line (with file; default 1).' },
        endLine: { type: 'integer', minimum: 1, description: '1-based last line, inclusive (with file; default EOF).' },
        maxChars: {
          type: 'integer',
          minimum: 200,
          maximum: 200000,
          description: 'Character cap on returned code (default 20000). Output notes truncation.',
        },
      },
    },
  },
  {
    name: 'project_overview',
    description:
      "Orient yourself in the indexed codebase in one call: repo root, node/edge counts by type, the biggest files by symbol count, and the most depended-upon symbols (highest inbound degree, ignoring folder-containment edges). Call this FIRST in a new session, or when the user asks 'what is this project', 'how is it structured', 'where should I start'. The listed hotspot ids are good seeds for traverse / get_code.",
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'shortest_path',
    description:
      "How are two symbols connected? Finds the shortest directed edge path between two node ids — use it to answer 'does A reach B', 'how does the request get from the route to the db call', or to check whether an edit to A can affect B. Edges are directed (imports/calls/contains flow source→target); if no forward path exists the reverse direction is tried and labeled as such. Get ids from find_symbols or search first.",
    inputSchema: {
      type: 'object',
      properties: {
        sourceId: { type: 'string', description: 'Start node id.' },
        targetId: { type: 'string', description: 'End node id.' },
      },
      required: ['sourceId', 'targetId'],
    },
  },
  {
    name: 'graph_schema',
    description:
      "Node & edge types actually present in this project's graph, with counts and what each edge type connects (e.g. Calls: Function→Function). Call this before passing edgeTypes to find_usages / traverse or nodeTypes to find_symbols — filtering on a type the graph doesn't contain silently returns nothing. Also lists the full edge-type vocabulary indexers can emit. Edges are directed (Calls A→B means A calls B); Contains is pure structure (Folder→File→Symbol), exclude it when you mean 'depends on'.",
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'list_projects',
    description:
      "List every indexed project on this machine (name, repo path, graph size). Every other tool accepts project: '<name>' to query one of these instead of the current project — use this to work across repos (e.g. a service in one repo calling an API defined in another) or when the user mentions a codebase that isn't the current directory.",
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'reindex',
    description:
      'Re-run the index → graph → embed pipeline for the current (or named) project. Call it when tool outputs carry an "Index may be stale" warning, when the user says results look outdated, or after you (or they) changed many files. Incremental — unchanged files are skipped via content hashes — but embedding changed nodes needs the embedding backend, so it can take a while on big diffs; the structural tools are refreshed even if embedding fails.',
    inputSchema: { type: 'object', properties: {} },
  },
];

// `ping_embedder` used to be listed here. It's an operator diagnostic, not
// something an agent should spend a tool call on: search / semantic_search
// already surface the upstream embedding error directly, so a pre-flight
// probe only adds a round trip. Still reachable as `ug doctor` and via
// `ug mcp call ping_embedder` for debugging.

// Every tool (list_projects aside) accepts an optional `project` to target
// another indexed project — one server instance serves all repos on the
// machine. Injected here rather than repeated in each definition.
for (const t of MCP_TOOLS) {
  if (t.name === 'list_projects') continue;
  t.inputSchema.properties.project = {
    type: 'string',
    description:
      "Optional: name of another indexed project to query (see list_projects). Default: the project this server was started for.",
  };
}

// ---------------------------------------------------------------------------
// Formatters — these are what the agent actually reads. Put enough metadata
// in the header that the agent can copy ids straight into a follow-up call,
// and end with a short "next actions" hint so it knows what to do next.
// ---------------------------------------------------------------------------

function previewSnippet(snippet) {
  if (!snippet) return null;
  const trimmed = snippet.trimEnd();
  if (trimmed.length <= SNIPPET_PREVIEW_CHARS) return { text: trimmed, truncated: false };
  return {
    text: trimmed.slice(0, SNIPPET_PREVIEW_CHARS),
    truncated: true,
    omitted: trimmed.length - SNIPPET_PREVIEW_CHARS,
  };
}

function summarizeNodeTypes(items) {
  const counts = new Map();
  for (const it of items) {
    counts.set(it.node_type, (counts.get(it.node_type) ?? 0) + 1);
  }
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .map(([t, n]) => `${t}×${n}`)
    .join(', ');
}

function formatRankedContext(ctx) {
  const lines = [];
  const items = ctx.items ?? [];

  lines.push(`# Knowledge-base results for: ${ctx.query}`);
  const meta = [`items=${items.length}`, `chars=${ctx.total_chars}`];
  if (ctx.seed_id) meta.push(`seed=${ctx.seed_id}`);
  if (items.length) meta.push(`types=[${summarizeNodeTypes(items)}]`);
  lines.push(meta.join('  •  '));
  lines.push('');

  if (!items.length) {
    lines.push('No matches. Try:');
    lines.push('- a broader query (drop qualifiers)');
    lines.push('- semantic_search for a pure-vector pass with whereClause filters');
    lines.push('- ping_embedder to confirm the embedding endpoint is up');
    return lines.join('\n');
  }

  items.forEach((it, idx) => {
    const loc = it.file ? `${it.file}:${it.start_line}-${it.end_line}` : '(no file)';
    const score = typeof it.distance === 'number' ? it.distance.toFixed(3) : '?';
    lines.push(
      `## [${idx + 1}] ${it.node_type} ${it.name}`,
    );
    lines.push(
      `- id: \`${it.id}\``,
    );
    lines.push(
      `- loc: ${loc}`,
    );
    lines.push(
      `- hop=${it.hop}  •  score=${score}`,
    );
    if (it.description) lines.push(`- desc: ${it.description}`);
    const snip = previewSnippet(it.snippet);
    if (snip) {
      lines.push('```');
      lines.push(snip.text);
      lines.push('```');
      if (snip.truncated) {
        lines.push(
          `(snippet truncated — ${snip.omitted} more chars; call get_code with id \`${it.id}\` for the full source)`,
        );
      }
    }
    lines.push('');
  });

  // Drill-down hints. The agent has the tool list, but spelling out a
  // ready-to-paste call shaves a step off the loop.
  const topId = items[0].id;
  lines.push('---');
  lines.push('Drill-down hints:');
  lines.push(`- Walk neighbors:  traverse({ nodeId: "${topId}", hops: 1 })`);
  lines.push(`- Find callers:    find_usages({ nodeId: "${topId}" })`);
  lines.push(
    `- Narrow search:   search({ query: "...", whereClause: "node_type = 'Function'" })`,
  );
  lines.push(
    `- Read full file:  use the loc above (file:start-end) with your file-read tool`,
  );

  return lines.join('\n');
}

function formatSemanticHits(query, hits) {
  const lines = [];
  lines.push(`# Semantic search for: ${query}`);
  const meta = [`hits=${hits.length}`];
  if (hits.length) meta.push(`types=[${summarizeNodeTypes(hits)}]`);
  lines.push(meta.join('  •  '));
  lines.push('');

  if (!hits.length) {
    lines.push('No matches. Loosen the whereClause or try search for graph-aware ranking.');
    return lines.join('\n');
  }

  hits.forEach((h, idx) => {
    const loc = h.file ? `${h.file}:${h.start_line}-${h.end_line}` : '(no file)';
    const score = typeof h.distance === 'number' ? h.distance.toFixed(3) : '?';
    lines.push(`[${idx + 1}] ${h.node_type} ${h.name}  •  id=\`${h.id}\`  •  dist=${score}`);
    lines.push(`    ${loc}`);
    if (h.description) lines.push(`    ${h.description}`);
  });

  lines.push('');
  lines.push(
    `Next: search({ query: "${query}" }) for graph-ranked snippets, or traverse({ nodeId: "${hits[0].id}" }) to expand.`,
  );
  return lines.join('\n');
}

function formatTraversal(traversal, header) {
  const nodes = traversal.nodes ?? [];
  const edges = traversal.edges ?? [];
  const lines = [];
  lines.push(`# ${header}`);
  lines.push(`nodes=${nodes.length}  •  edges=${edges.length}`);
  lines.push('');

  if (!nodes.length) {
    lines.push('Empty neighborhood — the seed may be isolated or filters were too tight.');
    return lines.join('\n');
  }

  // Group nodes by hop distance so the agent sees the seed first, then
  // 1-hop neighbors, then 2-hop, etc.
  const byHop = new Map();
  for (const n of nodes) {
    const d = n.distance ?? 0;
    if (!byHop.has(d)) byHop.set(d, []);
    byHop.get(d).push(n);
  }
  const hops = [...byHop.keys()].sort((a, b) => a - b);
  for (const h of hops) {
    lines.push(`## hop=${h}  (${byHop.get(h).length} nodes)`);
    for (const n of byHop.get(h)) {
      const loc = n.file ? `  •  ${n.file}` : '';
      lines.push(`- ${n.node_type} ${n.name}  \`${n.id}\`${loc}`);
    }
    lines.push('');
  }

  // Group edges by type for a readable structural view.
  if (edges.length) {
    const byType = new Map();
    for (const e of edges) {
      const t = e.edge_type || '(unknown)';
      if (!byType.has(t)) byType.set(t, []);
      byType.get(t).push(e);
    }
    lines.push('## edges by type');
    for (const [t, es] of byType) {
      lines.push(`- ${t}: ${es.length}`);
      // Show up to 8 examples per type — enough for the agent to spot
      // the pattern without flooding the prompt.
      for (const e of es.slice(0, 8)) {
        lines.push(`  - ${e.source}  →  ${e.target}`);
      }
      if (es.length > 8) lines.push(`  - … and ${es.length - 8} more`);
    }
    lines.push('');
  }

  lines.push('Drill-down hints:');
  lines.push('- Pick an interesting node id above and call traverse again to keep walking.');
  lines.push('- Call get_code with a node id to read its actual source.');
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// Graph-file-backed tools (find_symbols / file_outline / get_code /
// project_overview / shortest_path). These run off graph.json — the sibling
// of the ugdb dir — instead of the vector db: no embeddings involved, so
// they stay exact and cheap. Loaded lazily once per server process.
// ---------------------------------------------------------------------------

// Per-call project resolution: every tool accepts an optional `project`
// arg naming another indexed project under ~/.ug; without it the server's
// startup resolution (UG_PROJECT / UG_DB_PATH / cwd) applies. Contexts and
// graphs are cached per project for the life of the server process.
const projectCtxCache = new Map();
function projectCtx(project) {
  const key = project || '';
  if (projectCtxCache.has(key)) return projectCtxCache.get(key);
  let ctx;
  if (project) {
    const dir = projectDir(project);
    if (!existsSync(join(dir, 'ugdb')) && !existsSync(join(dir, 'graph.json'))) {
      throw new Error(`No indexed project '${project}' under ${ugHome()} — call list_projects to see what exists.`);
    }
    const meta = readProjectMeta(dir);
    ctx = { dbPath: join(dir, 'ugdb'), repoRoot: meta?.repoRoot || process.cwd() };
  } else {
    ctx = resolveDbAndRoot();
  }
  projectCtxCache.set(key, ctx);
  return ctx;
}

// graph.json sits next to the project's ugdb.
function graphPathFor(dbPath) {
  return join(dirname(resolve(dbPath)), 'graph.json');
}

// One call into the Rust agent-tool core (native/src/agent_tools.rs). Every
// graph-backed MCP tool goes through here, so MCP output is produced by the
// same code as `ug <tool>` instead of a parallel JS implementation that can
// drift. Params are passed through as-is: the Rust structs accept both the
// canonical snake_case names and these legacy camelCase spellings.
function agentTool(name, dbPath, repoRoot, params) {
  return ug.agentTool(
    name,
    graphPathFor(dbPath),
    repoRoot,
    JSON.stringify(params ?? {}),
    'markdown',
  );
}

const graphCaches = new Map();
function loadGraph(dbPath) {
  if (graphCaches.has(dbPath)) return graphCaches.get(dbPath);
  const path = join(dirname(resolve(dbPath)), 'graph.json');
  if (!existsSync(path)) {
    throw new Error(`graph.json not found at ${path} — run \`ug gen\` for this project first.`);
  }
  const raw = readFileSync(path, 'utf-8');
  const graph = JSON.parse(raw);
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  const cache = { raw, graph, byId, path };
  graphCaches.set(dbPath, cache);
  return cache;
}

// Index-freshness probe: graph.json's mtime vs the current mtimes of the
// files it indexed. One stat per indexed file, computed once per project
// per server process. Brand-new files aren't visible (that would need a
// full walk); changed + deleted indexed files are enough to warn usefully.
const stalenessCache = new Map();
function indexStaleness(dbPath, repoRoot) {
  if (stalenessCache.has(dbPath)) return stalenessCache.get(dbPath);
  let result = null;
  try {
    const cache = loadGraph(dbPath);
    const builtAt = statSync(cache.path).mtimeMs;
    const files = new Set();
    for (const n of cache.graph.nodes) {
      if (n.file && n.node_type !== 'Folder') files.add(n.file);
    }
    let changed = 0;
    let missing = 0;
    for (const f of files) {
      try {
        if (statSync(join(repoRoot, f)).mtimeMs > builtAt) changed += 1;
      } catch {
        missing += 1;
      }
    }
    result = { builtAt, files: files.size, changed, missing };
  } catch {
    // No graph yet — the tool that needed it raises its own error.
  }
  stalenessCache.set(dbPath, result);
  return result;
}

// Appended to tool outputs so agents don't silently trust an outdated
// index — confidently stale context is worse than none.
function stalenessNote(dbPath, repoRoot) {
  const s = indexStaleness(dbPath, repoRoot);
  if (!s || (s.changed === 0 && s.missing === 0)) return '';
  const days = Math.floor((Date.now() - s.builtAt) / 86400000);
  const bits = [];
  if (s.changed) bits.push(`${s.changed} changed`);
  if (s.missing) bits.push(`${s.missing} deleted`);
  const age = days > 0 ? ` (index built ${days} day(s) ago)` : '';
  return `\n\n⚠ Index may be stale: ${bits.join(', ')} of ${s.files} indexed files since the last index${age}. Call the reindex tool to refresh.`;
}

function invalidateProjectCaches(dbPath) {
  graphCaches.delete(dbPath);
  stalenessCache.delete(dbPath);
}

function listProjectsInfo() {
  const root = ugHome();
  const out = [];
  let entries = [];
  try {
    entries = readdirSync(root);
  } catch {
    return out;
  }
  for (const name of entries) {
    const dir = join(root, name);
    try {
      if (!statSync(dir).isDirectory()) continue;
    } catch {
      continue;
    }
    if (!existsSync(join(dir, 'ugdb')) && !existsSync(join(dir, 'graph.json'))) continue;
    const meta = readProjectMeta(dir);
    out.push({
      name,
      repoRoot: meta?.repoRoot ?? '(unknown)',
      nodes: meta?.nodes,
      edges: meta?.edges,
    });
  }
  return out;
}

function formatProjectList(projects, currentRepoRoot) {
  if (!projects.length) {
    return `No indexed projects under ${ugHome()} — run \`ug gen\` in a repo first.`;
  }
  const lines = [`# Indexed projects (${projects.length})`, ''];
  for (const p of projects) {
    const here = p.repoRoot === currentRepoRoot ? '  ← current' : '';
    lines.push(`- **${p.name}**  ${p.repoRoot}  (${p.nodes ?? '?'} nodes, ${p.edges ?? '?'} edges)${here}`);
  }
  lines.push('');
  lines.push("Pass project: '<name>' to any tool to query that project instead of the current one.");
  return lines.join('\n');
}

// Quiet re-run of the gen pipeline (index → graph → ingest) for the MCP
// reindex tool — no console output (stdout belongs to the protocol), blake3
// cache in the project dir keeps repeat runs cheap. Ingest failure (embedder
// down) is reported but doesn't fail the call: the graph-backed tools are
// already fresh at that point.
async function regenerateProject(dbPath, repoRoot) {
  if (!existsSync(repoRoot)) {
    throw new Error(`Repo root ${repoRoot} no longer exists — re-run \`ug gen -i <path>\` manually.`);
  }
  const outputDir = dirname(resolve(dbPath));
  mkdirSync(outputDir, { recursive: true });
  const indexJson = ug.indexWithCache(repoRoot, outputDir);
  const graph = ug.buildGraph(indexJson);
  writeFileSync(join(outputDir, 'graph.json'), graph);
  writeFileSync(join(outputDir, 'indexed-tree.json'), indexJson);
  const gd = JSON.parse(graph);
  const meta = readProjectMeta(outputDir);
  writeProjectMeta(outputDir, {
    name: meta?.name || basename(outputDir),
    repoRoot,
    nodes: gd.nodes?.length ?? 0,
    edges: gd.edges?.length ?? 0,
  });
  let ingestMsg;
  try {
    const stats = JSON.parse(await ug.dbIngest(graph, dbPath, embedderOptionsJson(), destOptionsJson()));
    ingestMsg = `db ingest: ${stats.nodes_written ?? stats.nodesWritten ?? '?'} nodes, ${stats.edges_written ?? stats.edgesWritten ?? '?'} edges embedded`;
  } catch (e) {
    ingestMsg = `db ingest FAILED (${e.message}) — graph tools (find_symbols/get_code/...) are fresh, but search serves the previous embeddings until the embedder is reachable`;
  }
  invalidateProjectCaches(dbPath);
  return `Reindexed ${repoRoot} → ${outputDir}\n${gd.nodes?.length ?? 0} nodes, ${gd.edges?.length ?? 0} edges\n${ingestMsg}`;
}


function claudeDesktopConfigPath() {
  const home = homedir();
  if (process.platform === 'darwin') {
    return join(home, 'Library', 'Application Support', 'Claude', 'claude_desktop_config.json');
  }
  if (process.platform === 'win32') {
    return join(process.env.APPDATA || join(home, 'AppData', 'Roaming'), 'Claude', 'claude_desktop_config.json');
  }
  return join(home, '.config', 'Claude', 'claude_desktop_config.json');
}

function mcpServersApply(config, server) {
  config.mcpServers = config.mcpServers || {};
  config.mcpServers.ultragraph = server;
}

// Returns whether an `ultragraph` entry existed (and deletes it if so), so
// callers can tell "removed" from "already wasn't there".
function mcpServersRemove(config) {
  const existed = !!config.mcpServers && Object.prototype.hasOwnProperty.call(config.mcpServers, 'ultragraph');
  if (existed) delete config.mcpServers.ultragraph;
  return existed;
}

// VS Code's user-level MCP config (`mcp.json` in the user profile dir) —
// same platform spread as Claude Desktop's config.
function vscodeGlobalConfigPath() {
  const home = homedir();
  if (process.platform === 'darwin') {
    return join(home, 'Library', 'Application Support', 'Code', 'User', 'mcp.json');
  }
  if (process.platform === 'win32') {
    return join(process.env.APPDATA || join(home, 'AppData', 'Roaming'), 'Code', 'User', 'mcp.json');
  }
  return join(home, '.config', 'Code', 'User', 'mcp.json');
}

// Each target: where its config lives (per scope), and how to graft a
// { command, args, env } server entry into that target's own JSON shape
// (schemas differ). `scopes.project` configs live in the current directory
// and apply to this repo only; `scopes.global` configs live in the user's
// home/profile dir and apply everywhere. Targets that only support one
// scope just omit the other. `format: 'toml'` targets skip `apply`/JSON
// entirely — see `upsertTomlServer`.
const MCP_INSTALL_TARGETS = {
  claude: {
    label: 'Claude Code',
    scopes: {
      project: () => join(process.cwd(), '.mcp.json'),
      global: () => join(homedir(), '.claude.json'),
    },
    apply: mcpServersApply,
    remove: mcpServersRemove,
  },
  'claude-desk': {
    label: 'Claude Desktop',
    scopes: { global: claudeDesktopConfigPath },
    apply: mcpServersApply,
    remove: mcpServersRemove,
  },
  cursor: {
    label: 'Cursor',
    scopes: {
      project: () => join(process.cwd(), '.cursor', 'mcp.json'),
      global: () => join(homedir(), '.cursor', 'mcp.json'),
    },
    apply: mcpServersApply,
    remove: mcpServersRemove,
  },
  windsurf: {
    label: 'Windsurf',
    scopes: { global: () => join(homedir(), '.codeium', 'windsurf', 'mcp_config.json') },
    apply: mcpServersApply,
    remove: mcpServersRemove,
  },
  vscode: {
    label: 'VS Code',
    scopes: {
      project: () => join(process.cwd(), '.vscode', 'mcp.json'),
      global: vscodeGlobalConfigPath,
    },
    apply: (config, server) => {
      config.servers = config.servers || {};
      config.servers.ultragraph = { type: 'stdio', ...server };
    },
    remove: (config) => {
      const existed = !!config.servers && Object.prototype.hasOwnProperty.call(config.servers, 'ultragraph');
      if (existed) delete config.servers.ultragraph;
      return existed;
    },
  },
  gemini: {
    label: 'Gemini CLI',
    scopes: {
      project: () => join(process.cwd(), '.gemini', 'settings.json'),
      global: () => join(homedir(), '.gemini', 'settings.json'),
    },
    apply: mcpServersApply,
    remove: mcpServersRemove,
  },
  codex: {
    label: 'Codex CLI',
    format: 'toml',
    scopes: { global: () => join(homedir(), '.codex', 'config.toml') },
  },
  hermes: {
    label: 'Hermes Agent',
    format: 'yaml',
    scopes: { global: () => join(homedir(), '.hermes', 'config.yaml') },
  },
  opencode: {
    label: 'opencode',
    scopes: {
      project: () => join(process.cwd(), 'opencode.json'),
      global: () => join(homedir(), '.config', 'opencode', 'opencode.json'),
    },
    // opencode's schema keys servers directly under `mcp` (no `servers`
    // nesting), and McpLocalConfig wants one `command` array (binary +
    // args combined) plus `environment` — not the generic {command, args,
    // env} shape the other targets use — with additionalProperties: false,
    // so any extra keys fail validation.
    apply: (config, server, skillDir) => {
      if (config['$schema'] === undefined) config['$schema'] = 'https://opencode.ai/config.json';
      config.mcp = config.mcp || {};
      config.mcp.ultragraph = {
        type: 'local',
        command: [server.command, ...server.args],
        environment: server.env,
        enabled: true,
      };
      if (skillDir && existsSync(skillDir)) {
        config.skills = config.skills || { paths: [] };
        if (!config.skills.paths.includes(skillDir)) {
          config.skills.paths.push(skillDir);
        }
      }
    },
    remove: (config) => {
      const existed = !!config.mcp && Object.prototype.hasOwnProperty.call(config.mcp, 'ultragraph');
      if (existed) delete config.mcp.ultragraph;
      return existed;
    },
  },
};

// Back-compat spellings for targets that were renamed; resolved before
// lookup so old docs/scripts keep working without showing up in usage.
const MCP_TARGET_ALIASES = {
  'claude-code': 'claude', // `claude` used to mean Claude Desktop (now `claude-desk`)
  'claude-desktop': 'claude-desk',
};

// ── Agent skill/rule file installation ────────────────────────────────────
// After installing the MCP config, we write the ug MCP tool guide as a rule
// file so the agent knows how to use the tools efficiently. Each agent has
// its own rules directory and frontmatter format.

const SKILL_TARGETS = {
  claude: {
    path: (root) => join(root, '.claude', 'rules', 'ug-mcp.md'),
    format: 'md',
    frontmatter: null,
  },
  cursor: {
    path: (root) => join(root, '.cursor', 'rules', 'ug-mcp.mdc'),
    format: 'mdc',
    frontmatter: { description: 'UltraGraph MCP tools guide — efficient codebase and knowledge-base search via a semantic knowledge graph', alwaysApply: false },
  },
  windsurf: {
    // Windsurf MCP config is always global, but rules go in the project dir.
    path: () => join(process.cwd(), '.windsurf', 'rules', 'ug-mcp.md'),
    format: 'windsurf-md',
    frontmatter: { trigger: 'model_decision', description: 'UltraGraph MCP tools guide — efficient codebase and knowledge-base search via a semantic knowledge graph' },
  },
};

function readSkillBody(skillDir) {
  const skillFile = join(skillDir, 'SKILL.md');
  if (!existsSync(skillFile)) return null;
  const content = readFileSync(skillFile, 'utf-8');
  const m = content.match(/^---\n[\s\S]*?\n---\n([\s\S]*)$/);
  return m ? m[1].trim() : content.trim();
}

function formatSkillContent(body, format, frontmatter) {
  if (!frontmatter) return body;
  const yaml = Object.entries(frontmatter).map(([k, v]) => `${k}: ${JSON.stringify(v)}`).join('\n');
  return `---\n${yaml}\n---\n\n${body}`;
}

function installSkillFile(target, scope) {
  const skillDir = join(dirname(fileURLToPath(import.meta.url)), 'ug-mcp-skill');
  const body = readSkillBody(skillDir);
  if (!body) return;
  const st = SKILL_TARGETS[target];
  if (!st) return;
  const root = scope === 'global' && target !== 'windsurf' ? homedir() : process.cwd();
  const ruleFile = st.path(root);
  const content = formatSkillContent(body, st.format, st.frontmatter);
  mkdirSync(dirname(ruleFile), { recursive: true });
  writeFileSync(ruleFile, content + '\n');
}

function uninstallSkillFile(target, scope) {
  const st = SKILL_TARGETS[target];
  if (!st) return;
  const root = scope === 'global' && target !== 'windsurf' ? homedir() : process.cwd();
  const ruleFile = st.path(root);
  if (existsSync(ruleFile)) rmSync(ruleFile);
}

// Codex's config is TOML, not JSON — rather than pull in a full TOML
// parser/writer for one write, surgically strip just the
// `[mcp_servers.<name>]` table (and its nested `.env` subtable) by text
// range, leaving the rest of the file untouched.
function removeTomlServerBlock(content, name) {
  const header = `[mcp_servers.${name}]`;
  const envHeader = `[mcp_servers.${name}.env]`;
  const out = [];
  let skipping = false;
  for (const line of content.split('\n')) {
    const trimmed = line.trim();
    const isOwnHeader = trimmed === header || trimmed === envHeader;
    const isOtherHeader = /^\[.+\]$/.test(trimmed) && !isOwnHeader;
    if (skipping) {
      if (isOtherHeader) skipping = false;
      else continue;
    }
    if (isOwnHeader) {
      skipping = true;
      continue;
    }
    out.push(line);
  }
  return out.join('\n').replace(/\n{3,}/g, '\n\n').replace(/\s+$/, '');
}

function upsertTomlServer(content, name, server) {
  const header = `[mcp_servers.${name}]`;
  const envHeader = `[mcp_servers.${name}.env]`;
  const hasEnv = server.env && Object.keys(server.env).length > 0;
  const block = [
    header,
    `command = ${JSON.stringify(server.command)}`,
    `args = ${JSON.stringify(server.args)}`,
    ...(hasEnv
      ? ['', envHeader, ...Object.entries(server.env).map(([k, v]) => `${k} = ${JSON.stringify(v)}`)]
      : []),
  ].join('\n');

  const remainder = removeTomlServerBlock(content, name);
  return (remainder ? remainder + '\n\n' : '') + block + '\n';
}

// Hermes Agent's config is YAML (`mcp_servers.<name>` under ~/.hermes/config.yaml)
// and, unlike Codex's TOML, is likely to carry the user's own comments — a
// text-range splice would risk mangling those, so this goes through a real
// parser. `parseDocument` (rather than plain `parse`/`stringify`) keeps a
// CST alongside the data, so `setIn` mutates in place and `toString()`
// preserves the surrounding formatting/comments instead of a full reprint.
function upsertYamlServer(content, name, server) {
  const doc = parseDocument(content || '');
  if (doc.contents === null) doc.contents = doc.createNode({});
  doc.setIn(['mcp_servers', name], server);
  return doc.toString();
}

// The command clients should launch for the MCP server. Prefer the native
// `ug` binary (its path is handed down via UG_BIN by the Rust wrapper, or
// found sitting next to this script in `.ug/`) so client configs are a
// plain `ug mcp` and don't depend on how Node is installed. Falls back to
// `node cli.mjs mcp` for Node-only installs with no binary around.
function resolveMcpServerCommand() {
  const candidates = [];
  if (process.env.UG_BIN) candidates.push(process.env.UG_BIN);
  const selfDir = dirname(fileURLToPath(import.meta.url));
  candidates.push(join(selfDir, process.platform === 'win32' ? 'ug.exe' : 'ug'));
  for (const bin of candidates) {
    if (existsSync(bin)) return { command: bin, args: ['mcp'] };
  }
  return { command: 'node', args: [fileURLToPath(import.meta.url), 'mcp'] };
}

function resolveMcpTarget(target) {
  target = MCP_TARGET_ALIASES[target] || target;
  const targetDef = MCP_INSTALL_TARGETS[target];
  if (!targetDef) {
    throw new Error(`Unknown MCP target '${target}' (expected: ${Object.keys(MCP_INSTALL_TARGETS).join(', ')})`);
  }
  return { target, targetDef };
}

// Numbered-list picker on stdin — used when `mcp install`/`uninstall` needs
// an answer the command line didn't provide. Non-TTY sessions (piped/CI)
// can't answer, so fail with the caller's usage hint instead of hanging.
async function promptChoice(title, choices, nonInteractiveHint) {
  if (!process.stdin.isTTY) {
    throw new Error(nonInteractiveHint);
  }
  console.log(chalk.bold(title));
  for (const [i, c] of choices.entries()) {
    console.log(`  ${chalk.cyan(String(i + 1).padStart(2))}) ${c.name.padEnd(14)} ${chalk.gray(c.hint || '')}`);
  }
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    for (;;) {
      const answer = (await rl.question(`Select [1-${choices.length}]: `)).trim();
      const idx = Number(answer);
      if (Number.isInteger(idx) && idx >= 1 && idx <= choices.length) return choices[idx - 1].value;
      const byName = choices.find((c) => c.name === answer);
      if (byName) return byName.value;
      console.log(chalk.yellow(`Enter a number between 1 and ${choices.length} (Ctrl+C to abort).`));
    }
  } finally {
    rl.close();
  }
}

// Writes (or merges into) an MCP client's config file so `ug` shows up as a
// tool source without the user hand-editing JSON / absolute paths themselves.
// `scope` is 'project' or 'global' and must be one the target supports.
function installMcpConfig(target, scope) {
  const { targetDef } = resolveMcpTarget(target);
  const pathFor = targetDef.scopes[scope];
  if (!pathFor) {
    throw new Error(`Target '${target}' has no ${scope} config (supported: ${Object.keys(targetDef.scopes).join(', ')})`);
  }
  const configPath = pathFor();
  const server = {
    ...resolveMcpServerCommand(),
    env: { UG_PROJECT: deriveProjectName('.') },
  };

  if (targetDef.format === 'toml') {
    const existing = existsSync(configPath) ? readFileSync(configPath, 'utf-8') : '';
    mkdirSync(dirname(configPath), { recursive: true });
    writeFileSync(configPath, upsertTomlServer(existing, 'ultragraph', server));
    return configPath;
  }

  if (targetDef.format === 'yaml') {
    const existing = existsSync(configPath) ? readFileSync(configPath, 'utf-8') : '';
    mkdirSync(dirname(configPath), { recursive: true });
    writeFileSync(configPath, upsertYamlServer(existing, 'ultragraph', server));
    return configPath;
  }

  let config = {};
  if (existsSync(configPath)) {
    try {
      config = JSON.parse(readFileSync(configPath, 'utf-8'));
    } catch (e) {
      throw new Error(`${configPath} exists but isn't valid JSON — fix or remove it, then retry (${e.message})`);
    }
  }
  const skillDir = join(dirname(fileURLToPath(import.meta.url)), 'ug-mcp-skill');
  targetDef.apply(config, server, skillDir);

  mkdirSync(dirname(configPath), { recursive: true });
  writeFileSync(configPath, JSON.stringify(config, null, 2) + '\n');
  installSkillFile(target, scope);
  return configPath;
}

// Reverses `installMcpConfig`: strips the `ultragraph` entry from a target's
// config, leaving everything else (other servers, comments, formatting)
// untouched. Returns `removed: false` (no write) when there was nothing to
// remove — a missing config file or a config that never had our entry.
function uninstallMcpConfig(target, scope) {
  const { targetDef } = resolveMcpTarget(target);
  const pathFor = targetDef.scopes[scope];
  if (!pathFor) {
    throw new Error(`Target '${target}' has no ${scope} config (supported: ${Object.keys(targetDef.scopes).join(', ')})`);
  }
  const configPath = pathFor();
  if (!existsSync(configPath)) {
    return { configPath, removed: false };
  }

  if (targetDef.format === 'toml') {
    const existing = readFileSync(configPath, 'utf-8');
    const remainder = removeTomlServerBlock(existing, 'ultragraph');
    const removed = remainder !== existing.replace(/\s+$/, '');
    if (removed) writeFileSync(configPath, remainder ? remainder + '\n' : '');
    return { configPath, removed };
  }

  if (targetDef.format === 'yaml') {
    const doc = parseDocument(readFileSync(configPath, 'utf-8'));
    const removed = doc.hasIn(['mcp_servers', 'ultragraph']);
    if (removed) {
      doc.deleteIn(['mcp_servers', 'ultragraph']);
      writeFileSync(configPath, doc.toString());
    }
    return { configPath, removed };
  }

  let config;
  try {
    config = JSON.parse(readFileSync(configPath, 'utf-8'));
  } catch (e) {
    throw new Error(`${configPath} exists but isn't valid JSON — fix or remove it, then retry (${e.message})`);
  }
  const removed = targetDef.remove(config);
  if (removed) {
    writeFileSync(configPath, JSON.stringify(config, null, 2) + '\n');
    uninstallSkillFile(target, scope);
  }
  return { configPath, removed };
}

// Pre-rename tool names. `tools/list` advertises only the canonical set, but
// an agent may have the old name cached from an earlier session, so keep
// accepting them. Same aliases the CLI honours for its subcommands.
const TOOL_ALIASES = {
  search_kb: 'search',
  hybrid_search: 'search',
  semantic_search_kb: 'semantic_search',
  traverse_kb: 'traverse',
  graph_path: 'shortest_path',
  path: 'shortest_path',
  list: 'list_projects',
  find_symbol: 'find_symbols',
  // graph_search was find_symbols over names *and* docstrings. Callers using
  // the old name get that behaviour back via the includeDocs default below.
  graph_search: 'find_symbols',
  search_graph: 'find_symbols',
};

// Tools whose legacy name implied a non-default param value.
const ALIAS_DEFAULTS = {
  graph_search: { includeDocs: true },
  search_graph: { includeDocs: true },
};

// Handled by callTool but deliberately absent from tools/list — operator
// diagnostics that would only waste an agent's tool call. Still invocable
// through `ug mcp call` for debugging.
const UNLISTED_TOOLS = new Set(['ping_embedder']);

function canonicalToolName(name) {
  return TOOL_ALIASES[name] ?? name;
}

/**
 * Run one MCP tool and return its text output.
 *
 * The single dispatch shared by the stdio server and `ug mcp call` — they
 * used to carry separate copies that drifted. Throws on unknown tools and
 * on validation failures; callers decide how to surface the error.
 *
 * The graph-backed tools delegate to the Rust core via `agentTool`, so their
 * output is identical to the matching `ug <tool>` command. The DB-backed
 * ones (search, traversal, embeddings) already shared Rust code through
 * their own napi entry points.
 */
async function callTool(rawName, rawArgs) {
  const name = canonicalToolName(rawName);
  const project = rawArgs?.project;
  const { dbPath, repoRoot } = projectCtx(project);
  const args = { ...(ALIAS_DEFAULTS[rawName] ?? {}), ...(rawArgs ?? {}), project: undefined };
  const withStaleness = (text) => text + stalenessNote(dbPath, repoRoot);

  // DB-backed: OverGraph/Neo4j + embeddings.
  if (name === 'search') {
    const parsed = SearchKbInput.parse(args);
    const json = await ug.dbHybridSearch(
      dbPath,
      JSON.stringify({ ...parsed, repoRoot }),
      embedderOptionsJson(),
      destOptionsJson(),
    );
    return withStaleness(formatRankedContext(JSON.parse(json)));
  }

  if (name === 'semantic_search') {
    const parsed = SemanticSearchInput.parse(args);
    const json = await ug.dbSemanticSearch(
      dbPath,
      parsed.query,
      parsed.k ?? 10,
      parsed.whereClause ?? null,
      embedderOptionsJson(),
      destOptionsJson(),
    );
    return withStaleness(formatSemanticHits(parsed.query, JSON.parse(json)));
  }

  if (name === 'traverse') {
    const parsed = TraverseInput.parse(args);
    const json = await ug.dbTraverse(
      dbPath,
      parsed.nodeId,
      parsed.hops,
      parsed.edgeTypes ?? null,
      parsed.direction,
      destOptionsJson(),
    );
    const header = `Traversal from [${parsed.nodeId.join(', ')}] (hops=${parsed.hops}, dir=${parsed.direction})`;
    return withStaleness(formatTraversal(JSON.parse(json), header));
  }

  // Graph-backed: one Rust implementation, shared with the CLI and HTTP API.
  // The zod schemas still validate and normalize; the lookup and formatting
  // live in native/src/agent_tools.rs.
  const GRAPH_TOOLS = {
    find_symbols: FindSymbolsInput,
    file_outline: FileOutlineInput,
    get_code: GetCodeInput,
    find_usages: FindUsagesInput,
    shortest_path: ShortestPathInput,
    project_overview: null,
    graph_schema: null,
  };
  if (name in GRAPH_TOOLS) {
    const schema = GRAPH_TOOLS[name];
    const params = schema ? schema.parse(args) : {};
    return withStaleness(agentTool(name, dbPath, repoRoot, params));
  }

  // Project-level.
  if (name === 'list_projects') {
    const text = formatProjectList(listProjectsInfo(), repoRoot);
    return project
      ? `\n⚠ list_projects ignores the project parameter — listing all projects under ${ugHome()}.\n\n` + text
      : text;
  }

  if (name === 'reindex') {
    return await regenerateProject(dbPath, repoRoot);
  }

  if (name === 'ping_embedder') {
    return await ug.pingEmbedder(embedderOptionsJson());
  }

  throw new Error(`Unknown tool: ${name}`);
}

async function runMcpServer() {
  const { dbPath: DB_PATH, repoRoot: REPO_ROOT } = resolveDbAndRoot();

  const server = new Server(
    { name: 'ultragraph', version: UG_VERSION },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: MCP_TOOLS,
  }));

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    const { name, arguments: rawArgs } = req.params;
    try {
      return { content: [{ type: 'text', text: await callTool(name, rawArgs) }] };
    } catch (err) {
      return {
        isError: true,
        content: [{ type: 'text', text: `Error: ${err.message ?? String(err)}` }],
      };
    }
  });

  const transport = new StdioServerTransport();
  // stdout is reserved for MCP's JSON-RPC frames — log status to stderr.
  // A TTY on stdin means a human ran this by hand, not an MCP client —
  // explain what this mode is instead of hanging silently.
  if (process.stdin.isTTY) {
    console.error(chalk.bold('UltraGraph MCP server (stdio mode)'));
    console.error('This command is meant to be launched by an AI agent, not run by hand:');
    console.error('agents spawn it themselves and speak JSON-RPC over stdin/stdout.');
    console.error(`To wire an agent up to it, run ${chalk.cyan('ug mcp install')} instead.`);
    console.error('(Waiting for JSON-RPC on stdin — Ctrl+C to exit.)');
  } else {
    console.error('start ultragraph mcp server...');
  }
  await server.connect(transport);
  if (!process.stdin.isTTY) console.error('ultragraph mcp server started.');
}

// Shown for bare `node cli.mjs` with no subcommand — a short "what do I do
// first" nudge instead of the full command wall (that's the `help` command).
function quickstartBanner() {
  return [
    chalk.bold('Welcome to UltraGraph') + ' — turn a codebase into a queryable knowledge graph.',
    '',
    chalk.bold('Quick start:'),
    '  ' + chalk.cyan('node cli.mjs gen') + '   Index this directory, build the graph, and ingest it (→ ~/.ug/<name>/)',
    '  ' + chalk.cyan('node cli.mjs mcp install') + '   Wire this up as an MCP server (interactive picker; or name claude, claude-desk, cursor, windsurf, vscode, gemini, codex, hermes, opencode)',
    '  ' + chalk.cyan('node cli.mjs doctor') + '  Show resolved project/db/embedder config and where it came from',
    '  ' + chalk.cyan('node cli.mjs help') + '  Full command reference',
    '',
    chalk.gray('(the `ug` standalone binary opens the server directly when run with no arguments)'),
  ].join('\n');
}

const commands = {
  index: {
    usage: '[<input dir>] [-i|--input <dir>] [-n|--name <project>] [-c|--cache <cache-dir>] [-o|--output <output-path>]',
    desc: 'Index a directory and output the symbol tree as JSON into a file specified by `--output` (default: `~/.ug/<name>/indexed-tree.json`). Use `--cache` to speed up re-indexing.',
    run: (args) => {
      const path = extractFlag(args, '-i') || extractFlag(args, '--input') || (args[0] || '.');
      const cachePath = extractFlag(args, '-c') || extractFlag(args, '--cache');
      const outputPath = extractFlag(args, '-o') || extractFlag(args, '--output')
        || join(projectDir(resolveProjectName(args, path)), 'indexed-tree.json');

      // Ensure output directory exists
      const outputDir = dirname(outputPath);
      if (!existsSync(outputDir)) {
        mkdirSync(outputDir, { recursive: true });
      }

      let result;
      if (cachePath) {
        result = ug.indexWithCache(path, cachePath);
      } else {
        result = ug.index(path);
      }
      writeFileSync(outputPath, result);
      return `Generated index in ${outputPath}`;
    }
  },
  graph: {
    usage: '[<indexed-tree-json-file>] [-i|--input <file>] [-n|--name <project>] [-o|--output <output-path>]',
    desc: 'Build graph from index result (i.e.: ~/.ug/<name>/indexed-tree.json) and generates graph.json',
    run: (args) => {
      const projDir = projectDir(resolveProjectName(args, '.'));
      const path = extractFlag(args, '-i') || extractFlag(args, '--input')
        || (args.length && !args[0].startsWith('-') ? args[0] : join(projDir, 'indexed-tree.json'));
      const outputPath = extractFlag(args, '-o') || extractFlag(args, '--output') || join(projDir, 'graph.json');

      // Ensure output directory exists
      const outputDir = dirname(outputPath);
      if (!existsSync(outputDir)) {
        mkdirSync(outputDir, { recursive: true });
      }

      const indexJson = readFileSync(path, 'utf-8');
      const index = JSON.parse(indexJson);
      const json = JSON.stringify(index);
      const result = ug.buildGraph(json);

      writeFileSync(outputPath, result);
      return `Generated graph in ${outputPath}`;
    }
  },
  gen: {
    usage: '[-i|--input <input-dir, default: .>] [-n|--name <project, default: input dir basename>] [-c|--cache <cache-dir>] [-o|--output <output-dir, default: ~/.ug/<name>>] [-d|--db <db-path, default: <output-dir>/ugdb>] [--no-ingest] [-m|--model <embedding-model-name>] [-b|--base-url <embedding-api-base-url>] [-a|--api-key <embedding-api-key>]',
    desc: 'Full pipeline: index → graph → visualization → OverGraph ingest. Outputs to ~/.ug/<project-name>/ by default. Pass --no-ingest to skip ingestion (no embedding endpoint required).',
    run: async (args) => {
      if (args.includes('-h') || args.includes('--help')) {
        console.log(`gen ${commands.gen.usage}`);
        console.log(`  ${commands.gen.desc}`);
        return;
      }
      const path = extractFlag(args, '-i') || extractFlag(args, '--input')
        || (args.length && !args[0].startsWith('-') ? args[0] : '.');
      const cachePath = extractFlag(args, '-c') || extractFlag(args, '--cache');
      const projectName = resolveProjectName(args, path);
      const outputDir = extractFlag(args, '-o') || extractFlag(args, '--output')
        || projectDir(projectName);

      console.log(chalk.cyan('\n⚡ Full pipeline: ') + chalk.white('index ') + chalk.gray('→') + chalk.white(' graph ') + chalk.gray('→') + chalk.white(' visualization ') + chalk.gray('→') + chalk.white(' OverGraph ingest'));

      if (!existsSync(outputDir)) {
        mkdirSync(outputDir, { recursive: true });
      }

      console.log(chalk.gray('▸') + ' ' + chalk.blue('Indexing') + ' ' + chalk.gray(path));
      let result;
      if (cachePath) {
        result = ug.indexWithCache(path, cachePath);
      } else {
        result = ug.index(path);
      }
      const index = JSON.parse(result);
      const json = JSON.stringify(index);
      const graph = ug.buildGraph(json);

      console.log(chalk.gray('▸') + ' ' + chalk.blue('Building graph'));
      const graphPath = join(outputDir, 'graph.json');
      writeFileSync(graphPath, graph);
      writeFileSync(join(outputDir, 'indexed-tree.json'), result);
      const graphData = JSON.parse(graph);
      const nodeCount = graphData.nodes?.length ?? 0;
      const edgeCount = graphData.edges?.length ?? 0;
      console.log('  ' + chalk.gray('nodes:') + ' ' + chalk.bold(nodeCount));
      console.log('  ' + chalk.gray('edges:') + ' ' + chalk.bold(edgeCount));

      // index.html / ug-vis.bundle.js are embedded in `ug serve` and served
      // directly, so we only emit the README here.
      console.log(chalk.gray('▸') + ' ' + chalk.blue('Writing visualization README'));
      const visSrc = join(__dirname, 'vis');
      const indexMdSrc = join(visSrc, 'visualization.md');

      if (existsSync(indexMdSrc)) {
        copyFileSync(indexMdSrc, join(outputDir, 'README.md'));
      }

      let repoRoot = path;
      try {
        repoRoot = realpathSync(resolve(path));
      } catch {}
      writeProjectMeta(outputDir, {
        name: projectName,
        repoRoot,
        nodes: nodeCount,
        edges: edgeCount,
      });

      console.log(chalk.gray('────────────────────────────────────────'));
      console.log(chalk.green('✓') + ' ' + chalk.bold('Generated project ') + chalk.cyan(projectName) + chalk.bold(' in') + ' ' + chalk.cyan(outputDir + '/'));
      console.log('  ' + chalk.green('✓') + ' ' + chalk.white('graph.json'));
      console.log('  ' + chalk.green('✓') + ' ' + chalk.white('indexed-tree.json'));
      console.log('  ' + chalk.green('✓') + ' ' + chalk.white('README.md'));
      console.log('  ' + chalk.green('✓') + ' ' + chalk.white('project.json'));

      if (args.includes('--no-ingest')) {
        console.log(chalk.yellow('⚠ ') + 'Skipping db-ingest (--no-ingest)');
        return chalk.cyan(`Run "ug serve" and visit http://localhost:8080 to view the graph`);
      }

      const dbPath = extractFlag(args, '-d') || extractFlag(args, '--db') || join(outputDir, 'ugdb');
      const embedderOptions = parseEmbedderOptions(args);
      const embedderArg = embedderOptions ? JSON.stringify(embedderOptions) : null;

      console.log('');
      console.log(chalk.gray('▸') + ' ' + chalk.blue('Ingesting into') + ' ' + chalk.gray(dbPath));
      try {
        const ingestResult = await ug.dbIngest(graph, dbPath, embedderArg);
        const stats = JSON.parse(ingestResult);
        const nodes = stats.nodes_written ?? stats.nodesWritten ?? '?';
        const edges = stats.edges_written ?? stats.edgesWritten ?? '?';
        console.log('  ' + chalk.green('✓') + ' ' + chalk.white(`${nodes} nodes, ${edges} edges embedded`));
      } catch (e) {
        console.warn(chalk.yellow('⚠ ') + 'db-ingest skipped — ' + e.message);
        console.warn(chalk.yellow('  Re-run later once the embedding endpoint is up:'));
        console.warn(chalk.gray('    node node/cli.mjs db-ingest') + ' ' + chalk.white(graphPath + ' ' + dbPath));
      }

      console.log(chalk.gray('────────────────────────────────────────'));
      console.log(chalk.cyan('Run "ug serve" and visit http://localhost:8080 to view the graph'));
      console.log(chalk.cyan(`Run "node node/cli.mjs db-rag -i ${dbPath} hello" to perform a RAG query on the DB.`));

      return;
    }
  },
  'graph-search': {
    usage: '<graph-json-file> <keyword> [-t|--type <node-type>]... [-o|--output <output-path>]',
    desc: 'Graph-based: Keyword search over in-memory graph nodes (case-insensitive substring on name/docstring).',
    run: (args) => {
      if (args.length < 2) {
        throw new Error(`Usage: graph-search ${commands['graph-search'].usage}\n  ${commands['graph-search'].desc}`);
      }
      const file = args[0];
      const keyword = args[1];
      const nodeTypes = [...new Set([...extractMultiFlags(args.slice(2), '--type'), ...extractMultiFlags(args.slice(2), '-t')])];
      const outputPath = extractFlag(args.slice(2), '--output') || extractFlag(args.slice(2), '-o');
      const graphJson = readFileSync(file, 'utf-8');
      const result = ug.graphKeywordSearch(graphJson, keyword, nodeTypes.length ? nodeTypes : null);
      if (outputPath) {
        const outputDir = dirname(outputPath);
        if (!existsSync(outputDir)) mkdirSync(outputDir, { recursive: true });
        writeFileSync(outputPath, result);
        return `Wrote search result to ${outputPath}`;
      }
      return JSON.parse(result);
    }
  },
  'db-ingest': {
    usage: '[-i|--input <graph-json-file>] [-o|--output <db-path>] [-b|--base-url <url>] [-a|--api-key <key>] [-m|--model <name>] [--embedding-dim <n>]',
    desc: 'OverGraph: Embed graph nodes and write to OverGraph. Requires a running embedding endpoint.',
    run: async (args) => {
      const graphFile = extractFlag(args, '-i') || extractFlag(args, '--input') || extractFlag(args, '-o');
      const dbPath = extractFlag(args, '-o') || extractFlag(args, '--output');
      if (!graphFile || !dbPath) {
        throw new Error(`Usage: db-ingest ${commands['db-ingest'].usage}\n  ${commands['db-ingest'].desc}`);
      }
      const embedderOptions = parseEmbedderOptions(args);
      const graphJson = readFileSync(graphFile, 'utf-8');
      const result = await ug.dbIngest(graphJson, dbPath, embedderOptions ? JSON.stringify(embedderOptions) : null);
      return JSON.parse(result);
    }
  },
  'db-traverse': {
    usage: '<db-path> <start-node-id> [-k <hops>] [-e|--edge-type <type>]... [--direction <outbound|inbound|both>]',
    desc: 'OverGraph: K-hop BFS traversal using edges table with optional edge-type filtering.',
    run: async (args) => {
      if (args.length < 3) {
        throw new Error(`Usage: db-traverse ${commands['db-traverse'].usage}\n  ${commands['db-traverse'].desc}`);
      }
      const dbPath = args[0];
      const startNodeId = args[1];
      const hops = extractArg(args.slice(2), '-k', '--hops', 2);
      const edgeTypes = [...new Set([...extractMultiFlags(args.slice(2), '--edge-type'), ...extractMultiFlags(args.slice(2), '-e')])];
      const direction = extractFlag(args.slice(2), '--direction') || 'outbound';
      const result = await ug.dbTraverse(dbPath, [startNodeId], hops, edgeTypes.length ? edgeTypes : null, direction);
      return JSON.parse(result);
    }
  },
  'db-rag': {
    usage: '[-i|--input <db-path>] <query> [-k <limit>] [--strategy <ppr|mmr>] [--restart-prob <0..1>] [--seed-pool <n>] [--direction <outbound|inbound|both>] [--edge-type <type>]... [-b|--base-url <url>] [-a|--api-key <key>] [-m|--model <name>] [--embedding-dim <n>]',
    desc: 'OverGraph: End-to-end GraphRAG retrieval. Default ranking: Personalized PageRank seeded by RRF (vector + FTS). Pass --strategy mmr for legacy seed+BFS+MMR.',
    run: async (args) => {
      const dbPath = extractFlag(args, '-i') || extractFlag(args, '--input');
      const restIdx = dbPath ? args.indexOf(dbPath) + 1 : 0;
      const query = args[restIdx];
      if (!dbPath || !query) {
        throw new Error(`Usage: db-rag ${commands['db-rag'].usage}\n  ${commands['db-rag'].desc}`);
      }
      const rest = args.slice(restIdx + 1);
      const k = extractArg(rest, '-k', '--limit', 10);
      const strategy = extractFlag(rest, '--strategy');
      const restartProbRaw = extractFlag(rest, '--restart-prob');
      const seedPool = extractArg(rest, '--seed-pool', '--seed-pool', NaN);
      const direction = extractFlag(rest, '--direction');
      const edgeTypes = [...new Set([...extractMultiFlags(rest, '--edge-type'), ...extractMultiFlags(rest, '-e')])];
      const embedderOptions = parseEmbedderOptions(rest);
      const opts = { query, k };
      if (strategy) opts.strategy = strategy;
      if (restartProbRaw && !isNaN(parseFloat(restartProbRaw))) opts.pprRestartProb = parseFloat(restartProbRaw);
      if (!isNaN(seedPool)) opts.pprSeedPool = seedPool;
      if (direction) opts.direction = direction;
      if (edgeTypes.length) opts.edgeTypes = edgeTypes;
      const result = await ug.dbHybridSearch(dbPath, JSON.stringify(opts), embedderOptions ? JSON.stringify(embedderOptions) : null);
      return JSON.parse(result);
    }
  },
  ping: {
    usage: '[-b|--base-url <url>] [-a|--api-key <key>] [-m|--model <name>] [--embedding-dim <n>]',
    desc: 'Probe the embedding endpoint to verify connectivity. Pass --embedding-dim to assert a specific dim; otherwise the probe just confirms the endpoint responds.',
    run: async (args) => {
      const embedderOptions = parseEmbedderOptions(args);
      const result = await ug.pingEmbedder(embedderOptions ? JSON.stringify(embedderOptions) : null);
      return result;
    }
  },
  list: {
    usage: '',
    desc: 'List generated projects under ~/.ug (or $UG_HOME)',
    run: () => {
      const projects = listProjects();
      const root = ugHome();
      if (!projects.length) {
        return `No projects found in ${root}. Run \`node node/cli.mjs gen\` in a repo to create one.`;
      }
      const cwdName = deriveProjectName('.');
      console.log(chalk.bold(`Projects in ${root}`) + chalk.gray(` (${projects.length})`) + '\n');
      console.log('  ' + chalk.bold('NAME'.padEnd(24) + 'NODES'.padStart(8) + 'EDGES'.padStart(9) + '  UPDATED'.padEnd(22) + 'REPO'));
      for (const { meta } of projects) {
        const marker = meta.name === cwdName ? chalk.green('*') : ' ';
        const updated = meta.updatedAt ? new Date(meta.updatedAt * 1000).toISOString().replace('T', ' ').slice(0, 19) : '-';
        console.log(`${marker} ${chalk.cyan(String(meta.name).padEnd(24))}${String(meta.nodes).padStart(8)}${String(meta.edges).padStart(9)}  ${updated.padEnd(20)}${meta.repoRoot || ''}`);
      }
      console.log('\n' + chalk.bold('*') + ' matches the current directory.');
      return;
    }
  },
  rm: {
    usage: '[<project>] [-n|--name <project>] [-f|--force]',
    desc: "Delete a project's data directory under ~/.ug (or $UG_HOME). Prompts for confirmation unless -f/--force (or -y/--yes) is given.",
    run: async (args) => {
      const flagged = extractFlag(args, '-n') || extractFlag(args, '--name');
      const positional = args[0] && !args[0].startsWith('-') ? args[0] : null;
      const name = sanitizeName(flagged || positional || deriveProjectName('.'));
      const dir = projectDir(name);

      if (!existsSync(dir)) {
        throw new Error(`No project named '${name}' found at ${dir}. Run \`node node/cli.mjs list\` to see available projects.`);
      }

      const meta = readProjectMeta(dir);
      console.log(chalk.bold(`About to remove project ${name}`));
      console.log(`  path:  ${dir}`);
      if (meta) {
        console.log(`  repo:  ${meta.repoRoot || ''}`);
        console.log(`  nodes: ${meta.nodes || 0}, edges: ${meta.edges || 0}`);
      }

      const force = args.includes('-f') || args.includes('--force') || args.includes('-y') || args.includes('--yes');
      if (!force) {
        // Non-interactive stdin (piped/CI) has no line to answer with, so
        // fail closed instead of hanging or silently proceeding.
        if (!process.stdin.isTTY) {
          throw new Error("Refusing to delete without confirmation in a non-interactive shell. Re-run with -f/--force (or -y/--yes).");
        }
        const rl = createInterface({ input: process.stdin, output: process.stdout });
        let answer = '';
        try {
          answer = await rl.question('Delete this project directory? This cannot be undone. [y/N] ');
        } finally {
          rl.close();
        }
        if (!/^y(es)?$/i.test(answer.trim())) {
          console.log('Aborted.');
          return;
        }
      }

      rmSync(dir, { recursive: true, force: true });
      console.log(chalk.green('✓') + ` Removed ${name} (${dir})`);
      return;
    }
  },
  doctor: {
    usage: '',
    desc: 'Show resolved MCP-server config (db path, repo root, embedder, destination) and which env var (if any) each value came from',
    run: () => {
      const line = (label, value, source) => `  ${label.padEnd(14)}${chalk.cyan(String(value))}  ${chalk.gray(`[${source}]`)}`;

      console.log(chalk.bold('UltraGraph doctor (MCP server resolution)'));
      console.log();

      console.log(chalk.bold('Project'));
      console.log(line('UG_HOME:', ugHome(), process.env.UG_HOME ? 'env:UG_HOME' : 'default: ~/.ug'));

      const { dbPath, repoRoot } = resolveDbAndRoot();
      const dbSource = process.env.UG_DB_PATH
        ? 'env:UG_DB_PATH'
        : process.env.UG_PROJECT
        ? 'env:UG_PROJECT'
        : existsSync(dbPath)
        ? 'derived from cwd basename'
        : 'derived from cwd basename (no db found yet)';
      console.log(line('db path:', dbPath, dbSource));
      console.log(`  ${'exists:'.padEnd(14)}${existsSync(dbPath) ? chalk.green('yes') : chalk.yellow('no — run `ug ingest`')}`);
      console.log(line('repo root:', repoRoot, process.env.UG_REPO_ROOT ? 'env:UG_REPO_ROOT' : 'project.json / cwd'));
      console.log();

      console.log(chalk.bold('Embeddings') + chalk.gray(' (used by search / traverse / ping_embedder)'));
      console.log(line('base_url:', process.env.UG_EMBED_BASE_URL || '(n/a — local in-process ONNX)', process.env.UG_EMBED_BASE_URL ? 'env:UG_EMBED_BASE_URL' : 'default'));
      console.log(line('api_key:', process.env.UG_EMBED_API_KEY ? '(set)' : '(default placeholder)', process.env.UG_EMBED_API_KEY ? 'env:UG_EMBED_API_KEY' : 'default'));
      console.log(line('model:', process.env.UG_EMBED_MODEL || '(default: bge-small-en-v1.5)', process.env.UG_EMBED_MODEL ? 'env:UG_EMBED_MODEL' : 'default'));
      console.log();

      console.log(chalk.bold('Destination'));
      const dest = (process.env.UG_DEST || 'overgraph').toLowerCase();
      console.log(line('backend:', dest, process.env.UG_DEST ? 'env:UG_DEST' : 'default: overgraph'));
      if (dest === 'neo4j' || dest === 'neo') {
        console.log(line('neo4j uri:', process.env.UG_NEO4J_URI || '(unset — required)', process.env.UG_NEO4J_URI ? 'env:UG_NEO4J_URI' : 'MISSING'));
        console.log(line('neo4j user:', process.env.UG_NEO4J_USER || 'neo4j', process.env.UG_NEO4J_USER ? 'env:UG_NEO4J_USER' : 'default'));
        console.log(line('neo4j password:', process.env.UG_NEO4J_PASSWORD ? '(set)' : '(unset — required)', process.env.UG_NEO4J_PASSWORD ? 'env:UG_NEO4J_PASSWORD' : 'MISSING'));
      }
      return;
    }
  },
  mcp: {
    usage: `[install|uninstall [${Object.keys(MCP_INSTALL_TARGETS).join('|')}] [--global|--project]] | call <tool> <json> | list`,
    desc: 'Start the MCP server (no args; see env vars in the source header), write this project into an MCP client config with `install [target]`, remove it with `uninstall [target]`, invoke a tool one-shot with `call <tool> <json>`, or list available tools. Omitted target/scope are asked for interactively.',
    run: async (args) => {
      if (args[0] === 'call' || args[0] === 'c') {
        const tool = args[1];
        if (!tool) throw new Error('Usage: mcp call <tool> <json>');
        const json = args[2] || '{}';
        let parsed;
        try {
          parsed = JSON.parse(json);
        } catch {
          throw new Error(`Invalid JSON: ${json}`);
        }
        const canonical = canonicalToolName(tool);
        const known =
          MCP_TOOLS.some((t) => t.name === canonical) || UNLISTED_TOOLS.has(canonical);
        if (!known) throw new Error(`Unknown tool '${tool}' — see \`ug mcp list\` for available tools.`);
        // Same dispatch the stdio server uses, so `mcp call` is a faithful
        // preview of what an agent sees.
        console.log(await callTool(tool, parsed));
        return '';
      }

      if (args[0] === 'list' || args[0] === 'ls') {
        const current = resolveDbAndRoot();
        console.log(chalk.bold(`Available MCP tools (project: ${basename(dirname(current.dbPath))}, repo: ${current.repoRoot})`));
        console.log();
        for (const t of MCP_TOOLS) {
          console.log(chalk.cyan(t.name.padEnd(18)) + chalk.gray(t.description.split('.')[0]));
        }
        console.log();
        console.log(`Run ${chalk.cyan('ug mcp call <tool> <json>')} to invoke one. Example:`);
        console.log(`  ${chalk.gray('ug mcp call find_symbols \'{"name":"run_mcp"}\'')}`);
        return '';
      }

      if (args[0] === 'install' || args[0] === 'uninstall') {
        const action = args[0];
        const rest = args.slice(1);
        const wantsGlobal = rest.includes('--global') || rest.includes('-g');
        const wantsProject = rest.includes('--project');
        if (wantsGlobal && wantsProject) {
          throw new Error('Pass at most one of --global / --project.');
        }

        let target = rest.find((a) => !a.startsWith('-'));
        if (!target) {
          target = await promptChoice(
            `${action === 'install' ? 'Install' : 'Uninstall'} the UltraGraph MCP server for which client?`,
            Object.entries(MCP_INSTALL_TARGETS).map(([name, def]) => ({ name, hint: def.label, value: name })),
            `Usage: mcp ${action} <${Object.keys(MCP_INSTALL_TARGETS).join('|')}> [--global|--project]`,
          );
        }
        const { target: resolved, targetDef } = resolveMcpTarget(target);
        target = resolved;
        const scopeNames = Object.keys(targetDef.scopes);

        const flagScope = wantsGlobal ? 'global' : wantsProject ? 'project' : null;
        if (flagScope && !targetDef.scopes[flagScope]) {
          throw new Error(`${targetDef.label} has no ${flagScope} config — it only supports: ${scopeNames.join(', ')}`);
        }

        const describeScope = (s) => s === 'project'
          ? `${targetDef.scopes.project()}  — this directory only`
          : `${targetDef.scopes.global()}  — all projects`;

        if (action === 'install') {
          let scope = flagScope || (scopeNames.length === 1 ? scopeNames[0] : null);
          if (!scope) {
            scope = await promptChoice(
              `Where should ${targetDef.label} pick up the server?`,
              scopeNames.map((s) => ({ name: s, hint: describeScope(s), value: s })),
              `'${target}' supports both a project and a global config — re-run with --project or --global.`,
            );
          }
          const configPath = installMcpConfig(target, scope);
          console.log(chalk.green('✓') + ' ' + chalk.white(`Wrote MCP config to ${configPath}`));
          console.log(chalk.cyan(`Restart ${targetDef.label} to pick it up.`));
          return;
        }

        // uninstall: a scope flag narrows it; with no flag, sweep every
        // scope the target supports and strip our entry wherever it is —
        // removal is precise (just the `ultragraph` key), so no prompt.
        const scopes = flagScope ? [flagScope] : scopeNames;
        let removedAny = false;
        for (const scope of scopes) {
          const { configPath, removed } = uninstallMcpConfig(target, scope);
          if (removed) {
            removedAny = true;
            console.log(chalk.green('✓') + ' ' + chalk.white(`Removed ultragraph from ${configPath}`));
          }
        }
        if (removedAny) {
          console.log(chalk.cyan(`Restart ${targetDef.label} to pick it up.`));
        } else {
          console.log(chalk.yellow('•') + ' ' + chalk.white(`No ultragraph entry found for ${targetDef.label} — nothing to do.`));
        }
        return;
      }
      await runMcpServer();
    }
  },
  help: {
    usage: '[command]',
    desc: 'Show help for commands',
    run: (args) => {
      if (args[0] && commands[args[0]]) {
        const cmd = commands[args[0]];
        return `${args[0]} ${cmd.usage}\n  ${cmd.desc}`;
      }
      return `Commands:\n${Object.entries(commands).map(([name, cmd]) =>
        `  ${name} ${cmd.usage}\n    ${cmd.desc}`
      ).join('\n')}`;
    }
  }
};

const cmd = process.argv[2];
const args = process.argv.slice(3);

if (!cmd) {
  console.log(quickstartBanner());
  process.exit(1);
}

if (cmd === 'help') {
  console.log(commands.help.run(args));
  process.exit(0);
}

if (commands[cmd]) {
  try {
    const start = Date.now();
    const result = commands[cmd].run(args);
    const handleResult = (res) => {
      const elapsed = ((Date.now() - start) / 1000).toFixed(2);
      if (res && typeof res === 'string' && res.startsWith('http')) {
        console.log(res);
      } else if (res && typeof res === 'object') {
        console.log(JSON.stringify(res, null, 2));
      }
      // Bare `mcp` keeps running (stdio transport) and must not print to
      // stdout, which is reserved for MCP's own JSON-RPC frames. `mcp
      // install`/`mcp uninstall` are normal one-shot commands and should
      // get the footer.
      const isMcpServer = cmd === 'mcp' && args[0] !== 'install' && args[0] !== 'uninstall';
      if (!isMcpServer && (cmd !== 'gen' || !args.includes('--no-ingest'))) {
        console.log(chalk.gray(`\nDone in ${elapsed}s`));
      }
    };
    if (result && typeof result.then === 'function') {
      result.then(handleResult).catch(e => {
        console.error(`Error: ${e.message}`);
        process.exit(1);
      });
    } else {
      handleResult(result);
    }
  } catch (e) {
    console.error(`Error: ${e.message}`);
    process.exit(1);
  }
} else {
  console.error(`Unknown command: ${cmd}`);
  console.error(`Run 'node cli.mjs help' for available commands`);
  process.exit(1);
}

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

const UG_VERSION = '0.1.0';

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

function embedderOptionsJson() {
  const o = {};
  if (process.env.UG_EMBED_BASE_URL) o.baseUrl = process.env.UG_EMBED_BASE_URL;
  if (process.env.UG_EMBED_API_KEY) o.apiKey = process.env.UG_EMBED_API_KEY;
  if (process.env.UG_EMBED_MODEL) o.model = process.env.UG_EMBED_MODEL;
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

const TraverseInput = z.object({
  startNodeIds: z.array(z.string()).min(1),
  hops: z.number().int().min(1).max(5).default(2),
  edgeTypes: z.array(z.string()).optional(),
  direction: z.enum(['outbound', 'inbound', 'both']).default('outbound'),
});

const FindUsagesInput = z.object({
  nodeId: z.string().min(1),
  hops: z.number().int().min(1).max(3).default(1),
  edgeTypes: z.array(z.string()).optional(),
});

// Tool registry — JSON Schema is what MCP wants on the wire; we keep
// zod for runtime validation and a hand-written JSON Schema for the
// tool list response. Avoiding a zod-to-json-schema dep keeps the
// install footprint tiny.
const MCP_TOOLS = [
  {
    name: 'search_kb',
    description:
      "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse_kb / find_usages. " +
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
    name: 'semantic_search_kb',
    description:
      'Lightweight pure-vector lookup over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance. Use this when search_kb would be overkill: ' +
      "(a) quick disambiguation ('which node is the user talking about?'), " +
      '(b) candidate generation before a deeper traverse_kb, ' +
      '(c) filtered lookups via whereClause (e.g. only Functions in a given folder). ' +
      'Cheaper and faster than search_kb. Switch to search_kb when you need actual code snippets or graph-aware ranking.',
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
    name: 'traverse_kb',
    description:
      'Walk the graph N hops from given seed node ids. The natural follow-up to search_kb / semantic_search_kb: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. Filters by edge type and direction. ' +
      "Use 'outbound' to see what the seed depends on; 'inbound' to see who depends on the seed. Output groups edges by type so the structure is easy to scan.",
    inputSchema: {
      type: 'object',
      properties: {
        startNodeIds: {
          type: 'array',
          items: { type: 'string' },
          description:
            'Seed node ids — typically copied from a prior search_kb / semantic_search_kb result.',
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
            'Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references.',
        },
        direction: {
          type: 'string',
          enum: ['outbound', 'inbound', 'both'],
          description:
            "Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on; 'both' = either.",
        },
      },
      required: ['startNodeIds'],
    },
  },
  {
    name: 'find_usages',
    description:
      "Find inbound references to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node. Convenience wrapper over traverse_kb with direction='inbound' and a sensible default edge-type set ['calls', 'references', 'imports', 'extends', 'implements']. " +
      "Use this when the user asks 'who uses X', 'what calls X', 'where is X imported', 'what would break if I change X', or before a refactor.",
    inputSchema: {
      type: 'object',
      properties: {
        nodeId: {
          type: 'string',
          description:
            'The node id to look up usages for. Get this from search_kb or semantic_search_kb.',
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
    name: 'ping_embedder',
    description:
      "Probe the configured embedding endpoint. Returns 'ok' on success or throws with the upstream error. Call this when search_kb / semantic_search_kb fails with an embedding-related error, or as a one-off health check before kicking off a batch of queries.",
    inputSchema: { type: 'object', properties: {} },
  },
];

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
    lines.push('- semantic_search_kb for a pure-vector pass with whereClause filters');
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
          `(snippet truncated — ${snip.omitted} more chars; read ${loc} for the full slice)`,
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
  lines.push(`- Walk neighbors:  traverse_kb({ startNodeIds: ["${topId}"], hops: 1 })`);
  lines.push(`- Find callers:    find_usages({ nodeId: "${topId}" })`);
  lines.push(
    `- Narrow search:   search_kb({ query: "...", whereClause: "node_type = 'Function'" })`,
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
    lines.push('No matches. Loosen the whereClause or try search_kb for graph-aware ranking.');
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
    `Next: search_kb({ query: "${query}" }) for graph-ranked snippets, or traverse_kb({ startNodeIds: ["${hits[0].id}"] }) to expand.`,
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
  lines.push('- Pick an interesting node id above and call traverse_kb again to keep walking.');
  lines.push('- Call search_kb with the node name to pull the actual source snippet.');
  return lines.join('\n');
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

// Each target: where its config lives, and how to graft a { command, args,
// env } server entry into that target's own JSON shape (schemas differ).
const MCP_INSTALL_TARGETS = {
  claude: {
    configPath: claudeDesktopConfigPath,
    apply: (config, server) => {
      config.mcpServers = config.mcpServers || {};
      config.mcpServers.ultragraph = server;
    },
  },
  cursor: {
    configPath: () => join(process.cwd(), '.cursor', 'mcp.json'),
    apply: (config, server) => {
      config.mcpServers = config.mcpServers || {};
      config.mcpServers.ultragraph = server;
    },
  },
  opencode: {
    configPath: () => join(process.cwd(), 'opencode.json'),
    apply: (config, server) => {
      if (config['$schema'] === undefined) config['$schema'] = 'https://opencode.ai/config.json';
      config.mcp = config.mcp || {};
      config.mcp.servers = config.mcp.servers || {};
      config.mcp.servers.ultragraph = { type: 'local', ...server, enabled: true };
    },
  },
};

// Writes (or merges into) an MCP client's config file so `ug` shows up as a
// tool source without the user hand-editing JSON / absolute paths themselves.
function installMcpConfig(target) {
  const targetDef = MCP_INSTALL_TARGETS[target];
  if (!targetDef) {
    throw new Error(`Unknown MCP target '${target}' (expected: ${Object.keys(MCP_INSTALL_TARGETS).join(', ')})`);
  }
  const configPath = targetDef.configPath();

  let config = {};
  if (existsSync(configPath)) {
    try {
      config = JSON.parse(readFileSync(configPath, 'utf-8'));
    } catch (e) {
      throw new Error(`${configPath} exists but isn't valid JSON — fix or remove it, then retry (${e.message})`);
    }
  }
  targetDef.apply(config, {
    command: 'node',
    args: [fileURLToPath(import.meta.url), 'mcp'],
    env: { UG_PROJECT: deriveProjectName('.') },
  });

  mkdirSync(dirname(configPath), { recursive: true });
  writeFileSync(configPath, JSON.stringify(config, null, 2) + '\n');
  return configPath;
}

async function runMcpServer() {
  const { dbPath: DB_PATH, repoRoot: REPO_ROOT } = resolveDbAndRoot();

  const server = new Server(
    { name: 'ultragraph', version: '0.2.0' },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: MCP_TOOLS,
  }));

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    const { name, arguments: rawArgs } = req.params;

    try {
      if (name === 'search_kb') {
        const args = SearchKbInput.parse(rawArgs ?? {});
        const opts = { ...args, repoRoot: REPO_ROOT };
        const json = await ug.dbHybridSearch(
          DB_PATH,
          JSON.stringify(opts),
          embedderOptionsJson(),
          destOptionsJson(),
        );
        const ctx = JSON.parse(json);
        return {
          content: [{ type: 'text', text: formatRankedContext(ctx) }],
        };
      }

      if (name === 'semantic_search_kb') {
        const args = SemanticSearchInput.parse(rawArgs ?? {});
        const json = await ug.dbSemanticSearch(
          DB_PATH,
          args.query,
          args.k ?? 10,
          args.whereClause ?? null,
          embedderOptionsJson(),
          destOptionsJson(),
        );
        const hits = JSON.parse(json);
        return {
          content: [{ type: 'text', text: formatSemanticHits(args.query, hits) }],
        };
      }

      if (name === 'traverse_kb') {
        const args = TraverseInput.parse(rawArgs ?? {});
        const json = await ug.dbTraverse(
          DB_PATH,
          args.startNodeIds,
          args.hops,
          args.edgeTypes ?? null,
          args.direction,
          destOptionsJson(),
        );
        const traversal = JSON.parse(json);
        const header = `Traversal from [${args.startNodeIds.join(', ')}] (hops=${args.hops}, dir=${args.direction})`;
        return {
          content: [{ type: 'text', text: formatTraversal(traversal, header) }],
        };
      }

      if (name === 'find_usages') {
        const args = FindUsagesInput.parse(rawArgs ?? {});
        const edgeTypes = args.edgeTypes ?? [
          'calls',
          'references',
          'imports',
          'extends',
          'implements',
        ];
        const json = await ug.dbTraverse(
          DB_PATH,
          [args.nodeId],
          args.hops,
          edgeTypes,
          'inbound',
          destOptionsJson(),
        );
        const traversal = JSON.parse(json);
        const header = `Usages of ${args.nodeId} (hops=${args.hops}, edges=[${edgeTypes.join(', ')}])`;
        return {
          content: [{ type: 'text', text: formatTraversal(traversal, header) }],
        };
      }

      if (name === 'ping_embedder') {
        const r = await ug.pingEmbedder(embedderOptionsJson());
        return { content: [{ type: 'text', text: r }] };
      }

      return {
        isError: true,
        content: [{ type: 'text', text: `Unknown tool: ${name}` }],
      };
    } catch (err) {
      return {
        isError: true,
        content: [{ type: 'text', text: `Error: ${err.message ?? String(err)}` }],
      };
    }
  });

  const transport = new StdioServerTransport();
  // stdout is reserved for MCP's JSON-RPC frames — log status to stderr.
  console.error('start ultragraph mcp server...');
  await server.connect(transport);
  console.error('ultragraph mcp server started.');
}

// Shown for bare `node cli.mjs` with no subcommand — a short "what do I do
// first" nudge instead of the full command wall (that's the `help` command).
function quickstartBanner() {
  return [
    chalk.bold('Welcome to UltraGraph') + ' — turn a codebase into a queryable knowledge graph.',
    '',
    chalk.bold('Quick start:'),
    '  ' + chalk.cyan('node cli.mjs gen') + '   Index this directory, build the graph, and ingest it (→ ~/.ug/<name>/)',
    '  ' + chalk.cyan('node cli.mjs mcp install claude') + '   Wire this up as an MCP server for Claude Desktop',
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

      console.log(chalk.bold('Embeddings') + chalk.gray(' (used by search_kb / traverse_kb / ping_embedder)'));
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
    usage: '[install <claude|cursor|opencode>]',
    desc: 'Start the MCP server (no args; see env vars in the source header), or write this project into an MCP client config with `install <target>`.',
    run: async (args) => {
      if (args[0] === 'install') {
        const target = args[1];
        if (!target) {
          throw new Error(`Usage: mcp install <${Object.keys(MCP_INSTALL_TARGETS).join('|')}>`);
        }
        const configPath = installMcpConfig(target);
        const appNames = { claude: 'Claude Desktop', cursor: 'Cursor', opencode: 'opencode' };
        console.log(chalk.green('✓') + ' ' + chalk.white(`Wrote MCP config to ${configPath}`));
        console.log(chalk.cyan(`Restart ${appNames[target] ?? target} to pick it up.`));
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
      // stdout, which is reserved for MCP's own JSON-RPC frames. `mcp install
      // ...` is a normal one-shot command and should get the footer.
      const isMcpServer = cmd === 'mcp' && args[0] !== 'install';
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

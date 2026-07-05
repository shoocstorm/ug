#!/usr/bin/env node
// MCP server exposing UltraGraph's GraphRAG knowledge-base tools.
//
// Configuration via env vars:
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
//
// Usage:
//   pnpm install
//   node node/mcp-server.mjs   # speaks MCP over stdio

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import { createRequire } from "module";
import { fileURLToPath } from "url";
import { dirname, resolve, join } from "path";
import { existsSync } from "fs";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));
const ug = require(resolve(__dirname, ".", "ultragraph.node"));
const project = require(resolve(__dirname, ".", "project.cjs"));

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
    const meta = project.readProjectMeta(dir);
    return {
      dbPath: join(dir, "ugdb"),
      repoRoot: process.env.UG_REPO_ROOT || meta?.repoRoot || process.cwd(),
    };
  };
  if (process.env.UG_PROJECT) {
    return fromProject(project.projectDir(process.env.UG_PROJECT));
  }
  const derived = project.projectDir(project.deriveProjectName("."));
  if (existsSync(join(derived, "ugdb"))) {
    return fromProject(derived);
  }
  return {
    dbPath: "./ugdb",
    repoRoot: process.env.UG_REPO_ROOT || process.cwd(),
  };
}

const { dbPath: DB_PATH, repoRoot: REPO_ROOT } = resolveDbAndRoot();

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
  const dest = (process.env.UG_DEST || "overgraph").toLowerCase();
  if (dest === "overgraph" || dest === "og") return null;
  if (dest === "neo4j" || dest === "neo") {
    const uri = process.env.UG_NEO4J_URI;
    const password = process.env.UG_NEO4J_PASSWORD;
    if (!uri) {
      throw new Error("UG_DEST=neo4j requires UG_NEO4J_URI");
    }
    if (!password) {
      throw new Error("UG_DEST=neo4j requires UG_NEO4J_PASSWORD");
    }
    return JSON.stringify({
      kind: "neo4j",
      uri,
      user: process.env.UG_NEO4J_USER || "neo4j",
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
  direction: z.enum(["outbound", "inbound", "both"]).optional(),
  maxChars: z.number().int().min(100).max(200000).optional(),
  mmrLambda: z.number().min(0).max(1).optional(),
  whereClause: z.string().optional(),
  includeSnippets: z.boolean().optional(),
  strategy: z.enum(["ppr", "mmr"]).optional(),
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
  direction: z.enum(["outbound", "inbound", "both"]).default("outbound"),
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
const TOOLS = [
  {
    name: "search_kb",
    description:
      "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse_kb / find_usages. " +
      "Trigger phrases include: 'how does X work', 'where is X', 'what is X', 'find / show me code for X', 'explain X', 'is there a function that...', 'how is X implemented', 'before I change X look up...', 'context on X', or any question whose answer likely lives in the repo. Prefer calling this once with a focused natural-language query over guessing file paths. " +
      "Internals: RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance. Pass strategy='mmr' for the legacy diversity-first BFS+MMR cascade.",
    inputSchema: {
      type: "object",
      properties: {
        query: {
          type: "string",
          description:
            "Natural-language query. Be specific — name the concept, function, or behavior you're after (e.g. 'how does the embedder probe its dim' beats 'embedder').",
        },
        k: {
          type: "integer",
          minimum: 1,
          maximum: 50,
          description:
            "How many context items to return (default 8). Bump to 15-20 when surveying a subsystem; keep 5-8 when answering a focused question.",
        },
        hops: {
          type: "integer",
          minimum: 0,
          maximum: 5,
          description:
            "MMR-only: graph expansion radius from each seed (default 2). Ignored under PPR.",
        },
        edgeTypes: {
          type: "array",
          items: { type: "string" },
          description:
            "Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. Leave unset for the default mix.",
        },
        direction: {
          type: "string",
          enum: ["outbound", "inbound", "both"],
          description:
            "Edge direction during the walk (default 'both'). Use 'inbound' when you care about who depends on the seed; 'outbound' for what the seed depends on.",
        },
        maxChars: {
          type: "integer",
          minimum: 100,
          maximum: 200000,
          description:
            "Approximate character budget for assembled context (default ~16k). Lower it when you only need a sketch.",
        },
        mmrLambda: {
          type: "number",
          minimum: 0,
          maximum: 1,
          description:
            "MMR balance (only when strategy='mmr'): 1 = max relevance, 0 = max diversity (default 0.6).",
        },
        whereClause: {
          type: "string",
          description:
            "Optional SQL WHERE applied during seed search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\".",
        },
        includeSnippets: {
          type: "boolean",
          description:
            "Read source slice for each item (default true). Set false when you only need IDs and locations for a follow-up traversal.",
        },
        strategy: {
          type: "string",
          enum: ["ppr", "mmr"],
          description:
            "Ranking strategy. 'ppr' (default) = Personalized PageRank seeded by RRF — best general-purpose. 'mmr' = legacy seed+BFS+MMR, prefer when you specifically want diversity over centrality.",
        },
        pprRestartProb: {
          type: "number",
          minimum: 0.01,
          maximum: 0.99,
          description:
            "PPR teleport probability (default 0.15). Higher = stay closer to seeds; lower = let centrality dominate.",
        },
        pprMaxIter: {
          type: "integer",
          minimum: 1,
          maximum: 200,
          description: "PPR power-iteration cap (default 30).",
        },
        pprSeedPool: {
          type: "integer",
          minimum: 1,
          maximum: 200,
          description:
            "How many RRF hits feed the personalization vector (default 16). Larger = more robust to a noisy top hit.",
        },
        pprEdgeWeights: {
          type: "object",
          additionalProperties: { type: "number", minimum: 0 },
          description:
            "Override edge-type weights, e.g. { calls: 1.0, imports: 0.7, contains: 0.3 }. Keys are case-insensitive.",
        },
      },
      required: ["query"],
    },
  },
  {
    name: "semantic_search_kb",
    description:
      "Lightweight pure-vector lookup over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance. Use this when search_kb would be overkill: " +
      "(a) quick disambiguation ('which node is the user talking about?'), " +
      "(b) candidate generation before a deeper traverse_kb, " +
      "(c) filtered lookups via whereClause (e.g. only Functions in a given folder). " +
      "Cheaper and faster than search_kb. Switch to search_kb when you need actual code snippets or graph-aware ranking.",
    inputSchema: {
      type: "object",
      properties: {
        query: {
          type: "string",
          description: "Natural-language query.",
        },
        k: {
          type: "integer",
          minimum: 1,
          maximum: 100,
          description: "How many candidate nodes to return (default 10).",
        },
        whereClause: {
          type: "string",
          description:
            "Optional SQL WHERE filter applied to the vector search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\", \"node_type IN ('Class','Interface')\".",
        },
      },
      required: ["query"],
    },
  },
  {
    name: "traverse_kb",
    description:
      "Walk the graph N hops from given seed node ids. The natural follow-up to search_kb / semantic_search_kb: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. Filters by edge type and direction. " +
      "Use 'outbound' to see what the seed depends on; 'inbound' to see who depends on the seed. Output groups edges by type so the structure is easy to scan.",
    inputSchema: {
      type: "object",
      properties: {
        startNodeIds: {
          type: "array",
          items: { type: "string" },
          description:
            "Seed node ids — typically copied from a prior search_kb / semantic_search_kb result.",
        },
        hops: {
          type: "integer",
          minimum: 1,
          maximum: 5,
          description: "Hop radius (default 2). Use 1 for direct neighbors only.",
        },
        edgeTypes: {
          type: "array",
          items: { type: "string" },
          description:
            "Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references.",
        },
        direction: {
          type: "string",
          enum: ["outbound", "inbound", "both"],
          description:
            "Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on; 'both' = either.",
        },
      },
      required: ["startNodeIds"],
    },
  },
  {
    name: "find_usages",
    description:
      "Find inbound references to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node. Convenience wrapper over traverse_kb with direction='inbound' and a sensible default edge-type set ['calls', 'references', 'imports', 'extends', 'implements']. " +
      "Use this when the user asks 'who uses X', 'what calls X', 'where is X imported', 'what would break if I change X', or before a refactor.",
    inputSchema: {
      type: "object",
      properties: {
        nodeId: {
          type: "string",
          description:
            "The node id to look up usages for. Get this from search_kb or semantic_search_kb.",
        },
        hops: {
          type: "integer",
          minimum: 1,
          maximum: 3,
          description:
            "How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages.",
        },
        edgeTypes: {
          type: "array",
          items: { type: "string" },
          description:
            "Override the default ['calls', 'references', 'imports', 'extends', 'implements'] set if you only care about a subset (e.g. ['calls']).",
        },
      },
      required: ["nodeId"],
    },
  },
  {
    name: "ping_embedder",
    description:
      "Probe the configured embedding endpoint. Returns 'ok' on success or throws with the upstream error. Call this when search_kb / semantic_search_kb fails with an embedding-related error, or as a one-off health check before kicking off a batch of queries.",
    inputSchema: { type: "object", properties: {} },
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
    .join(", ");
}

function formatRankedContext(ctx) {
  const lines = [];
  const items = ctx.items ?? [];

  lines.push(`# Knowledge-base results for: ${ctx.query}`);
  const meta = [`items=${items.length}`, `chars=${ctx.total_chars}`];
  if (ctx.seed_id) meta.push(`seed=${ctx.seed_id}`);
  if (items.length) meta.push(`types=[${summarizeNodeTypes(items)}]`);
  lines.push(meta.join("  •  "));
  lines.push("");

  if (!items.length) {
    lines.push("No matches. Try:");
    lines.push("- a broader query (drop qualifiers)");
    lines.push("- semantic_search_kb for a pure-vector pass with whereClause filters");
    lines.push("- ping_embedder to confirm the embedding endpoint is up");
    return lines.join("\n");
  }

  items.forEach((it, idx) => {
    const loc = it.file ? `${it.file}:${it.start_line}-${it.end_line}` : "(no file)";
    const score = typeof it.distance === "number" ? it.distance.toFixed(3) : "?";
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
      lines.push("```");
      lines.push(snip.text);
      lines.push("```");
      if (snip.truncated) {
        lines.push(
          `(snippet truncated — ${snip.omitted} more chars; read ${loc} for the full slice)`,
        );
      }
    }
    lines.push("");
  });

  // Drill-down hints. The agent has the tool list, but spelling out a
  // ready-to-paste call shaves a step off the loop.
  const topId = items[0].id;
  lines.push("---");
  lines.push("Drill-down hints:");
  lines.push(`- Walk neighbors:  traverse_kb({ startNodeIds: ["${topId}"], hops: 1 })`);
  lines.push(`- Find callers:    find_usages({ nodeId: "${topId}" })`);
  lines.push(
    `- Narrow search:   search_kb({ query: "...", whereClause: "node_type = 'Function'" })`,
  );
  lines.push(
    `- Read full file:  use the loc above (file:start-end) with your file-read tool`,
  );

  return lines.join("\n");
}

function formatSemanticHits(query, hits) {
  const lines = [];
  lines.push(`# Semantic search for: ${query}`);
  const meta = [`hits=${hits.length}`];
  if (hits.length) meta.push(`types=[${summarizeNodeTypes(hits)}]`);
  lines.push(meta.join("  •  "));
  lines.push("");

  if (!hits.length) {
    lines.push("No matches. Loosen the whereClause or try search_kb for graph-aware ranking.");
    return lines.join("\n");
  }

  hits.forEach((h, idx) => {
    const loc = h.file ? `${h.file}:${h.start_line}-${h.end_line}` : "(no file)";
    const score = typeof h.distance === "number" ? h.distance.toFixed(3) : "?";
    lines.push(`[${idx + 1}] ${h.node_type} ${h.name}  •  id=\`${h.id}\`  •  dist=${score}`);
    lines.push(`    ${loc}`);
    if (h.description) lines.push(`    ${h.description}`);
  });

  lines.push("");
  lines.push(
    `Next: search_kb({ query: "${query}" }) for graph-ranked snippets, or traverse_kb({ startNodeIds: ["${hits[0].id}"] }) to expand.`,
  );
  return lines.join("\n");
}

function formatTraversal(traversal, header) {
  const nodes = traversal.nodes ?? [];
  const edges = traversal.edges ?? [];
  const lines = [];
  lines.push(`# ${header}`);
  lines.push(`nodes=${nodes.length}  •  edges=${edges.length}`);
  lines.push("");

  if (!nodes.length) {
    lines.push("Empty neighborhood — the seed may be isolated or filters were too tight.");
    return lines.join("\n");
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
      const loc = n.file ? `  •  ${n.file}` : "";
      lines.push(`- ${n.node_type} ${n.name}  \`${n.id}\`${loc}`);
    }
    lines.push("");
  }

  // Group edges by type for a readable structural view.
  if (edges.length) {
    const byType = new Map();
    for (const e of edges) {
      const t = e.edge_type || "(unknown)";
      if (!byType.has(t)) byType.set(t, []);
      byType.get(t).push(e);
    }
    lines.push("## edges by type");
    for (const [t, es] of byType) {
      lines.push(`- ${t}: ${es.length}`);
      // Show up to 8 examples per type — enough for the agent to spot
      // the pattern without flooding the prompt.
      for (const e of es.slice(0, 8)) {
        lines.push(`  - ${e.source}  →  ${e.target}`);
      }
      if (es.length > 8) lines.push(`  - … and ${es.length - 8} more`);
    }
    lines.push("");
  }

  lines.push("Drill-down hints:");
  lines.push("- Pick an interesting node id above and call traverse_kb again to keep walking.");
  lines.push("- Call search_kb with the node name to pull the actual source snippet.");
  return lines.join("\n");
}

const server = new Server(
  { name: "ultragraph", version: "0.2.0" },
  { capabilities: { tools: {} } },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: TOOLS,
}));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const { name, arguments: rawArgs } = req.params;

  try {
    if (name === "search_kb") {
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
        content: [{ type: "text", text: formatRankedContext(ctx) }],
      };
    }

    if (name === "semantic_search_kb") {
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
        content: [{ type: "text", text: formatSemanticHits(args.query, hits) }],
      };
    }

    if (name === "traverse_kb") {
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
      const header = `Traversal from [${args.startNodeIds.join(", ")}] (hops=${args.hops}, dir=${args.direction})`;
      return {
        content: [{ type: "text", text: formatTraversal(traversal, header) }],
      };
    }

    if (name === "find_usages") {
      const args = FindUsagesInput.parse(rawArgs ?? {});
      const edgeTypes = args.edgeTypes ?? [
        "calls",
        "references",
        "imports",
        "extends",
        "implements",
      ];
      const json = await ug.dbTraverse(
        DB_PATH,
        [args.nodeId],
        args.hops,
        edgeTypes,
        "inbound",
        destOptionsJson(),
      );
      const traversal = JSON.parse(json);
      const header = `Usages of ${args.nodeId} (hops=${args.hops}, edges=[${edgeTypes.join(", ")}])`;
      return {
        content: [{ type: "text", text: formatTraversal(traversal, header) }],
      };
    }

    if (name === "ping_embedder") {
      const r = await ug.pingEmbedder(embedderOptionsJson());
      return { content: [{ type: "text", text: r }] };
    }

    return {
      isError: true,
      content: [{ type: "text", text: `Unknown tool: ${name}` }],
    };
  } catch (err) {
    return {
      isError: true,
      content: [{ type: "text", text: `Error: ${err.message ?? String(err)}` }],
    };
  }
});

const transport = new StdioServerTransport();
console.log("start ultragraph mcp server...");
await server.connect(transport);
console.log("ultragraph mcp server started.");

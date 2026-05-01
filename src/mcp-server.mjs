#!/usr/bin/env node
// MCP server exposing the Phase 4 GraphRAG `search_kb` tool.
//
// Configuration via env vars:
//   UG_DB_PATH         - LanceDB directory (default: ./ug-out/ug-db)
//   UG_REPO_ROOT       - root for resolving snippet file paths (default: cwd)
//   UG_EMBED_BASE_URL  - override embedding endpoint base URL
//   UG_EMBED_API_KEY   - override embedding API key
//   UG_EMBED_MODEL     - override embedding model name
//
// Usage:
//   pnpm install
//   node src/mcp-server.mjs   # speaks MCP over stdio

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import { createRequire } from "module";
import { fileURLToPath } from "url";
import { dirname, resolve } from "path";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));
const ug = require(resolve(__dirname, ".", "ultragraph-kb.node"));

const DB_PATH = process.env.UG_DB_PATH || "./ug-db";
const REPO_ROOT = process.env.UG_REPO_ROOT || process.cwd();

function embedderOptionsJson() {
  const o = {};
  if (process.env.UG_EMBED_BASE_URL) o.baseUrl = process.env.UG_EMBED_BASE_URL;
  if (process.env.UG_EMBED_API_KEY) o.apiKey = process.env.UG_EMBED_API_KEY;
  if (process.env.UG_EMBED_MODEL) o.model = process.env.UG_EMBED_MODEL;
  return Object.keys(o).length ? JSON.stringify(o) : null;
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

const TraverseInput = z.object({
  startNodeIds: z.array(z.string()).min(1),
  hops: z.number().int().min(1).max(5).default(2),
  edgeTypes: z.array(z.string()).optional(),
  direction: z.enum(["outbound", "inbound", "both"]).default("outbound"),
});

// Tool registry — JSON Schema is what MCP wants on the wire; we keep
// zod for runtime validation and a hand-written JSON Schema for the
// tool list response. Avoiding a zod-to-json-schema dep keeps the
// install footprint tiny.
const TOOLS = [
  {
    name: "search_kb",
    description:
      "Graph-based RAG retrieval. RRF (vector+FTS) seeds become a Personalized PageRank personalization vector over the edge graph; PPR scores combine seed proximity with structural centrality (replaces the older BFS+MMR cascade). Set strategy='mmr' for the legacy diversity-first path.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Natural-language query." },
        k: {
          type: "integer",
          minimum: 1,
          maximum: 50,
          description: "Number of context items to return (default 8).",
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
            "Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references.",
        },
        direction: {
          type: "string",
          enum: ["outbound", "inbound", "both"],
          description:
            "Edge direction during the walk (default 'both').",
        },
        maxChars: {
          type: "integer",
          minimum: 100,
          maximum: 200000,
          description: "Approximate character budget for assembled context.",
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
          description: "Optional SQL WHERE applied during seed search.",
        },
        includeSnippets: {
          type: "boolean",
          description:
            "Read source slice for each item (default true).",
        },
        strategy: {
          type: "string",
          enum: ["ppr", "mmr"],
          description:
            "Ranking strategy. 'ppr' (default) = Personalized PageRank seeded by RRF. 'mmr' = legacy seed+BFS+MMR.",
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
    name: "traverse_kb",
    description:
      "Walk the graph N hops from given seed node ids. Filters by edge type and direction.",
    inputSchema: {
      type: "object",
      properties: {
        startNodeIds: {
          type: "array",
          items: { type: "string" },
          description: "Seed node ids.",
        },
        hops: {
          type: "integer",
          minimum: 1,
          maximum: 5,
          description: "Hop radius (default 2).",
        },
        edgeTypes: { type: "array", items: { type: "string" } },
        direction: {
          type: "string",
          enum: ["outbound", "inbound", "both"],
        },
      },
      required: ["startNodeIds"],
    },
  },
  {
    name: "ping_embedder",
    description:
      "Probe the configured embedding endpoint. Use this to confirm the local embedding server is running before larger queries.",
    inputSchema: { type: "object", properties: {} },
  },
];

function formatRankedContext(ctx) {
  // Compact, prompt-friendly text. Each item gets a header line plus its
  // snippet when available. Distance is shown so the model can weigh
  // relevance; hop tells it whether the item came from the seed or
  // graph expansion.
  const lines = [];
  lines.push(`Query: ${ctx.query}`);
  if (ctx.seed_id) lines.push(`Seed: ${ctx.seed_id}`);
  lines.push(`Items: ${ctx.items.length}  TotalChars: ${ctx.total_chars}`);
  lines.push("");
  for (const it of ctx.items) {
    const loc = it.file ? `${it.file}:${it.start_line}-${it.end_line}` : "(no file)";
    lines.push(
      `## ${it.node_type} ${it.name}  [hop=${it.hop} dist=${it.distance.toFixed(3)}]`
    );
    lines.push(loc);
    if (it.description) lines.push(`> ${it.description}`);
    if (it.snippet) {
      lines.push("```");
      lines.push(it.snippet.trimEnd());
      lines.push("```");
    }
    lines.push("");
  }
  return lines.join("\n");
}

const server = new Server(
  { name: "ultragraph-kb", version: "0.1.0" },
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
      const json = await ug.dbHybridSearch(DB_PATH, JSON.stringify(opts), embedderOptionsJson());
      const ctx = JSON.parse(json);
      return {
        content: [{ type: "text", text: formatRankedContext(ctx) }],
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
      );
      return {
        content: [{ type: "text", text: json }],
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
await server.connect(transport);

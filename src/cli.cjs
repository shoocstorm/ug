#!/usr/bin/env node

const { join, dirname } = require('path');
const { readFileSync, existsSync, writeFileSync, mkdirSync, copyFileSync } = require('fs');

const binding = join(dirname(__dirname), 'native', 'ultragraph-kb.node');
const ug = require(binding);

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
  const baseUrl = extractFlag(args, '--base-url');
  const apiKey = extractFlag(args, '--api-key');
  const model = extractFlag(args, '--model');
  if (!baseUrl && !apiKey && !model) return null;
  const opts = {};
  if (baseUrl) opts.baseUrl = baseUrl;
  if (apiKey) opts.apiKey = apiKey;
  if (model) opts.model = model;
  return opts;
}

const commands = {
  index: {
    usage: '[<input dir>] [-i|--input <dir>] [--cache <cache-dir>] [--output <output-path>]',
    desc: 'Index a directory and output the symbol tree as JSON into a file specified by `--output` (default: `out/indexed-tree.json`). Use `--cache` to speed up re-indexing.',
    run: (args) => {
      const inputIdx = args.indexOf('-i') >= 0 ? args.indexOf('-i') : args.indexOf('--input');
      const path = inputIdx >= 0 ? args[inputIdx + 1] : (args[0] || '.');
      const cacheIdx = args.indexOf('--cache') >= 0 ? args.indexOf('--cache') : args.indexOf('-c');
      const cachePath = cacheIdx >= 0 ? args[cacheIdx + 1] : null;
      const outputIdx = args.indexOf('--output') >= 0 ? args.indexOf('--output') : args.indexOf('-o');
      const outputPath = outputIdx >= 0 ? args[outputIdx + 1] : 'out/indexed-tree.json';

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
    usage: '[<indexed-tree-json-file>] [--output <output-path>]',
    desc: 'Build graph from index result (i.e.: out/indexed-tree.json) and generates graph.json',
    run: (args) => {
      const pathIdx = args.indexOf('--input');
      const path = pathIdx >= 0 ? args[pathIdx + 1] : (args.length ? args[0] : 'out/indexed-tree.json');
      const outputIdx = args.indexOf('--output');
      const outputPath = outputIdx >= 0 ? args[outputIdx + 1] : 'out/graph.json';

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
    usage: '[--input <path>] [--cache <cache-dir>] [--output <output-dir>]',
    desc: 'Index directory and generate graph + visualization',
    run: (args) => {
      const pathIdx = args.indexOf('--input');
      const path = pathIdx >= 0 ? args[pathIdx + 1] : (args.length ? args[0] : '.');
      const cacheIdx = args.indexOf('--cache');
      const cachePath = cacheIdx >= 0 ? args[cacheIdx + 1] : null;
      const outputIdx = args.indexOf('--output');
      const outputDir = outputIdx >= 0 ? args[outputIdx + 1] : 'out';

      // Ensure output directory exists
      if (!existsSync(outputDir)) {
        mkdirSync(outputDir, { recursive: true });
      }

      // Generate graph
      let result;
      if (cachePath) {
        result = ug.indexWithCache(path, cachePath);
      } else {
        result = ug.index(path);
      }
      const index = JSON.parse(result);
      const json = JSON.stringify(index);
      const graph = ug.buildGraph(json);

      // Write graph.json
      const graphPath = join(outputDir, 'graph.json');
      writeFileSync(graphPath, graph);

      // Copy visualization files
      const visSrc = join(__dirname, 'vis');
      const indexHtmlSrc = join(visSrc, 'visualization.html');
      const indexMdSrc = join(visSrc, 'visualization.md');

      if (existsSync(indexHtmlSrc)) {
        copyFileSync(indexHtmlSrc, join(outputDir, 'index.html'));
      }
      if (existsSync(indexMdSrc)) {
        copyFileSync(indexMdSrc, join(outputDir, 'README.md'));
      }

      console.log(`Generated in ${outputDir}/:`);
      console.log('  - graph.json');
      console.log('  - index.html (open in browser with HTTP server)');
      console.log('  - README.md');

      //return JSON.parse(graph);
      return `Visit http://localhost:8080 to view the graph`;
    }
  },
  'graph-search': {
    usage: '<graph-json-file> <keyword> [--type <node-type>]... [--output <output-path>]',
    desc: 'Graph-based: Keyword search over in-memory graph nodes (case-insensitive substring on name/docstring).',
    run: (args) => {
      if (args.length < 2) {
        throw new Error(`Usage: graph-search ${commands['graph-search'].usage}\n  ${commands['graph-search'].desc}`);
      }
      const file = args[0];
      const keyword = args[1];
      const nodeTypes = [];
      let outputPath = null;
      for (let i = 2; i < args.length; i++) {
        const a = args[i];
        if (a === '--type' || a === '-t') {
          if (i + 1 < args.length) nodeTypes.push(args[++i]);
        } else if (a === '--output' || a === '-o') {
          if (i + 1 < args.length) outputPath = args[++i];
        }
      }
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
    usage: '<graph-json-file> <db-path> [--base-url <url>] [--api-key <key>] [--model <name>]',
    desc: 'LanceDB: Embed graph nodes and write to LanceDB. Requires a running embedding endpoint.',
    run: async (args) => {
      if (args.length < 2) {
        throw new Error(`Usage: db-ingest ${commands['db-ingest'].usage}\n  ${commands['db-ingest'].desc}`);
      }
      const graphFile = args[0];
      const dbPath = args[1];
      const embedderOptions = parseEmbedderOptions(args.slice(2));
      const graphJson = readFileSync(graphFile, 'utf-8');
      const result = await ug.dbIngest(graphJson, dbPath, embedderOptions ? JSON.stringify(embedderOptions) : null);
      return JSON.parse(result);
    }
  },
  'db-traverse': {
    usage: '<db-path> <start-node-id> [-k <hops>] [--edge-type <type>]... [--direction <outbound|inbound|both>]',
    desc: 'LanceDB: K-hop BFS traversal using edges table with optional edge-type filtering.',
    run: async (args) => {
      if (args.length < 3) {
        throw new Error(`Usage: db-traverse ${commands['db-traverse'].usage}\n  ${commands['db-traverse'].desc}`);
      }
      const dbPath = args[0];
      const startNodeId = args[1];
      const hops = extractArg(args.slice(2), '-k', '--hops', 2);
      const edgeTypes = extractMultiFlags(args.slice(2), '--edge-type');
      const direction = extractFlag(args.slice(2), '--direction') || 'outbound';
      const result = await ug.dbTraverse(dbPath, [startNodeId], hops, edgeTypes.length ? edgeTypes : null, direction);
      return JSON.parse(result);
    }
  },
  'db-rag': {
    usage: '<db-path> <query> [-k <limit>] [--base-url <url>] [--api-key <key>] [--model <name>]',
    desc: 'LanceDB: End-to-end GraphRAG hybrid retrieval (vector + FTS + graph expansion).',
    run: async (args) => {
      if (args.length < 2) {
        throw new Error(`Usage: db-rag ${commands['db-rag'].usage}\n  ${commands['db-rag'].desc}`);
      }
      const dbPath = args[0];
      const query = args[1];
      const rest = args.slice(2);
      const k = extractArg(rest, '-k', '--limit', 10);
      const embedderOptions = parseEmbedderOptions(rest);
      const optionsJson = JSON.stringify({ query, k, maxHops: 2 });
      const result = await ug.dbHybridSearch(dbPath, optionsJson, embedderOptions ? JSON.stringify(embedderOptions) : null);
      return JSON.parse(result);
    }
  },
  ping: {
    usage: '[--base-url <url>] [--api-key <key>] [--model <name>]',
    desc: 'Probe the embedding endpoint to verify connectivity.',
    run: async (args) => {
      const embedderOptions = parseEmbedderOptions(args);
      const result = await ug.pingEmbedder(embedderOptions ? JSON.stringify(embedderOptions) : null);
      return result;
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

if (!cmd || cmd === 'help') {
  const cmdArgs = cmd ? args : [];
  console.log(commands.help.run(cmdArgs));
  process.exit(cmd ? 0 : 1);
}

if (commands[cmd]) {
  try {
    const start = Date.now();
    const result = commands[cmd].run(args);
    const handleResult = (res) => {
      const elapsed = ((Date.now() - start) / 1000).toFixed(2);
      console.log(JSON.stringify(res, null, 2));
      console.log(`\nDone in ${elapsed}s`);
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
  console.error(`Run 'node cli.cjs help' for available commands`);
  process.exit(1);
}
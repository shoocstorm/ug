#!/usr/bin/env node

const { join, dirname, resolve } = require('path');
const { readFileSync, existsSync, writeFileSync, mkdirSync, copyFileSync, realpathSync } = require('fs');
const chalk = require('chalk');
chalk.level = 2;

const project = require('./project.cjs');

const binding = join(dirname(__dirname), '.ug', 'ultragraph.node');
const ug = require(binding);

// Project name for an invocation: -n/--name flag wins, else derived
// from the given input path's basename (see project.cjs).
function resolveProjectName(args, inputPath) {
  const flagged = extractFlag(args, '-n') || extractFlag(args, '--name');
  if (flagged) return project.sanitizeName(flagged);
  return project.deriveProjectName(inputPath || '.');
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

const commands = {
  index: {
    usage: '[<input dir>] [-i|--input <dir>] [-n|--name <project>] [-c|--cache <cache-dir>] [-o|--output <output-path>]',
    desc: 'Index a directory and output the symbol tree as JSON into a file specified by `--output` (default: `~/.ug/<name>/indexed-tree.json`). Use `--cache` to speed up re-indexing.',
    run: (args) => {
      const path = extractFlag(args, '-i') || extractFlag(args, '--input') || (args[0] || '.');
      const cachePath = extractFlag(args, '-c') || extractFlag(args, '--cache');
      const outputPath = extractFlag(args, '-o') || extractFlag(args, '--output')
        || join(project.projectDir(resolveProjectName(args, path)), 'indexed-tree.json');

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
      const projectDir = project.projectDir(resolveProjectName(args, '.'));
      const path = extractFlag(args, '-i') || extractFlag(args, '--input')
        || (args.length && !args[0].startsWith('-') ? args[0] : join(projectDir, 'indexed-tree.json'));
      const outputPath = extractFlag(args, '-o') || extractFlag(args, '--output') || join(projectDir, 'graph.json');

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
        || project.projectDir(projectName);

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
      project.writeProjectMeta(outputDir, {
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
        console.warn(chalk.gray('    node src/cli.cjs db-ingest') + ' ' + chalk.white(graphPath + ' ' + dbPath));
      }

      console.log(chalk.gray('────────────────────────────────────────'));
      console.log(chalk.cyan('Run "ug serve" and visit http://localhost:8080 to view the graph'));
      console.log(chalk.cyan(`Run "node node/cli.cjs db-rag -i ${dbPath} hello" to perform a RAG query on the DB.`));

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
      const projects = project.listProjects();
      const root = project.ugHome();
      if (!projects.length) {
        return `No projects found in ${root}. Run \`node node/cli.cjs gen\` in a repo to create one.`;
      }
      const cwdName = project.deriveProjectName('.');
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
      if (res && typeof res === 'string' && res.startsWith('http')) {
        console.log(res);
      } else if (res && typeof res === 'object') {
        console.log(JSON.stringify(res, null, 2));
      }
      if (cmd !== 'gen' || !args.includes('--no-ingest')) {
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
  console.error(`Run 'node cli.cjs help' for available commands`);
  process.exit(1);
}
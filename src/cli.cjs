#!/usr/bin/env node

const { join, dirname } = require('path');
const { readFileSync, existsSync, writeFileSync, mkdirSync, copyFileSync } = require('fs');

const binding = join(dirname(__dirname), 'native', 'ultragraph-kb.node');
const ug = require(binding);

const commands = {
  index: {
    usage: '[<input dir>] [--cache <cache-dir>] [--output <output-path>]',
    desc: 'Index a directory and output the symbol tree as JSON into a file specified by `--output` (default: `out/indexed-tree.json`). Use `--cache` to speed up re-indexing.',
    run: (args) => {
      const path = args[0] || '.';
      const cacheIdx = args.indexOf('--cache');
      const cachePath = cacheIdx >= 0 ? args[cacheIdx + 1] : null;
      const outputIdx = args.indexOf('--output');
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
    desc: 'Build graph from index result (i.e.: out/indexed-tree.json)',
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
  search: {
    usage: '<graph-json-file> <start-node-id> <k>',
    desc: 'Perform K-hop BFS traversal',
    run: (args) => {
      if (args.length < 3) {
        throw new Error('Usage: k-hop-bfs <graph-json-file> <start-node-id> <k>');
      }
      const [file, startNodeId, k] = args;
      const graphJson = readFileSync(file, 'utf-8');
      const result = ug.kHopBfs(graphJson, startNodeId, parseInt(k, 10));
      return JSON.parse(result);
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
    const result = commands[cmd].run(args);
    console.log(JSON.stringify(result, null, 2));
  } catch (e) {
    console.error(`Error: ${e.message}`);
    process.exit(1);
  }
} else {
  console.error(`Unknown command: ${cmd}`);
  console.error(`Run 'node cli.cjs help' for available commands`);
  process.exit(1);
}
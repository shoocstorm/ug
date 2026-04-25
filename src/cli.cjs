#!/usr/bin/env node

const { join, dirname } = require('path');
const { readFileSync, existsSync, writeFileSync, mkdirSync, copyFileSync } = require('fs');

const binding = join(dirname(__dirname), 'native', 'ultragraph-kb.node');
const ug = require(binding);

const commands = {
  index: {
    usage: '<path> [--cache <cache-dir>]',
    desc: 'Index a directory and return symbols',
    run: (args) => {
      const path = args[0] || '.';
      const cacheIdx = args.indexOf('--cache');
      const cachePath = cacheIdx >= 0 ? args[cacheIdx + 1] : null;

      let result;
      if (cachePath) {
        result = ug.indexWithCache(path, cachePath);
      } else {
        result = ug.index(path);
      }
      return JSON.parse(result);
    }
  },
  graph: {
    usage: '<index-json-file>',
    desc: 'Build graph from index result',
    run: (args) => {
      if (!args[0]) {
        throw new Error('Usage: build-graph <index-json-file>');
      }
      const indexJson = readFileSync(args[0], 'utf-8');
      const index = JSON.parse(indexJson);
      const json = JSON.stringify(index);
      const result = ug.buildGraph(json);
      return JSON.parse(result);
    }
  },
  gen: {
    usage: '<path> [--cache <cache-dir>] [--output <output-dir>]',
    desc: 'Index directory and generate graph + visualization',
    run: (args) => {
      const path = args[0] || '.';
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

      return JSON.parse(graph);
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
#!/usr/bin/env node
// Bundles node/cli.mjs (+ its npm deps: chalk, zod, @modelcontextprotocol/sdk)
// into a single dependency-free .ug/cli.mjs. Needed because release archives
// only ship the .ug/ folder, not node_modules — a plain file copy left
// `require('chalk')` etc. unresolvable for anyone who downloads a release
// standalone (no repo/node_modules around it). The dynamic
// `require(join(..., 'ug.node'))` call for the native addon is a computed
// expression, so esbuild can't (and shouldn't) inline it — it stays a normal
// runtime require against the .ug/ug.node file sitting next to this bundle.
import { build } from 'esbuild';
import { existsSync, cpSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));

await build({
  entryPoints: [join(repoRoot, 'node', 'cli.mjs')],
  outfile: join(repoRoot, '.ug', 'cli.mjs'),
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node20',
  // Bundled CJS deps (chalk, etc.) call `require()` internally. ESM has no
  // ambient `require`, so esbuild's CJS-interop shim throws unless one
  // exists — define it via createRequire before any bundled code runs.
  banner: { js: "import { createRequire as __createRequire } from 'node:module';\nconst require = __createRequire(import.meta.url);" },
});

// Copy the skill directory alongside the bundled cli.mjs so the MCP install
// command can find it when running from the release archive (which ships
// .ug/ without the repo's node/ directory tree).
const srcSkill = join(repoRoot, 'node', 'ug-mcp-skill');
const dstSkill = join(repoRoot, '.ug', 'ug-mcp-skill');
if (existsSync(srcSkill)) {
  cpSync(srcSkill, dstSkill, { recursive: true, force: true });
  console.error(`Copied skill: ${srcSkill} → ${dstSkill}`);
}

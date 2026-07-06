#!/usr/bin/env node
// Cross-platform replacement for `cp ../node/cli.mjs ../.ug/` in package.json's
// build script. Needed because Windows has no `cp`.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const distDir = join(repoRoot, '.ug');

mkdirSync(distDir, { recursive: true });
copyFileSync(join(repoRoot, 'node', 'cli.mjs'), join(distDir, 'cli.mjs'));

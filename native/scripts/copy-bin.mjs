#!/usr/bin/env node
// Cross-platform replacement for `mkdir -p ../.ug && cp target/<profile>/ug ../.ug/ug`.
// Needed because Windows has neither `cp` nor `mkdir -p`, and the `ug` binary
// gains a `.exe` suffix there.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const nativeDir = dirname(dirname(fileURLToPath(import.meta.url)));
const repoRoot = dirname(nativeDir);
const profileDir = process.argv[2] ?? 'release';
const ext = process.platform === 'win32' ? '.exe' : '';
const distDir = join(repoRoot, '.ug');

mkdirSync(distDir, { recursive: true });
copyFileSync(join(nativeDir, 'target', profileDir, `ug${ext}`), join(distDir, `ug${ext}`));

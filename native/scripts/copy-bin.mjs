#!/usr/bin/env node
// Cross-platform replacement for `mkdir -p ../.ug && cp target/<profile>/ug ../.ug/ug`.
// Needed because Windows has neither `cp` nor `mkdir -p`, and the `ug` binary
// gains a `.exe` suffix there.
import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const nativeDir = dirname(dirname(fileURLToPath(import.meta.url)));
const repoRoot = dirname(nativeDir);
const profileDir = process.argv[2] ?? 'release';
const ext = process.platform === 'win32' ? '.exe' : '';
const distDir = join(repoRoot, '.ug');

mkdirSync(distDir, { recursive: true });
copyFileSync(join(nativeDir, 'target', profileDir, `ug${ext}`), join(distDir, `ug${ext}`));
// Desktop shell (native/src/bin/ug_app.rs) — launched by `ug app` alongside
// `ug serve`. Built as a matter of course since `cargo build` with no
// --bin filter builds every [[bin]] target in the crate.
copyFileSync(join(nativeDir, 'target', profileDir, `ug-app${ext}`), join(distDir, `ug-app${ext}`));

// liteparse (used by indexer/document.rs for PDF/Word/Excel/PowerPoint
// indexing) loads PDFium at runtime via `libloading` rather than linking it
// in — its `liteparse-pdfium-sys` build script downloads a prebuilt shared
// library and copies it into `target/<profile>/deps/`. It isn't referenced
// by cargo's own artifact list, so it's not something `napi build` or the
// `ug`/`ug-app` copies above pick up on their own. pdfium's runtime loader
// searches next to the loaded native module (`.ug/ug.node`, via dladdr) as
// one of its resolution paths, so dropping it in `distDir` here is enough —
// no env var or extra wiring needed at runtime.
const pdfiumName =
  process.platform === 'darwin' ? 'libpdfium.dylib'
  : process.platform === 'win32' ? 'pdfium.dll'
  : 'libpdfium.so';
const pdfiumSrc = join(nativeDir, 'target', profileDir, 'deps', pdfiumName);
if (existsSync(pdfiumSrc)) {
  copyFileSync(pdfiumSrc, join(distDir, pdfiumName));
} else {
  console.warn(`copy-bin: ${pdfiumName} not found at ${pdfiumSrc} — PDF/Word/Excel/PowerPoint indexing will fail to load PDFium at runtime.`);
}

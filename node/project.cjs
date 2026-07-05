// Project-folder resolution for the ~/.ug/<project> data layout.
// Mirrors native/src/project.rs — keep the two in sync. All
// project.json reads/writes go through here so the metadata backend
// can later be swapped for the project's own OverGraph db.

const { join, basename, resolve } = require('path');
const { homedir } = require('os');
const { existsSync, readFileSync, writeFileSync, mkdirSync, readdirSync, statSync, realpathSync } = require('fs');

const UG_VERSION = '0.1.0';

function ugHome() {
  const env = process.env.UG_HOME;
  if (env && env.trim()) return env;
  return join(homedir(), '.ug');
}

// Chars outside [A-Za-z0-9._-] become '-'; leading '.'/'-' stripped;
// capped at 64 chars; empty or './..' fall back to "default".
function sanitizeName(raw) {
  const mapped = String(raw).trim().replace(/[^A-Za-z0-9._-]/g, '-');
  const stripped = mapped.replace(/^[.-]+/, '').slice(0, 64);
  if (!stripped || stripped === '.' || stripped === '..') return 'default';
  return stripped;
}

function deriveProjectName(inputPath) {
  let canon;
  try {
    canon = realpathSync(resolve(inputPath || '.'));
  } catch {
    canon = resolve(inputPath || '.');
  }
  const base = basename(canon);
  return base ? sanitizeName(base) : 'default';
}

function projectDir(name) {
  return join(ugHome(), sanitizeName(name));
}

function metaPath(dir) {
  return join(dir, 'project.json');
}

function readProjectMeta(dir) {
  try {
    return JSON.parse(readFileSync(metaPath(dir), 'utf-8'));
  } catch {
    return null;
  }
}

// Writes project.json, preserving createdAt from any existing file.
function writeProjectMeta(dir, meta) {
  const now = Math.floor(Date.now() / 1000);
  const existing = readProjectMeta(dir);
  const out = {
    name: meta.name,
    repoRoot: meta.repoRoot || '',
    createdAt: existing && existing.createdAt ? existing.createdAt : now,
    updatedAt: now,
    nodes: meta.nodes || 0,
    edges: meta.edges || 0,
    ugVersion: meta.ugVersion || UG_VERSION,
  };
  mkdirSync(dir, { recursive: true });
  writeFileSync(metaPath(dir), JSON.stringify(out, null, 2));
  return out;
}

// Subdirs of ugHome() containing project.json or graph.json, sorted by
// updatedAt descending. Synthesizes metadata when project.json is missing.
function listProjects() {
  const root = ugHome();
  if (!existsSync(root)) return [];
  const out = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const dir = join(root, entry.name);
    const meta = readProjectMeta(dir);
    if (meta) {
      out.push({ dir, meta });
      continue;
    }
    const graph = join(dir, 'graph.json');
    if (existsSync(graph)) {
      let mtime = 0;
      try {
        mtime = Math.floor(statSync(graph).mtimeMs / 1000);
      } catch {}
      out.push({
        dir,
        meta: {
          name: entry.name, repoRoot: '', createdAt: mtime, updatedAt: mtime,
          nodes: 0, edges: 0, ugVersion: '',
        },
      });
    }
  }
  out.sort((a, b) => (b.meta.updatedAt || 0) - (a.meta.updatedAt || 0));
  return out;
}

// Default db path for read commands when no explicit path is given:
// ~/.ug/<cwd-basename>/ugdb if it exists → legacy ./.ug/ugdb if it
// exists → ~/.ug/<cwd-basename>/ugdb (errors point at the new layout).
function defaultReadDbPath() {
  const newPath = join(projectDir(deriveProjectName('.')), 'ugdb');
  if (existsSync(newPath)) return newPath;
  if (existsSync('.ug/ugdb')) return '.ug/ugdb';
  return newPath;
}

module.exports = {
  ugHome,
  sanitizeName,
  deriveProjectName,
  projectDir,
  readProjectMeta,
  writeProjectMeta,
  listProjects,
  defaultReadDbPath,
};

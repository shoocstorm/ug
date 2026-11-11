#!/usr/bin/env bash
#
# package-skill.sh — the `ug` agent skill as a .tar.gz and a .zip, for skill
# marketplaces. agensi.io accepts only the .zip; both hold the same files.
#
# The skill body is native/src/mcp/ug-skill.md — the same file the binary
# embeds and `ug connect --cli` installs — so the package cannot drift from
# what ships. Output: dist/ug-skill-v<version>.{tar.gz,zip}, containing
#
#   ug/SKILL.md      the skill, verbatim
#   ug/README.md     what it is, the prerequisite (the ug binary), install steps
#
# Usage: scripts/package-skill.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/native/Cargo.toml" | head -1)"
OUT="$ROOT/dist"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/ug" "$OUT"
cp "$ROOT/native/src/mcp/ug-skill.md" "$STAGE/ug/SKILL.md"

cat > "$STAGE/ug/README.md" <<EOF
# ug — codebase knowledge-graph skill for AI coding agents

Version ${VERSION} · https://ultra-graph.web.app · https://github.com/shoocstorm/ug

This skill teaches an agent (Claude Code, Cursor, Codex, Gemini CLI, opencode,
Windsurf, …) to answer questions about a codebase by querying its **ug
(UltraGraph)** knowledge graph instead of grepping and reading files:

- **Before an edit** — \`ug context <symbol>\`: source, callers with call
  sites, tests that reach it, dependencies and linked docs, in one budgeted call.
- **After an edit** — \`ug analyze diff_impact\` / \`diff_retest_scope\`:
  blast radius and the tests to re-run.
- **Aggregates in one call** — counts, rankings, hotspots, dead code,
  boundaries, via 35+ \`ug analyze\` presets or hand-written GQL.
- **Wildcards everywhere** — \`ug find_usages 'validate_*'\` is one call, not a loop.
- **Read a change, not a patch** — \`ug walk main...HEAD\`.

## Requirements

The skill drives the \`ug\` CLI, which runs fully locally (no cloud, no API key):

\`\`\`bash
curl -fsSL https://ultra-graph.web.app/install.sh | sh
\`\`\`

Windows: \`ultragraph-windows-x64.zip\` from
https://github.com/shoocstorm/ug/releases/latest

## Install the skill

Copy the \`ug/\` folder into your agent's skills directory, e.g. for Claude Code:

\`\`\`bash
mkdir -p ~/.claude/skills && cp -r ug ~/.claude/skills/
\`\`\`

Then index a repo once and keep it current:

\`\`\`bash
cd your-repo
ug gen            # index → ~/.ug/<name>/
ug hook install   # re-index after every commit, merge and rebase
\`\`\`
EOF

TARBALL="$OUT/ug-skill-v${VERSION}.tar.gz"
# COPYFILE_DISABLE keeps macOS from adding ._* AppleDouble files.
COPYFILE_DISABLE=1 tar -czf "$TARBALL" -C "$STAGE" ug
ZIPFILE="$OUT/ug-skill-v${VERSION}.zip"
rm -f "$ZIPFILE"
# -X drops extra file attributes (uid/gid, macOS metadata).
(cd "$STAGE" && zip -qrX "$ZIPFILE" ug)
echo "$TARBALL"
echo "$ZIPFILE"
unzip -l "$ZIPFILE"

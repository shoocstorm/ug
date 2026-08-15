#!/usr/bin/env bash
#
# Regenerate the live demo at docs/ug-website/demo/.
#
# The demo is a published snapshot of an indexed repo: `graph.json` plus the
# visualization page, wrapped for static hosting. It has no server behind it,
# so it deploys with the rest of the site — `firebase.json` publishes the
# whole website folder and nothing here needs to be registered anywhere.
#
#   ./scripts/gen-demo.sh              # regenerate, using ./native/target/…/ug
#   ./scripts/gen-demo.sh --preview    # …then serve the site locally
#
# Environment overrides:
#   UG          path to the ug binary (default: a local build, else `ug` on PATH)
#   DEMO_INPUT  what to index (default: native/src — see WHY THIS SUBTREE below)
#   DEMO_LABEL  the name shown on the page
#   PORT        --preview port (default 8081, see PICKING A PORT below)
#
# ── PICKING A PORT ──────────────────────────────────────────────────────────
# Not 8000. It is the obvious choice and the wrong one: it is `http.server`'s
# default, which makes it the port every other local tool also reaches for —
# oMLX's admin panel polls `localhost:8000/admin/api/stats` once a second, for
# one real example, and its 404s bury the demo's own four log lines. Not 8080
# either: that is `ug serve`. 8081 is neither.
#
# ── WHY THIS SUBTREE, NOT THE WHOLE REPO ────────────────────────────────────
# The renderer draws a graph in full up to 10,000 elements and switches to
# solo mode — one node and its neighbourhood, on an otherwise empty canvas —
# above that. Solo mode is correct for a real repo and a poor first
# impression for a demo, and the whole `ug` checkout lands just over the line
# (~10.3k edges). `native/src` is the engine itself, ~2.9k nodes / ~8.7k
# edges, and draws whole. `ug demo` warns if whatever you point it at would
# cross the threshold, so this is checked at generation time, not guessed.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"

OUT="docs/ug-website/demo"
DEMO_INPUT="${DEMO_INPUT:-native/src}"
# No apostrophe: bash 3.2 (what macOS ships) mis-lexes a single quote inside
# a ${VAR:-default} that is itself inside double quotes.
DEMO_LABEL="${DEMO_LABEL:-The UltraGraph engine}"
SOURCE_URL="https://github.com/shoocstorm/ug"

# Build rather than hunt for a binary. `ug demo` *embeds* the visualization
# page (build.rs assembles it into the binary), so publishing with a stale
# `ug` silently ships whatever page that build was compiled with — an edit to
# native/src/vis/ would appear to have no effect, with nothing to explain
# why. A debug build is used on purpose: the profile only changes how fast
# the indexing runs, and the published bytes are identical either way.
if [ -z "${UG:-}" ]; then
  if command -v cargo >/dev/null 2>&1; then
    echo "▸ building ug from this checkout"
    cargo build --manifest-path native/Cargo.toml --bin ug
    UG="$ROOT/native/target/debug/ug"
  elif [ -x "$ROOT/native/target/release/ug" ]; then
    UG="$ROOT/native/target/release/ug"
    echo "⚠  No cargo — using $UG, which may predate native/src/vis/."
  else
    echo "error: no cargo and no local build. Install Rust, or set UG=<path>." >&2
    exit 1
  fi
fi

if ! "$UG" demo --help --no-logo >/dev/null 2>&1; then
  echo "error: $UG has no \`demo\` command — it predates this script." >&2
  exit 1
fi

echo "▸ ug        $UG ($("$UG" -v | tail -1))"
echo "▸ indexing  $DEMO_INPUT"
echo

"$UG" demo \
  --no-logo \
  -i "$DEMO_INPUT" \
  -o "$OUT" \
  --label "$DEMO_LABEL" \
  --source-url "$SOURCE_URL"

echo
echo "▸ Committed artifacts — these are what Firebase publishes:"
git -C "$ROOT" status --short -- "$OUT" | sed 's/^/    /'
echo
echo "Next:"
echo "    git add $OUT && git commit -m 'chore: refresh the live demo'"
echo "    cd docs/ug-website && firebase deploy --only hosting:ultra-graph"

if [ "${1:-}" = "--preview" ]; then
  PORT="${PORT:-8081}"
  echo
  if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "error: something is already listening on port $PORT." >&2
    echo "       Re-run with PORT=<other> ./scripts/gen-demo.sh --preview" >&2
    exit 1
  fi
  echo "▸ Serving the site at http://localhost:$PORT/demo/ — Ctrl-C to stop."
  # From the website root, not the demo folder: the demo's "Install ug" links
  # point at the site root (`/#get-started`), which only resolves from here.
  #
  # Bound to 127.0.0.1 rather than the default all-interfaces: this serves an
  # unreleased copy of the site, and there is no reason for it to be reachable
  # from the network you happen to be on.
  cd "$ROOT/docs/ug-website"
  python3 -m http.server "$PORT" --bind 127.0.0.1
fi

#!/usr/bin/env bash
#
# release-notes.sh — the notes for a release, from the commit subjects since
# the tag before it, grouped by Conventional Commit type.
#
# One script for both places that need them: release.sh previews them and
# puts them in the release commit, and .github/workflows/release.yml sets
# them as the GitHub Release body once every platform build has uploaded.
#
# Usage:
#   scripts/release-notes.sh            # notes for HEAD (what the next release would say)
#   scripts/release-notes.sh v0.1.23    # notes for an existing tag
#
# feat: → Features · fix: → Bug Fixes · refactor: → Refactors · anything else
# → Other. A scope (`fix(indexer):`) and a breaking-change `!` are accepted
# and stripped; `release:` commits are skipped.
set -euo pipefail

TO="${1:-HEAD}"

# For a tag, the previous release is the newest tag *before* it; for HEAD it
# is the newest tag reachable from HEAD (nothing new since → empty notes).
if git rev-parse -q --verify "refs/tags/${TO}" >/dev/null; then
  PREV="$(git describe --tags --abbrev=0 "${TO}^" 2>/dev/null || true)"
else
  PREV="$(git describe --tags --abbrev=0 "${TO}" 2>/dev/null || true)"
fi
RANGE="${PREV:+${PREV}..}${TO}"

git log "$RANGE" --format='%s' --no-decorate | awk '
  BEGIN { f = ""; b = ""; r = ""; o = "" }
  {
    m = $0
    if (m ~ /^release:/) { next }
    # sub() returns how many replacements it made: that, not a comparison
    # with the input line, is what says which prefix the subject had.
    if (sub(/^feat(\([^)]*\))?!?:[[:space:]]*/, "", m))     { f = f "* " m "\n"; next }
    if (sub(/^fix(\([^)]*\))?!?:[[:space:]]*/, "", m))      { b = b "* " m "\n"; next }
    if (sub(/^refactor(\([^)]*\))?!?:[[:space:]]*/, "", m)) { r = r "* " m "\n"; next }
    sub(/^[a-z]+(\([^)]*\))?!?:[[:space:]]*/, "", m)
    o = o "* " m "\n"
  }
  END {
    if (f) print "### Features\n" f
    if (b) print "### Bug Fixes\n" b
    if (r) print "### Refactors\n" r
    if (o) print "### Other\n" o
  }'

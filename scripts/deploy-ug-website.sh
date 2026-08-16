#!/usr/bin/env bash
#
# Deploy docs/ug-website to Firebase Hosting.
#
# The site is static — hand-written HTML plus the generated demo/ — with no
# build step. `firebase.json` sets `"public": "."`, so the deploy must run
# from *inside* the website folder; this script does that for you.
#
#   ./scripts/deploy-ug-website.sh        # everything, any directory
#   ./scripts/deploy-ug-website.sh --skip  # don't prompt, just deploy
#
# Deploying publishes to a live public site. Edit and commit freely but
# deploy deliberately — there is no undo for a bad landing page.

set -euo pipefail

cd "$(dirname "$0")/../docs/ug-website"
ROOT="$PWD/.."

SKIP_PROMPT=0
for arg in "$@"; do
  case "$arg" in
    --skip) SKIP_PROMPT=1 ;;
    --help|-h)
      echo "usage: ./scripts/deploy-ug-website.sh [--skip]" >&2
      echo "  --skip  deploy without the confirmation prompt" >&2
      exit 0 ;;
    *)
      echo "error: unknown argument \`$arg\` (see --help)" >&2
      exit 1 ;;
  esac
done

if ! command -v firebase >/dev/null 2>&1; then
  echo "error: no \`firebase\` on PATH. Install the Firebase CLI:" >&2
  echo "    npm install -g firebase-tools" >&2
  exit 1
fi

# Firebase caches the confirmed login under .firebase/ (gitignored).
if [ ! -d ".firebase" ]; then
  echo "▸ first deploy — logging in (once)"
  firebase login
fi

echo
echo "▸ deploying from $(pwd)"
if ! git -C "$ROOT" status --porcelain -- docs/ug-website | grep -q .; then
  echo "▸ working tree clean"
else
  echo "⚠  uncommitted changes exist — this deploy publishes the working tree:"
  git -C "$ROOT" status --short -- docs/ug-website | sed 's/^/    /'
  echo "    (regenerate the demo if you touched native/src/vis/ — the live"
  echo "     page is a copy of the one embedded in the ug binary)"
fi

echo
if [ "$SKIP_PROMPT" -eq 1 ]; then
  echo "▸ --skip — deploying"
else
  read -r -p "Deploy to https://ultra-graph.web.app now? [y/N] " ans
  case "$ans" in
    y|Y|yes) echo "▸ deploying" ;;
    *) echo "aborted."; exit 1 ;;
  esac
fi

exec firebase deploy --only hosting:ultra-graph
#!/usr/bin/env bash
#
# release.sh — cut a release in one command.
#
# Bumps the version in every manifest, commits the working tree, tags the
# new version, and pushes. Pushing the tag triggers .github/workflows/
# release.yml, which builds the platform archives and publishes a GitHub
# Release. The tag name always matches the version the built binary reports.
#
# Version lives in four places, all kept in lockstep here:
#   - package.json
#   - native/package.json
#   - native/Cargo.toml   ([package] version)
#   - native/Cargo.lock   (the "ultragraph" entry)
#
# Usage:
#   scripts/release.sh                # bump patch (0.1.4 -> 0.1.5)
#   scripts/release.sh 0.2.0          # bump to an explicit version
#   scripts/release.sh 0.2.0 --yes    # skip the confirmation prompt
#   scripts/release.sh --dry-run      # show what would happen, change nothing
#
set -euo pipefail

# --- locate repo root -------------------------------------------------------
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "error: not inside a git repository" >&2; exit 1
}
cd "$ROOT"

# --- parse args -------------------------------------------------------------
NEW_VERSION=""
ASSUME_YES=0
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    -y|--yes)     ASSUME_YES=1 ;;
    -n|--dry-run) DRY_RUN=1 ;;
    -h|--help)    grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*)           echo "error: unknown flag: $arg" >&2; exit 1 ;;
    *)            NEW_VERSION="$arg" ;;
  esac
done

# --- current version (source of truth: root package.json) -------------------
CUR_VERSION="$(node -p "require('./package.json').version")"
[[ -n "$CUR_VERSION" ]] || { echo "error: could not read current version" >&2; exit 1; }

# --- compute the target version ---------------------------------------------
if [[ -z "$NEW_VERSION" ]]; then
  IFS='.' read -r MA MI PA <<<"$CUR_VERSION"
  NEW_VERSION="${MA}.${MI}.$((PA + 1))"
fi

# semver X.Y.Z (plain — no prerelease/build metadata for release tags)
if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: '$NEW_VERSION' is not a valid X.Y.Z version" >&2; exit 1
fi
if [[ "$NEW_VERSION" == "$CUR_VERSION" ]]; then
  echo "error: new version equals current version ($CUR_VERSION)" >&2; exit 1
fi

TAG="v${NEW_VERSION}"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"

# --- preflight checks -------------------------------------------------------
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "error: tag ${TAG} already exists locally" >&2; exit 1
fi
if git ls-remote --exit-code --tags origin "${TAG}" >/dev/null 2>&1; then
  echo "error: tag ${TAG} already exists on origin" >&2; exit 1
fi
if [[ "$BRANCH" != "main" ]]; then
  echo "warning: current branch is '${BRANCH}', not 'main'" >&2
fi

echo "Release plan"
echo "  version:  ${CUR_VERSION}  ->  ${NEW_VERSION}"
echo "  tag:      ${TAG}"
echo "  branch:   ${BRANCH}  (push origin ${BRANCH} ${TAG})"
echo "  publish:  pushing ${TAG} triggers .github/workflows/release.yml"
echo
echo "Working tree that will go into the release commit:"
git status --short || true
echo

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] no files changed, nothing committed or pushed."
  exit 0
fi

# --- bump every manifest ----------------------------------------------------
bump_lockfile() {  # $1=file $2=anchor-name  — replace version in a Cargo.lock entry
  local file="$1" name="$2" tmp
  tmp="$(mktemp)"
  awk -v new="$NEW_VERSION" -v name="$name" '
    $0 == "name = \"" name "\"" { inpkg = 1 }
    inpkg && /^version = "/ { sub(/"[^"]*"/, "\"" new "\""); inpkg = 0 }
    { print }
  ' "$file" >"$tmp" && mv "$tmp" "$file"
}

bump_cargo_toml() {  # $1=file — replace the first (i.e. [package]) version line
  local file="$1" tmp
  tmp="$(mktemp)"
  awk -v new="$NEW_VERSION" '
    !done && /^version = "/ { sub(/"[^"]*"/, "\"" new "\""); done = 1 }
    { print }
  ' "$file" >"$tmp" && mv "$tmp" "$file"
}

echo "Bumping manifests to ${NEW_VERSION}..."
# package.json + package-lock.json (root and native) via npm — no git side effects
npm version "$NEW_VERSION" --no-git-tag-version --allow-same-version >/dev/null
( cd native && npm version "$NEW_VERSION" --no-git-tag-version --allow-same-version >/dev/null )
bump_cargo_toml native/Cargo.toml
bump_lockfile   native/Cargo.lock ultragraph

# --- verify every location took ---------------------------------------------
check() {  # $1=file $2=grep-pattern
  grep -q "$2" "$1" || { echo "error: ${1} still not at ${NEW_VERSION}" >&2; exit 1; }
}
check package.json           "\"version\": \"${NEW_VERSION}\""
check native/package.json    "\"version\": \"${NEW_VERSION}\""
check native/Cargo.toml      "^version = \"${NEW_VERSION}\""
check native/Cargo.lock      "^version = \"${NEW_VERSION}\""
echo "  all four manifests updated."
echo
echo "Version-file diff:"
git --no-pager diff -- package.json native/package.json native/Cargo.toml native/Cargo.lock \
  package-lock.json native/package-lock.json | sed -n '1,80p'
echo

# --- confirm before anything leaves the machine -----------------------------
if [[ "$ASSUME_YES" -ne 1 ]]; then
  read -r -p "Commit, tag ${TAG}, and push to origin (this publishes)? [y/N] " reply
  case "$reply" in
    y|Y|yes|YES) ;;
    *) echo "Aborted. Manifests are bumped but nothing was committed."; exit 1 ;;
  esac
fi

# --- commit, tag, push ------------------------------------------------------
git add -A
git commit -m "release: ${TAG}"
git tag -a "${TAG}" -m "${TAG}"
git push origin "${BRANCH}" "${TAG}"

echo
echo "✔ Pushed ${TAG}. Release workflow: https://github.com/shoocstorm/ug/actions/workflows/release.yml"

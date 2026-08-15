#!/bin/sh
# Installs the ug CLI (UltraGraph) from the latest GitHub release.
#
# Usage:
#   curl -fsSL https://ultra-graph.web.app/install.sh | sh
#   curl -fsSL https://ultra-graph.web.app/install.sh | sh -s -- v0.2.0   # pin a version
#
# Windows users: download the ultragraph-windows-x64.zip asset from
# https://github.com/shoocstorm/ug/releases/latest and extract it yourself.

set -eu

REPO="shoocstorm/ug"
VERSION="${1:-latest}"
INSTALL_ROOT="${UG_INSTALL_ROOT:-$HOME/.local/share/ultragraph}"
BIN_DIR="${UG_BIN_DIR:-$HOME/.local/bin}"

info() { printf '\033[36m==>\033[0m %s\n' "$1"; }
die() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64) asset="macos-arm64" ;;
      x86_64) asset="macos-x64" ;;
      *) die "unsupported macOS architecture: $arch" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64) asset="linux-x64" ;;
      *) die "unsupported Linux architecture: $arch (only x86_64 release builds exist today)" ;;
    esac
    ;;
  *)
    die "unsupported OS: $os — Windows users should download ultragraph-windows-x64.zip from https://github.com/$REPO/releases/latest"
    ;;
esac

archive="ultragraph-${asset}.tar.gz"

if [ "$VERSION" = "latest" ]; then
  release_url="https://api.github.com/repos/$REPO/releases/latest"
else
  release_url="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi

info "Looking up $VERSION release for $asset..."
download_url=$(curl -fsSL "$release_url" | grep '"browser_download_url"' | grep "$archive" | sed -E 's/.*"(https:[^"]+)".*/\1/')

[ -n "$download_url" ] || die "no $archive asset found for $VERSION release of $REPO — has a release been published yet?"

tmpfile=$(mktemp)
trap 'rm -f "$tmpfile"' EXIT

info "Downloading $archive..."
curl -fsSL "$download_url" -o "$tmpfile"

info "Installing to $INSTALL_ROOT/.ug ..."
rm -rf "$INSTALL_ROOT/.ug"
mkdir -p "$INSTALL_ROOT/.ug"
tar -xzf "$tmpfile" -C "$INSTALL_ROOT/.ug"
chmod +x "$INSTALL_ROOT/.ug/ug"

mkdir -p "$BIN_DIR"
ln -sf "$INSTALL_ROOT/.ug/ug" "$BIN_DIR/ug"

info "Installed ug to $BIN_DIR/ug"
"$BIN_DIR/ug" -v 2>/dev/null || true

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    printf '\n\033[33mNote:\033[0m %s is not on your PATH.\n' "$BIN_DIR"
    printf 'Add this to your shell profile (~/.zshrc, ~/.bashrc, ...):\n\n'
    printf '  export PATH="%s:$PATH"\n\n' "$BIN_DIR"
    ;;
esac

info "Next: run 'ug gen' in a repo to build your first knowledge graph."
info "For MCP (Claude / Cursor etc.) setup: ug mcp install claude"

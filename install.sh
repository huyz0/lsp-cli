#!/usr/bin/env sh
# Downloads the latest lsp-cli release for this machine's OS/arch and
# installs the `lsp` binary to $INSTALL_DIR (default ~/.local/bin).
#
#   curl -fsSL https://raw.githubusercontent.com/huyz0/lsp-cli/main/install.sh | sh
#
# Prefer `brew install huyz0/tap/lsp-cli` if you have Homebrew — this
# script is the no-package-manager fallback (mirrors what rustup/eget-style
# installers do). Linux/macOS only: this tool's background daemon is
# Unix Domain Socket-only today, so there's no Windows build to fetch.
set -eu

REPO="huyz0/lsp-cli"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux) platform_os="unknown-linux-gnu" ;;
  Darwin) platform_os="apple-darwin" ;;
  *)
    echo "error: unsupported OS '$os' — download a release manually from https://github.com/$REPO/releases" >&2
    exit 1
    ;;
esac

case "$arch" in
  x86_64 | amd64) platform_arch="x86_64" ;;
  arm64 | aarch64) platform_arch="aarch64" ;;
  *)
    echo "error: unsupported architecture '$arch' — download a release manually from https://github.com/$REPO/releases" >&2
    exit 1
    ;;
esac

target="${platform_arch}-${platform_os}"
url="https://github.com/$REPO/releases/latest/download/lsp-${target}.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading lsp-cli ($target)..."
curl -fsSL "$url" -o "$tmp/lsp.tar.gz" || {
  echo "error: no release asset for $target — see https://github.com/$REPO/releases" >&2
  exit 1
}

tar -xzf "$tmp/lsp.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
mv "$tmp/lsp" "$INSTALL_DIR/lsp"
chmod +x "$INSTALL_DIR/lsp"

echo "Installed to $INSTALL_DIR/lsp"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "note: $INSTALL_DIR is not on your PATH — add it, e.g.: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
"$INSTALL_DIR/lsp" --help >/dev/null && echo "lsp-cli installed successfully. Run 'lsp --help' to get started."

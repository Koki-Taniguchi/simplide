#!/usr/bin/env bash
#
# simplide installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Koki-Taniguchi/simplide/main/install.sh | sh
#
# Environment variables:
#   SIMPLIDE_VERSION      Specific version tag to install (default: latest)
#   SIMPLIDE_INSTALL_DIR  Install destination (default: $HOME/.local/bin)

set -euo pipefail

REPO="Koki-Taniguchi/simplide"
BIN_NAME="simplide"
INSTALL_DIR="${SIMPLIDE_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf '==> %s\n' "$*"; }

require() {
  command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}

detect_target() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *) err "unsupported OS: $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64)  arch="x86_64" ;;
    *) err "unsupported architecture: $(uname -m)" ;;
  esac
  printf '%s-%s' "$arch" "$os"
}

latest_version() {
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n1
}

main() {
  require curl
  require tar
  require uname

  local version target asset url tmp
  version="${SIMPLIDE_VERSION:-$(latest_version)}"
  [ -n "$version" ] || err "could not determine latest release; set SIMPLIDE_VERSION"
  target="$(detect_target)"
  asset="simplide-${version}-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/download/${version}/${asset}"

  info "installing ${BIN_NAME} ${version} (${target})"
  info "downloading ${url}"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  curl -fsSL "$url" -o "$tmp/$asset" \
    || err "download failed: $url"
  tar -xzf "$tmp/$asset" -C "$tmp"
  [ -f "$tmp/$BIN_NAME" ] || err "archive did not contain $BIN_NAME"

  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$tmp/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
  info "installed: $INSTALL_DIR/$BIN_NAME"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      printf '\n'
      printf 'note: %s is not in PATH.\n' "$INSTALL_DIR"
      printf 'add this line to your shell rc (~/.zshrc, ~/.bashrc):\n'
      printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
      ;;
  esac
}

main "$@"

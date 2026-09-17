#!/usr/bin/env bash
# Reads the receipt install.sh wrote rather than recomputing the path, so a
# binary that was installed under an old name or a custom INSTALL_DIR still gets
# removed instead of silently left behind on PATH.
set -euo pipefail

# Mirrors the TOOLS list in install.sh.
TOOLS=(
  "graph-server:terraform-http"
  "graph-tui:cerebro"
)
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

for tool in "${TOOLS[@]}"; do
  PKG="${tool%%:*}"
  BIN="${tool#*:}"
  STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/$PKG"
  RECEIPT="$STATE_DIR/receipt"

  target="$INSTALL_DIR/$BIN"
  [ -f "$RECEIPT" ] && target="$(sed -n 's/^bin=//p' "$RECEIPT")"
  target="${target:-$INSTALL_DIR/$BIN}"

  if [ -e "$target" ]; then
    rm -f "$target"
    echo "removed $target"
  else
    echo "not found: $target"
  fi

  if [ -d "$STATE_DIR" ]; then
    rm -rf "$STATE_DIR"
    echo "removed $STATE_DIR"
  fi
done

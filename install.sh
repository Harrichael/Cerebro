#!/usr/bin/env bash
# Deploys a COPY to ~/.local/bin, never a symlink into target/: a `cargo clean`
# or a rebuild in progress must not be able to break the live command.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

# The only lines that differ between tool repos: one entry per installed
# command, as package:binary:extra-cargo-flags. The package name keys the
# receipt, so it has to survive a binary rename -- otherwise the old receipt is
# orphaned and the old binary along with it.
TOOLS=(
  "graph-server:terraform-http:--features scip"
  "graph-tui:cerebro:"
)

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found. install Rust: https://rustup.rs" >&2
  exit 1
}

# Which clone deployed this? Any checkout anywhere can run this script and
# become the live version, so record the source. Best-effort, because it must
# keep working from a tarball with no .git. No timestamp, so re-running
# converges byte-for-byte; uninstall.sh and the rename cleanup below read it.
commit="$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty=no
[ -n "$(git -C "$HERE" status --porcelain 2>/dev/null)" ] && dirty=yes

mkdir -p "$INSTALL_DIR"

for tool in "${TOOLS[@]}"; do
  PKG="${tool%%:*}"
  rest="${tool#*:}"
  BIN="${rest%%:*}"
  FLAGS="${rest#*:}"
  STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/$PKG"
  RECEIPT="$STATE_DIR/receipt"

  # Not `cargo install --path`: that ignores Cargo.lock unless given --locked,
  # so it can ship dependency versions nothing was ever tested against, and it
  # keeps its own bookkeeping under ~/.cargo -- a second registry of truth,
  # which is exactly how two copies of a binary at different commits once ended
  # up on one PATH. -p pins the package so nothing else in the workspace is
  # built. graph-server takes --features scip so `--scip-index` works from the
  # installed command; the indexers themselves are found at run time.
  echo "==> building $PKG"
  # shellcheck disable=SC2086 -- FLAGS is a deliberate word list, often empty.
  cargo build --release -p "$PKG" $FLAGS

  # A rename leaves the old name live on PATH at a stale commit forever, with
  # nothing remaining that would ever update it. The receipt remembers what the
  # last install put down, so clean it up here rather than warn about it later.
  if [ -f "$RECEIPT" ]; then
    old_bin="$(sed -n 's/^bin=//p' "$RECEIPT")"
    if [ -n "$old_bin" ] && [ "$old_bin" != "$INSTALL_DIR/$BIN" ] && [ -e "$old_bin" ]; then
      rm -f "$old_bin"
      echo "==> removed renamed binary: $old_bin"
    fi
  fi

  mkdir -p "$STATE_DIR"
  install -m 0755 "target/release/$BIN" "$INSTALL_DIR/$BIN"
  printf 'bin=%s\nsource=%s\ncommit=%s\ndirty=%s\n' \
    "$INSTALL_DIR/$BIN" "$HERE" "$commit" "$dirty" > "$RECEIPT"

  echo "==> installed: $INSTALL_DIR/$BIN ($commit$([ "$dirty" = yes ] && echo -dirty))"
done

# Advise, never edit: this repo does not own anyone's shell rc. On a dev-setup
# machine the entry is already there; anywhere else the hint is the fix.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "note: $INSTALL_DIR is not on your PATH. Add to your shell rc:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

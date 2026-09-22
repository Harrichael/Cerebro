#!/usr/bin/env bash
# Gets cerebro onto a macOS or Linux box from nothing:
#
#     curl -fsSL https://raw.githubusercontent.com/Harrichael/Cerebro/main/get-cerebro.sh | bash
#
# There is no binary to download -- cerebro is built here, from source -- so
# the machine needs a Rust toolchain and a C linker. This script will not
# install either of them, nor Neovim, nor an indexer. It says the one command
# that will, for the package manager it found. A script that reaches into
# someone's toolchain on its way past is a worse trade than a line to paste.
#
# Knobs, all environment: INSTALL_DIR (default ~/.local/bin), CEREBRO_SRC
# (where the checkout lives), CEREBRO_REPO, CEREBRO_REF.
set -euo pipefail

REPO="${CEREBRO_REPO:-https://github.com/Harrichael/Cerebro}"
REF="${CEREBRO_REF:-main}"
SRC="${CEREBRO_SRC:-${XDG_CACHE_HOME:-$HOME/.cache}/cerebro/src}"

# Edition 2024, which is the whole workspace, landed in 1.85.
RUST_MIN_MAJOR=1
RUST_MIN_MINOR=85

case "${1:-}" in
  -h|--help)
    # The comment at the top of this file is the help text, unless the file is
    # not on disk -- which is exactly the case when it arrived down a pipe.
    if [ -r "${BASH_SOURCE[0]:-}" ]; then
      sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    else
      echo "usage: get-cerebro.sh   # builds and installs cerebro from source"
      echo "env: INSTALL_DIR CEREBRO_SRC CEREBRO_REPO CEREBRO_REF"
    fi
    exit 0
    ;;
  "") ;;
  *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
esac

have() { command -v "$1" >/dev/null 2>&1; }
say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) OS=macos ;;
  Linux) OS=linux ;;
  *) die "$(uname -s) is not supported. cerebro builds on macOS and Linux." ;;
esac

# How this machine installs things. Only ever used to print a line for a human
# to run, so an unknown package manager is a vaguer message, not a failure.
pkg_cmd() {
  if have brew; then echo "brew install $1"
  elif have apt-get; then echo "sudo apt-get install -y $1"
  elif have dnf; then echo "sudo dnf install -y $1"
  elif have pacman; then echo "sudo pacman -S --needed $1"
  elif have zypper; then echo "sudo zypper install -y $1"
  elif have apk; then echo "sudo apk add $1"
  elif [ "$OS" = macos ]; then echo "brew install $1, once Homebrew is on (https://brew.sh)"
  else echo "install $1 with your package manager"
  fi
}

# --- what the build cannot do without ------------------------------------

if ! have cc && ! have gcc && ! have clang; then
  if [ "$OS" = macos ]; then
    die "no C compiler. run: xcode-select --install"
  elif have apt-get; then
    die "no C compiler. run: sudo apt-get install -y build-essential"
  elif have pacman; then
    die "no C compiler. run: sudo pacman -S --needed base-devel"
  else
    die "no C compiler. install your distribution's gcc and make"
  fi
fi

have git || die "git not found. run: $(pkg_cmd git)"

have cargo || die "cargo not found. install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

# A toolchain too old for edition 2024 fails deep in the build with an error
# about the edition key, which reads as a bug in this repo rather than an
# out-of-date rustup. Say it here, where the fix is one word.
cargo_version="$(cargo --version | awk '{print $2}')"
cargo_major="${cargo_version%%.*}"
cargo_rest="${cargo_version#*.}"
cargo_minor="${cargo_rest%%.*}"
if [ "$cargo_major" -lt "$RUST_MIN_MAJOR" ] ||
   { [ "$cargo_major" -eq "$RUST_MIN_MAJOR" ] && [ "$cargo_minor" -lt "$RUST_MIN_MINOR" ]; }; then
  hint="update Rust"
  have rustup && hint="run: rustup update stable"
  die "cargo $cargo_version is older than $RUST_MIN_MAJOR.$RUST_MIN_MINOR, which edition 2024 needs. $hint"
fi

# --- the source -----------------------------------------------------------

# A checkout someone already has is theirs, and is built exactly as it stands:
# named in CEREBRO_SRC, or the clone this script is sitting in. Only the cache
# directory below is ours to fetch into and overwrite, which is why that is the
# one branch allowed to reset.
HERE=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]:-}" ]; then
  HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
if [ -n "${CEREBRO_SRC:-}" ]; then
  [ -f "$SRC/crates/graph-tui/Cargo.toml" ] || die "no cerebro checkout at $SRC"
  say "building $SRC"
elif [ -n "$HERE" ] && [ -f "$HERE/crates/graph-tui/Cargo.toml" ]; then
  SRC="$HERE"
  say "building the checkout this script came from: $SRC"
elif [ -d "$SRC/.git" ]; then
  say "updating $SRC"
  git -C "$SRC" fetch --quiet --depth 1 origin "$REF"
  git -C "$SRC" reset --quiet --hard FETCH_HEAD
else
  say "cloning $REPO into $SRC"
  mkdir -p "$(dirname "$SRC")"
  git clone --quiet --depth 1 --branch "$REF" "$REPO" "$SRC"
fi

# install.sh owns building and deploying a checkout -- the receipt, the
# rename cleanup, the PATH note. This script owns getting a checkout and
# saying what else the machine wants. Keeping that line means there is one
# copy of the part that can put a stale binary on a PATH.
"$SRC/install.sh" cerebro

# --- what cerebro reaches for while it runs -------------------------------

# Hints worth keeping in step with `install_hint` in
# crates/scip-producer/src/indexers/, which is what cerebro itself prints when
# it goes looking for an indexer and finds nothing.
missing=0
note() { missing=1; printf '  %-16s %s\n     %s\n' "$1" "$2" "$3"; }

echo
echo "optional, and none of it is needed to start:"
have nvim || note "nvim" "the editor pane, on 'o'" "$(pkg_cmd neovim)"
have rust-analyzer || note "rust-analyzer" "exact references in Rust" "rustup component add rust-analyzer"
have npx || note "node" "exact references in TypeScript" "$(pkg_cmd node)   (npx fetches scip-typescript itself)"
have go && ! have scip-go && note "scip-go" "exact references in Go" "go install github.com/scip-code/scip-go/cmd/scip-go@latest"
[ "$missing" -eq 0 ] && echo "  all present"

cat <<EOF

  cerebro .                draw this tree, references resolved by an indexer
  cerebro --treesitter .   parse it instead: starts at once, references matched by name
  ?                        inside cerebro, the list of keys

to remove it again: $SRC/uninstall.sh cerebro
EOF

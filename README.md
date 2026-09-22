# Terraform

> **Form your codebase to a new molding. Rapidly understand and edit your code.**

Terraform is an open-source, terminal-based platform for reimagining how developers interact with and edit code. Built in Rust using [ratatui](https://github.com/ratatui-org/ratatui) and [crossterm](https://github.com/crossterm-rs/crossterm), it provides a highly extensible TUI environment where different tools and "apps" can plug in to transform the editing experience.

---

## Features

### cerebro — the codebase as a diagram

`cerebro` draws a project as boxes and lines in the terminal. A box is a
folder, a file, or a struct; a line is a call, an import, a type used. Open a
box and the one node becomes several — and the lines that named the box are
still there, because a file is what `use crate::x;` points at.

- **Open one box at a time** — `↵` expands what is selected into its children,
  `⌫` folds it back, and everything else stays where you put it.
- **References a compiler agrees with** — the graph is built from a SCIP index,
  generated on demand by rust-analyzer, scip-typescript or scip-go.
  `--treesitter` trades exactness for starting at once, matching by name.
- **Your Neovim in the right-hand pane** — `o` opens the selected entity in it,
  your config and colours and all. `ctrl-w d` on an identifier selects whatever
  it refers to; `:w` rebuilds the diagram around what you changed, keeping the
  expansion you had.
- **Mouse or keyboard** — drag a box, sweep out a group, scroll to pan,
  ctrl-scroll to zoom. Or never leave the home row.

`?` inside cerebro lists every key.

---

## Installation

### Prerequisites

macOS or Linux, and a machine that can build Rust:

- Rust toolchain (1.85+, for edition 2024): https://rustup.rs/
- a C linker — `xcode-select --install`, or your distribution's `build-essential`

### cerebro, in one line

```bash
curl -fsSL https://raw.githubusercontent.com/Harrichael/Terraform/main/get-cerebro.sh | bash
```

It clones into `~/.cache/cerebro/src`, builds, and puts `cerebro` in
`~/.local/bin`. Nothing is downloaded pre-built and no toolchain is installed
behind your back: anything missing is reported with the one command that
installs it. Re-run it to update. `INSTALL_DIR`, `CEREBRO_SRC`, `CEREBRO_REPO`
and `CEREBRO_REF` override where each part goes.

Worth having, none of it required to start: **Neovim** for the editor pane
(`o`), and an indexer for the language you are reading — `rust-analyzer`,
`scip-go`, or Node for `scip-typescript`. Without one, `cerebro --treesitter .`
parses instead, matching references by name.

### Build from source

```bash
git clone https://github.com/Harrichael/Terraform
cd Terraform
cargo build --release
```

The binaries land in `target/release/`: `cerebro` and `terraform-http`.

`./install.sh` deploys the commands from a checkout: `cerebro` and
`terraform-http` (with SCIP support) into `~/.local/bin`, recording what it
deployed under `~/.local/state/<package>`. Name one — `./install.sh cerebro` —
to build only that. `./uninstall.sh` reverses it, and takes the same argument.

---

## Usage

```bash
# Draw the current directory (default), indexing it if the index is stale
cerebro

# A specific directory, or one file
cerebro path/to/project/
cerebro path/to/file.rs

# Parse with tree-sitter instead of indexing: starts at once, and the
# references are matched by name rather than resolved
cerebro --treesitter .

# Draw an index you already have
cerebro --scip index.scip .
```

It opens on the project root as a single box. `↵` opens it, and keeps opening
whatever is selected; `⌫` goes back. `o` puts Neovim beside the diagram on the
selected entity, `ctrl-w h` comes back to it, and `?` lists the rest.

### Browser viewer

Installed as `terraform-http` (see Installation); from a checkout, substitute
`cargo run -p graph-server --features scip --` for `terraform-http`.

```bash
# Serve the graph at http://127.0.0.1:7878/ (tree-sitter)
terraform-http .

# Build the graph from a SCIP index instead: generate one (rust-analyzer,
# scip-typescript or scip-go, picked from the manifest; cached under the
# system temp dir) or point at an existing one
terraform-http --scip-index .
terraform-http --scip index.scip .

# Diff view: the union of a git ref and the working tree, tagged by change
terraform-http --diff main .
terraform-http --diff main --scip-index .
```

The toolbar's **tests** checkbox hides test code (`#[cfg(test)]` items,
`tests/` folders, `*_test`/`*.spec` files, and everything inside them) along
with the references it makes.

---

## Keyboard Shortcuts

`?` inside cerebro is the list, kept next to the code that handles the keys so
it cannot drift. The ones worth knowing before you start:

| Key | Action |
|-----|--------|
| `↵` / `+` | Expand the selected node into its children |
| `⌫` / `-` | Collapse it back into its parent |
| `↑ ↓ ← →` | Move the selection to the nearest node that way |
| `tab` / `⇧tab` | Focus into the box under the cursor, or out to the one around it |
| `o` / `ctrl-w l` | Open the editor pane, and go to it |
| `ctrl-w h` | From the pane, back to the diagram |
| `ctrl-w d` | In the pane: select whatever the word under the cursor refers to |
| `scroll` / `⇧scroll` | Pan up and down, or left and right |
| `ctrl-scroll` / `z` | Zoom: the same graph drawn larger or smaller |
| `L` | Lay the whole picture out afresh |
| `?` | Every key, with what it does |
| `q` / `esc` / `ctrl-c` | Quit |

---

## Architecture

A Cargo workspace. Producers build an `EntityGraph`; consumers read it and
never learn which producer built it.

```
crates/
├── entity-graph/         # The contract: Entity, EntityGraph, Reference (+ test_support fixtures)
├── treesitter-producer/  # Source files → EntityGraph via tree-sitter; one fn: graph_from_path
├── scip-producer/        # SCIP index → EntityGraph, plus `index` to run rust-analyzer/scip-typescript/scip-go
├── coalesce/             # Cursor: a cut through the containment tree, and the references projected onto it
├── graph-diff/           # Two EntityGraphs of one project → one union graph tagged by change
├── nvim-ui/              # A headless Neovim spoken to over msgpack-rpc, drawn into a ratatui Buffer
├── graph-tui/            # cerebro: the diagram, its layout and edge routing, and the editor pane
└── graph-server/         # localhost viewer: raw/coalesced graph, inspector, git diff view
```

`graph-diff` is the odd one out: it consumes two graphs and produces one. Its
whole point is that a diff is not a new kind of thing — it returns the
**union** of a base tree and a working tree as an ordinary `EntityGraph` (one
root, dense ids, everything from either side present exactly once), so
coalescing, layout and the viewer keep working on it untouched. What changed
rides alongside in side tables indexed by union id: a status per entity and
per reference, `(+lines, -lines)` churn, and per-file line ops for showing
old and new together. Entities are matched top-down by (parent, name, kind,
ordinal), which is why a rename reads as a removal plus an addition.

### Node Kinds

A kind is a role, not a depth: any kind may hold any other, and any of them
may be either end of a reference.

| Kind | Description |
|------|-------------|
| `Folder` | Directory |
| `Module` | Rust `mod`, Python packages |
| `File` | Source file |
| `Class` | `struct`, `enum`, `trait`, `impl`, `class`, `interface`, `type alias`, SQL table/view |
| `Function` | `fn`, method, `def`, TypeScript method signature |

---

## Tech Stack

| Component | Library |
|-----------|---------|
| TUI framework | [ratatui](https://github.com/ratatui-org/ratatui) |
| Terminal backend | [crossterm](https://github.com/crossterm-rs/crossterm) |
| Resolved references | [SCIP](https://github.com/sourcegraph/scip), via rust-analyzer, scip-typescript, scip-go |
| Parsing | [tree-sitter](https://tree-sitter.github.io/) (Rust, Python, JavaScript, TypeScript, TSX, SQL) |
| Editor pane | [Neovim](https://neovim.io/), headless, over msgpack-rpc |
| CLI arguments | [clap](https://github.com/clap-rs/clap) |

---

## Roadmap

- [ ] In-place structural code editing (rename, extract, inline)
- [ ] Parameter add/remove with automatic propagation through callers
- [ ] Git integration (blame, diff, stage)
- [ ] LSP integration for richer cross-file symbol references
- [ ] AI-assisted edits
- [ ] Live collaboration
- [ ] Community-built TUI apps

---

## Contributing

Star the repo, open issues for feature ideas, or submit PRs for parsers, new apps, or UX improvements!

**License:** MIT

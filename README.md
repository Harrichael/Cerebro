# Terraform

> **Form your codebase to a new molding. Rapidly understand and edit your code.**

Terraform is an open-source, terminal-based platform for reimagining how developers interact with and edit code. Built in Rust using [ratatui](https://github.com/ratatui-org/ratatui) and [crossterm](https://github.com/crossterm-rs/crossterm), it provides a highly extensible TUI environment where different tools and "apps" can plug in to transform the editing experience.

---

## Features

### Hierarchical Code Viewer (Flagship App)

The first — and flagship — app treats every line of source code as a node in a dynamic tree. Users can:

- **Open a directory or file directly** — Pass any path (or none to open `.`). Directory roots expand through Folder → File → code constructs.
- **Switch granularity per node** — Use `l`/`Right` and `h`/`Left` to expand or shrink the detail level of a *single* node without affecting its siblings. Granularity levels: Folder → Module → File → Class/Struct → Function → Block (if/for/while) → Line.
- **Filter nodes instantly** — Type a pattern to narrow the view to matching names or content.
- **Symbolic references (Lib section)** — Symbols defined in multiple files are deduplicated; the canonical definition is promoted to a `[Lib]` section at the bottom, and duplicates become `[ref]` nodes. Press `Enter` on a `[ref]` node to jump to the definition.
- **Multi-language support** — Rust, Python, and JavaScript powered by [Tree-sitter](https://tree-sitter.github.io/). Plain text files are shown line-by-line.
- **Keyboard-driven** — Fast, mouse-free navigation throughout.

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

The TUI binary will be at `target/release/terraform`.

`./install.sh` deploys the commands from a checkout: `cerebro` and
`terraform-http` (with SCIP support) into `~/.local/bin`, recording what it
deployed under `~/.local/state/<package>`. Name one — `./install.sh cerebro` —
to build only that. `./uninstall.sh` reverses it, and takes the same argument.

---

## Usage

```bash
# Open the current directory (default)
terraform

# Open a specific directory
terraform path/to/project/

# Open a single source file
terraform path/to/file.rs
```

When opening a directory, the view starts at **File granularity** — only folders and files are shown. Use `l`/`Right` on a file to drill into its code constructs.

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

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `PgUp` | Page up |
| `PgDn` | Page down |
| `g` / `Home` | Jump to top |
| `G` / `End` | Jump to bottom |
| **`l` / `→`** | **Expand cursor node to next finer granularity** |
| **`h` / `←`** | **Shrink cursor node to next coarser granularity** |
| `Space` | Toggle full collapse/expand of cursor node |
| `Enter` | Toggle collapse, or jump to SymRef definition |
| `[` | Collapse all nodes |
| `]` | Expand all nodes |
| `/` | Enter filter mode |
| `Esc` | Clear filter / cancel |
| `?` / `F1` | Toggle help overlay |
| `q` / `Ctrl+C` | Quit |

### Granularity Levels

From coarsest to finest:

```
Folder → Module → File → Class/Struct → Function/Method → Block (if/for/while) → Line
```

`l`/`Right` expands one step finer; `h`/`Left` shrinks one step coarser. Changes apply **only to the node under the cursor** — siblings are unaffected.

---

## Architecture

A Cargo workspace. Producers build an `EntityGraph`; consumers read it and
never learn which producer built it.

```
crates/
├── entity-graph/         # The contract: Entity, EntityGraph, Reference (+ test_support fixtures)
├── treesitter-producer/  # Source files → EntityGraph via tree-sitter; one fn: graph_from_path
├── coalesce/             # Cursor: which entities are in view at this zoom, and the edges between them
├── graph-diff/           # Two EntityGraphs of one project → one union graph tagged by change
├── scip-producer/        # SCIP index → EntityGraph, plus `indexer` to run rust-analyzer/scip-typescript/scip-go
└── graph-server/         # localhost viewer: raw/coalesced graph, inspector, git diff view
src/                      # The TUI (bin `terraform`)
├── main.rs               # Entry point, terminal setup, render loop
├── app/state.rs          # AppState — loading, zoom/fold navigation state
├── graph/
│   ├── navigator.rs      # Projects the coalesced view into a renderable GraphTree
│   └── tree.rs           # GraphTree — spanning-forest layout of the reference graph
└── ui/                   # ratatui rendering and keyboard handling
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

| Kind | Description |
|------|-------------|
| `Folder` | Directory |
| `Module` | Rust `mod`, Python packages |
| `File` | Source file |
| `Class` | `struct`, `enum`, `trait`, `impl`, `class`, `interface`, `type alias`, SQL table/view |
| `Function` | `fn`, method, `def`, TypeScript method signature |
| `Block` | `if`/`for`/`while`/`match`/`switch` constructs, SQL statements |
| `Line` | Individual source lines |

---

## Tech Stack

| Component | Library |
|-----------|---------|
| TUI framework | [ratatui](https://github.com/ratatui-org/ratatui) |
| Terminal backend | [crossterm](https://github.com/crossterm-rs/crossterm) |
| Parsing | [tree-sitter](https://tree-sitter.github.io/) (Rust, Python, JavaScript, TypeScript, TSX, SQL) |
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

# xl

A fast, native spreadsheet for Linux. Rust + GPUI (GPU-rendered), with an
incremental recalculation engine. See [PLAN.md](PLAN.md) for the full design and roadmap.

## Build & run

```sh
# one-time: system libraries (Ubuntu/Debian)
sudo apt install -y libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libx11-xcb-dev cmake

./scripts/fetch-gpui.sh          # shallow-clones the pinned GPUI (Zed) revision into vendor/
cargo run --release -p xl        # or: cargo run --release -p xl -- budget.xlsx
```

Rust 1.98.1 is pinned in `rust-toolchain.toml` (rustup installs it automatically).

## Test & benchmark

```sh
cargo test --workspace           # engine unit/property tests + headless UI interaction tests
cargo run --release -p xl-bench  # recalc benchmarks
```

## Layout

| Path | What |
|---|---|
| `crates/xl-engine` | Headless core: multi-sheet workbook, formula parser, 77 functions, Excel number formats, dates, dependency graph, incremental recalc |
| `crates/xl-io` | xlsx read (streaming) / write, CSV/TSV |
| `crates/xl-app` | The GPUI desktop app (`xl` binary): grid, formula bar, sheet tabs, format toolbar |
| `crates/xl-bench` | Recalc benchmarks |
| `assets/fonts` | Bundled Inter (SIL OFL) |

## Keyboard

| Keys | Action |
|---|---|
| Arrows / Tab / Enter (+Shift) | Move; Shift+Arrow extends selection |
| Ctrl+Arrow, Ctrl+Home/End | Jump to data edges / start / end |
| Type, F2, double-click | Edit cell (type over / edit in place) |
| `=` then arrows or click | Point at cells to build references; Shift extends to a range |
| Alt+Enter, Esc | Newline in cell, cancel edit |
| Ctrl+C / X / V | Copy / cut / paste (formulas shift their relative references) |
| Ctrl+D / Ctrl+R | Fill down / right |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Delete | Clear selection |
| Ctrl+O / Ctrl+S / Ctrl+Shift+S | Open / save / save as (.xlsx or .csv); you can also drop a file on the window |
| Ctrl+PageUp / PageDown | Previous / next sheet (also works mid-formula to point at another sheet) |
| Ctrl+Shift+N | New sheet; double-click a tab to rename it |
| Ctrl+Shift+1 / 3 / 4 / 5 / ` | Number / date / currency / percent / General format |

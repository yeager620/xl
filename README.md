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

## Install

```sh
make install                         # just for you: ~/.local (no sudo)
sudo make install PREFIX=/usr/local  # system-wide
make uninstall                       # same PREFIX as you installed with
```

This installs the `xl` binary, a desktop entry (so `.xlsx` / `.csv` files open in
xl), the app icon and AppStream metadata.

## Packages

| Command | Output | Notes |
|---|---|---|
| `make deb` | `target/packages/xl_<version>_<arch>.deb` | Needs only `dpkg-deb`. Install with `sudo apt install ./target/packages/xl_*.deb`. |
| `make appimage` | `target/packages/xl-<version>-<arch>.AppImage` | Downloads appimagetool 1.9.1 into `target/tools/` on first run. |
| `make flatpak` | `target/packages/xl.flatpak` | Needs `flatpak-builder` (`sudo apt install flatpak-builder`) and `uv`; downloads the Freedesktop 25.08 runtime on first run. |

Built packages link against the build machine's glibc, so they run on that
distro release and newer. For the widest compatibility, build release packages
on the oldest distro you want to support (e.g. in an Ubuntu 22.04 container).

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
| `crates/xl-app` | The GPUI desktop app (`xl` binary): grid, formula bar, sheet tabs, format & style toolbar, command palette, IME input |
| `crates/xl-bench` | Recalc benchmarks |
| `crates/xl-pack` | Packaging helpers (renders the app icon PNGs) |
| `packaging/` | Desktop entry, AppStream metadata, icon source, .deb / AppImage / Flatpak build scripts |
| `assets/fonts` | Bundled Source Sans 3 (SIL OFL) |
| `assets/icons` | Bundled Lucide icons (ISC) |

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
| Ctrl+W / Ctrl+Q | Close / quit (asks first if there are unsaved changes) |
| Ctrl+PageUp / PageDown | Previous / next sheet (also works mid-formula to point at another sheet) |
| Ctrl+Shift+N | New sheet; double-click a tab to rename it |
| Ctrl+Shift+1 / 3 / 4 / 5 / ` | Number / date / currency / percent / General format |
| Ctrl+B / I / U / 5 | Bold / italic / underline / strikethrough |
| Ctrl+K or Ctrl+Shift+P | Command palette: every command, `B12` to jump, `xlo` to start `=XLOOKUP(` |

# xl

A fast, native spreadsheet for Linux. Rust + GPUI (GPU-rendered), with an
incremental recalculation engine. See [PLAN.md](PLAN.md) for the full design and roadmap.

## Install

On Debian or Ubuntu (22.04 and newer), add the xl APT repository once:

```sh
curl -fsSL https://yeager620.github.io/xl/xl-archive-keyring.gpg | sudo tee /usr/share/keyrings/xl-archive-keyring.gpg >/dev/null
echo "deb [signed-by=/usr/share/keyrings/xl-archive-keyring.gpg] https://yeager620.github.io/xl stable main" | sudo tee /etc/apt/sources.list.d/xl.list
sudo apt update
sudo apt install xl
```

Updates then arrive with `sudo apt upgrade`. Other options: download the `.deb` or
AppImage from [Releases](https://github.com/yeager620/xl/releases).

## Build & run

```sh
# one-time: system libraries (Ubuntu/Debian)
sudo apt install -y libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libx11-xcb-dev cmake

./scripts/fetch-gpui.sh          # shallow-clones the pinned GPUI (Zed) revision into vendor/
cargo run --release -p xl        # or: cargo run --release -p xl -- budget.xlsx
```

Rust 1.98.1 is pinned in `rust-toolchain.toml` (rustup installs it automatically).

## Install from source

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
| `make flatpak` | installs `io.github.yeager620.xl` for your user | Builds the Flathub manifest from this checkout and runs Flathub's linter (`packaging/flathub/check.sh`). Uses `uv` and per-user Flatpak installs; first run downloads ~2 GB of runtimes. |

Built packages link against the build machine's glibc, so they run on that
distro release and newer. Official releases are built in an Ubuntu 22.04
container for that reason.

## Releasing

1. One time: `./packaging/apt-keygen.sh` creates the APT signing key and stores it
   as the `APT_SIGNING_KEY` repository secret. Back the key up offline.
2. Bump `version` in `Cargo.toml` and add a `<release>` to
   `packaging/io.github.yeager620.xl.metainfo.xml`, commit, then tag and push:
   `git tag v0.3.0 && git push origin v0.3.0`.
3. The Release workflow builds the `.deb` and AppImage on Ubuntu 22.04, publishes
   a GitHub Release, and adds the `.deb` to the APT repository on GitHub Pages.

## Test & benchmark

```sh
cargo test --workspace           # engine unit/property tests + headless UI interaction tests
cargo run --release -p xl-bench  # recalc benchmarks
cargo run --release -p xl-bench -- bigdata [rows]  # 10M-row CSV load, SUM(A:A), SQL group-by
```

Sheets hold 16,777,216 rows (16x Excel). Large CSV files open in parallel
into a compact column store (~8 bytes per number), with progress and Cancel;
**Query with SQL…** (command palette) runs SQLite over sheets and tables by
name and CSV files by path, and **Refresh query** re-runs it. Sheets taller
than 1,048,576 rows can't be saved as .xlsx; xl offers CSV instead.

xlsx saving patches the opened file; `docs/xlsx-fidelity.md` explains what is
preserved and how to run the round-trip checker over the real-world corpus
(CI does on every push):

```sh
scripts/fetch-xlsx-corpus.sh && cargo run -p xl-io --release --example roundtrip -- \
  --known-failures scripts/xlsx-corpus/known-failures.txt target/xlsx-corpus
```

## Layout

| Path | What |
|---|---|
| `crates/xl-engine` | Headless core: multi-sheet workbook, formula parser, 77 functions, Excel number formats, dates, dependency graph, incremental recalc |
| `crates/xl-io` | xlsx read (streaming) / write, CSV/TSV (parallel import, type inference) |
| `crates/xl-sql` | SQL over sheets, tables and CSV files (embedded SQLite) |
| `crates/xl-app` | The GPUI desktop app (`xl` binary): grid, formula bar, sheet tabs, format & style toolbar, command palette, IME input |
| `crates/xl-bench` | Recalc and big-data benchmarks |
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
| Alt+Home | Home: new blank workbook, templates, recent files (Esc goes back to the open workbook) |
| Ctrl+N | New blank workbook (asks first if there are unsaved changes) |
| Ctrl+O / Ctrl+S / Ctrl+Shift+S | Open / save / save as (.xlsx or .csv); you can also drop a file on the window |
| Ctrl+W / Ctrl+Q | Close / quit (asks first if there are unsaved changes) |
| Ctrl+PageUp / PageDown | Previous / next sheet (also works mid-formula to point at another sheet) |
| Ctrl+Shift+N | New sheet; double-click a tab to rename it |
| Ctrl+Shift+1 / 3 / 4 / 5 / ` | Number / date / currency / percent / General format |
| Ctrl+B / I / U / 5 | Bold / italic / underline / strikethrough |
| Ctrl+K or Ctrl+Shift+P | Command palette: every command, `B12` to jump, `xlo` to start `=XLOOKUP(` |

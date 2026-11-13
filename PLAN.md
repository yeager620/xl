# xl — a fast, native spreadsheet for Linux

Goal: a native Linux spreadsheet that opens and recalculates instantly, is easy to pick up for anyone who knows Excel, installs like any other Linux app, and looks good.

---

## 1. Research summary

### What exists today, and where it falls short
| Option | Notes | Gap |
|---|---|---|
| LibreOffice Calc | Full-featured, native | Slow on large sheets, old UI, heavy startup |
| Gnumeric | Fast, accurate | GTK2/3-era UI, little active development |
| OnlyOffice / WPS | Excel-like look | Electron/Qt web-ish stacks, heavy |
| Excel on the web | Best compatibility | Browser only, no native Linux app |
| **IronCalc** (Rust, v0.9, Oct 2026) | Modern open-source engine, good Excel function coverage, xlsx I/O | Library, not a desktop app; built for correctness first, so recalculation is not fine-grained incremental |

**What's missing:** a native, GPU-rendered spreadsheet with a real incremental engine. That's what we build.

### Key technical findings
- **UI: GPUI** (the GPU-accelerated Rust framework behind Zed). It handles Wayland and X11, GPU text rendering, IME, keybindings and 120 Hz scrolling, and it holds up at Zed's scale. One catch: the crates.io release (0.2.2) predates the February 2026 platform split and is stale. Maintainers say automated releases are coming, so for now we **pin a git rev** of the Zed repo. Fallback if it gets painful: `winit` + `wgpu` + `vello` (2D) + `parley` (text), which is more work but leaves nothing upstream to depend on.
- **Engine: write our own incremental core.** We use IronCalc as a test oracle (differential testing of function results) and possibly as a source of function implementations (check its license first; it should be MIT/Apache).
- **File I/O:** `calamine` reads xlsx/xls/xlsb/ods quickly. `rust_xlsxwriter` writes xlsx. CSV goes through the `csv` crate with mmap.
- **Packaging:** Flatpak (Flathub) as the main channel, plus `.deb` via `cargo-deb` and an AppImage. All three get a `.desktop` file, MIME registration for xlsx/csv/ods, and icons.
- **Toolchain on this machine:** Rust 1.91.1, Ubuntu 26.04, Wayland session. Ready to go.

---

## 2. Architecture

```
┌──────────────────────────── xl (binary) ────────────────────────────┐
│  UI (GPUI)                                                          │
│   ├─ GridView: custom element, virtualized, only paints visible cells│
│   ├─ FormulaBar: syntax highlight, autocomplete, arg hints          │
│   ├─ CommandPalette (Ctrl+K), SheetTabs, StatusBar (SUM/AVG/COUNT)  │
│   └─ Theme: light/dark, follows system via xdg-desktop-portal       │
├─────────────────────────────────────────────────────────────────────┤
│  App core: Workbook model, selection, undo/redo (command log),      │
│            clipboard (TSV + HTML), autosave                         │
├─────────────────────────────────────────────────────────────────────┤
│  xl-engine (separate crate, no UI deps, fully testable headless)    │
│   ├─ Storage: per-sheet columnar chunks (values, formulas, styles)  │
│   ├─ Parser: Excel formula grammar → AST → bytecode                 │
│   ├─ DepGraph: cell→cell edges + range index for range deps         │
│   ├─ Scheduler: dirty propagation, topo order, parallel levels      │
│   ├─ Evaluator: bytecode VM + vectorized range ops                  │
│   └─ Functions: ~400 Excel functions, prioritized by usage          │
├─────────────────────────────────────────────────────────────────────┤
│  xl-io: xlsx/ods/csv import-export, native .xl fast-save format     │
└─────────────────────────────────────────────────────────────────────┘
```

Cargo workspace: `crates/xl-engine`, `crates/xl-io`, `crates/xl-app`, `crates/xl-bench`.

### 2.1 Incremental computation (the core of "lightning fast")
1. **Formula interning.** A formula copied down 100k rows is *one* formula in relative (R1C1) form plus 100k anchors. That saves memory, parsing and compilation, and opens the door to vectorized evaluation.
2. **Dependency graph**
   - Single-cell references become direct edges.
   - Range references (`SUM(A:A)`, `A1:Z9999`) are **not** expanded into per-cell edges. They go into a 2-D interval index (R-tree or sorted row/col-band index). When a cell changes, we query "which ranges contain me?" in O(log n).
   - Volatile functions (`NOW`, `RAND`, `OFFSET`, `INDIRECT`) live in a set that gets marked dirty on every recalc. Dynamic refs (`INDIRECT`/`OFFSET`) are re-resolved each time.
3. **Recalc on edit**
   - Mark the edited cell, then do a push-based dirty walk through dependents.
   - Topologically order only the dirty subgraph (Kahn's algorithm, levels).
   - **Early cutoff:** if a recomputed value equals its old value, its dependents are not recomputed.
   - **Parallel levels:** independent cells at the same topo level are evaluated with `rayon` when the level is large enough (threshold ~1–5k cells).
   - **Async & cancellable:** recalc runs off the UI thread and visible cells are prioritized. If the user types again, the stale recalc is cancelled.
4. **Cycles:** detected during topo sort. The cells get a `#CIRC!` marker; Excel-style iterative calc is an option.
5. **Spill / dynamic arrays** (`FILTER`, `SORT`, `UNIQUE`, `SEQUENCE`): spill ranges are tracked as graph nodes, and `#SPILL!` appears on collision.
6. **Aggregate caches:** `SUM`/`COUNT` over large static ranges keep running totals that update by delta (`+new − old`) instead of rescanning.

**Performance targets (CI benchmarks in `xl-bench`):**
| Scenario | Target |
|---|---|
| Cold start to usable window | < 150 ms |
| Edit one cell, 1M-cell sheet, 10 dependents | < 1 ms recalc |
| Edit cell feeding 100k-row copied-down formula | < 20 ms |
| Scroll 1M-row sheet | locked at display refresh (120 fps) |
| Open 50 MB xlsx | < 2 s, progressive display |
| Memory, 1M numeric cells | < 40 MB |

### 2.2 Rendering
- The grid is one custom GPUI element. Each frame it computes the visible row/col window from cumulative-size arrays (binary search, so variable row heights stay O(log n)) and emits only those quads and glyph runs.
- A shaped-text cache is keyed by (string, style, width). Numbers are formatted lazily, and only for visible cells.
- Gridlines are drawn as a few long lines, not one rect per cell.

### 2.3 Native file format
- Primary interchange is **xlsx** (open and save, round-tripping what we support).
- `.xl` is a fast native format: a zstd-compressed columnar binary that includes the serialized dependency graph, so files open with **no** full recalc. It's optional, and xlsx stays the default "Save".

---

## 3. UX & design

**Principles:** Excel muscle memory works on day one, the interface stays calm, and every action is reachable from the keyboard.

- **Excel-compatible shortcuts:** F2, Ctrl+Arrow, Ctrl+Shift+Arrow, Ctrl+D/R, Ctrl+;, F4 ref-cycling, Ctrl+Z/Y, Ctrl+Space and so on.
- **Command palette (Ctrl+K):** every command, every function (with docs), and "go to cell/sheet".
- **Formula bar:** syntax coloring, color-matched reference highlighting in the grid (like Excel's colored ref boxes), function signature popups, fuzzy autocomplete, and multi-line editing with Alt+Enter.
- **Status bar:** live SUM/AVG/COUNT/MIN/MAX of the selection, plus recalc time (it's fast, so we show it).
- **Smart paste:** paste CSV/TSV/HTML tables from the web and get typed, parsed values.
- **Quick filter/sort** from column headers, and freeze panes by dragging.
- **Visual style**
  - Font: Inter for UI, JetBrains Mono / Inter tabular-nums for cells.
  - Neutral grays, one accent color (configurable), hairline gridlines at ~8% opacity, soft 6 px rounded selection with an accent border and a 10% fill.
  - Light and dark themes that follow the system (portal `color-scheme`), designed together.
  - Subtle 120 ms motion on the selection move and on panel open; no gratuitous animation.
  - Header row/column in a slightly tinted surface, with the active row/col header highlighted.

---

## 4. Roadmap

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 – Skeleton** (week 1) | Workspace, GPUI window, virtualized grid with 1M×16k addressable cells, navigation, editing raw values | Smooth scroll, type into cells |
| **M1 – Engine core** (weeks 2–4) | Parser, bytecode VM, dep graph, incremental recalc, ~60 core functions (math, logic, text, lookup), errors | Benchmarks above pass for single-thread cases; property tests vs. naïve full recalc |
| **M2 – Usable app** (weeks 5–7) | Formula bar, ref highlighting, undo/redo, copy/paste, multi-sheet, CSV + xlsx open/save, number formats | Can do real work on a real xlsx |
| **M3 – Polish & speed** (weeks 8–10) | Command palette, themes, parallel recalc, async recalc, aggregate caches, spill arrays, formatting (fonts, fills, borders, alignment) | Feels better than Calc; differential tests vs IronCalc on 300+ functions |
| **M4 – Ship** (weeks 11–12) | Flatpak manifest, .deb, AppImage, .desktop + MIME, icon, autosave/crash recovery | `flatpak install` → double-click an .xlsx opens in xl |
| **Later** | Charts, conditional formatting, pivot tables, data validation, Python/Lua scripting, collaboration (CRDT) | — |

## 5. Testing strategy
- **Property tests** (`proptest`): random edit sequences. Incremental results must equal a from-scratch full recalc.
- **Differential tests:** run the same formulas through IronCalc (and LibreOffice headless for spot checks) and compare.
- **Golden xlsx corpus:** open, then save, then reopen, and compare values and styles.
- **Benchmarks** in CI with `criterion`; fail the build on >10% regression.

## 6. Risks
| Risk | Mitigation |
|---|---|
| GPUI API churn / unpublished crate | Pin git rev; isolate it behind `xl-app`; engine is UI-agnostic; fallback stack identified |
| Excel function long tail & edge cases | Prioritize by usage, differential-test vs IronCalc, borrow implementations where licensing allows |
| xlsx fidelity (styles, charts) | Preserve unknown parts on round-trip; be explicit about unsupported features |
| Scope creep | Milestones are gated; "Later" list is deliberately parked |

---

## Status (2026-11-13)

**M0 done, M1 mostly done.** What exists:
- `xl-engine`: parser (Excel precedence, `$` refs, whole-column ranges), 58 functions (math, logic, text, SUMIF/COUNTIF/AVERAGEIF, VLOOKUP/HLOOKUP/INDEX/MATCH, volatile NOW/RAND), columnar BTreeMap storage, dependency graph with column-banded range index, Kahn-ordered incremental recalc with early cutoff, Tarjan-based cycle detection (only true cycle members get `#CIRC!`).
- Verified by a property test: 200 random 300-edit sequences, checking incremental results against a from-scratch recalc after **every** edit (also stress-run at 1,000 sequences).
- `xl` app: virtualized GPU grid (1,048,576 × 16,384), Excel keyboard model, point mode, colored reference highlighting, formula bar, undo/redo, clipboard with reference shifting, fill down/right, column resize/autofit, CSV open/save, light/dark themes, live selection stats, recalc timing.

**Benchmarks (release):** edit with 2 dependents in a 200k-cell sheet ~2 ms; unchanged re-entry ~1 µs (early cutoff); 100k-long chain ~31 ms; edit at the bottom of 5k running totals ~0.1 ms.

**Known gaps / next:** initial bulk recalc is slower than it should be (200k formulas ≈ 0.8 s; needs a dedicated topo pass instead of the hash-heavy incremental path); recalc runs on the UI thread; range index is a linear scan per column; no IME/compose input yet; scrollbars are not draggable; xlsx I/O, number formats and multi-sheet are M2.

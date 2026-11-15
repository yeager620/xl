# Workbooks in git, and version history

xlsx files are zip archives, so `git diff` normally just says "Binary files
differ". The `xl` binary has two command-line modes that make spreadsheet
changes readable in git. Neither starts the GUI, so they work over SSH and in
CI.

## `xl diff OLD NEW`

A cell-level diff of two workbooks (`.xlsx`, `.xlsm`, `.csv`, `.tsv`, mixed
freely):

```text
$ xl diff budget-v1.xlsx budget-v2.xlsx
--- budget-v1.xlsx
+++ budget-v2.xlsx
+ Sheet1 row 3 inserted
+   Sheet1!A3: "Tea"
+   Sheet1!B3: 2
+   Sheet1!D3: =B3*C3 (8)
Sheet1!A1: style bold on
Sheet1!C2: 14.5 → 15
Sheet1!D2: 43.5 → 45 (=B2*C2)
Sheet1!D5 (was D4): =B4*C4 → =B5*C5*1.1 (3 → 3.3)
Sheet1!D6 (was D5): 60 → 69.8 (=SUM(D2:D5))
Sheet1!D6 (was D5): format General → $#,##0.00
~ Sheet1: column B width default → 140px
+ sheet Notes added (1 cell)
+   Notes!A1: "Checked"
```

- One change per line, addressed as `Sheet!A1`, in a stable order (sheet by
  sheet: inserted/deleted rows and columns, then cells top to bottom, then
  layout), so the output itself diffs and greps well.
- Lines starting with `+ ` were added, `- ` removed, `~ ` are sheet-level or
  layout changes (renamed sheets, widths, merges, frozen panes, filters,
  charts, notes, links, data validation, conditional formats, tables,
  defined names). Changed cells have no prefix.
- Rows and columns are aligned by content: inserting a row reports
  `+ row 3 inserted` instead of every row below it as changed, and a
  formula that merely followed the insertion (`=SUM(D2:D4)` becoming
  `=SUM(D2:D5)` because a row was added inside the range) isn't a change
  unless its result changed. Cells that moved show `(was D4)`.
- Text is quoted, numbers are shown at full precision, formulas with their
  results in parentheses. A changed result of an unchanged formula is shown
  as `old → new (=formula)`; volatile functions (`RAND`, `NOW`…) aren't
  reported for recomputing.
- `-q` / `--quiet` prints nothing.

Exit status, as diff(1): **0** the workbooks are the same, **1** they differ,
**2** trouble (unreadable file, bad arguments).

## `xl textconv FILE`

A stable text dump, one cell per line, for git's `textconv`:

```text
# Sheet1
Sheet1!A1	Item
Sheet1!B2	3
Sheet1!D2	43.5	=B2*C2
# name Total	=Sheet1!$D$5
```

Each sheet starts with a `# Sheet` line; then cells in row-major order as
`Sheet!A1<TAB>value[<TAB>=formula]`. Values are exact (numbers at full
precision); tabs, newlines and backslashes inside values are escaped as
`\t`, `\n` and `\\`. Defined names come last.

## Setting up git

Tell git which files are workbooks — in the repository's `.gitattributes`
(commit it so everyone gets it):

```gitattributes
*.xlsx diff=xl
*.xlsm diff=xl
```

Then pick one (or both) of these, in your git config (`--global` for all
repositories):

### Option A: textconv (works everywhere: `diff`, `log -p`, `show`, blame…)

```sh
git config --global diff.xl.textconv "xl textconv"
git config --global diff.xl.cachetextconv true   # optional: cache dumps per blob
```

`git diff` then shows an ordinary line diff of the dumps:

```diff
-Sheet1!C2	14.5
+Sheet1!C2	15
```

### Option B: `xl diff` as the diff driver (cell-aware output)

```sh
git config --global diff.xl.command "xl diff"
```

git runs `xl diff` with its seven external-diff arguments (path, old file,
old hash, old mode, new file, new hash, new mode); `xl` recognizes that form,
prints a `diff --xl a/… b/…` header and exits 0 as git expects. Added or
deleted files arrive as `/dev/null`, which `xl` treats as an empty workbook.
`git diff` uses external drivers by default; `git log -p` and `git show`
need `--ext-diff`. With both A and B configured, the command wins where
external diffs are used and textconv is used everywhere else.

### `git difftool`

```sh
git config --global difftool.xl.cmd 'xl diff "$LOCAL" "$REMOTE"'
git difftool -t xl HEAD~1 -- budget.xlsx
```

(`git difftool` ignores the exit status unless given `--trust-exit-code`.)

### Merges

Workbooks can't be merged line by line; mark them binary for merging so git
never tries (and never leaves conflict markers inside a zip):

```gitattributes
*.xlsx diff=xl merge=binary
```

On a conflict, keep one side (`git checkout --ours budget.xlsx` or
`--theirs`), look at what the other side changed with
`xl diff budget.xlsx <(git show MERGE_HEAD:budget.xlsx)` (or check both out
and compare them in xl with **Compare with file…**), and re-apply those
edits by hand. No custom merge driver is needed.

## Version history in the app

Independently of git, xl keeps a local history of every workbook you save:
**Menu → Version history…** or the command palette ("Version history",
"Changes since last save", "Compare with file…", "Name this version…").

- Every save records a version. Pick one in the panel to see what changed
  since: changed cells are highlighted on the grid (green added, red removed
  or cleared, yellow changed, a red rule where rows/columns were deleted),
  hovering one shows old → new, and the list of changes jumps to each.
  **Restore this version** makes the workbook match it as a single edit you
  can undo; **Open as copy** opens it in a new window as an unsaved copy.
- **Changes since last save** compares the workbook with its file on disk;
  **Compare with file…** with any other xlsx/csv.
- **Name…** labels a version; named versions are never pruned.

Versions are stored under `$XDG_DATA_HOME/xl/history/` (default
`~/.local/share/xl/history/`), one directory per document. A document is
identified by its path; **Save as** carries the history over to the new path.
Each version is a small text manifest listing the file's zip entries by
SHA-256; the entries themselves are stored once each, deflate-compressed, so
repeated saves of a big workbook only store the parts that changed.

Retention: the newest version and named versions are always kept; otherwise
all versions from the last day, the newest per hour for a week, and the
newest per day for a month. Each document's history is also capped (256 MB
by default; the oldest unnamed versions go first). Override in
`~/.local/share/xl/history/config`:

```ini
keep_all_hours = 24
hourly_days = 7
daily_days = 30
max_mb = 256
autosave_minutes = 0   # >0: also record a version this often while editing
```

# xlsx fidelity

Saving over an `.xlsx` that xl opened **patches the original package** instead
of writing a new file (`crates/xl-io/src/{package.rs,xlsx_patch.rs}`; plan in
`PLAN.md`, "M6 — Lossless xlsx").

## What's preserved

- **Every part xl didn't change is copied byte for byte**: charts, pivot
  tables, drawings and images, comments, custom XML, VBA projects, themes, printer
  settings, external links, extension lists. A save without edits leaves every
  part identical.
- **Changed sheets are spliced, not rewritten**: only the elements whose
  content changed (`sheetData`, `dimension`, `cols`, `mergeCells`,
  `conditionalFormatting`, `autoFilter`, the frozen pane) are regenerated; the
  sheet's other elements stay as they were.
- **Styles and shared strings are append-only**: cells that still look the same
  keep their original style index (so formatting xl doesn't model, like text
  rotation, survives); new styles and strings are appended.
- New, renamed and reordered sheets and defined names patch `workbook.xml`.
  `calcChain.xml` is dropped when cells change (Excel rebuilds it).

Some edits can't be patched yet — mainly inserting or deleting rows/columns
in files with parts that hold cell addresses (hyperlinks, validation,
drawings, charts, pivot caches). Those saves write a fresh file, which loses
what xl doesn't model; xl warns first and says why (`xl_io::save_plan`).

## Checking it

`crates/xl-io/examples/roundtrip.rs` runs each file through a no-op save and
six edits (a new value below the data, a changed value in the middle, bold on
a cell, an added sheet, a renamed unreferenced sheet, an inserted row). After
each it checks that only the parts the edit concerns changed, nothing was
dropped, changed XML is well-formed, and the file reopens in xl with the same
values everywhere. An inserted row that has to rewrite the file counts as
"fell back" rather than failed. The report names files by index and basename
only.

Against the pinned real-world corpus (≈90 files from Apache POI, LibreOffice
and calamine test suites — Excel, Excel Online, Mac Excel, LibreOffice and
openpyxl-made, with charts, pivots, tables, comments, validation, conditional
formatting, rich text; sources, licenses and SHA-256s in
`scripts/xlsx-corpus/manifest.txt`; the files aren't committed):

```sh
scripts/fetch-xlsx-corpus.sh             # into target/xlsx-corpus/, hash-checked
cargo run -p xl-io --release --example roundtrip -- \
  --known-failures scripts/xlsx-corpus/known-failures.txt target/xlsx-corpus
```

Options: `--out DIR` (saved copies, default `target/roundtrip-out`, listed in
`DIR/outputs.tsv`), `--json FILE`, `--summary FILE`, `--quiet`. It also takes
any `.xlsx` files or directories of your own. It exits non-zero if a file
fails, except those in `--known-failures` (known bugs, each with a one-line
reason; the checker tells you when one starts passing).

With LibreOffice installed, `scripts/lo-check-xlsx.sh target/roundtrip-out`
then opens every saved copy headlessly and converts it to CSV (all sheets);
no-op saves must give the same CSVs as the original.

To add corpus files: append a line to the manifest with `-` as its hash, run
the fetch script (it prints the hash and fails), and paste the hash in.
Intentionally malformed fixtures are listed with `skip` and never fetched.

## CI

`.github/workflows/xlsx-corpus.yml` runs on every push to main and every pull
request: it builds the checker, fetches the corpus (cached by the manifest's
hash), runs the checker, then installs `libreoffice-calc-nogui` and runs
`lo-check-xlsx.sh` over everything the checker saved. The text/JSON reports
and the LibreOffice report are uploaded as the `xlsx-roundtrip-report`
artifact, and the totals go in the job summary.

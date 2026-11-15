#!/usr/bin/env bash
# Open every file the roundtrip checker saved in headless LibreOffice:
#
#   scripts/lo-check-xlsx.sh OUT_DIR [KNOWN_FAILURES]
#
# OUT_DIR is the roundtrip example's --out directory; its outputs.tsv lists
# (input, scenario, output). Every output must open and convert to CSV (all
# sheets); for no-op saves, the CSVs must equal those of the original file.
# Writes OUT_DIR/lo-report.txt. Files listed in KNOWN_FAILURES (basenames,
# as for the roundtrip example) are reported but don't fail the run.
#
# Needs soffice (e.g. apt-get install libreoffice-calc-nogui). Parallelism:
# LO_JOBS (default: nproc). Per-conversion timeout: LO_TIMEOUT (default 120s).
set -euo pipefail
OUT="${1:?usage: lo-check-xlsx.sh OUT_DIR [KNOWN_FAILURES]}"
KNOWN="${2:-}"
TSV="$OUT/outputs.tsv"
[ -f "$TSV" ] || { echo "no $TSV (run the roundtrip example with --out $OUT first)" >&2; exit 2; }
command -v soffice >/dev/null || { echo "soffice not found" >&2; exit 2; }
JOBS="${LO_JOBS:-$(nproc)}"
TIMEOUT="${LO_TIMEOUT:-120}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/lo-check.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
# All sheets, UTF-8, values as shown (filter token 12 = -1: one CSV per sheet).
FILTER='csv:Text - txt - csv (StarCalc):44,34,76,1,,0,false,true,true,false,false,-1'

# Jobs: one per output, plus the original of every file with a no-op save.
# Each is staged as jobs/<k>/book.xlsx so CSV names match between jobs.
k=0
: > "$WORK/jobs.tsv"
while IFS=$'\t' read -r input scenario output; do
  if [ "$scenario" = noop ]; then
    k=$((k + 1)); mkdir -p "$WORK/jobs/$k"; cp "$input" "$WORK/jobs/$k/book.xlsx"
    printf '%s\t%s\t%s\n' "$k" "$(basename "$input")" original >> "$WORK/jobs.tsv"
  fi
  k=$((k + 1)); mkdir -p "$WORK/jobs/$k"; cp "$output" "$WORK/jobs/$k/book.xlsx"
  printf '%s\t%s\t%s\n' "$k" "$(basename "$input")" "$scenario" >> "$WORK/jobs.tsv"
done < "$TSV"
echo "converting $k files with $JOBS LibreOffice workers"

# Each worker has its own LibreOffice profile (one soffice per profile).
for w in $(seq 0 $((JOBS - 1))); do
  (
    profile="file://$WORK/profile-$w"
    awk -F'\t' -v w="$w" -v n="$JOBS" '(NR - 1) % n == w { print $1 }' "$WORK/jobs.tsv" | while read -r id; do
      dir="$WORK/jobs/$id"
      if timeout "$TIMEOUT" soffice "-env:UserInstallation=$profile" --headless --norestore \
           --convert-to "$FILTER" --outdir "$dir/csv" "$dir/book.xlsx" > "$dir/log" 2>&1 \
         && compgen -G "$dir/csv/*.csv" > /dev/null; then
        touch "$dir/ok"
      fi
    done
  ) &
done
wait

is_known() {
  [ -n "$KNOWN" ] && awk -v n="$1" '{ sub(/#.*/, ""); gsub(/[ \t]+/, "") } $0 == n { f = 1 } END { exit !f }' "$KNOWN"
}
REPORT="$OUT/lo-report.txt"
: > "$REPORT"
fails=0 known=0 ok=0
declare -A original_job
while IFS=$'\t' read -r id name scenario; do
  dir="$WORK/jobs/$id"
  status=OK reason=""
  if [ "$scenario" = original ]; then
    # Only the reference for the no-op comparison; listed just if it fails.
    original_job[$name]=$id
    [ -f "$dir/ok" ] && continue
    status=NOTE reason="the original itself doesn't convert in LibreOffice"
  elif [ ! -f "$dir/ok" ]; then
    status=FAIL reason="LibreOffice couldn't open/convert it ($(tail -n1 "$dir/log" 2>/dev/null | cut -c1-120))"
  elif [ "$scenario" = noop ]; then
    orig="${original_job[$name]:-}"
    if [ -z "$orig" ] || [ ! -f "$WORK/jobs/$orig/ok" ]; then
      status=SKIP reason="the original doesn't convert in LibreOffice either"
    elif ! diff -rq "$WORK/jobs/$orig/csv" "$dir/csv" > "$dir/diff" 2>&1; then
      status=FAIL reason="CSV differs from the original's: $(wc -l < "$dir/diff") sheet file(s) differ"
    fi
  fi
  if [ "$status" = FAIL ]; then
    if is_known "$name"; then status="FAIL(known)"; known=$((known + 1)); else fails=$((fails + 1)); fi
  elif [ "$status" = OK ]; then
    ok=$((ok + 1))
  fi
  printf '%s\t%s\t%s\t%s\n' "$status" "$name" "$scenario" "$reason" >> "$REPORT"
done < "$WORK/jobs.tsv"

grep -v '^OK' "$REPORT" || true
echo "LibreOffice: $ok ok, $fails failed, $known known failures (report: $REPORT)"
[ "$fails" -eq 0 ]

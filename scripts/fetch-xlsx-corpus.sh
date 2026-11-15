#!/usr/bin/env bash
# Download the pinned xlsx test corpus (see scripts/xlsx-corpus/manifest.txt)
# into a cache directory, verifying each file's SHA-256.
#
#   scripts/fetch-xlsx-corpus.sh [DIR]      (default: target/xlsx-corpus)
#
# Files already present with the right hash are kept. Entries marked "skip"
# in the manifest (intentionally broken fixtures) are not downloaded. An entry
# whose hash is "-" is downloaded and its hash printed, so new entries can be
# pinned; the script then exits non-zero.
#
# The files are untrusted data: they are only downloaded and hashed here.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/scripts/xlsx-corpus/manifest.txt"
DIR="${1:-$ROOT/target/xlsx-corpus}"
mkdir -p "$DIR"

fetched=0 kept=0 skipped=0 failed=0 unpinned=0
while read -r name sha license url note; do
  case "$name" in ''|'#'*) continue ;; esac
  if [ "$sha" = skip ]; then
    skipped=$((skipped + 1)); continue
  fi
  case "$name" in */*|.*) echo "bad name in manifest: $name" >&2; failed=$((failed + 1)); continue ;; esac
  dest="$DIR/$name"
  if [ "$sha" != - ] && [ -f "$dest" ] && [ "$(sha256sum "$dest" | cut -d' ' -f1)" = "$sha" ]; then
    kept=$((kept + 1)); continue
  fi
  tmp="$dest.part"
  if ! curl -fsSL --retry 3 --retry-delay 2 --max-time 120 -o "$tmp" "$url"; then
    echo "FAIL download $name ($url)" >&2
    rm -f "$tmp"; failed=$((failed + 1)); continue
  fi
  got="$(sha256sum "$tmp" | cut -d' ' -f1)"
  if [ "$sha" = - ]; then
    echo "UNPINNED $name $got"
    mv "$tmp" "$dest"; unpinned=$((unpinned + 1)); continue
  fi
  if [ "$got" != "$sha" ]; then
    echo "FAIL sha256 $name: expected $sha, got $got" >&2
    rm -f "$tmp"; failed=$((failed + 1)); continue
  fi
  mv "$tmp" "$dest"; fetched=$((fetched + 1))
done < "$MANIFEST"

echo "xlsx corpus in $DIR: $fetched fetched, $kept already there, $skipped skipped (known broken), $failed failed, $unpinned unpinned"
[ "$failed" -eq 0 ] && [ "$unpinned" -eq 0 ]

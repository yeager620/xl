#!/usr/bin/env bash
# Fetch the pinned GPUI (Zed) revision as a shallow clone into vendor/zed.
# GPUI's crates.io release is stale, so we build against a pinned Zed commit.
set -euo pipefail
REV=dc93abce2fa6c0bf349d425310db4884aa953467
DIR="$(cd "$(dirname "$0")/.." && pwd)/vendor/zed"
if [ -d "$DIR/.git" ] && [ "$(git -C "$DIR" rev-parse HEAD)" = "$REV" ]; then
  echo "vendor/zed already at $REV"; exit 0
fi
rm -rf "$DIR"; mkdir -p "$DIR"; cd "$DIR"
git init -q
git remote add origin https://github.com/zed-industries/zed.git
git fetch -q --depth 1 origin "$REV"
git checkout -q FETCH_HEAD
echo "vendor/zed at $(git rev-parse --short HEAD)"

#!/usr/bin/env bash
# Build, install and lint xl as a Flatpak from this working tree, the way
# Flathub will build it. Log: target/flathub/check.log
#
# Downloads: Flathub's builder (org.flatpak.Builder), the Freedesktop 25.08
# runtime + SDK + Rust extension (~2 GB once), and the crates in Cargo.lock.
# Everything is installed per-user; no sudo.
set -euo pipefail
cd "$(dirname "$0")/../.."
OUT=target/flathub
mkdir -p "$OUT"
exec > >(tee "$OUT/check.log") 2>&1

GEN_COMMIT=9d86d336a39b9af38b86f114b6ce5820fe260f91
GEN="$OUT/flatpak-cargo-generator.py"
echo "== 1/4 Offline crate sources"
[ -f "$GEN" ] || curl -fsSL -o "$GEN" "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$GEN_COMMIT/cargo/flatpak-cargo-generator.py"
if [ ! -f packaging/flathub/cargo-sources.json ] || [ Cargo.lock -nt packaging/flathub/cargo-sources.json ]; then
  uv run -q --script "$GEN" Cargo.lock -o packaging/flathub/cargo-sources.json
fi
echo "   $(python3 -c 'import json;print(len(json.load(open("packaging/flathub/cargo-sources.json"))))') sources"

echo "== 2/4 Flathub builder and linter"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user -y --noninteractive flathub org.flatpak.Builder

echo "== 3/4 Build from this working tree"
# Same manifest, but the xl source is this checkout instead of the release tag.
python3 - "$OUT" <<'PY'
import re, shutil, sys
out = sys.argv[1]
src = open("packaging/flathub/io.github.yeager620.xl.yml").read()
local = re.sub(
    r"      - type: git\n        url: https://github.com/yeager620/xl.git\n.*?\n.*?\n",
    "      - type: dir\n        path: ../..\n        skip: [target, vendor, .git]\n",
    src, count=1, flags=re.S)
open(f"{out}/io.github.yeager620.xl.yml", "w").write(local)
shutil.copy("packaging/flathub/cargo-sources.json", out)
PY
flatpak run org.flatpak.Builder --user --install-deps-from=flathub --force-clean --disable-rofiles-fuse \
  --repo="$OUT/repo" --install "$OUT/build" "$OUT/io.github.yeager620.xl.yml"

echo "== 4/4 Flathub linter"
flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo "$OUT/repo" && echo "   repo: OK"
flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest "$OUT/io.github.yeager620.xl.yml" || true

echo
echo "Done. Try it: flatpak run io.github.yeager620.xl"

#!/usr/bin/env bash
# Build a Flatpak bundle into target/packages/xl.flatpak.
# Needs: flatpak-builder (sudo apt install flatpak-builder) and uv (for the
# cargo source generator). The first build downloads the Freedesktop runtime.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v flatpak-builder >/dev/null || { echo "flatpak-builder is not installed: sudo apt install flatpak-builder"; exit 1; }
[ -d vendor/zed/.git ] || ./scripts/fetch-gpui.sh

# Offline crate sources for the sandboxed build, generated from Cargo.lock.
GEN=target/tools/flatpak-cargo-generator.py
if [ ! -f "$GEN" ]; then
  mkdir -p target/tools
  curl -fsSL -o "$GEN" "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py"
fi
uv run -q --with aiohttp --with tomlkit python3 "$GEN" Cargo.lock -o packaging/flatpak/cargo-sources.json

flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean \
  --repo=target/flatpak-repo target/flatpak-build packaging/flatpak/io.github.yeager620.xl.yml
mkdir -p target/packages
flatpak build-bundle target/flatpak-repo target/packages/xl.flatpak io.github.yeager620.xl
echo "Built target/packages/xl.flatpak (install with: flatpak install --user target/packages/xl.flatpak)"

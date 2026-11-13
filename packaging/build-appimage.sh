#!/usr/bin/env bash
# Build a portable AppImage into target/packages/.
# Graphics (Vulkan/EGL/Wayland) and glibc come from the host, as AppImages should;
# build on the oldest distro you want to support (glibc is forward-compatible only).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(uname -m)
TOOL_VERSION=1.9.1
TOOL="target/tools/appimagetool-$TOOL_VERSION-$ARCH.AppImage"
APPDIR="$PWD/target/appdir/xl.AppDir"

if [ ! -x "$TOOL" ]; then
  echo "Downloading appimagetool $TOOL_VERSION…"
  mkdir -p target/tools
  curl -fsSL -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VERSION/appimagetool-$ARCH.AppImage"
  chmod +x "$TOOL"
fi

rm -rf "$APPDIR"
make install DESTDIR="$APPDIR" PREFIX=/usr
strip "$APPDIR/usr/bin/xl"
# AppImage layout: desktop entry and icon at the root, AppRun launches the binary.
cp packaging/io.github.yeager620.xl.desktop "$APPDIR/"
cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/io.github.yeager620.xl.png" "$APPDIR/io.github.yeager620.xl.png"
ln -sf usr/bin/xl "$APPDIR/AppRun"

mkdir -p target/packages
OUT="target/packages/xl-$VERSION-$ARCH.AppImage"
# Extract-and-run avoids needing FUSE for the tool itself.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$TOOL" --no-appstream "$APPDIR" "$OUT"
echo "Built $OUT"

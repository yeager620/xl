#!/usr/bin/env bash
# Build a .deb for the host architecture into target/packages/.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg-architecture -qDEB_HOST_ARCH)
MAINTAINER=${MAINTAINER:-"xl contributors <xl@users.noreply.invalid>"}
STAGE="$PWD/target/packages/deb/xl_${VERSION}_${ARCH}"

rm -rf "$STAGE"
make install DESTDIR="$STAGE" PREFIX=/usr
strip "$STAGE/usr/bin/xl"
install -Dm644 packaging/copyright "$STAGE/usr/share/doc/xl/copyright"

# Libraries the binary links directly (dpkg-shlibdeps wants a debian/control to run from).
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/debian"
printf 'Source: xl\n\nPackage: xl\nArchitecture: any\n' > "$WORK/debian/control"
SHLIBS=$(cd "$WORK" && dpkg-shlibdeps -O -e "$STAGE/usr/bin/xl" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
# Loaded at runtime with dlopen, so shlibdeps can't see them.
RUNTIME="libvulkan1, libwayland-client0, libegl1, libx11-xcb1"

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: xl
Version: $VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $(du -sk "$STAGE/usr" | cut -f1)
Depends: ${SHLIBS:+$SHLIBS, }$RUNTIME
Recommends: mesa-vulkan-drivers | vulkan-icd
Section: misc
Priority: optional
Description: Fast native spreadsheet
 xl opens and saves Excel workbooks and CSV files and recalculates only
 what changed, so edits show up instantly even in large sheets. It has
 Excel-style formulas, number formats, cell styles, multiple sheets,
 autosave and crash recovery.
CONTROL

mkdir -p target/packages
dpkg-deb --root-owner-group --build "$STAGE" "target/packages/xl_${VERSION}_${ARCH}.deb" >/dev/null
echo "Built target/packages/xl_${VERSION}_${ARCH}.deb"

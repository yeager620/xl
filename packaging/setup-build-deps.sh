#!/usr/bin/env bash
# Install build dependencies on Debian/Ubuntu (used by CI and container builds).
# Run as root (CI containers) or with sudo.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
SUDO=$([ "$(id -u)" = 0 ] && echo "" || echo sudo)
$SUDO apt-get update -qq
$SUDO apt-get install -y -qq --no-install-recommends \
  build-essential pkg-config cmake clang git curl ca-certificates file binutils \
  dpkg-dev apt-utils gnupg \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev \
  libssl-dev libzstd-dev libasound2-dev >/dev/null
# Rust: rustup installs the toolchain pinned in rust-toolchain.toml on first use.
if ! command -v cargo >/dev/null; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none >/dev/null
fi
# Install the toolchain pinned in rust-toolchain.toml (rustup ≥1.28 doesn't auto-install).
. "$HOME/.cargo/env" 2>/dev/null || true
rustup toolchain install >/dev/null

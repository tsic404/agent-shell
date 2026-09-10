#!/usr/bin/env bash
# Regression check for the vendored libspa-sys UOS 20 compatibility patch.
#
# UOS 20 Pro (deepin 20) ships pipewire 0.3.15, whose SPA headers lack the
# spa_type_* symbols that pipewire-rs 0.6+ references unconditionally in
# type-info.c. The vendored copy gates those symbols behind PW_CHECK_VERSION.
#
# This script compiles the vendored type-info.c against the real deepin
# libspa-0.2-dev + libpipewire-0.3-dev 0.3.15.1 headers and asserts:
#   * the gated (patched) file compiles; and
#   * the ungated (every PW_CHECK_VERSION gate forced on) file does NOT
#     compile — proving the fetched headers genuinely lack the symbols the
#     gates skip (otherwise this test would be void).
#
# Requires: curl, dpkg-deb, cc (gcc).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
type_info="$repo_root/vendor/libspa-sys/src/type-info.c"

MIRROR="${DEEPIN_MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/deepin}"
VERSION="0.3.15.1-1"
ARCH="amd64"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fetch() {
  local pkg="$1"
  local url="$MIRROR/pool/main/p/pipewire/${pkg}_${VERSION}_${ARCH}.deb"
  curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 "$url" -o "$tmp/$pkg.deb"
}

echo "Fetching deepin pipewire ${VERSION} headers (${MIRROR})..."
fetch libspa-0.2-dev
fetch libpipewire-0.3-dev

mkdir -p "$tmp/spa" "$tmp/pw"
dpkg-deb -x "$tmp/libspa-0.2-dev.deb" "$tmp/spa"
dpkg-deb -x "$tmp/libpipewire-0.3-dev.deb" "$tmp/pw"

cflags=(
  -I"$tmp/pw/usr/include/pipewire-0.3"
  -I"$tmp/spa/usr/include/spa-0.2"
)

# Positive: gated file must compile against old headers.
cc "${cflags[@]}" -c "$type_info" -o "$tmp/type-info.o"

# Negative: with every gate forced on, the same headers must fail to compile.
sed 's/#if PW_CHECK_VERSION([0-9,]*)/#if 1/' "$type_info" > "$tmp/ungated.c"
if cc "${cflags[@]}" -c "$tmp/ungated.c" -o "$tmp/ungated.o" 2>/dev/null; then
  echo "ERROR: ungated type-info.c compiled against 0.3.15.1 headers (expected failure)" >&2
  exit 1
fi

echo "OK: type-info.c compiles against deepin pipewire 0.3.15.1 headers"

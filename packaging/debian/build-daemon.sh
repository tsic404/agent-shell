#!/usr/bin/env bash
# Build agent-shell-daemon on a Debian 12 (bookworm) compatible builder and copy
# the binary into target/release/ so packaging/debian/build-deb.sh can bundle it.
#
# The daemon links libpipewire-0.3 at runtime, so it cannot be musl-static like
# the pure-Rust binaries (see packaging/musl/). Debian 12 (glibc 2.36, pipewire
# 0.3.65) is the oldest still-supported distro that both satisfies the
# pipewire/libspa crates' SPA >= 0.3.65 requirement and stays under DDE 25's
# glibc 2.38. DDE 20 (glibc 2.28) still requires a native UOS 20 / deepin 20
# build — see packaging/musl/README.md.
#
# Requires: docker (with BuildKit for --output), binutils (objdump).
# Usage: ./packaging/debian/build-daemon.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$repo_root/target/release"
mkdir -p "$out"

cd "$repo_root"
docker build -f packaging/debian/Dockerfile \
    --output type=local,dest="$out" \
    "$repo_root"

bin="$out/agent-shell-daemon"
echo "Built glibc-2.36-compatible daemon: $bin"

# Gate: the compat build must not require a glibc newer than DDE 25's 2.38,
# otherwise DDE 25 coverage regresses silently.
"$repo_root/scripts/check-daemon-glibc.sh" "$bin"

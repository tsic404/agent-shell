#!/usr/bin/env bash
# Build static musl binaries for agent-shell's pure-Rust targets and copy them
# into target/x86_64-unknown-linux-musl/release/.
#
# Requires: docker (with BuildKit for --output).
# Usage: ./packaging/musl/build-musl.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$repo_root/target/x86_64-unknown-linux-musl/release"
mkdir -p "$out"

cd "$repo_root"
docker build -f packaging/musl/Dockerfile \
    --output type=local,dest="$out" \
    "$repo_root"

echo "Built static musl binaries in $out:"
for bin in agent-shell agent-shell-rootd agent-shell-mcp; do
    if [ -x "$out/$bin" ]; then
        echo "  $out/$bin"
    fi
done

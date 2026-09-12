#!/usr/bin/env bash
# Verify agent-shell-daemon was built against a compatible glibc.
#
# The daemon dynamically links libpipewire-0.3 and dlopens libwayland-client, so
# it must be built on a distro whose glibc is old enough for the target hosts.
# The container compat builder is Debian 12 (bookworm, glibc 2.36); the target
# is DDE 25 (glibc 2.38). This check fails if the binary requires any GLIBC
# symbol version newer than 2.38, which would silently break DDE 25 coverage.
#
# Requires: objdump (binutils).
# Usage: scripts/check-daemon-glibc.sh [path-to-binary]
set -euo pipefail

bin="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/agent-shell-daemon}"
# DDE 25 glibc — the max the compat-built daemon may require.
max_glibc="${MAX_GLIBC:-2.38}"

[ -x "$bin" ] || { echo "ERROR: $bin not found or not executable" >&2; exit 1; }

# Dynamic deps sanity: the daemon is NOT static (it links libpipewire at runtime).
if ! objdump -p "$bin" | grep -q 'NEEDED'; then
    echo "ERROR: $bin has no dynamic deps (expected libpipewire-0.3 linkage)" >&2
    exit 1
fi

# Highest GLIBC_x.y.z symbol version required by the binary.
highest="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | sed 's/^GLIBC_//')"

if [ -z "$highest" ]; then
    echo "ERROR: could not determine GLIBC symbol versions in $bin" >&2
    exit 1
fi

if [ "$(printf '%s\n%s\n' "$max_glibc" "$highest" | sort -V | tail -1)" != "$max_glibc" ]; then
    echo "ERROR: $bin requires GLIBC_$highest (> $max_glibc); rebuild on a glibc<=2.38 builder" >&2
    exit 1
fi

echo "ok: $bin requires GLIBC_$highest (<= $max_glibc)"

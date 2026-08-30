#!/bin/bash
# qa/tests/test_hostname_validation.sh — offline fixture for rootd hostname validation.
#
# QA-side execution surface for the TSI-2707 assertions. The per-case
# assertions live in rootd/src/lib.rs `hostname_validation` — single source
# of truth; this script only provides the runnable qa entry point. It needs
# no root and no hostnamectl: invalid hostnames (`a..b`, `b.`) are rejected
# by `validate_hostname` before any spawn, and the legal `a.b` only fails at
# the real spawn (no hostnamectl offline → still an error there).
#
# Usage: qa/tests/test_hostname_validation.sh   (exit 0 = all assertions pass)

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

cd "$ROOT"
cargo test -p agent-shell-rootd --lib hostname_validation

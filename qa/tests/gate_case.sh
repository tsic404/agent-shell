#!/bin/bash
# qa/tests/gate_case.sh — driver for the pk_gate_unreachable assertions in
# qa/tests/test_pkkit.sh.
#
# Runs one scenario per process. The seam is PATH: `pk_priv` with
# PK_SUDO=direct resolves its argv through path lookup, so fake
# executables on PATH override the system tools. Bash function stubs do
# not work here — with `>/dev/null 2>&1` attached to the call, bash 5.3
# stops xtracing the function body and the stubs appear inert; PATH-level
# stubs sidestep the ambiguity.
#
# Stubs split into two classes:
#   - STATE-CHANGING (mv): must really do the thing. A no-op `mv` that
#     returns 0 would pass the call but leave the activation at its
#     original path, so the function's file-presence assertion would
#     MISMATCH. The state-changing assertion is what proves the gate.
#   - CONTROLLABLE (systemctl, pgrep, pkill): may be stubs.
#
# Each case drives one reachable code path. All fail-fast paths
# auto-restore before returning rc=1 (that's the point of the new
# teardown contract): activation is unhidden and polkit.service is
# unmasked even on the failure branch.
#   CASE 1: happy path                                -> rc=0 hidden=y dir=n
#   CASE 2: mask fails (first || return 1)            -> rc=1 hidden=n dir=y
#   CASE 3: hide fails (second || return 1)           -> rc=1 hidden=n dir=y
#   CASE 4: pgrep still returns a PID after stop      -> rc=1 hidden=n dir=y
#   CASE 5: stop fails after mask succeeded           -> rc=1 hidden=n dir=y
# The MISMATCH branch when the activation file is still present (pgrep
# empty but the file is) is NOT reachable by stubs alone: if `mv`
# succeeds the file is gone, and if `mv` fails the second || handler
# fires first. That branch is a defensive guard, not a live code path —
# leaving it uncovered is honest, not a gap.
#
# CASE 3 requires a non-root environment. It works by chmod-ing the
# parent directory of the activation file to a-w so `mv -f` fails with
# EACCES. Root ignores directory permission bits, so on a root host
# CASE 3 would silently pass (rc=0) instead of exercising the fail
# path. Skip the driver on root hosts, or run it under a non-root user.
#
# Usage: PK_KIT=<path> WORKDIR=<dir> CASE=1..5 qa/tests/gate_case.sh
# Prints: rc=<n> hidden=<y|n> dir=<y|n>
set -u

KIT="${PK_KIT:?PK_KIT is required}"
WORK="${WORKDIR:?WORKDIR is required}"
CASE="${CASE:?CASE is required}"

BIN="$WORK/bin"
mkdir -p "$BIN"

# Real mv: forwards to /usr/bin/mv. The state-changing assertion at the
# end of pk_gate_unreachable re-checks the filesystem, so a fake mv that
# just returns 0 would leave the activation in place and the gate would
# MISMATCH.
printf '#!/bin/bash\nexec /usr/bin/mv "$@"\n' > "$BIN/mv"
chmod +x "$BIN/mv"

# pkill is stubbed to fail. pk_gate_unreachable tolerates this with
# `|| true` — proves the tolerance without touching a real polkitd.
printf '#!/bin/bash\nexit 1\n' > "$BIN/pkill"
chmod +x "$BIN/pkill"

# systemctl: stateful stub. `mode=first` = first call rc 0, later rc 1
# (drives the third || return 1 on `systemctl stop`). Otherwise the
# single exit code applies to every call.
make_systemctl() { # $1 = 0|1, $2 = optional "first"
  if [ "${2:-}" = first ]; then
    printf '#!/bin/bash\nn=$(cat %q 2>/dev/null || echo 0); n=$((n+1)); printf %%s "$n" > %q; if [ "$n" = 1 ]; then exit 0; fi; exit 1\n' \
      "$BIN/sc_n" "$BIN/sc_n" > "$BIN/systemctl"
  else
    printf '#!/bin/bash\nexit %s\n' "$1" > "$BIN/systemctl"
  fi
  chmod +x "$BIN/systemctl"
  rm -f "$BIN/sc_n"
}

# pgrep: stateful stub. `mode=alive` = first 2 calls print a PID (drives
# the loop and its MISMATCH), `mode=gone` = always empty.
make_pgrep() { # $1 = alive|gone
  if [ "$1" = alive ]; then
    printf '#!/bin/bash\nn=$(cat %q 2>/dev/null || echo 0); n=$((n+1)); printf %%s "$n" > %q; if [ "$n" -le 2 ]; then printf %%s\\n 999; fi; exit 0\n' \
      "$BIN/pg_n" "$BIN/pg_n" > "$BIN/pgrep"
  else
    printf '#!/bin/bash\nexit 1\n' > "$BIN/pgrep"
  fi
  chmod +x "$BIN/pgrep"
  rm -f "$BIN/pg_n"
}

case "$CASE" in
  1) make_systemctl 0;  make_pgrep gone
     ;;
  2) make_systemctl 1;  make_pgrep gone
     ;;
  3) make_systemctl 0;  make_pgrep gone
     ;;
  4) make_systemctl 0;  make_pgrep alive
     ;;
  5) make_systemctl 0 first; make_pgrep gone
     ;;
  *) echo "unknown CASE=$CASE" >&2; exit 2 ;;
esac

export PATH="$BIN:$PATH"
export PK_SUDO=direct
export PK_RULE_DIR="$WORK/rules.d"
export PK_RULE_NAME="gate-case-$CASE"
export PK_RULE_ORDER=10
export PK_ACTIVATE="$WORK/activation.service"
# Point the lock dir at a scratch dir so the harness doesn't touch
# /run/lock/agent-shell-qa. The lock file itself is still opened via
# `exec 9>>` — which is what the driver is testing.
export PK_LOCK_DIR="$WORK/locks"
export PK_LOCK_TIMEOUT=5
mkdir -p "$PK_RULE_DIR" "$PK_LOCK_DIR"

source "$KIT"

ACT="$PK_ACTIVATE"
rm -f "$ACT" "$ACT.verity-hidden"

case "$CASE" in
  3)
    # Requires non-root: root ignores directory permission bits, so on a
    # root host `mv -f` would succeed and this case would pass silently
    # instead of exercising the fail path. The driver assumes a non-root
    # shell; skip on root hosts (see header).
    printf '%s\n' 'bus name' > "$ACT"
    chmod a-w "$WORK"
    chmod u+w "$BIN"   # keep the stubs writable in case they need to
    ;;
  *)
    printf '%s\n' 'bus name' > "$ACT"
esac

pk_gate_unreachable com.agentshell.mount >/dev/null 2>&1
rc=$?

# Restore writability so later cases in a shared WORK can still write.
chmod u+rwx "$WORK" 2>/dev/null || true

h=$([ -e "$ACT.verity-hidden" ] && echo y || echo n)
d=$([ -e "$ACT" ] && echo y || echo n)
printf 'rc=%s hidden=%s dir=%s\n' "$rc" "$h" "$d"

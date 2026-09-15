#!/bin/bash
# polkit-cleanup.sh — safe teardown / restore for the agent-shell polkit action file.
#
# Why this exists
# ────────────────
# The 2026-08-29 QA run cleaned up with
#     sudo rm -f /usr/share/polkit-1/actions/com.agentshell.policy
# which left polkitd without a single com.agentshell.* action registered. Nothing
# later noticed, because "action not registered" is indistinguishable from "policy
# denied" when you only assert on the denial. The deletion had to be undone by
# hand one session later (TSI-2586, TSI-2628).
#
# This script makes the teardown step idempotent and self-verifying, so one
# command maintains this invariant:
#
#   the action file is never deleted, never left truncated, and never left
#   unparseable — and if a run destroys it, the teardown that should have run
#   quietly restores it from packaging/ instead.
#
# Usage
# ─────
#   polkit-cleanup.sh cleanup     # validate; restore if missing/broken (never deletes)
#   polkit-cleanup.sh restore     # force re-install from packaging/
#   polkit-cleanup.sh verify      # assert pkaction still lists EXPECTED actions
#   polkit-cleanup.sh status      # print state, no mutation
#   polkit-cleanup.sh teardown    # cleanup + verify; the QA run's last command
#
# Exit codes: 0 = invariant holds, 1 = invariant violated, 2 = BLOCKED
# (environment or tooling missing, so the invariant could not be confirmed).
#
# Environment overrides (nothing here is repo-path dependent):
#   PK_SOURCE        explicit path to the trusted policy file
#   PK_SOURCE_DIR    directory containing com.agentshell.policy (usually packaging/)
#   PK_POLICY_DIR    install dir (default /usr/share/polkit-1/actions)
#   PK_POLICY        action file name (default com.agentshell.policy)
#   PK_EXPECTED      expected `pkaction | grep -c com.agentshell` (default 13)
#   PK_SUDO          privilege command; "auto" (default) picks sudo when needed,
#                    "" when already root. "sudo -n" for non-interactive.
#   PK_RELOAD        what to run to ask polkitd to re-read actions
#                    (default: systemctl reload polkit || systemctl restart polkit)
#   PK_NO_RELOAD=1   skip the reload attempt (e.g. containers without polkitd)

set -u

PK_POLICY_DIR="${PK_POLICY_DIR:-/usr/share/polkit-1/actions}"
PK_POLICY="${PK_POLICY:-com.agentshell.policy}"
PK_EXPECTED="${PK_EXPECTED:-13}"
PK_SUDO="${PK_SUDO:-auto}"
PK_NO_RELOAD="${PK_NO_RELOAD:-0}"

TARGET="$PK_POLICY_DIR/$PK_POLICY"

BLOCKED=0
FAILED=0

log()  { printf '[polkit-cleanup] %s\n' "$*"; }
warn() { printf '[polkit-cleanup] WARN: %s\n' "$*" >&2; }
note() { printf '[polkit-cleanup] %s\n' "$*" >&2; }

# ── privilege ─────────────────────────────────────────────────────────────
if [ "$PK_SUDO" = "" ] || [ "$PK_SUDO" = "auto" ]; then
  if [ "$(id -u)" -eq 0 ]; then
    PK_SUDO=""
  elif command -v sudo >/dev/null 2>&1; then
    PK_SUDO="sudo -n"
  else
    PK_SUDO="sudo"
  fi
fi

need_root() { [ "$PK_SUDO" != "" ] && { $PK_SUDO true 2>/dev/null; }; }
root_available() { [ "$PK_SUDO" = "" ] || need_root; }

# ── source resolution ─────────────────────────────────────────────────────
# The trusted copy lives in packaging/ inside a checkout of tsip404/agent-shell.
resolve_source() {
  if [ -n "${PK_SOURCE:-}" ]; then
    if [ -f "$PK_SOURCE" ]; then printf '%s\n' "$PK_SOURCE"; return 0; fi
    warn "PK_SOURCE=$PK_SOURCE is not a readable file"
  fi

  local candidates=()
  if [ -n "${PK_SOURCE_DIR:-}" ]; then
    candidates+=("$PK_SOURCE_DIR/$PK_POLICY")
  fi
  candidates+=(
    "./packaging/$PK_POLICY"
    "$PWD/packaging/$PK_POLICY"
  )
  # Walk up from this script and from the caller, looking for a checkout.
  local d
  for d in "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)" "$PWD"; do
    while [ "$d" != "/" ]; do
      candidates+=("$d/packaging/$PK_POLICY")
      d="$(dirname "$d")"
    done
  done

  local c
  for c in "${candidates[@]}"; do
    if [ -f "$c" ] && [ -s "$c" ]; then printf '%s\n' "$c"; return 0; fi
  done
  return 1
}

# ── validators ────────────────────────────────────────────────────────────
# xmllint is the primary checker; python3 is the fallback so the script does
# not depend on libxml2 being installed.
xml_ok() {
  local f="$1"
  if command -v xmllint >/dev/null 2>&1; then
    xmllint --noout "$f" >/dev/null 2>&1
    return $?
  fi
  if command -v python3 >/dev/null 2>&1; then
    python3 -c "import sys,xml.etree.ElementTree as ET; ET.parse(sys.argv[1])" "$f" \
      >/dev/null 2>&1
    return $?
  fi
  warn "neither xmllint nor python3 available — XML well-formedness unchecked"
  return 0
}

# validate_file returns 0 only when the file is real, non-trivial, carries
# com.agentshell action ids and parses. A truncated file, an unexpanded
# template and a malformed document all fail here, before anything downstream
# asserts on a decision that was never registered.
validate_file() {
  local f="$1" sz
  local bad=""

  [ -f "$f" ] || bad="missing"

  if [ -z "$bad" ]; then
    sz="$(stat -c %s "$f" 2>/dev/null || echo 0)"
    [ "$sz" -ge 100 ] || bad="truncated (${sz} bytes)"
  fi
  if [ -z "$bad" ] && ! grep -q 'com\.agentshell' "$f" 2>/dev/null; then
    bad="no com.agentshell action ids"
  fi
  if [ -z "$bad" ] && grep -qE '%[a-zA-Z]|\$\{|\{\{|<TBD>|TODO' "$f" 2>/dev/null; then
    bad="contains unexpanded placeholder"
  fi
  if [ -z "$bad" ] && ! xml_ok "$f"; then
    bad="malformed XML"
  fi

  if [ -n "$bad" ]; then
    note "invalid: $f — $bad"
    return 1
  fi
  log "valid:   $f ($(stat -c %s "$f") bytes)"
  return 0
}

# ── reload ────────────────────────────────────────────────────────────────
# polkitd >= 0.113 watches its actions directory and reparses on change, so the
# reload is belt-and-braces. Both commands are refused on hosts whose polkit
# policy does not grant non-interactive reload, which is why this is
# best-effort: the assertion in verify() is the proof, the reload is a hint.
reload_polkit() {
  if [ "$PK_NO_RELOAD" = "1" ]; then
    return 0
  fi
  if [ -n "${PK_RELOAD:-}" ]; then
    $PK_SUDO sh -c "$PK_RELOAD" >/dev/null 2>&1 && return 0
  fi
  if [ "$PK_SUDO" = "" ] || need_root; then
    $PK_SUDO systemctl reload polkit >/dev/null 2>&1 && return 0
    $PK_SUDO systemctl restart polkit >/dev/null 2>&1 && return 0
  fi
  warn "polkitd reload refused (needs interactive auth) — relying on the actions-dir watch"
  return 0
}

# ── actions oracle ────────────────────────────────────────────────────────
count_actions() {
  if ! command -v pkaction >/dev/null 2>&1; then
    note "pkaction not found — cannot observe polkitd's registered actions"
    BLOCKED=1
    return 2
  fi
  local n
  n="$(pkaction 2>/dev/null | grep -c 'com\.agentshell' || true)"
  printf '%s\n' "$n"
}

# ── the restore ───────────────────────────────────────────────────────────
restore_from_source() {
  local src
  src="$(resolve_source)" || {
    note "no trusted source found; set PK_SOURCE or PK_SOURCE_DIR (repo packaging/)"
    BLOCKED=1
    return 2
  }
  validate_file "$src" || return 1

  if ! root_available; then
    note "restoring $TARGET requires root; $PK_SUDO is not usable from here"
    BLOCKED=1
    return 2
  fi

  log "restoring $TARGET from $src"
  $PK_SUDO mkdir -p "$PK_POLICY_DIR" 2>/dev/null
  $PK_SUDO install -D -m 644 "$src" "$TARGET" || {
    warn "install failed"
    return 1
  }
  reload_polkit
  validate_file "$TARGET" || return 1
  return 0
}

# ── commands ──────────────────────────────────────────────────────────────
# The one thing this script refuses to do is delete. Teardown means
# "leave the action file in a state polkitd can still parse".
cmd_cleanup() {
  if validate_file "$TARGET"; then
    log "action file present and parseable — nothing to restore"
    if ! resolve_source >/dev/null; then
      warn "healthy file, but no trusted source is reachable (set PK_SOURCE or PK_SOURCE_DIR): if it is destroyed later, 'cleanup' cannot restore it"
    fi
    reload_polkit
    return 0
  fi
  note "action file missing or invalid — restoring"
  restore_from_source
}

cmd_restore() {
  restore_from_source
}

cmd_verify() {
  local n
  n="$(count_actions)" || return 2
  if [ "$n" = "$PK_EXPECTED" ]; then
    log "pkaction lists $n com.agentshell actions (expected $PK_EXPECTED) — OK"
    return 0
  fi
  note "MISMATCH: pkaction lists $n, expected $PK_EXPECTED"
  pkaction 2>/dev/null | grep 'com\.agentshell' || true
  FAILED=1
  return 1
}

cmd_status() {
  local n ok=0
  if validate_file "$TARGET"; then
    ok=1
  else
    note "action file: MISSING/INVALID"
  fi
  n="$(count_actions)" || return 2
  log "pkaction com.agentshell count: $n (expected $PK_EXPECTED)"
  pkaction 2>/dev/null | grep 'com\.agentshell' | sed 's/^/  /'
  local src
  src="$(resolve_source)" && log "source: $src" || note "source: not found"
  [ "$BLOCKED" -eq 0 ] && [ "$ok" -eq 1 ]
}

usage() {
  sed -n '/^# Usage/,/^# Environment/p' "$0" | sed 's/^# \?//'
  exit 2
}

main() {
  local cmd="${1:-cleanup}"
  case "$cmd" in
    cleanup)   cmd_cleanup ;;
    restore)   cmd_restore ;;
    verify)    cmd_verify ;;
    status)    cmd_status ;;
    teardown)  cmd_cleanup && cmd_verify ;;
    -h|--help|help) usage ;;
    *)         note "unknown command: $cmd"; usage ;;
  esac
  local rc=$?
  if [ "$BLOCKED" -ne 0 ]; then
    log "BLOCKED — environment cannot confirm the invariant"
    return 2
  fi
  [ "$FAILED" -eq 0 ] || return 1
  return "$rc"
}

main "$@"

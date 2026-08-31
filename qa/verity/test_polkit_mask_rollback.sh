#!/bin/bash
# Integration test for the polkit mask rollback path (TSI-2615).
#
# Requires passwordless root (`sudo -n`) and a live systemd.
#
# IT1-IT3 and IT5 exercise the destructive mask/unmask path against a scratch
# directory (via a retargeted `verity_maskdirs`), so they never create or remove
# real /run/systemd/system or /etc/systemd/system unit entries. IT4 alone still
# needs a real polkitd teardown/restart: it hides the D-Bus activation file,
# stops polkit, kills polkitd, and asserts the helper brings it back — that
# observable is a live daemon and cannot be decoupled without faking the very
# state the helper must restore. A suite-entry snapshot of
# /etc/polkit-1/rules.d and the mask dirs is compared at exit; if a third party
# changed them mid-run the suite reports BLOCKED instead of PASS/FAIL.
# All assertions are about observable state, not source text.
set -u
# Resolve the helper by the script's real location, not the caller's cwd.
# `basename %/*` yields "" for a bare-name invocation ("bash
# test_polkit_mask_rollback.sh"), which used to leave the helper unsourced and
# every oracle call hitting "command not found".
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HELPER="$HERE/verity_polkit.sh"
source "$HELPER"
PKACT=/usr/share/dbus-1/system-services/org.freedesktop.PolicyKit1.service
# `systemctl mask --runtime` accepts several names and creates one symlink each.
UNITS="polkit.service polkitd.service org.freedesktop.PolicyKit1.service"

PASS=0; FAIL=0
ok()  { echo "  ok: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
info() { echo "  info: $1"; }

# Retarget `verity_maskdirs` before any test runs: with MASKDIR_PATCH set every
# mask helper addresses the scratch dir; without it, the original real dirs.
# The restore helper still calls `systemctl unmask`/`daemon-reload` on the real
# units — that is the existing IT5-accepted behavior, not mask/stop/pkill.
eval 'verity_maskdirs_orig() { '"$(declare -f verity_maskdirs | tail -n +2)"' }'
mk_maskdirs_patch() {
  eval 'verity_maskdirs() { if [ -n "${MASKDIR_PATCH:-}" ]; then printf "%s\\n" "$MASKDIR_PATCH"; else verity_maskdirs_orig; fi; }'
}
mk_maskdirs_patch

# Count only real mask links: a symlink resolving to /dev/null. A plain
# `ls | wc -l` over a mask dir counts unrelated entries and is a lie.
nmasklinks() {
  local d u n=0
  for d in $(verity_maskdirs); do
    for u in $UNITS; do
      [ -L "$d/$u" ] && [ "$(readlink "$d/$u" 2>/dev/null)" = "/dev/null" ] && n=$((n+1))
    done
  done
  printf '%s' "$n"
}
# Scratch-only counterpart of `systemctl mask --runtime $UNITS`: creates the
# same three /dev/null mask symlinks, but inside $MASKDIR_PATCH.
mask3() {
  local u
  for u in $UNITS; do
    ln -s /dev/null "$MASKDIR_PATCH/$u"
  done
}

# Suite-entry snapshot of the real shared-host polkit state this suite must
# leave untouched, so a third-party change mid-run is detected rather than
# reported as our own PASS/FAIL.
snapshot_polkit_state() {
  sudo -n find /etc/polkit-1/rules.d -maxdepth 1 \( -type f -o -type l \) \
      -printf '%f|%l\n' 2>/dev/null | sort
  local d u p
  for d in /run/systemd/system /etc/systemd/system; do
    for u in $UNITS; do
      p="$d/$u"
      if [ -L "$p" ]; then
        printf '%s|L|%s\n' "$p" "$(readlink "$p" 2>/dev/null)"
      elif [ -e "$p" ]; then
        printf '%s|F\n' "$p"
      else
        printf '%s|A\n' "$p"
      fi
    done
  done
}

SNAP_BEFORE=$(mktemp)
SNAP_AFTER=$(mktemp)
snapshot_polkit_state > "$SNAP_BEFORE"

cleanup() {
  # The final restore must always address the real units, not a scratch dir
  # left behind by a mid-run crash.
  MASKDIR_PATCH=""
  echo
  echo "### cleanup: restore polkit regardless of how the test exited"
  verity_restore_polkit >/dev/null 2>&1 \
    || echo "  WARNING: final restore failed, polkit may be left masked" >&2
  snapshot_polkit_state > "$SNAP_AFTER"
  if cmp -s "$SNAP_BEFORE" "$SNAP_AFTER"; then
    rm -f -- "$SNAP_BEFORE" "$SNAP_AFTER"
  else
    echo "  BLOCKED: shared-host polkit state (rules.d / mask dirs) changed during"
    echo "           the run; a sibling QA task may be active. Diff (before -> after):"
    diff "$SNAP_BEFORE" "$SNAP_AFTER" | sed 's/^/    /'
    rm -f -- "$SNAP_BEFORE" "$SNAP_AFTER"
    echo "RESULT: BLOCKED"
    exit 3
  fi
}
trap cleanup EXIT

echo "### IT1: harness clean-slate restore when nothing is masked"
TMP=$(mktemp -d)
MASKDIR_PATCH="$TMP"
verity_restore_polkit >/dev/null 2>&1 && ok "idempotent restore, rc=0" || bad "restore rc!=0"
verity_path_is_mask "$TMP/polkit.service" && bad "polkit.service reports masked" || ok "polkit.service unmasked"
rm -rf -- "$TMP"; MASKDIR_PATCH=""

echo "### IT2: harness masks 3 names; rollback must clear all 3"
TMP=$(mktemp -d)
MASKDIR_PATCH="$TMP"
mask3
info "mask links present after mask: $(nmasklinks)"
# One assertion per unit, on the path the scratch mask actually writes.
for u in $UNITS; do
  [ -L "$TMP/$u" ] \
    && ok "mask symlink present: $TMP/$u" \
    || bad "expected mask symlink for $u under scratch dir"
done
out=$(verity_restore_polkit) && ok "restore rc=0" || bad "restore rc!=0"
case "$out" in
  *"removed 3 mask symlink(s)"*) ok "report counts 3 mask symlinks" ;;
  *) bad "report missing 'removed 3 mask symlink(s)': ${out:-<empty>}" ;;
esac
for u in $UNITS; do
  verity_path_is_mask "$TMP/$u" && bad "$u still masked" || ok "$u unmasked"
done
rm -rf -- "$TMP"; MASKDIR_PATCH=""

echo "### IT3: the TSI-2615 trap — unmask only the first name, then restore"
TMP=$(mktemp -d)
MASKDIR_PATCH="$TMP"
mask3
# Simulate the buggy unmask that removes only the first name: drop just
# polkit.service, leaving polkitd.service and org.freedesktop.PolicyKit1.service
# masked — the exact trap state `systemctl unmask polkit.service` leaves on
# affected hosts. The regression the helper guards is restoring the rest.
rm -f -- "$TMP/polkit.service"
info "first name unmasked, remaining mask links: $(nmasklinks)"
# The assertion that matters is that the helper restores polkit either way.
verity_restore_polkit >/dev/null 2>&1 && ok "restore cleared the trap state" || bad "restore failed on trap state"
for u in $UNITS; do
  verity_path_is_mask "$TMP/$u" && bad "$u masked after restore" || ok "$u unmasked after restore"
done
rm -rf -- "$TMP"; MASKDIR_PATCH=""

echo "### IT4: unreachable-mode teardown then restore (the s20/s21 harness path)"
# Cannot decouple: this IT must observe a real polkitd teardown and the helper's
# live restart. It still hides the activation file, stops polkit, and kills
# polkitd on this host; only the mask creation is scratch (IT2/IT3 already prove
# the mask-clear branch, so no real mask is needed here).
TMP=$(mktemp -d)
MASKDIR_PATCH="$TMP"
mask3
sudo mv -f "$PKACT" "$PKACT.verity-hidden" 2>/dev/null
sudo systemctl stop polkit.service >/dev/null 2>&1
sudo pkill -x polkitd 2>/dev/null
sleep 2
pgrep -x polkitd >/dev/null && bad "polkitd still alive after teardown" || ok "polkitd gone (unreachable)"
verity_restore_polkit >/dev/null 2>&1 && ok "restore rc=0" || bad "restore rc!=0"
sleep 1
pgrep -x polkitd >/dev/null && ok "polkitd back up" || bad "polkitd did not restart"
[ -f "$PKACT" ] && ok "D-Bus activation file restored" || bad "activation file missing"
verity_is_masked polkitd.service && bad "polkitd.service masked" || ok "polkitd.service clean"
rm -rf -- "$TMP"; MASKDIR_PATCH=""

echo "### IT5: restore must not delete non-mask paths (review: data loss)"
# The mask dirs are a function, so a test can retarget them to a scratch
# directory: the destructive path is then exercised without touching any real
# unit file. Without this IT5 does not test the actual code.

TMP=$(mktemp -d)
for u in $UNITS; do
  printf '# admin-written local override, not a mask\n' > "$TMP/$u"
done
MASKDIR_PATCH="$TMP"
verity_restore_polkit >/dev/null 2>&1
for u in $UNITS; do
  [ -f "$TMP/$u" ] && ok "admin override preserved: $u" \
    || bad "DATA LOSS: restore deleted admin override $u"
done
rm -rf -- "$TMP"; MASKDIR_PATCH=""

TMP=$(mktemp -d)
printf 'redirect target\n' > "$TMP/redirect.target"
ln -s "$TMP/redirect.target" "$TMP/polkit.service"
MASKDIR_PATCH="$TMP"
verity_restore_polkit >/dev/null 2>&1
[ -L "$TMP/polkit.service" ] && ok "non-mask symlink preserved (redirect, not /dev/null)" \
  || bad "DATA LOSS: restore deleted a symlink that does not resolve to /dev/null"
rm -rf -- "$TMP"; MASKDIR_PATCH=""

TMP=$(mktemp -d)
ln -s /dev/null "$TMP/polkit.service"
MASKDIR_PATCH="$TMP"
verity_restore_polkit >/dev/null 2>&1
[ -L "$TMP/polkit.service" ] && bad "regression: restore did not remove a /dev/null mask" \
  || ok "real mask still removed (/dev/null symlink)"
rm -rf -- "$TMP"; MASKDIR_PATCH=""

echo "### IT6: oracle still usable after restore (harness gate integrity)"
# The oracle must parse whatever polkit answers -- any of: authorized, denied, or
# the action absent. pkcheck exit codes are 0 authorized, 1 Not authorized,
# 2 authentication required, 127 action not registered, 126 usage error.
# Classification must follow the subject's own polkit session state:
#   allow_active=auth_admin_keep  -> yes on an active session
#   allow_inactive=no             -> Not authorized on an inactive session
# Both are correct answers for this action. If the case fails to distinguish
# "authorized" from "not parseable", the check flips between hosts and the suite
# breaks on hosts where the daemon denies. The assertion is that pkcheck runs
# and returns a parseable answer after the restore, not that it grants access.
if ! command -v pkcheck >/dev/null 2>&1; then
  info "pkcheck not installed on this host; oracle check skipped"
else
  RAW=$(pkcheck -a com.agentshell.mount -p $$ 2>&1 | head -1)
  case "$RAW" in
    *'polkit\56result='*) ok "pkcheck answered: $RAW" ;;
    *"Not authorized."*)  ok "pkcheck answered, denied on this host: $RAW" ;;
    *"No such action"*|*"not registered"*) ok "action not installed on this host, pkcheck still responds: $RAW" ;;
    *) bad "pkcheck unparseable: $RAW" ;;
  esac
fi

echo
echo "RESULT: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]

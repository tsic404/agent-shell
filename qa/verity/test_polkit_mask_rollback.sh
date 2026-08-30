#!/bin/bash
# Integration test for the polkit mask rollback path (TSI-2615).
#
# Requires passwordless root (`sudo -n`) and a live systemd. Every step masks
# and then un-masks the real polkit units, so the host is left as it was.
# All assertions are about observable host state, not source text.
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
mask3() { sudo systemctl mask --runtime $UNITS >/dev/null 2>&1; }
cleanup() {
  echo
  echo "### cleanup: restore polkit regardless of how the test exited"
  verity_restore_polkit >/dev/null 2>&1 \
    || echo "  WARNING: final restore failed, polkit may be left masked" >&2
}
trap cleanup EXIT

echo "### IT1: harness clean-slate restore when nothing is masked"
verity_restore_polkit >/dev/null 2>&1 && ok "idempotent restore, rc=0" || bad "restore rc!=0"
verity_is_masked polkit.service && bad "polkit.service reports masked" || ok "polkit.service unmasked"

echo "### IT2: harness masks 3 names; rollback must clear all 3"
mask3
info "mask links present after mask: $(nmasklinks)"
# One assertion per unit, on the path `--runtime` actually writes.
for u in $UNITS; do
  [ -L "/run/systemd/system/$u" ] \
    && ok "mask symlink present: /run/systemd/system/$u" \
    || bad "expected mask symlink for $u under /run/systemd/system"
done
verity_restore_polkit >/dev/null 2>&1 && ok "restore rc=0" || bad "restore rc!=0"
for u in $UNITS; do
  verity_is_masked "$u" && bad "$u still masked" || ok "$u unmasked"
done

echo "### IT3: the TSI-2615 trap — unmask only the first name, then restore"
mask3
sudo systemctl unmask polkit.service >/dev/null 2>&1
if verity_is_masked polkit.service; then
  info "TSI-2615 trap reproduced: unmask returned rc=0 but polkit.service is still masked"
else
  info "unmask worked on this host; the TSI-2615 regression is absent here"
fi
# Reporting whether the bug exists is informational: the assertion that matters
# is that the helper restores polkit either way. Failing a healthy host whose
# unmask works inverts the goal of this test.
verity_restore_polkit >/dev/null 2>&1 && ok "restore cleared the trap state" || bad "restore failed on trap state"
for u in $UNITS; do
  verity_is_masked "$u" && bad "$u masked after restore" || ok "$u unmasked after restore"
done

echo "### IT4: unreachable-mode teardown then restore (the s20/s21 harness path)"
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

echo "### IT5: restore must not delete non-mask paths (review: data loss)"
# The mask dirs are a function, so a test can retarget them to a scratch
# directory: the destructive path is then exercised without touching any real
# unit file. Without this IT5 does not test the actual code.
eval 'verity_maskdirs_orig() { '"$(declare -f verity_maskdirs | tail -n +2)"' }'
mk_maskdirs_patch() {
  eval 'verity_maskdirs() { if [ -n "${MASKDIR_PATCH:-}" ]; then printf "%s\\n" "$MASKDIR_PATCH"; else verity_maskdirs_orig; fi; }'
}
mk_maskdirs_patch

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

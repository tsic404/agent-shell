#!/usr/bin/env bash
# verity_polkit.sh — hardened mask/unmask rollback helper for Verity QA harnesses.
#
# Why this exists (TSI-2615, reproduced on systemd 261, 2026-08-30):
#
#   $ sudo systemctl mask --runtime polkit.service   # rc=0, symlink created
#   $ sudo systemctl unmask polkit.service           # rc=0, NO error
#   $ ls -l /run/systemd/system/polkit.service
#   lrwxrwxrwx 1 root root 9 ... /run/systemd/system/polkit.service -> /dev/null
#   $ sudo systemctl start polkit.service
#   Failed to start polkit.service: Unit polkit.service is masked.   # rc=1
#
# `unmask` reports success without removing the /run/systemd/system symlink, so
# the mask survives and polkitd can never be restarted. Root cause and severity
# are host/packaging dependent — some installs unmask correctly — so a harness
# must VERIFY the rollback instead of trusting the exit code.
#
# Second trap: `systemctl mask --runtime` takes one or more names, so masking
# several names in one call creates several symlinks, and one later `unmask`
# removes only the first. The unit names a harness may mask are enumerated
# below and every one of them is removed.
#
# Third trap (why the restore is narrower than "delete this path"): a mask is
# specifically a symlink whose target is /dev/null. A *regular* unit file named
# polkit.service under /etc/systemd/system is an admin-written local override,
# not a mask — and a symlink that resolves elsewhere is a redirect. `rm -f` on
# either one destroys data this harness never created. The rollback therefore
# only removes symlinks that readlink resolves to /dev/null; everything else is
# left exactly as found and re-checked.
#
# Provides:
#   verity_polkit_unit_names ()   -> prints the unit names this harness may mask
#   verity_maskdirs ()            -> the dirs masks may live in
#   verity_is_masked <unit>       -> 0 if the unit is still masked (mask symlink or state)
#   verity_is_unmasked <unit>     -> 0 only if no mask symlink remains AND state is clean
#   verity_restore_polkit [unit...] -> drop /dev/null mask symlinks, call unmask,
#                                      daemon-reload, start polkitd if dead; 0/1
#
# Idempotent and safe to call when nothing is masked (the TSI-2590 harness calls it
# as a clean slate before every scenario). Requires passwordless root (`sudo -n`).
# Nothing here touches product source (rootd / daemon / CLI); it only manages
# polkit systemd unit state and the D-Bus activation file it hides.

# Unit names a harness may have masked. Every name `systemctl mask --runtime`
# creates a symlink for, even when the unit file does not exist — `systemctl`
# only prints "Unit <x> does not exist, proceeding anyway." and still creates it.
verity_polkit_unit_names() {
  printf '%s\n' polkit.service polkitd.service org.freedesktop.PolicyKit1.service
}

# Directories systemd honours for mask symlinks, most-specific first.
verity_maskdirs() {
  printf '%s\n' /run/systemd/system /etc/systemd/system
}

# Is this path a mask? Only a symlink resolving to /dev/null qualifies — that
# is what `systemctl mask` writes. Anything else at the same name is someone's
# configuration and must not be deleted by this helper.
verity_path_is_mask() {
  local p="$1"
  [ -L "$p" ] && [ "$(readlink "$p" 2>/dev/null)" = "/dev/null" ]
}

# A mask is a /dev/null symlink in a mask dir. `systemctl show` is the second
# channel: it catches a mask this helper cannot see (another search root, or a
# runtime mask systemd still holds in memory).
verity_is_masked() {
  local unit="$1" d link
  for d in $(verity_maskdirs); do
    link="$d/$unit"
    verity_path_is_mask "$link" && return 0
  done
  case "$(sudo -n systemctl show "$unit" --property=UnitFileState 2>/dev/null)" in
    masked*) return 0 ;;
  esac
  return 1
}

verity_is_unmasked() {
  verity_is_masked "$1" || return 0
  return 1
}

# Count mask symlinks present for the given units. Used by the restore to prove
# `systemctl unmask` removed something on hosts that honour it.
verity_count_masks() {
  local units=("$@") d unit n=0
  for unit in "${units[@]}"; do
    for d in $(verity_maskdirs); do
      verity_path_is_mask "$d/$unit" && n=$((n+1))
    done
  done
  printf '%s' "$n"
}

verity_restore_polkit() {
  # Usage: verity_restore_polkit [unit.service ...]
  # Omit arguments to restore every name listed by verity_polkit_unit_names.
  local units=("$@") d unit removed=0 still=0
  if [ "${#units[@]}" -eq 0 ]; then
    units=($(verity_polkit_unit_names))
  fi

  # 1. Remove the mask symlinks directly. `systemctl unmask` alone is NOT
  #    sufficient — see the report above. Only symlinks resolving to /dev/null
  #    are removed; a regular file is an admin override, a symlink elsewhere a
  #    redirect. Neither may be destroyed here.
  for unit in "${units[@]}"; do
    for d in $(verity_maskdirs); do
      if verity_path_is_mask "$d/$unit"; then
        sudo -n rm -f -- "$d/$unit"
        removed=$((removed + 1))
      fi
    done
  done

  # 2. Belt and braces: also call unmask, so a future host that does honour it
  #    still gets it. Never rely on this step alone. Count the change rather
  #    than parsing the exit code, which this issue proved to be a lie.
  local before after
  before=$(verity_count_masks "${units[@]}")
  for unit in "${units[@]}"; do
    sudo -n systemctl unmask "$unit" >/dev/null 2>&1
  done
  after=$(verity_count_masks "${units[@]}")
  if [ "$after" -lt "$before" ]; then
    removed=$((removed + before - after))
  fi

  # 3. /run/systemd/system is a runtime dir; systemd keeps the mask in memory
  #    until it reloads, so a start can still fail right after the rm.
  sudo -n systemctl daemon-reload >/dev/null 2>&1

  # 4. Verify. This is the assertion that would have caught TSI-2615.
  for unit in "${units[@]}"; do
    verity_is_masked "$unit" || continue
    echo "verity_restore_polkit: $unit STILL MASKED after rollback" >&2
    for d in $(verity_maskdirs); do
      ls -l "$d/$unit" 2>&1 | sed 's/^/  /' >&2
    done
    still=1
  done

  # 5. Re-enable D-Bus activation and make sure polkitd is actually up.
  #    A masked-but-running polkitd keeps the bus name registered, so "polkitd
  #    unreachable" scenarios need both the unit and the activation file gone.
  local pkact=/usr/share/dbus-1/system-services/org.freedesktop.PolicyKit1.service
  [ -L "$pkact" ] || [ -e "$pkact" ] || sudo -n mv -f -- "$pkact.verity-hidden" "$pkact" 2>/dev/null

  if ! pgrep -x polkitd >/dev/null 2>&1; then
    sudo -n systemctl start polkit.service >/dev/null 2>&1
    local i
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
      pgrep -x polkitd >/dev/null 2>&1 && break
      sleep 1
    done
    if ! pgrep -x polkitd >/dev/null 2>&1; then
      echo "verity_restore_polkit: polkitd did not come back up" >&2
      sudo -n systemctl start polkit.service 2>&1 | sed 's/^/  start: /' >&2
      return 1
    fi
  fi

  if [ "$still" -eq 1 ]; then
    return 1
  fi

  if [ "$removed" -gt 0 ]; then
    echo "verity_restore_polkit: removed $removed mask symlink(s); state clean, polkitd up"
  else
    echo "verity_restore_polkit: no mask symlinks present; state already clean"
  fi
  return 0
}

# Standalone use: `bash verity_polkit.sh` restores polkit and reports.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  verity_restore_polkit
fi

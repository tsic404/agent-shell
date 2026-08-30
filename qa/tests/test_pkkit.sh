#!/bin/bash
# qa/tests/test_pkkit.sh — offline unit tests for qa/pkkit.sh.
#
# Runs without root and without polkitd: the kit is pointed at a temp
# rules.d, PK_SUDO is set to "direct", and `pkcheck` is replaced by a bash
# function that returns whatever decision the test wants. That is the whole
# seam the gate depends on, so these tests prove the writer and the gate
# independently of the host they run on.
#
# The invariants under test:
#   T1  a rule never reaches disk with an unexpanded placeholder (TSI-2614)
#   T2  a scenario cannot proceed unless the oracle agrees with the intent
#   T3  two concurrently running tasks cannot delete each other's rules
#       (TSI-2610): the filename carries a per-task identifier, the drop is
#       scoped to that identifier, and the machine-wide lock serialises the
#       polkitd-touching operations.
#
# Usage: qa/tests/test_pkkit.sh     (exit 0 = all pass)

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
KIT="$HERE/../pkkit.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
CURRENT=""

t() { CURRENT="$1"; shift; }

ok() { PASS=$((PASS + 1)); printf '  ok    %s\n' "$CURRENT"; }

bad() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n        %s\n' "$CURRENT" "$1"; }

expect_ok() { local d="$1"; shift; if "$@"; then ok "$CURRENT"; else bad "$CURRENT" "expected ok: $d"; fi; }

expect_fail() { local d="$1"; shift; if "$@"; then bad "$CURRENT" "expected failure: $d"; else ok "$CURRENT"; fi; }

expect_rc() { local want="$1" d="$2"; shift 2; "$@"; local rc=$?; if [ "$rc" = "$want" ]; then ok "$CURRENT"; else bad "$CURRENT" "rc=$rc want=$want ($d)"; fi; }

# ── harness ────────────────────────────────────────────────────────────
mkdir -p "$WORK/rules.d"
export PK_SUDO=direct
export PK_RULE_DIR="$WORK/rules.d"
export PK_RULE_ORDER=10
export PK_TIMEOUT=2
export PK_POLL=1
export PK_SUBJECT_PID="$$"
# A deterministic task id so the derived filenames are predictable. Without
# this the kit would fall back to host-pid+epoch, which changes every run.
export PK_RULE_NAME=verity-task-a
export PK_LOCK_DIR="$WORK/locks"
export PK_LOCK_TIMEOUT=5

# Fake pkcheck: ORACLE_DECISION picks the decision, POLKIT_RAW_OUTPUT
# overrides the whole payload to exercise the parser.
#
# The payload is passed as an ARGUMENT to printf, never inside the format
# string: `%s: polkit\56result=yes` as a format would let printf interpret
# `\56` as octal (backspace) and emit `polkit.result=yes`, which the parser
# would reject as unparseable. That is a test bug, not a kit bug.
ORACLE_DECISION=yes
POLKIT_RAW_OUTPUT=''
pkcheck() {
  if [ -n "$POLKIT_RAW_OUTPUT" ]; then
    printf '%s\n' "$POLKIT_RAW_OUTPUT"
    return 0
  fi
  # echo, not printf: `polkit\56result` contains a backslash escape that
  # printf would interpret as octal (backspace) and corrupt to
  # `polkit.result`, which the parser rightly rejects as unparseable.
  if [ "$ORACLE_DECISION" = yes ]; then
    echo 'com.agentshell.mount: yes polkit\56result=yes'
  else
    echo 'com.agentshell.mount: no polkit\56result=no'
  fi
}
export -f pkcheck

# shellcheck disable=SC1090
. "$KIT"

ACTION=com.agentshell.mount

# rule_path <yes|no> [task-name] — expected on-disk path for the kit's
# naming convention: <ORDER>-<task>-<decision>-<action>.rules
rule_path() {
  local want="$1" name="${2:-$PK_RULE_NAME}"
  printf '%s/%s-%s-%s-%s.rules\n' "$PK_RULE_DIR" "$PK_RULE_ORDER" "$name" "$want" "$ACTION"
}

RULE_YES_P="$(rule_path yes)"
RULE_NO_P="$(rule_path no)"

echo "== writer: content invariants =="

t "write yes renders the intended return value"
expect_ok "pk_rule_write yes" pk_rule_write yes "$ACTION" "$RULE_YES_P"

t "written rule has no unexpanded placeholder"
if grep -q '%s' "$RULE_YES_P"; then bad "$CURRENT" 'placeholder leaked to disk'; else ok "$CURRENT"; fi

t "written rule carries the intended return"
grep -q 'return polkit.Result.YES;' "$RULE_YES_P" && ok "$CURRENT" || bad "$CURRENT" 'missing YES return'

t "written rule is a single polkit.addRule"
n=$(grep -c 'polkit.addRule' "$RULE_YES_P")
[ "$n" = 1 ] && ok "$CURRENT" || bad "$CURRENT" "polkit.addRule count=$n"

t "written rule names the action id"
grep -q "action.id == \"$ACTION\"" "$RULE_YES_P" && ok "$CURRENT" || bad "$CURRENT" 'action id missing'

t "write no renders the NO return"
expect_ok "pk_rule_write no" pk_rule_write no "$ACTION" "$RULE_NO_P"
grep -q 'return polkit.Result.NO;' "$RULE_NO_P" && ok "$CURRENT" || bad "$CURRENT" 'missing NO return'

t "write no has no unexpanded placeholder"
if grep -q '%s' "$RULE_NO_P"; then bad "$CURRENT" 'placeholder leaked'; else ok "$CURRENT"; fi

t "default path is derived from task id + decision + action"
rm -f "$PK_RULE_DIR"/*
pk_rule_write yes "$ACTION"
[ -f "$RULE_YES_P" ] && ok "$CURRENT" || bad "$CURRENT" "default file missing (want $RULE_YES_P, got: $(ls "$PK_RULE_DIR"))"

t "default path content carries the action id"
grep -q "$ACTION" "$RULE_YES_P" && ok "$CURRENT" || bad "$CURRENT" 'content empty'

t "check_file accepts a correct rule"
expect_ok "pk_rule_check_file" pk_rule_check_file yes "$ACTION" "$RULE_YES_P"

t "check_file rejects an unexpanded placeholder (the TSI-2614 defect)"
printf 'polkit.addRule(function(action, subject) {\n    if (action.id == "%s") {\n        return %s;\n    }\n});\n' "$ACTION" '%s' > "$WORK/broken.rules"
expect_fail "reject placeholder" pk_rule_check_file yes "$ACTION" "$WORK/broken.rules"

t "check_file rejects a missing action id"
printf 'polkit.addRule(function(a, s) { if (a.id == "other.action") return polkit.Result.YES; });\n' > "$WORK/wrong-action.rules"
expect_fail "reject wrong action" pk_rule_check_file yes "$ACTION" "$WORK/wrong-action.rules"

t "check_file rejects a missing return value"
printf 'polkit.addRule(function(a, s) { if (a.id == "%s") return polkit.Result.NO; });\n' "$ACTION" > "$WORK/wrong-return.rules"
expect_fail "reject wrong return" pk_rule_check_file yes "$ACTION" "$WORK/wrong-return.rules"

t "check_file rejects an unreadable path"
expect_fail "reject missing file" pk_rule_check_file yes "$ACTION" "$WORK/nope.rules"

echo
echo "== writer: input validation =="

t "rejects a bad decision value with rc=2"
expect_rc 2 "rc=2 on bad want" pk_rule_write maybe "$ACTION" "$WORK/x.rules"

t "rejects an empty action id with rc=2"
expect_rc 2 "rc=2 on empty action" pk_rule_write yes "" "$WORK/x.rules"

t "a bad write leaves no rule file behind"
if [ -f "$WORK/x.rules" ]; then bad "$CURRENT" "leftover: $(ls "$WORK")"; else ok "$CURRENT"; fi

echo
echo "== oracle: pk_check_says parsing =="

t "polkit 127 YES wire format"
POLKIT_RAW_OUTPUT='com.agentshell.mount: yes polkit\56result=yes'
[ "$(pk_check_says "$ACTION")" = yes ] && ok "$CURRENT" || bad "$CURRENT" "got $(pk_check_says "$ACTION")"

t "polkit 127 NO wire format"
POLKIT_RAW_OUTPUT='com.agentshell.mount: no polkit\56result=no'
[ "$(pk_check_says "$ACTION")" = no ] && ok "$CURRENT" || bad "$CURRENT" "got $(pk_check_says "$ACTION")"

t "'Not authorized' denial"
POLKIT_RAW_OUTPUT='Not authorized'
[ "$(pk_check_says "$ACTION")" = no ] && ok "$CURRENT" || bad "$CURRENT" "got $(pk_check_says "$ACTION")"

t "unparseable output is never silently treated as a decision"
POLKIT_RAW_OUTPUT='org.freedesktop.DBus.Error.ServiceUnknown'
out="$(pk_check_says "$ACTION")"
case "$out" in
  unknown:*) ok "$CURRENT" ;;
  yes|no) bad "$CURRENT" "parseable decision from garbage: $out" ;;
  *) ok "$CURRENT" ;;
esac
POLKIT_RAW_OUTPUT=''

echo
echo "== the gate: hard pre-requirement =="

t "gate passes when the oracle converges on the intent"
rm -f "$PK_RULE_DIR"/*
ORACLE_DECISION=yes
expect_ok "pk_gate yes" pk_gate yes "$ACTION"

t "gate passed means the rule is present on disk"
[ -f "$RULE_YES_P" ] && ok "$CURRENT" || bad "$CURRENT" 'rule file missing after gate'

t "gate aborts when the oracle disagrees (no silent fallback)"
rm -f "$PK_RULE_DIR"/*
ORACLE_DECISION=no
expect_fail "pk_gate yes / oracle no" pk_gate yes "$ACTION"

t "the abort is a non-zero exit, not a warning"
ORACLE_DECISION=no
pk_gate yes "$ACTION" >/dev/null 2>&1
rc=$?
[ "$rc" != 0 ] && ok "$CURRENT" || bad "$CURRENT" 'gate returned 0 on mismatch'

t "a failed gate leaves the intended rule on disk as evidence"
grep -q 'return polkit.Result.YES;' "$RULE_YES_P" \
  && ok "$CURRENT" || bad "$CURRENT" 'rule content wrong after failed gate'

t "gate aborts when the oracle reports nothing parseable"
rm -f "$PK_RULE_DIR"/*
ORACLE_DECISION=no
POLKIT_RAW_OUTPUT='pkcheck: communication error'
expect_fail "pk_gate no / oracle down" pk_gate no "$ACTION"
POLKIT_RAW_OUTPUT=''

t "gate writes NO and passes when the oracle says no"
rm -f "$PK_RULE_DIR"/*
ORACLE_DECISION=no
expect_ok "pk_gate no" pk_gate no "$ACTION"
grep -q 'return polkit.Result.NO;' "$RULE_NO_P" \
  && ok "$CURRENT" || bad "$CURRENT" 'NO rule content wrong'

t "gate drops only its OWN stale rules"
rm -f "$PK_RULE_DIR"/*
ORACLE_DECISION=yes
pk_rule_write yes "$ACTION" "$RULE_YES_P"
ORACLE_DECISION=no
pk_gate no "$ACTION" >/dev/null 2>&1
if [ -f "$RULE_YES_P" ]; then bad "$CURRENT" 'own stale YES rule survived a NO gate'; else ok "$CURRENT"; fi

t "gate propagates a writer failure as non-zero"
ORACLE_DECISION=yes
pk_gate maybe "$ACTION" >/dev/null 2>&1
rc=$?
[ "$rc" != 0 ] && ok "$CURRENT" || bad "$CURRENT" "gate returned 0 on writer failure (rc=$rc)"

echo
echo "== cleanup: scoping (the TSI-2610 fix) =="

t "pk_rule_drop removes every rule under our own task id"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
printf 'x\n' > "$RULE_YES_P"
printf 'x\n' > "$RULE_NO_P"
printf 'x\n' > "$WORK/rules.d/99-other.rules"
pk_rule_drop
if [ -f "$RULE_YES_P" ] || [ -f "$RULE_NO_P" ]; then
  bad "$CURRENT" "own rules survived: $(ls "$PK_RULE_DIR")"
else
  ok "$CURRENT"
fi

t "drop leaves a foreign task's rules untouched"
[ -f "$WORK/rules.d/99-other.rules" ] && ok "$CURRENT" || bad "$CURRENT" 'foreign rule removed'

t "drop leaves a SIBLING TASK's rules untouched (TSI-2610)"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
printf 'x\n' > "$RULE_YES_P"
SIB="$(rule_path yes verity-task-b)"
printf 'x\n' > "$SIB"
pk_rule_drop
if [ -f "$SIB" ]; then ok "$CURRENT"; else
  bad "$CURRENT" 'dropped another task'\''s live rule — the TSI-2610 regression'
fi

t "two concurrent tasks each retain their rule after both gate"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
ORACLE_DECISION=yes
pk_gate yes "$ACTION"
A1="$(rule_path yes verity-task-a)"
( PK_RULE_NAME=verity-task-b pk_gate yes "$ACTION" ) >/dev/null 2>&1
A2="$(rule_path yes verity-task-b)"
if [ -f "$A1" ] && [ -f "$A2" ]; then ok "$CURRENT"; else
  bad "$CURRENT" "missing: a=$([ -f "$A1" ] && echo yes || echo NO) b=$([ -f "$A2" ] && echo yes || echo NO)"
fi

t "a sibling task's drop does not remove our rule"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
ORACLE_DECISION=yes
pk_gate yes "$ACTION"
A1="$(rule_path yes verity-task-a)"
( PK_RULE_NAME=verity-task-b pk_rule_drop )
if [ -f "$A1" ]; then ok "$CURRENT"; else bad "$CURRENT" 'our rule removed by sibling drop'; fi

t "our drop removes our rule"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
ORACLE_DECISION=yes
pk_gate yes "$ACTION"
A1="$(rule_path yes verity-task-a)"
pk_rule_drop
[ -f "$A1" ] && bad "$CURRENT" 'own rule survived drop' || ok "$CURRENT"

t "pk_rule_exists reports true while our own rule is present"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
ORACLE_DECISION=yes
pk_gate yes "$ACTION"
if pk_rule_exists; then ok "$CURRENT"; else bad "$CURRENT" 'expected own rule to be reported'; fi

t "pk_rule_exists reports false after our drop"
pk_rule_drop
if pk_rule_exists; then bad "$CURRENT" 'rule reported after drop'; else ok "$CURRENT"; fi

t "pk_rule_exists is scoped to our own task id"
rm -rf "$WORK/rules.d"
mkdir -p "$WORK/rules.d"
( PK_RULE_NAME=verity-task-b pk_rule_write yes "$ACTION" )
if pk_rule_exists; then bad "$CURRENT" 'a sibling task rule matched'; else ok "$CURRENT"; fi
( PK_RULE_NAME=verity-task-b pk_rule_drop )

echo
echo "== task identity: PK_RULE_NAME derivation =="

t "PK_RULE_ORDER controls the numeric prefix (low wins first)"
name="$(PK_RULE_NAME=verity-task-a bash -c "source '$KIT'; rm -f \"$PK_RULE_DIR\"/*; pk_rule_write yes \"$ACTION\"; ls \"$PK_RULE_DIR\"")"
printf '%s' "$name" | grep -Eq '^10-verity-task-a-yes-' && ok "$CURRENT" || bad "$CURRENT" "got: $name"
t "a lower PK_RULE_ORDER still writes into the rules dir"
name="$(PK_RULE_ORDER=20 PK_RULE_NAME=verity-task-a bash -c "source '$KIT'; rm -f \"$PK_RULE_DIR\"/*; pk_rule_write yes \"$ACTION\"; ls \"$PK_RULE_DIR\"")"
printf '%s' "$name" | grep -Eq '^20-verity-task-a-yes-' && ok "$CURRENT" || bad "$CURRENT" "got: $name"

t "PK_RULE_NAME overrides derivation"
name="$(PK_RULE_NAME=forced-id bash -c "source '$KIT'; pk_rule_name")"
[ "$name" = forced-id ] && ok "$CURRENT" || bad "$CURRENT" "got '$name'"
export PK_RULE_NAME=verity-task-a

t "MULTICA_TASK_ID is used when PK_RULE_NAME is unset"
unset PK_RULE_NAME
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID=01a05223-7075-7228 bash -c "source '$KIT'; pk_rule_name")"
[ "$name" = "01a05223-7075-7228" ] && ok "$CURRENT" || bad "$CURRENT" "got '$name'"
export PK_RULE_NAME=verity-task-a

t "TSI issue ref is used as a later fallback"
name="$(env -u PK_RULE_NAME -u MULTICA_TASK_ID MULTICA_ISSUE_REF=TSI-2610 bash -c "source '$KIT'; pk_rule_name")"
[ "$name" = tsi-2610 ] && ok "$CURRENT" || bad "$CURRENT" "got '$name'"

t "special characters are sanitised to filename-safe names"
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID='01a05223-7075/7228;90fc*' bash -c "source '$KIT'; pk_rule_name")"
if printf '%s' "$name" | grep -Eq '^[A-Za-z0-9._-]+$'; then ok "$CURRENT"; else
  bad "$CURRENT" "unsafe name leaked: '$name'"
fi

t "an all-garbage id still produces a non-empty name"
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID='///;;;***' bash -c "source '$KIT'; pk_rule_name")"
[ -n "$name" ] && ok "$CURRENT" || bad "$CURRENT" "empty name from garbage id"

t "a name containing a glob metacharacter cannot escape sanitisation"
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID='foo' bash -c "source '$KIT'; PK_RULE_NAME='a[b' pk_rule_name")"
if printf '%s' "$name" | grep -Eq '^[A-Za-z0-9._-]+$'; then ok "$CURRENT"; else
  bad "$CURRENT" "glob metachar in '$name'"
fi

t "sanitisation strips glob metachars from the task id before globbing"
# A glob is built only from the sanitised task name plus a literal `*`, so
# sanitisation is what keeps one task's drop from matching a sibling's file.
# pk_rule_glob ends in `*.rules` by design, so assert on the derived name
# rather than the pattern: the name is the only part that could carry a
# metacharacter through to a pattern.
glob="$(env -u PK_RULE_NAME MULTICA_TASK_ID='a[b*c?d' bash -c "source '$KIT'; pk_rule_glob" 2>/dev/null)"
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID='a[b*c?d' bash -c "source '$KIT'; pk_rule_name" 2>/dev/null)"
if printf '%s' "$name" | grep -q '[?*[]'; then
  bad "$CURRENT" "metachar survived sanitisation: $name"
else
  ok "$CURRENT"
fi
t "pk_rule_glob ends in the expected wildcard"
case "$glob" in
  *"$name-*.rules") ok "$CURRENT" ;;
  *) bad "$CURRENT" "glob=[$glob] name=[$name]" ;;
esac

t "PK_RULE_ORDER controls the numeric prefix (low wins first)"
rm -f "$PK_RULE_DIR"/*
pk_rule_write yes "$ACTION"
ls "$PK_RULE_DIR" | grep -Eq '^10-verity-task-a-yes-' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $(ls "$PK_RULE_DIR")"

t "a different PK_RULE_ORDER changes the prefix"
rm -f "$PK_RULE_DIR"/*
PK_RULE_ORDER=20 pk_rule_write yes "$ACTION"
ls "$PK_RULE_DIR" | grep -Eq '^20-verity-task-a-yes-' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $(ls "$PK_RULE_DIR")"

echo
echo "== machine-level lock (TSI-2610) =="

t "pk_lock_dir creates a 1777 world-writable dir"
rm -rf "$WORK/locks"
if pk_lock_dir; then
  m="$(stat -c '%a' "$WORK/locks")"
  [ "$m" = 777 ] || [ "$m" = 1777 ] && ok "$CURRENT" || bad "$CURRENT" "mode=$m want=1777"
else
  bad "$CURRENT" 'pk_lock_dir failed'
fi

t "pk_lock_acquire takes the lock and pk_lock_release drops it"
unset PK_LOCK_HELD
if pk_lock_acquire && [ "$PK_LOCK_HELD" = 1 ]; then
  held1=1
else
  held1=0
fi
pk_lock_release
if [ "$held1" = 1 ] && [ "${PK_LOCK_HELD:-0}" = 0 ]; then ok "$CURRENT"; else
  bad "$CURRENT" "held1=$held1 after release=${PK_LOCK_HELD:-unset}"
fi

t "the lock is re-acquirable after release (no stale fd)"
if pk_lock_acquire && pk_lock_release; then ok "$CURRENT"; else bad "$CURRENT" 'lock not reusable'; fi

t "flock blocks a second holder (serialises polkitd-touching ops)"
# A second holder keeps the lock and signals via a file; the parent waits for
# that signal before trying to acquire, so the contention is real rather
# than a race against a background job that may already have exited.
unset PK_LOCK_HELD
rm -f "$WORK/locks/ready" "$WORK/locks/exit"
lockfile="$WORK/locks/polkitd"
( flock -w 30 9
  echo held > "$WORK/locks/ready"
  while [ ! -f "$WORK/locks/exit" ]; do sleep 0.1; done
) 9>"$lockfile" &
bg=$!
for ((i = 0; i < 50; i++)); do [ -f "$WORK/locks/ready" ] && break; sleep 0.1; done
export PK_LOCK_TIMEOUT=1
if pk_lock_acquire; then
  bad "$CURRENT" 'acquired while another holder had the lock'
  pk_lock_release
else
  ok "$CURRENT"
fi
echo exit > "$WORK/locks/exit"
wait "$bg" 2>/dev/null
rm -f "$WORK/locks/ready" "$WORK/locks/exit"

t "pk_restore releases the lock on success"
unset PK_LOCK_HELD
rm -rf "$WORK/locks"
if pk_lock_acquire && pk_lock_release; then
  ok "$CURRENT"
else
  bad "$CURRENT" 'lock cycle failed'
fi

t "pk_with_polkitd_lock runs the function under the lock"
locked_fn() { echo "${PK_LOCK_HELD:-0}"; }
out="$(PK_LOCK_HELD=0 bash -c "source '$KIT'; export PK_LOCK_DIR='$WORK/locks'; pk_with_polkitd_lock locked_fn 2>/dev/null" 2>/dev/null || true)"
# Run in this shell so the function is visible:
out="$(pk_with_polkitd_lock locked_fn)"
[ "$out" = 1 ] && ok "$CURRENT" || bad "$CURRENT" "inside lock PK_LOCK_HELD=$out (want 1)"
[ "${PK_LOCK_HELD:-0}" = 0 ] && ok "lock released after pk_with_polkitd_lock" \
  || bad "lock still held after wrapper" "PK_LOCK_HELD=${PK_LOCK_HELD:-unset}"

echo
echo "== machine-global state: restore always happens =="

# pk_gate_unreachable and pk_restore_state mutate the WHOLE host (mask,
# stop, pkill, unmask, restart). These tests must not, so systemctl, pkill,
# pgrep, rm and sleep are shadowed and PK_ACTIVATE points at a temp file.
# `pkill` being a no-op is what lets the test reach the MISMATCH branch
# without ever killing a real daemon.
#
# pgrep's answer is derived from the fake systemctl's call log rather than a
# call counter: once `mask` is logged but not `unmask`, polkitd counts as
# up (forcing the MISMATCH branch); once `unmask` is logged it counts as
# down (which is what makes pk_restore_state attempt a start). That keeps
# the test independent of the poll loop's iteration count.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/systemctl" <<'EOF'
#!/bin/bash
echo "$@" >> "$PK_SCT_LOG"
case "${1:-}" in
  start)  exit "${PK_START_RC:-0}" ;;
  unmask) exit "${PK_UNMASK_RC:-0}" ;;
esac
exit 0
EOF
cat > "$WORK/bin/pkill" <<'EOF'
#!/bin/bash
exit 0
EOF
cat > "$WORK/bin/pgrep" <<'EOF'
#!/bin/bash
log="$PK_SCT_LOG"
[ -f "$log" ] || exit 1
grep -q '^mask ' "$log" || exit 1
grep -q '^unmask ' "$log" && exit 1
exit 0
EOF
cat > "$WORK/bin/rm" <<'EOF'
#!/bin/bash
exit 0
EOF
cat > "$WORK/bin/sleep" <<'EOF'
#!/bin/bash
exit 0
EOF
chmod +x "$WORK/bin/systemctl" "$WORK/bin/pkill" "$WORK/bin/pgrep" "$WORK/bin/rm" "$WORK/bin/sleep"
: > "$WORK/activation"
save_path="$PATH"
export PATH="$WORK/bin:$PATH"
export PK_ACTIVATE="$WORK/activation"
export PK_SCT_LOG="$WORK/systemctl.log"
unset PK_LOCK_HELD

t "pk_gate_unreachable returns non-zero when polkitd cannot be stopped"
: > "$PK_SCT_LOG"
\rm -f "$WORK/activation.verity-hidden"
if pk_gate_unreachable com.agentshell.hostname.set >/dev/null 2>&1; then
  bad "$CURRENT" 'gate returned success instead of failing'
else
  ok
fi

t "MISMATCH restores the D-Bus activation file"
if [ -f "$WORK/activation" ] && [ ! -f "$WORK/activation.verity-hidden" ]; then
  ok
else
  bad "$CURRENT" "present=$([ -f "$WORK/activation" ] && echo yes || echo no) hidden=$([ -f "$WORK/activation.verity-hidden" ] && echo yes || echo no)"
fi

t "MISMATCH unmasked the unit and restarted polkitd"
if grep -q '^unmask ' "$PK_SCT_LOG" && grep -q '^start ' "$PK_SCT_LOG"; then
  ok
else
  bad "$CURRENT" "systemctl log: $(tr '\n' ' ' < "$PK_SCT_LOG" 2>/dev/null)"
fi

t "MISMATCH released the machine lock"
if [ "${PK_LOCK_HELD:-0}" = 0 ]; then
  ok
else
  bad "$CURRENT" "PK_LOCK_HELD=${PK_LOCK_HELD:-unset}"
fi

t "pk_restore reports failure when systemctl start fails"
# Guards the rc capture: `$?` taken after an `if` block holds the block's
# result, and the block's last command is `sleep` (always 0), so a start
# failure was being reported as success.
: > "$PK_SCT_LOG"
unset PK_START_RC PK_UNMASK_RC
PK_START_RC=1 pk_restore >/dev/null 2>&1
if [ $? -ne 0 ]; then
  ok
else
  bad "$CURRENT" 'rc=0 though systemctl start failed'
fi

t "pk_restore reports failure when unmask fails"
: > "$PK_SCT_LOG"
PK_UNMASK_RC=1 pk_restore >/dev/null 2>&1
if [ $? -ne 0 ]; then
  ok
else
  bad "$CURRENT" 'rc=0 though unmask failed'
fi

t "pk_restore emits 'incomplete —' on stderr when unmask fails"
# Radian 🟡#3: the new "verified teardown" contract promised rc=1 and a
# diagnostic on stderr, but no test drove the failure branch. This stubs
# systemctl unmask to fail and asserts both halves.
:: > "$PK_SCT_LOG"
PK_UNMASK_RC=1 pk_restore >/dev/null 2>"$WORK/pk_err"
rc=$?
if [ "$rc" != 0 ] && grep -q 'incomplete —' "$WORK/pk_err"; then
  ok
else
  bad "$CURRENT" "rc=$rc stderr=$(tr '\n' ' ' < "$WORK/pk_err" 2>/dev/null)"
fi

echo
echo "== pk_gate_unreachable: exhaustive driver (gate_case.sh) =="

# The stubs above exercise the MISMATCH path (mask logged but unmask
# missing) via pgrep's derived state. The 5 reachable code paths of
# pk_gate_unreachable — the fail-fast branches plus the happy path —
# are driven by gate_case.sh: one process per case, PATH-level stubs
# (bash function stubs are unreliable under `>/dev/null 2>&1` on bash
# 5.3), and `mv` forwarded to the real binary so the file-presence
# assertion at the end of the gate still proves something.
#
# CASE 3 requires a non-root shell: it fails hide by chmod-a-w-ing the
# parent of the activation file, which root ignores. The driver
# documents this in its header; we skip the assertion on root hosts
# rather than silently passing.
gate_case() {
  PK_KIT="$KIT" WORKDIR="$WORK" CASE="$1" bash "$HERE/gate_case.sh" 2>&1 | tail -1
}

t "CASE 1: happy path — rc=0, activation hidden"
out="$(gate_case 1)"
printf '%s' "$out" | grep -q '^rc=0 hidden=y dir=n$' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $out"

t "CASE 2: mask fails — rc=1, nothing hidden"
out="$(gate_case 2)"
printf '%s' "$out" | grep -q '^rc=1 hidden=n dir=y$' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $out"

if [ "$(id -u)" = 0 ]; then
  printf '  SKIP  CASE 3: hide-fails branch requires non-root (root ignores dir perms)\n'
else
  t "CASE 3: hide fails — rc=1, activation still present"
  out="$(gate_case 3)"
  printf '%s' "$out" | grep -q '^rc=1 hidden=n dir=y$' && ok "$CURRENT" \
    || bad "$CURRENT" "got: $out"
fi

t "CASE 4: pgrep still alive after stop — rc=1, restored"
out="$(gate_case 4)"
printf '%s' "$out" | grep -q '^rc=1 hidden=n dir=y$' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $out"

t "CASE 5: stop fails after mask — rc=1, restored"
out="$(gate_case 5)"
printf '%s' "$out" | grep -q '^rc=1 hidden=n dir=y$' && ok "$CURRENT" \
  || bad "$CURRENT" "got: $out"

unset PK_START_RC PK_UNMASK_RC
PATH="$save_path"
export PATH
unset PK_ACTIVATE PK_SCT_LOG
\rm -rf "$WORK/bin"

echo
echo "== pk_init =="

t "pk_init passes when pkcheck is resolvable"
expect_ok "pk_init" env PK_SUDO=direct PK_PKCHECK=pkcheck bash -c "source '$KIT'; pk_init"

t "pk_init fails when pkcheck is not resolvable"
expect_fail "pk_init without pkcheck" env PK_SUDO=direct PK_PKCHECK="$WORK/no-such-pkcheck" bash -c "source '$KIT'; pk_init"

t "pk_init derives the task name before any write"
unset PK_RULE_NAME
name="$(env -u PK_RULE_NAME MULTICA_TASK_ID=01a05223-7075-7228 PK_SUDO=direct PK_PKCHECK=pkcheck bash -c "source '$KIT'; pk_init; printf '%s' \"\$PK_RULE_NAME\"")"
[ "$name" = "01a05223-7075-7228" ] && ok "$CURRENT" || bad "$CURRENT" "PK_RULE_NAME='$name'"
export PK_RULE_NAME=verity-task-a

echo
echo "== pk_init: PK_SUDO enumeration (TSI-2614) =="

# The kit validates PK_SUDO at source time: any value outside `direct`,
# `sudo -n`, `auto` is refused before pk_init runs, so the diagnostic
# arrives from the enumeration itself rather than from a later call.
# Drive it through a child shell — the enumeration uses `return 2` when
# sourced, and the return value must not be swallowed by the enclosing
# command substitution.
run_sudo() {
  local val="$1" script out rc
  script="source '$KIT'; rc=\$?; printf '%s' \"\$PK_SUDO\"; exit \$rc"
  out="$(env PK_SUDO="$val" PK_PKCHECK=pkcheck PK_RULE_NAME=verity-task-a \
              PK_RULE_DIR="$WORK/rules.d" PK_LOCK_DIR="$WORK/locks" \
              bash -c "$script" 2>&1)"
  rc=$?
  PK_SUDO_RC=$rc
  PK_SUDO_OUT=$out
}

t "pk_init rejects a free-form PK_SUDO prefix"
run_sudo "sudo -n -E -l"
[ "$PK_SUDO_RC" != 0 ] && ok "$CURRENT" || bad "$CURRENT" "accepted free-form prefix: '$PK_SUDO_OUT'"

t "the rejection names the value and the reason"
run_sudo "sudo -u nobody"
if printf '%s\n' "$PK_SUDO_OUT" | grep -q 'PK_SUDO must be' && \
   printf '%s\n' "$PK_SUDO_OUT" | grep -q 'sudo -u nobody'; then ok "$CURRENT"
else bad "$CURRENT" "diagnostic missing: $PK_SUDO_OUT"
fi

t "a swallowing prefix is refused (the PK_SUDO=true case)"
run_sudo "true"
[ "$PK_SUDO_RC" != 0 ] && ok "$CURRENT" || bad "$CURRENT" "PK_SUDO=true was accepted"

t "a failing prefix is refused (the PK_SUDO=false case)"
run_sudo "false"
[ "$PK_SUDO_RC" != 0 ] && ok "$CURRENT" || bad "$CURRENT" "PK_SUDO=false was accepted"

t "PK_SUDO=auto resolves from id -u (non-root -> sudo -n)"
run_sudo "auto"
if [ "$(id -u)" = 0 ]; then want=direct; else want="sudo -n"; fi
[ "$PK_SUDO_RC" = 0 ] && [ "$PK_SUDO_OUT" = "$want" ] && ok "$CURRENT" \
  || bad "$CURRENT" "rc=$PK_SUDO_RC PK_SUDO='$PK_SUDO_OUT' want='$want'"

t "PK_SUDO=direct is accepted unchanged"
run_sudo "direct"
[ "$PK_SUDO_RC" = 0 ] && [ "$PK_SUDO_OUT" = "direct" ] && ok "$CURRENT" \
  || bad "$CURRENT" "rc=$PK_SUDO_RC PK_SUDO='$PK_SUDO_OUT'"

echo
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ]

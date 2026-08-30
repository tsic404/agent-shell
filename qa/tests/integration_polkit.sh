#!/bin/bash
# qa/tests/integration_polkit.sh — live polkitd integration for qa/pkkit.sh.
#
# Requires: root-or-sudo, a live polkitd, and the com.agentshell.* actions
# defined in /usr/share/polkit-1/actions (installed from the repo's
# packaging/com.agentshell.policy; this script does not modify policy files).
#
# What it proves, against a real polkitd rather than a fake pkcheck:
#   1. pk_gate yes / pk_gate no converge on the intended decision — the
#      writer is structurally incapable of emitting a broken rule.
#   2. A gate that cannot converge aborts non-zero and dumps evidence.
#   3. RULE ORDERING: polkitd sorts rules.d lexicographically ascending and
#      the FIRST match wins. Pinned empirically against this host's polkitd,
#      because no polkit documentation states the rule it relies on.
#   4. TWO-TASK CONCURRENCY (the TSI-2610 defect): two kits with different
#      task ids share /etc/polkit-1/rules.d; neither may delete the other's
#      live rule, and each must still observe its own decision.
#   5. pk_gate_unreachable + pk_restore round-trip and leave polkitd up.
#      This stops polkitd for real, so it is gated behind RUN_UNREACHABLE=1
#      and runs last.
#   6. No SyntaxError in the polkitd journal during the run (TSI-2614).
#
# Usage:
#   qa/tests/integration_polkit.sh             # sections 1-4, 6
#   RUN_UNREACHABLE=1 qa/tests/integration_polkit.sh
#
# Exit 0 = all pass.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
KIT="$HERE/../pkkit.sh"

PASS=0
FAIL=0
CURRENT=""

t() { CURRENT="$1"; shift; }
ok() { PASS=$((PASS + 1)); printf '  ok    %s\n' "$CURRENT"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n        %s\n' "$CURRENT" "$1"; }

# rules.d is 750 root:polkitd, so unprivileged listing fails even though we
# can write through sudo. Every inspection goes through sudo.
#
# sroot is deliberately not delegated to pk_priv: this script's skip guards
# run before qa/pkkit.sh is sourced, and this file is meant to stand alone.
# Dispatch on id -u so the header claim ("root-or-sudo") is true: on a
# root CI runner the direct path is taken, otherwise `sudo -n`.
RULES_DIR=/etc/polkit-1/rules.d
sroot() {
  if [ "$(id -u)" = 0 ]; then "$@"
  else sudo -n "$@"
  fi
}

# An action whose default is auth_admin_keep, so an explicit rule is needed
# to authorize anything and the effect of every rule is observable.
ACTION=com.agentshell.hostname.set
# A second action, so two concurrent kits can be told apart by policy.
ACTION_B=com.agentshell.mount

PK_PKCHECK="${PK_PKCHECK:-/usr/bin/pkcheck}"
export PK_PKCHECK
PK_RULE_ORDER="${PK_RULE_ORDER:-10}"
export PK_RULE_ORDER
PK_TIMEOUT="${PK_TIMEOUT:-10}"
export PK_TIMEOUT
PK_POLL="${PK_POLL:-1}"
export PK_POLL
# PK_ACTIVATE intentionally has no override: the kit's default is the path
# polkit actually installs on this distro, and hardcoding a different one
# makes pk_gate_unreachable's move a silent no-op.
PK_LOCK_DIR="${PK_LOCK_DIR:-/run/lock/agent-shell-qa}"
export PK_LOCK_DIR

# ── preconditions ─────────────────────────────────────────────────────
echo "== preconditions =="

t "pkcheck available"
if command -v "$PK_PKCHECK" >/dev/null; then ok "$CURRENT"; else
  bad "$CURRENT" "not found: $PK_PKCHECK"
fi

t "flock available"
if command -v flock >/dev/null; then ok "$CURRENT"; else
  bad "$CURRENT" "flock not found — pk_gate_unreachable cannot serialise"
fi

t "polkitd is running"
if pgrep -x polkitd >/dev/null; then ok "$CURRENT"; else
  bad "$CURRENT" "no polkitd process"
fi

t "$ACTION is defined with an auth_admin_keep default"
# polkit 127 prints `implicit active:  auth_admin_keep` (variable spacing);
# older builds print `allow_active=auth_admin_keep`. Match both loosely.
if pkaction -v -a "$ACTION" 2>/dev/null | grep -i active | grep -q 'auth_admin_keep'; then
  ok "$CURRENT"
else
  bad "$CURRENT" "unexpected or missing default for $ACTION"
fi

t "can write to $RULES_DIR"
if sroot test -w "$RULES_DIR" 2>/dev/null; then ok "$CURRENT"; else
  bad "$CURRENT" "rules.d is not writable through sudo -n"
fi

. "$KIT"

t "the D-Bus activation file exists"
if [ -f "$PK_ACTIVATE" ]; then ok "$CURRENT"; else
  bad "$CURRENT" "not found: $PK_ACTIVATE (unreachable gate cannot be tested)"
fi

pk_init || { echo "integration: pk_init failed"; exit 1; }

# Drop only THIS kit's own rules from a previous run. Anything else is
# another task's business — never delete it (the TSI-2610 lesson).
pk_rule_drop

WINDOW_FROM="$(date -Is)"

# ── the gate against a real polkitd ───────────────────────────────────
echo
echo "== pk_gate yes =="

# Write to an explicit path: pk_rule_glob returns an unexpanded glob, which
# the calling shell would try to expand against a rules.d it cannot read.
F="$PK_RULE_DIR/${PK_RULE_ORDER}-${PK_RULE_NAME}-yes-$ACTION.rules"

t "pk_rule_write produces a rule polkitd can read"
if pk_rule_write yes "$ACTION" "$F"; then ok "$CURRENT"; else bad "$CURRENT" "write failed"; fi

t "the rule file is present and readable"
if sroot test -r "$F"; then ok "$CURRENT"; else bad "$CURRENT" "no readable rule at $F"; fi

t "the written rule is JS polkitd can compile"
if sroot grep -q 'return polkit.Result.YES;' "$F" 2>/dev/null; then ok "$CURRENT"; else
  bad "$CURRENT" "rule has no YES return"
fi

t "the gate converges on YES against live polkitd"
if pk_gate yes "$ACTION"; then ok "$CURRENT"; else bad "$CURRENT" "gate did not converge"; fi

t "the oracle reports yes"
if [ "$(pk_check_says "$ACTION")" = yes ]; then ok "$CURRENT"; else
  bad "$CURRENT" "oracle says $(pk_check_says "$ACTION")"
fi

echo
echo "== pk_gate no =="

t "pk_gate no converges on NO"
if pk_gate no "$ACTION"; then ok "$CURRENT"; else bad "$CURRENT" "gate did not converge"; fi

t "the YES rule was dropped"
if sroot bash -c "ls -1 '${PK_RULE_DIR}/${PK_RULE_ORDER}-${PK_RULE_NAME}-yes-${ACTION}.rules'" 2>/dev/null | grep -q .; then
  bad "$CURRENT" "stale YES rule survived"
else
  ok "$CURRENT"
fi

t "the oracle reports no"
if [ "$(pk_check_says "$ACTION")" = no ]; then ok "$CURRENT"; else
  bad "$CURRENT" "oracle says $(pk_check_says "$ACTION")"
fi

echo
echo "== mismatch aborts =="

# Ask the gate to prove YES for an action that has no policy at all, so the
# oracle can never converge. A genuine mismatch, not a timing race.
t "a gate that cannot converge returns non-zero"
MISSING_ACTION=com.agentshell.definitely.not.defined
env PK_TIMEOUT=3 PK_POLL=1 bash -c ". '$KIT'; pk_gate yes '$MISSING_ACTION'" >/dev/null 2>&1
if [ $? -ne 0 ]; then ok "$CURRENT"; else bad "$CURRENT" "gate returned 0 on mismatch"; fi

t "the mismatch marker is on stderr"
out="$(env PK_TIMEOUT=3 PK_POLL=1 bash -c ". '$KIT'; pk_gate yes '$MISSING_ACTION'" 2>&1 >/dev/null)"
if printf '%s' "$out" | grep -q 'pk_gate MISMATCH'; then ok "$CURRENT"; else
  bad "$CURRENT" "no MISMATCH marker in: $out"
fi

pk_rule_drop

# ── rule ordering ──────────────────────────────────────────────────────
# polkitd sorts rules.d by filename ascending and evaluates the first match.
# Nothing in the polkit documentation states this, so the kit pins it here
# against the live daemon: a LOW numeric prefix is what makes our rule beat
# a leftover from an earlier run (e.g. a 60-verity-* file). Getting this
# backwards silently negates every rule the kit writes.
echo
echo "== rule ordering (first match wins) =="

t "a lower-prefixed rule beats a higher-prefixed one"
low="$PK_RULE_DIR/05-ordering-yes-$ACTION.rules"
high="$PK_RULE_DIR/95-ordering-no-$ACTION.rules"
sroot tee "$low" >/dev/null <<EOF
polkit.addRule(function(action, subject) {
    if (action.id == "$ACTION") {
        return polkit.Result.YES;
    }
});
EOF
sroot tee "$high" >/dev/null <<EOF
polkit.addRule(function(action, subject) {
    if (action.id == "$ACTION") {
        return polkit.Result.NO;
    }
});
EOF
pk_rule_drop   # leave only the two ordering probes
sleep 2
if [ "$(pk_check_says "$ACTION")" = yes ]; then ok "$CURRENT"; else
  bad "$CURRENT" "lower prefix lost: $(pk_check_says "$ACTION")"
fi

t "swapping the prefixes inverts the decision"
sroot mv -f "$low" "$low.tmp"
sroot mv -f "$high" "$low"
sroot mv -f "$low.tmp" "$high"
sleep 2
if [ "$(pk_check_says "$ACTION")" = no ]; then ok "$CURRENT"; else
  bad "$CURRENT" "expected NO after swap, got $(pk_check_says "$ACTION")"
fi

sroot rm -f "$low" "$high" "$low.tmp" "$high.tmp"

echo
echo "== two concurrent tasks share rules.d =="

# Two kits, different task ids, same machine-level rules.d. Both write, then
# both gate. Neither may remove the other's rule, and each must observe its
# own decision. Before the TSI-2610 fix the teardown glob was shared, so one
# task deleted the other's live rule mid-scenario.
t "two tasks each observe their own decision"
rm -f /tmp/pk-conc-A.done /tmp/pk-conc-B.done
(
  export PK_RULE_NAME=tsi2610-task-a PK_RULE_DIR PK_RULE_ORDER PK_TIMEOUT PK_POLL PK_ACTIVATE PK_LOCK_DIR
  export PK_SUDO=direct
  if [ "$(id -u)" != 0 ]; then export PK_SUDO='sudo -n'; fi
  . "$KIT"
  pk_rule_write yes "$ACTION"
  pk_gate yes "$ACTION" >/dev/null 2>&1 && echo A-pass || echo A-fail
) &
PA=$!
(
  export PK_RULE_NAME=tsi2610-task-b PK_RULE_DIR PK_RULE_ORDER PK_TIMEOUT PK_POLL PK_ACTIVATE PK_LOCK_DIR
  export PK_SUDO=direct
  if [ "$(id -u)" != 0 ]; then export PK_SUDO='sudo -n'; fi
  . "$KIT"
  pk_rule_write yes "$ACTION_B"
  pk_gate yes "$ACTION_B" >/dev/null 2>&1 && echo B-pass || echo B-fail
) &
PB=$!
wait "$PA" "$PB"
if sroot test -f "$PK_RULE_DIR/${PK_RULE_ORDER}-tsi2610-task-a-yes-$ACTION.rules" \
   && sroot test -f "$PK_RULE_DIR/${PK_RULE_ORDER}-tsi2610-task-b-yes-$ACTION_B.rules"; then
  ok "$CURRENT"
else
  bad "$CURRENT" "a concurrent task's rule was deleted:
$(sroot ls -1 "$RULES_DIR" 2>/dev/null)"
fi

t "one task's drop leaves the sibling's rule intact"
if PK_RULE_NAME=tsi2610-task-a pk_rule_drop; then
  :
else
  bad "$CURRENT" "task-a's own drop failed"
fi
if sroot test -f "$PK_RULE_DIR/${PK_RULE_ORDER}-tsi2610-task-b-yes-$ACTION_B.rules"; then
  ok "$CURRENT"
else
  bad "$CURRENT" "task-b's rule was deleted by task-a's scoped drop"
fi

t "the sibling still observes its own decision"
if [ "$(pk_check_says "$ACTION_B")" = yes ]; then ok "$CURRENT"; else
  bad "$CURRENT" "oracle says $(pk_check_says "$ACTION_B")"
fi

PK_RULE_NAME=tsi2610-task-b pk_rule_drop

if [ "${RUN_UNREACHABLE:-0}" = 1 ]; then
  echo
  echo "== unreachable gate + restore (RUN_UNREACHABLE=1) =="

  t "pk_gate_unreachable proves polkitd is gone"
  if pk_gate_unreachable "$ACTION"; then ok "$CURRENT"; else
    bad "$CURRENT" "unreachable gate failed"
  fi

  t "with polkitd gone the oracle cannot return yes"
  case "$(pk_check_says "$ACTION")" in
    yes) bad "$CURRENT" 'polkitd answered while it should have been down' ;;
    *)   ok "$CURRENT" ;;
  esac

  t "pk_restore brings polkitd back"
  pk_restore
  sleep 3
  if pgrep -x polkitd >/dev/null; then ok "$CURRENT"; else
    bad "$CURRENT" "polkitd did not come back"
  fi

  t "pk_restore re-enables the activation file"
  if [ -f "$PK_ACTIVATE" ]; then ok "$CURRENT"; else
    bad "$CURRENT" "activation file missing after restore"
  fi

  t "pk_restore left no rules behind"
  if pk_rule_exists; then
    bad "$CURRENT" "our rules survived: $(sroot ls -1 "$RULES_DIR" 2>/dev/null | tr '\n' ' ')"
  else
    ok "$CURRENT"
  fi
else
  echo
  echo "== unreachable gate + restore: SKIPPED (set RUN_UNREACHABLE=1 to run) =="
fi

echo
echo "== polkitd journal: no SyntaxError during the run =="

t "zero SyntaxError entries in the polkitd journal since the run started"
# polkitd compiles rules asynchronously, so allow a few seconds for a reload
# right at the end of the run to have reached the journal.
sleep 3
journal="$(sroot journalctl -u polkit.service --since "$WINDOW_FROM" --no-pager 2>/dev/null)"
if [ -z "$journal" ]; then
  ok "$CURRENT"
  echo "        (no journal available on this host; assertion vacuous)"
elif printf '%s' "$journal" | grep -qi 'SyntaxError'; then
  bad "$CURRENT" "polkitd hit a SyntaxError during this run"
else
  ok "$CURRENT"
fi

echo
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ]

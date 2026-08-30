#!/bin/bash
# qa/pkkit.sh — reusable polkit harness kit for agent-shell QA.
#
# Every scenario that depends on a polkit rule MUST first pass a hard gate
# proving polkitd actually loaded the intended rule. Without it the suite
# cannot tell "my rule decided this" from "my rule was invalid and polkitd
# silently fell back to the action default" — see TSI-2614, which invalidated
# the entire TSI-2590 retest once.
#
# Three properties the whole kit exists to guarantee:
#
#  1. WRITE. pk_rule_write emits the rule body as ONE heredoc. The earlier
#     defect split the format string and its argument across two printf
#     calls, so a literal `%s` reached /etc/polkit-1/rules.d and polkitd
#     reported `SyntaxError: parse error (line 3)` only in the journal.
#     The writer therefore returns an error when the rendered file does not
#     contain the intended return value, so an unexpanded placeholder can
#     never reach disk.
#
#  2. GATE. pk_gate polls `pkcheck` after the write and returns non-zero
#     when the observed decision does not converge to the expected one. A
#     mismatch aborts the scenario — it never prints a warning and
#     continues. polkitd compiles rules on its own schedule, so the poll is
#     how a scenario learns the file write is actually in effect.
#
#  3. ISOLATION. /etc/polkit-1/rules.d is MACHINE-WIDE shared state, not
#     per-task state: every QA task on the host writes into the same
#     directory and polkitd reads the union of all of their rules. One
#     namespace alone is not enough, so the kit layers three:
#
#       a. NAMESPACE. Every filename carries a per-task identifier
#          (PK_RULE_NAME, default derived from the Multica task id), so a
#          task's glob can only ever match its own rules. The earlier harness
#          used a shared literal prefix plus `rm -f 60-verity-*.rules`, which
#          silently deleted a sibling task's in-flight rule and flipped its
#          exit-0 assertion into exit-2 — see TSI-2610.
#
#       b. SCOPE. pk_rule_drop removes ONLY files this kit instance wrote.
#          It never touches another task's rule.
#
#       c. LOCK. pk_gate_unreachable / pk_restore serialise the machine-global
#          part of the operation — masking the unit, hiding the D-Bus
#          activation file, restarting polkitd — behind one shared flock, so
#          two tasks cannot mask polkitd off each other. pk_gate itself takes
#          no lock: with unique filenames, concurrent writes to distinct rules
#          are safe and serialising them would starve other tasks for nothing.
#
# Usage:
#   source qa/pkkit.sh
#   pk_init          # refuse to continue if pkcheck/sudo are missing
#   if ! pk_gate yes com.agentshell.mount; then exit 7; fi
#   ... run the command under test ...
#   pk_restore       # best-effort teardown, safe to call twice
#
# pk_gate is the single choke point that turns "file written" into "decision
# in effect", so any future scenario inherits the invariant with one line.
#
# Configuration (all overridable; no repo paths are hardcoded):
#   PK_ACTION      action id for CLI subcommands (default com.agentshell.mount)
#   PK_RULE_DIR    polkit rules.d directory (default /etc/polkit-1/rules.d)
#   PK_RULE_NAME   task/issue identifier baked into every filename we create.
#                  Must be unique per concurrently running task — that is what
#                  makes the namespace work. Default: derived from
#                  MULTICA_TASK_ID, else TSI-<n> in MULTICA_ISSUE_REF, else
#                  a host+pid+time fallback. Override with your issue id.
#   PK_RULE_ORDER  numeric ordering prefix (default "10"). polkitd sorts
#                  rules.d lexicographically and the FIRST match wins, so a
#                  low number beats a leftover `60-verity-*` from an earlier
#                  run.
#   PK_ACTIVATE    D-Bus activation unit file (default /usr/share/dbus-1/
#                  system-services/org.freedesktop.PolicyKit1.service)
#   PK_LOCK_DIR    machine-level lock dir (default /run/lock/agent-shell-qa,
#                  mode 1777 so concurrent tasks can share it)
#   PK_LOCK_FILE   basename of the polkitd lock inside PK_LOCK_DIR
#   PK_LOCK_TIMEOUT seconds to wait for the polkitd lock (default 120)
#   PK_SUDO        privilege mode (default: auto)
#                      auto    root -> direct, otherwise `sudo -n`
#                      direct  always run unprivileged (already root)
#                      sudo    always `sudo -n`
#                      <cmd>   literal prefix, e.g. `sudo -n`
#   PK_PKCHECK     pkcheck binary (default: pkcheck)
#   PK_SUBJECT_PID polkit subject PID; polkit 127 requires a real process,
#                  so the default is the caller's own PID
#   PK_TIMEOUT     gate poll deadline in seconds (default 45)
#   PK_POLL        gate poll interval in seconds (default 1)

set -u

: "${PK_ACTION:=com.agentshell.mount}"
: "${PK_RULE_DIR:=/etc/polkit-1/rules.d}"
: "${PK_RULE_ORDER:=10}"
: "${PK_ACTIVATE:=/usr/share/dbus-1/system-services/org.freedesktop.PolicyKit1.service}"
: "${PK_LOCK_DIR:=/run/lock/agent-shell-qa}"
: "${PK_LOCK_FILE:=polkitd}"
: "${PK_LOCK_TIMEOUT:=120}"
: "${PK_PKCHECK:=pkcheck}"
: "${PK_SUBJECT_PID:=}"
: "${PK_TIMEOUT:=45}"
: "${PK_POLL:=1}"

# ── privilege runner ───────────────────────────────────────────────────
# Every privileged action goes through pk_priv, which runs "$@" as one argv
# array under the configured prefix. A word-list prefix cannot accidentally
# swallow the command itself the way `PK_SUDO=true mv file` would — `true`
# takes no arguments and would silently do nothing.
PK_SUDO="${PK_SUDO:-auto}"
if [ "$PK_SUDO" = "auto" ]; then
  if [ "$(id -u)" = 0 ]; then
    PK_SUDO="direct"
  else
    PK_SUDO="sudo -n"
  fi
fi

pk_priv() {
  if [ "$PK_SUDO" = "direct" ]; then
    "$@"
  else
    $PK_SUDO "$@"
  fi
}

# Shell PID of the caller — stable across function calls, so it is a
# faithful polkit subject (a subshell or job PID may already be gone).
pk_self() { printf '%s\n' "$$"; }

# ── task identity ──────────────────────────────────────────────────────
# PK_RULE_NAME is the single variable that makes concurrent QA tasks safe.
# It is derived once, so every filename and every drop in this shell — and in
# any function it calls — agrees on the same namespace.
#
# Derivation order (first non-empty wins):
#   PK_RULE_NAME            explicit override
#   MULTICA_TASK_ID         per-delegation task id, e.g. 01a05223-7075-7228-...
#   MULTICA_ISSUE_REF       TSI-2610 / issue ref
#   MULTICA_WORKSPACE_ID    per-workspace; not unique per task, so only a
#                           late fallback
#   fallback                host-pid+epoch; always unique within one run
pk_task_id() {
  if [ -n "${PK_RULE_NAME:-}" ]; then printf '%s\n' "$PK_RULE_NAME"; return; fi
  if [ -n "${MULTICA_TASK_ID:-}" ]; then printf '%s\n' "$MULTICA_TASK_ID"; return; fi
  if [ -n "${MULTICA_ISSUE_REF:-}" ]; then
    printf '%s\n' "$MULTICA_ISSUE_REF" | tr '[:upper:]' '[:lower:]' | head -c 32; return
  fi
  if [ -n "${MULTICA_WORKSPACE_ID:-}" ]; then printf '%s\n' "$MULTICA_WORKSPACE_ID"; return; fi
  printf '%s-%s-%s\n' "$(hostname -s 2>/dev/null || echo host)" "$$" "$(date +%s)"
}

# pk_rule_name — the task id, sanitised to filename-safe characters.
#
# Sanitisation replaces every character outside [A-Za-z0-9._-] with '-'. That
# set is deliberately chosen so a value can NEVER be a glob pattern or a path:
# no '*', '?', '[', ']' and no '/', so a glob built from it only ever matches
# its literal siblings and cannot reach another task's directory.
# The character set is validated explicitly too, because trusting tr alone
# would let a stray metacharacter through if this function were edited later.
pk_rule_name() {
  local raw t
  raw="$(pk_task_id)"
  t="$(printf '%s' "$raw" | tr -c 'A-Za-z0-9._-' '-')"
  t="$(printf '%s' "$t" | sed 's/^-*//; s/-*$//; s/--\+/-/g')"
  t="$(printf '%s' "$t" | cut -c1-64)"
  t="$(printf '%s' "$t" | sed 's/-*$//')"
  if [ -z "$t" ]; then
    t="$(hostname -s 2>/dev/null || echo host)-$$"
  fi
  if ! printf '%s' "$t" | grep -Eq '^[A-Za-z0-9._-]+$'; then
    echo "pk_rule_name: derived task id '$raw' is not filename-safe" >&2
    return 1
  fi
  PK_RULE_NAME="$t"
  printf '%s\n' "$t"
}

# pk_rule_glob — the filename pattern for this task's rules, quoted for
# embedding inside the root bash -c call. Returns 1 if the name is unsafe.
# The pattern ends in `*` on purpose; sanitisation keeps the task-id segment
# itself free of glob metacharacters, which is what makes the match scoped.
pk_rule_glob() {
  local name
  name="$(pk_rule_name)" || return 1
  printf '%s/%s-%s-*.rules\n' "$PK_RULE_DIR" "$PK_RULE_ORDER" "$name"
}

# ── machine-level lock ─────────────────────────────────────────────────
# pk_lock_dir — ensure the lock directory exists and is world-writable, so
# every QA task on the host can share one lock file. Created via pk_priv
# because /run/lock is root-owned; 1777 makes it sticky+writable so tasks do
# not need to trust each other to create their own lock files.
pk_lock_dir() {
  pk_priv mkdir -p "$PK_LOCK_DIR" 2>/dev/null || return 1
  pk_priv chmod 1777 "$PK_LOCK_DIR" 2>/dev/null
  test -d "$PK_LOCK_DIR" && test -w "$PK_LOCK_DIR"
}

# pk_lock_acquire — take the polkitd lock on fd 9.
#
# The lock is acquired here, in the harness shell, and held by the caller
# until pk_lock_release. Two constraints:
#
#   * The fd must be opened by THIS shell. Opening it inside `sudo` would
#     release the lock the moment the sudo'd shell exits, because the fd is
#     per-process — so every pk_priv call happens while fd 9 is held here.
#
#   * The open must be checked. `exec 9>>file` prints an error and returns 1
#     but does NOT exit the shell, so an unchecked open would silently run
#     unlocked and reintroduce the race this lock exists to prevent.
#
# Timeout is enforced with flock(1)'s -w, which releases cleanly on timeout
# rather than leaving a stale fd behind.
pk_lock_acquire() {
  local lockfile
  pk_lock_dir || { echo "pk_lock_acquire: cannot prepare $PK_LOCK_DIR" >&2; return 1; }
  lockfile="$PK_LOCK_DIR/$PK_LOCK_FILE"

  if ! command -v flock >/dev/null; then
    echo "pk_lock_acquire: flock not available — refusing to run unlocked" >&2
    return 1
  fi

  exec 9>>"$lockfile"
  if [ $? -ne 0 ] || [ ! -e /proc/self/fd/9 ]; then
    echo "pk_lock_acquire: failed to open lock fd for $lockfile — refusing to run unlocked" >&2
    return 1
  fi

  if ! flock -w "$PK_LOCK_TIMEOUT" 9; then
    echo "pk_lock_acquire: timed out after ${PK_LOCK_TIMEOUT}s waiting for $lockfile" >&2
    return 1
  fi
  PK_LOCK_HELD=1
  return 0
}

# pk_lock_release — drop the lock. Idempotent; closing an unopened fd is a
# no-op so teardown stays best-effort.
pk_lock_release() {
  if [ "${PK_LOCK_HELD:-0}" = 1 ]; then
    flock -u 9 2>/dev/null
    exec 9>&- 2>/dev/null
    PK_LOCK_HELD=0
  fi
}

# pk_with_polkitd_lock <fn> [args...]
#
# Run <fn> under the polkitd lock. Used only by the two functions that
# mutate machine-global state. The lock is taken after the caller's own
# pk_rule_drop so a task never blocks on another task just to delete its
# own rule.
pk_with_polkitd_lock() {
  local fn="$1"; shift
  pk_lock_acquire || return 1
  "$fn" "$@"
  local rc=$?
  pk_lock_release
  return "$rc"
}

# ── writer ─────────────────────────────────────────────────────────────
# pk_rule_write <yes|no> <action-id> [file]
#
# Emits the JS rule as a single heredoc so the format string and its
# argument cannot be split across separate printf calls, substitutes the
# placeholders with awk, and verifies the bytes on disk before returning
# success.
#
# The default filename is <ORDER>-<task-name>-<want>-<action>.rules. The
# task name is what keeps two QA tasks apart; the low ORDER prefix is what
# makes our rule win over a leftover `60-verity-*` from an earlier run,
# because polkitd sorts rules.d lexicographically and the FIRST match wins.
pk_rule_write() {
  local want="$1" action="$2" file="${3:-}" name
  local body tmpfile rendered
  case "$want" in
    yes) body=polkit.Result.YES ;;
    no)  body=polkit.Result.NO ;;
    *)   echo "pk_rule_write: want must be yes|no, got '$want'" >&2; return 2 ;;
  esac
  if [ -z "$action" ]; then
    echo "pk_rule_write: empty action id" >&2
    return 2
  fi
  name="$(pk_rule_name)" || return 1
  if [ -z "$file" ]; then
    file="$PK_RULE_DIR/${PK_RULE_ORDER}-${name}-${want}-$action.rules"
  fi

  tmpfile="$(mktemp 2>/dev/null)" || return 2

  # Single heredoc: nothing is interpolated by printf, so no format string
  # can ever be split from its argument. Quoted delimiter = literal output.
  cat > "$tmpfile" <<'PK_RULE_EOF'
polkit.addRule(function(action, subject) {
    if (action.id == "ACTION_PLACEHOLDER") {
        return BODY_PLACEHOLDER;
    }
});
PK_RULE_EOF

  # awk does the substitution on a regular file, not on the awk program
  # itself, so action/body cannot be reinterpreted as a format string.
  rendered="$tmpfile.rendered"
  if ! awk -v action="$action" -v body="$body" '
        { gsub(/ACTION_PLACEHOLDER/, action); gsub(/BODY_PLACEHOLDER/, body); print }
      ' "$tmpfile" > "$rendered"; then
    echo "pk_rule_write: substitution failed" >&2
    rm -f "$tmpfile" "$rendered"
    return 1
  fi

  # Install the finished bytes with one root-owned rename, so rules.d never
  # holds a partially written rule — a half-written rule is exactly what
  # polkitd would compile and then silently drop.
  if ! pk_priv mv -f "$rendered" "$file"; then
    echo "pk_rule_write: install to $file failed" >&2
    rm -f "$tmpfile" "$rendered"
    return 1
  fi
  pk_priv chmod 644 "$file"
  rm -f "$tmpfile" "$rendered"

  pk_rule_check_file "$want" "$action" "$file"
}

# pk_rule_check_file <yes|no> <action-id> <file>
#
# Content assertion for files this kit did not write. Returns 1 when the
# file carries an unexpanded placeholder or is missing the intended action
# and return value, so a stale or broken rule is detected before it can be
# mistaken for a real polkit decision.
pk_rule_check_file() {
  local want="$1" action="$2" file="$3" body
  case "$want" in
    yes) body=polkit.Result.YES ;;
    no)  body=polkit.Result.NO ;;
  esac
  if ! pk_priv test -r "$file"; then
    echo "pk_rule_check_file: $file not readable" >&2
    return 1
  fi
  if pk_priv grep -q 'return %s;' "$file"; then
    echo "pk_rule_check_file: unexpanded placeholder in $file" >&2
    pk_priv sed 's/^/  rule: /' "$file" >&2
    return 1
  fi
  if ! pk_priv grep -q "action.id == \"$action\"" "$file"; then
    echo "pk_rule_check_file: action '$action' not present in $file" >&2
    pk_priv sed 's/^/  rule: /' "$file" >&2
    return 1
  fi
  if ! pk_priv grep -q "return $body;" "$file"; then
    echo "pk_rule_check_file: expected 'return $body;' not in $file" >&2
    pk_priv sed 's/^/  rule: /' "$file" >&2
    return 1
  fi
  return 0
}

# pk_rule_drop — remove ONLY the rules this kit instance wrote.
#
# Scoped to our own <ORDER>-<task-name>-*.rules pattern. The earlier version
# globbed a shared `60-verity-*.rules` prefix, so one task's teardown deleted
# a sibling task's live rule — the exact failure in TSI-2610. Other tasks'
# rules are left untouched; they are theirs to clean up.
#
# Runs as root because rules.d is 750 root:polkitd, so a local glob never
# expands and `rm -f` keeps everything.
pk_rule_drop() {
  local name
  name="$(pk_rule_name)" || return 1
  # Quote the directory and order, leave the glob's trailing `*` unquoted so
  # it still expands in the root shell. Quoting the whole pattern turned `*`
  # into a literal name and made every drop a silent no-op.
  pk_priv bash -c "rm -f '$PK_RULE_DIR'/${PK_RULE_ORDER}-$name-*.rules"
}

# pk_rule_exists — root-side check for any rule this kit instance owns.
# Rules must be enumerated through root: rules.d is not listable by an
# unprivileged caller, so a local glob expansion matches nothing. Returns 0
# when at least one own rule is present.
pk_rule_exists() {
  local name
  name="$(pk_rule_name)" || return 1
  pk_priv bash -c "ls '$PK_RULE_DIR'/${PK_RULE_ORDER}-$name-*.rules >/dev/null 2>&1"
}

# ── oracle ─────────────────────────────────────────────────────────────
# pk_check_raw — verbatim pkcheck output for the configured subject.
# Rootd resolves the polkit subject from the caller's PID, so the harness
# shell's own PID is the faithful subject (polkit 127 refuses a bare action
# check and refuses cross-identity subjects).
pk_check_raw() {
  local action="$1"
  if [ -z "$PK_SUBJECT_PID" ]; then PK_SUBJECT_PID="$(pk_self)"; fi
  "$PK_PKCHECK" -a "$action" -p "$PK_SUBJECT_PID" 2>&1 | sed 's/[[:space:]]*$//'
}

# pk_check_says — normalise the oracle to yes|no|unknown. polkit 127 prints
# the decision as the escaped JS string `polkit\56result`; some builds
# report a denial as `Not authorized` instead.
pk_check_says() {
  local action="$1" raw
  raw="$(pk_check_raw "$action")"
  case "$raw" in
    *'polkit\56result=yes'*)            echo yes ;;
    *'polkit\56result=no'*|*'Not authorized'*) echo no ;;
    *)                                  echo "unknown: $(printf '%s' "$raw" | head -1)" ;;
  esac
}

# ── the gate ───────────────────────────────────────────────────────────
# pk_gate <yes|no> <action-id>
#
# Hard pre-requirement for any polkit-dependent scenario: write the rule,
# then poll the oracle until the observed decision equals the expected one.
# Returns non-zero on timeout or mismatch so the caller aborts. The gate
# never warns and continues — a stale rule can never be mistaken for a real
# polkit decision.
#
# pk_gate takes NO lock. Rule files are named per task, so concurrent gates
# write to distinct filenames and each observes its own rule. Taking a
# machine-wide lock here would serialise every task behind one and gain
# nothing — the only truly global state is the polkitd process itself,
# which pk_gate never touches.
pk_gate() {
  local want="$1" action="$2" said="" i wait_s
  # Drop only our own stale rules. A blanket delete is the bug: it removed
  # another task's in-flight rule mid-scenario.
  pk_rule_drop || return 1
  pk_rule_write "$want" "$action" || return 1

  wait_s=$((PK_TIMEOUT / PK_POLL))
  [ "$wait_s" -lt 1 ] && wait_s=1
  for ((i = 1; i <= wait_s; i++)); do
    sleep "$PK_POLL"
    said="$(pk_check_says "$action")"
    if [ "$said" = "$want" ]; then
      echo "pk_gate: intended=$want observed=$said after ${i}s"
      return 0
    fi
  done
  echo "pk_gate MISMATCH: intended=$want observed=$said action=$action" >&2
  echo "  raw: $(pk_check_raw "$action" | head -1)" >&2
  pk_priv bash -c "ls -l '$PK_RULE_DIR'" 2>&1 | sed 's/^/  rules.d: /' >&2
  return 1
}

# pk_gate_unreachable <action-id>
#
# Companion gate for scenarios that need polkitd to be absent. Masking the
# unit alone is not enough: a still-running polkitd keeps the bus name
# registered, so the call would be authorized anyway. Everything must be
# true at once, and the result is proved by pgrep rather than assumed.
#
# Locked. Masking the unit, moving the D-Bus activation file and killing
# polkitd all affect EVERY process on the host, so two concurrent QA tasks
# would each unmask/restart the other's masked state.
#
# Restores itself on failure. Reaching the MISMATCH branch below means the
# unit is masked, the activation file is moved aside and polkitd was
# killed, so returning without undoing that would leave the whole host
# without polkitd — the same class of blast radius this kit exists to
# prevent (see TSI-2610).
#
# Lock lifecycle. The success path returns with the lock still held: the
# caller must run pk_restore, which takes the lock and releases it. The
# MISMATCH path restores in place and releases before returning. Holding
# the lock across the success return is deliberate — releasing it there
# would let another task mask or restart polkitd while this task still has
# it down, the TSI-2610 window. The fd lives in the harness shell, so a
# shell exit or crash drops flock in the kernel and the lock cannot wedge.
pk_gate_unreachable() {
  pk_rule_drop || return 1
  pk_lock_acquire || return 1
  PK_LOCK_HELD=1

  pk_priv systemctl mask --runtime polkit.service >/dev/null 2>&1
  pk_priv mv -f "$PK_ACTIVATE" "$PK_ACTIVATE.verity-hidden" 2>/dev/null
  pk_priv systemctl stop polkit.service >/dev/null 2>&1
  pk_priv pkill -x polkitd 2>/dev/null

  local i
  for ((i = 1; i <= 15; i++)); do
    if ! pgrep -x polkitd >/dev/null; then break; fi
    sleep 1
  done
  if pgrep -x polkitd >/dev/null; then
    echo "pk_gate_unreachable MISMATCH: polkitd still running ($(pgrep -x polkitd | tr '\n' ' '))" >&2
    # Undo the mask/activation/stop done above while the lock is still held.
    # Do NOT call pk_restore here: its pk_lock_acquire runs
    # `exec 9>>$lockfile`, which dup2s over the fd this shell is holding, so
    # the old fd's flock is released before the new fd is re-locked by
    # `flock -w`. That is a brief gap in which another task can take the
    # lock — releasing the lock on a lock-held path is exactly the window
    # this kit is meant to close. A partial restore is still better than
    # returning with the whole host masked off.
    if ! pk_restore_state; then
      echo "  (restore was partial; pk_restore should be retried)" >&2
    fi
    pk_lock_release
    return 1
  fi
  echo "pk_gate_unreachable: action=$1 polkitd procs=0 activation=$(if [ -f "$PK_ACTIVATE" ]; then echo present; else echo removed; fi)"
  return 0
}

# pk_restore_state — the restore sequence proper: no lock, no drop.
#
# Every machine-global change pk_gate_unreachable makes is undone here, so a
# failure part-way through the unreachable gate can hand this back and leave
# the host the way it found it. Callers take the lock and release it.
#
# Each failing step keeps its own variable rather than sharing one. A single
# `rc=1` would be overwritten by the next assignment, and taking `$?` after an
# `if` construct holds the construct's result rather than `systemctl`'s — the
# block's last command is `sleep`, which is always 0, so a start failure was
# being reported as success.
#
# Returns non-zero if the unit could not be unmasked or polkitd could not be
# brought back up.
pk_restore_state() {
  local unmask_failed=0 start_failed=0

  # `systemctl unmask` leaves the /run/systemd/system symlink behind on some
  # hosts (rc 0, mask retained), so remove it directly before unmasking.
  pk_priv mv -f "$PK_ACTIVATE.verity-hidden" "$PK_ACTIVATE" 2>/dev/null
  pk_priv rm -f /run/systemd/system/polkit.service /etc/systemd/system/polkit.service
  if ! pk_priv systemctl unmask polkit.service >/dev/null 2>&1; then
    unmask_failed=1
  fi
  pk_priv systemctl daemon-reload 2>/dev/null

  if ! pgrep -x polkitd >/dev/null; then
    pk_priv systemctl start polkit.service >/dev/null 2>&1
    start_failed=$?
    sleep 3
  fi

  return $((unmask_failed + start_failed))
}

# pk_restore — restore machine-global state and drop this task's rules.
# Safe to call more than once; the drop is scoped to this task's own
# filenames, so an extra call cannot touch another task's rules.
#
# Locked, because unmasking, reloading the daemon and restarting polkitd
# restore state for the whole host — the mirror image of
# pk_gate_unreachable. If a prior run held the lock without releasing it
# (crash), flock will time out rather than let two tasks restart polkitd
# concurrently.
pk_restore() {
  pk_rule_drop || return 1
  pk_lock_acquire || return 1
  PK_LOCK_HELD=1

  pk_restore_state
  local rc=$?
  pk_lock_release
  return "$rc"
}

# pk_init — fail fast when the kit cannot do its job.
pk_init() {
  local missing=""
  command -v "$PK_PKCHECK" >/dev/null || missing="$missing pkcheck"
  case "$PK_SUDO" in
    direct|auto) : ;;
    *) command -v sudo >/dev/null || missing="$missing sudo" ;;
  esac
  if [ -n "$missing" ]; then
    echo "pk_init: missing tools:$missing" >&2
    return 1
  fi
  # Deriving PK_RULE_NAME here means every later call agrees on the same
  # namespace, and a malformed id fails before any file is written.
  pk_rule_name >/dev/null || return 1
  return 0
}

# Direct invocation runs the kit against itself, so it is usable and
# verifiable outside a scenario:
#   qa/pkkit.sh write   yes|no [action]  — write only, print the rendered file
#   qa/pkkit.sh check   yes|no [action]  — write and gate on it
#   qa/pkkit.sh probe            [action] — print raw + normalised oracle
#   qa/pkkit.sh drop | restore
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  CMD="${1:-help}"
  ARG="${2:-}"
  ACTION="${3:-$PK_ACTION}"
  case "$CMD" in
    write)
      if pk_rule_write "$ARG" "$ACTION"; then
        pk_priv cat "$PK_RULE_DIR/${PK_RULE_ORDER}-$(pk_rule_name)-$ARG-$ACTION.rules"
      fi
      ;;
    check|gate) pk_gate "$ARG" "$ACTION" ;;
    probe)
      ACTION="${ARG:-$PK_ACTION}"
      echo "raw:    $(pk_check_raw "$ACTION")"
      echo "normal: $(pk_check_says "$ACTION")"
      ;;
    drop)    pk_rule_drop ;;
    restore) pk_restore ;;
    help|*)
      echo "usage: qa/pkkit.sh {write|check|probe|drop|restore} [yes|no] [action-id]"
      echo "   env: PK_ACTION PK_RULE_DIR PK_RULE_NAME PK_RULE_ORDER PK_ACTIVATE PK_SUDO"
      echo "        PK_PKCHECK PK_SUBJECT_PID PK_TIMEOUT PK_POLL"
      echo "        PK_LOCK_DIR PK_LOCK_FILE PK_LOCK_TIMEOUT"
      echo "   PK_RULE_NAME must be unique per concurrent task (default: task id)."
      echo "   PK_RULE_ORDER is a LOW number: polkitd sorts rules.d and the first"
      echo "   match wins, so a low prefix beats a leftover 60-verity-* rule."
      ;;
  esac
fi

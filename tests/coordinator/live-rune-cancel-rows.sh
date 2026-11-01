#!/usr/bin/env bash
# live-rune-cancel-rows.sh — `cancel.rn`'s two *held* branches, with real children (rows 20a).
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-cancel-rows.sh
#
# WHY. Core refuses a cancellation once a release has been attempted, so the two answers this file
# witnesses can only happen at a **held** child: `observed` (the exit was seen) and `unobserved` (the
# intent is durable and the exit was not seen). While every door a plugin could reach ran hold and
# release in one act, no plugin could stand at that child, and both branches were unwitnessed.
#
# What it witnesses, and each is a way the port can be wrong:
#   R1  a cancel of a held child observes its exit: the log's word is `exited`;
#   R2  and that word reaches the **ledger row**, not just the prose — the half a text check cannot
#       prove, because the row is what `coordinator_release` reads;
#   R3  the release guard then passes for a worker whose exit was observed;
#   R4  a durable cancellation intent whose exit was **not** observed answers `attempted`, claims no
#       stop and no exit, and is refused by `coordinator_release` in the guard's own words;
#   R5  and that intent is on the row as `attempted`, with the operation it belongs to.
#
# THE STOP IS NOT A RACE, and the two halves of that are worth stating because this is the one place
# in the slice where a wrong handle still produces a green line:
#   * the **pid** is the harness's own fact, not a pattern. The child journals its `Held` receipt —
#     `managed-start/v1 {…, "pid": N, "session": …}` — *before* it reports held to the parent, so by
#     the time `custody_hold` answers, the pid is already written down. The watcher reads that pid,
#     then checks `/proc/<pid>` exists, that `/proc/<pid>/cmdline` names the pinned binary and
#     `--managed-operation <op>`, and only then signals. `pgrep -f` is deliberately not used: it
#     matches the fixture's own argv and can answer several pids, and a `SIGSTOP` aimed at the wrong
#     process is a witness that proves nothing.
#   * the **order** is the workflow's, not luck's. `rune-cancel-rows.rn` calls
#     `coordinator_wait_for_stop` between adopting the child and cancelling it, and that tool refuses
#     until the watcher has published its marker — written only after `SIGSTOP` and after
#     `/proc/<pid>/stat` reads back `T`.
#
# Teardown is a separate step, after the checks, and says so: a fixture that left a stopped child
# behind would leak into the next run and would invite a later reader to think the `attempted` word
# was proven by something the teardown did.
#
# Local only: nothing installed, no operator config, no monitor, no compositor. The harness is the
# binary in EIDOLON_BIN and never PATH's — a refusal to find it is a SKIP (3).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-cancel-rows.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi
EID_REAL=$(realpath "$EID")
# The pin, in the output rather than in somebody's message: a witness that cannot say which binary
# it ran is not reproducible.
say_bin() {
    printf 'EIDOLON_BIN=%s\n' "$EID"
    stat -c '  size=%s mtime=%y' "$EID" | sed 's/^/  /'
    sha256sum "$EID" | sed 's/^/  sha256=/'
}

runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime
stopped_pid=""
watcher_pid=""
cleanup() {
    # Last resort only. The ordinary teardown runs after the checks and says so; this exists so an
    # early exit (a refused trust, a missing tool) cannot leave a stopped process behind.
    [ -n "$watcher_pid" ] && kill "$watcher_pid" 2>/dev/null
    if [ -n "$stopped_pid" ] && [ -d "/proc/$stopped_pid" ]; then
        printf 'cleanup: releasing the stopped child %s (early exit)\n' "$stopped_pid"
        kill -CONT "$stopped_pid" 2>/dev/null
        sleep 0.2
        kill -9 "$stopped_pid" 2>/dev/null
    fi
    rm -rf "$runtime" 2>/dev/null || true
}
trap cleanup EXIT

fail=0
checks=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then say "ok   $1"; else
        say "FAIL $1: wanted to find [$3] in the answer"; fail=$((fail + 1)); fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"; fail=$((fail + 1)); else say "ok   $1"; fi
}
check_re() { # check_re <name> <haystack> <regex>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qE -- "$3"; then say "ok   $1"; else
        say "FAIL $1: wanted to match /$3/ in [$2]"; fail=$((fail + 1)); fi
}
field() { # field <json-text> <key> — one top-level scalar out of a JSON object, or ""
    printf '%s' "$1" | python3 -c '
import json,sys
raw=sys.stdin.read()
i=raw.find("{")
if i < 0: print(""); raise SystemExit
try: d=json.loads(raw[i:raw.rfind("}")+1])
except Exception: print(""); raise SystemExit
v=d.get(sys.argv[1])
print("" if v is None else v)
' "$2"
}

say "=== the harness under test ==="
say_bin

# ---- the plugin, copied byte for byte, plus the fixture's own tools -------------------------------
plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/lifecycle-fixture/tools/hold_only.rn" "$here/lifecycle-fixture/tools/dump_record.rn" \
   "$here/lifecycle-fixture/tools/dump_rows.rn" "$here/lifecycle-fixture/tools/wait_for_stop.rn" \
   "$plug/tools/"
python3 - "$plug/plugin.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"hold_only", "dump_record", "dump_rows", "wait_for_stop"'
assert '"hook_peer"]' in s, "the claims list is not where this expects it"
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'wait_for_stop' "$plug/plugin.rn" || { say "FAIL the fixture tools are not registered"; exit 1; }
cp "$here/rune-cancel-rows.rn" "$plug/workflows/rune-cancel-rows.rn"
"$EID" plugins trust coordinator >/dev/null 2>&1 || { say "FAIL the plugin would not vouch"; exit 1; }

# ---- the one policy row a workflow start needs ---------------------------------------------------
#
# A workflow's start goes through the same policy table as a tool call, and the harness answers a
# name the table has not seen with a question — which nothing can answer in an unattended run. The
# harness seeds its own table into this config; the fixture adds **one** row for the one workflow it
# runs. `--yolo` would answer every question in the fixture rather than this one, and is not used.
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-cancel-rows" => v(ALLOW, "fixture: start this one workflow", false),\n'
assert '        _ => {' in s, "no fallthrough arm to put the row in front of"
if 'workflow:coordinator:rune-cancel-rows' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

observed_op='cancel-rows-observed'
unobserved_op='cancel-rows-unobserved'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
stop_marker=$XDG_STATE_HOME/unobserved-stopped
watcher_log=$XDG_STATE_HOME/watcher.log
mkdir -p "$sessions"

# ---- the watcher: the held receipt's own pid, checked, stopped, and read back ---------------------
#
# It polls the child's journal for the `Held` receipt of `unobserved_op` only. A receipt for any
# other operation is not this watcher's business, and the parent session's own journal never carries
# one — the marker is written by the child at its barrier, before it tells the parent it is held.
held_receipt_pid() { # held_receipt_pid <op> — "<pid> <session>" of a verified held child, or ""
    python3 - "$sessions" "$1" "$EID_REAL" <<'PY'
import json, os, sys
root, op, exe = sys.argv[1], sys.argv[2], sys.argv[3]
found = []
try: names = os.listdir(root)
except FileNotFoundError: names = []
for name in names:
    if not name.endswith(".eid"):
        continue
    try: blob = open(os.path.join(root, name), "rb").read()
    except OSError:
        continue
    at = 0
    while True:
        at = blob.find(b"managed-start/v1 {", at)
        if at < 0:
            break
        start = at + len(b"managed-start/v1 ")
        end = blob.find(b"}", start)
        at = start
        if end < 0:
            break
        try: d = json.loads(blob[start:end + 1].decode("utf-8", "replace"))
        except Exception:
            continue
        if d.get("state") != "held" or d.get("operation") != op:
            continue
        found.append((d.get("pid"), d.get("session")))
for pid, session in found:
    if not isinstance(pid, int) or pid <= 0 or not isinstance(session, str) or not session:
        continue
    try: cmdline = open("/proc/%d/cmdline" % pid, "rb").read().split(b"\0")
    except OSError:
        continue
    argv = [c.decode("utf-8", "replace") for c in cmdline if c]
    if not argv:
        continue
    if os.path.realpath(argv[0]) != os.path.realpath(exe):
        continue
    if "--managed-operation" not in argv or op not in argv:
        continue
    if "--managed-start-fd" not in argv:
        continue
    print(pid, session)
    raise SystemExit
print("")
PY
}

stop_state() { # stop_state <pid> — the state letter from /proc/<pid>/stat, or "gone"
    python3 - "$1" <<'PY'
import sys
try: stat = open("/proc/%s/stat" % sys.argv[1]).read()
except OSError: print("gone"); raise SystemExit
# The second field is the executable name in parentheses and may contain spaces.
print(stat.rsplit(")", 1)[-1].split()[0])
PY
}

watch() {
    local deadline=$((SECONDS + 30)) got pid session state
    while [ "$SECONDS" -lt "$deadline" ]; do
        got=$(held_receipt_pid "$unobserved_op")
        if [ -n "$got" ]; then
            pid=${got%% *}; session=${got##* }
            printf 'held receipt for %s: pid=%s session=%s\n' "$unobserved_op" "$pid" "$session" >>"$watcher_log"
            state=$(stop_state "$pid")
            # Only a live child is a child this watcher may stop. Anything else — a state letter it
            # does not recognise, a zombie, a pid that vanished — is reported and not signalled.
            case "$state" in
                R|S|D) ;;
                *)
                    printf 'pid %s was in state [%s] before the stop; not signalling it\n' "$pid" "$state" >>"$watcher_log"
                    continue
                    ;;
            esac
            kill -STOP "$pid" 2>>"$watcher_log" || { printf 'SIGSTOP %s failed\n' "$pid" >>"$watcher_log"; continue; }
            # Read the stop back before claiming it: a signal that was delivered and one that was
            # not look the same from the sender's side.
            local n=0
            while [ "$n" -lt 200 ]; do
                n=$((n + 1))
                state=$(stop_state "$pid")
                if [ "$state" = "T" ] || [ "$state" = "t" ]; then
                    printf 'stopped pid %s (session %s); /proc/%s/stat state=%s after %s reads\n' \
                        "$pid" "$session" "$pid" "$state" "$n" >>"$watcher_log"
                    printf '%s %s\n' "$pid" "$session" >"$stop_marker.tmp" && mv "$stop_marker.tmp" "$stop_marker"
                    return 0
                fi
                sleep 0.005
            done
            printf 'SIGSTOP sent to %s but /proc state is [%s], not T; not claiming a stop\n' "$pid" "$state" >>"$watcher_log"
        fi
        sleep 0.01
    done
    printf 'gave up: no held receipt for %s appeared; the cancel must not be issued\n' "$unobserved_op" >>"$watcher_log"
    printf 'watcher-timeout\n' >"$stop_marker.tmp" && mv "$stop_marker.tmp" "$stop_marker"
    return 1
}

: >"$watcher_log"
watch &
watcher_pid=$!

say "=== driving the two held children ==="
say "     (observed_op=$observed_op, unobserved_op=$unobserved_op, watcher pid $watcher_pid)"
run=$("$EID" workflow run "$plug" rune-cancel-rows \
        --args "$(python3 -c 'import json,sys; print(json.dumps({"observed_op":sys.argv[1],"unobserved_op":sys.argv[2],"stop_marker":sys.argv[3],"workspace":sys.argv[4],"model":"mock-text"}))' \
            "$observed_op" "$unobserved_op" "$stop_marker" "$ws")" \
        --provider mock 2>&1 || true)
# The workflow's `report` is one JSON string; unescape it once and check against the text the tools
# actually answered. Checking the escaped form would be checking the transport's spelling.
rep=$(printf '%s' "$run" | python3 -c '
import json,sys
raw=sys.stdin.read()
cand=[l for l in raw.splitlines() if l.startswith("{\"report\"")]
if not cand:
    print(raw); raise SystemExit
print(json.loads(cand[-1]).get("report",""))
')
printf '%s\n' "$rep" | sed 's/^/     /'

say ""
say "=== the fixture's own observation of the stopped child ==="
sed 's/^/     /' "$watcher_log"

# A run that failed outright would satisfy every absence check below, so the run itself is checked
# first, and each answer is checked for the shape it must have.
check "the workflow run completed" "$run" '"status":"completed"'
first=$(printf '%s\n' "$rep" | sed -n 's/^C0-HELD: //p' | tail -1)
second=$(printf '%s\n' "$rep" | sed -n 's/^C5-HELD: //p' | tail -1)
first_session=$(field "$first" session)
second_session=$(field "$second" session)
check "C0a the first child was held" "$rep" "C0-HELD:"
check_re "C0b and it is a session address" "$first_session" '^[0-9]{6,}$'
check "C5a the second child was held" "$rep" "C5-HELD:"
check_re "C5b and it is a session address" "$second_session" '^[0-9]{6,}$'
check "C0c the first child was adopted through the shipped tool" "$rep" "C1-ADOPT-OK:"
check "C5c the second child was adopted through the shipped tool" "$rep" "C6-ADOPT-OK:"

# ---- row 20a `observed` -------------------------------------------------------------------------
check "R1 the cancel observed the held child's exit" "$rep" "C2-CANCEL-OK: exit observed for \`$observed_op\`"
check "R1b in the log's own outcome word" "$rep" 'outcome word is `exited`,'
record_observed=$(printf '%s\n' "$rep" | sed -n 's/^C3-RECORD: //p' | tail -1)
check "R1c the log's typed word is exited" "$record_observed" '"cancellation":"exited"'
check "R3 the release passes once the exit is observed" "$rep" "C4-RELEASE-OK: released $first_session (epoch 1)"

# ---- row 20a `unobserved` -----------------------------------------------------------------------
check "R4a the fixture said it stopped the child before the cancel" "$rep" "C6b-STOPPED: watcher marker:"
check "R4b the watcher's pid is the child the hold answered" "$(cat "$stop_marker" 2>/dev/null)" "$second_session"
check "R4c the cancel recorded a durable intent" "$rep" "C7-CANCEL-ERR: cancellation of \`$unobserved_op\` recorded a durable intent"
check "R4d and says the exit was not observed" "$rep" "the exit was **not** observed"
check "R4e and that its own outcome word is attempted" "$rep" 'outcome word is `attempted`, not `exited`'
check "R4f and that nothing may be reported as stopped" "$rep" "nothing here may be reported as stopped"
check_absent "R4g and it did not answer as an observed exit" "$rep" "C7-CANCEL-OK"
check_absent "R4h and no stop was claimed anywhere in the report" "$rep" "A stop may be reported"
record_unobserved=$(printf '%s\n' "$rep" | sed -n 's/^C8-RECORD: //p' | tail -1)
check "R4i the log's typed word is attempted" "$record_unobserved" '"cancellation":"attempted"'
check "R5a the release refuses that worker in the guard's own words" "$rep" "C9-RELEASE-ERR: the exit of $second_session was never observed"
check "R5b and names the operation it belongs to" "$rep" "operation \`$unobserved_op\`"
check_absent "R5c and nothing released that worker" "$rep" "C9-RELEASE-OK"

# ---- the ledger, as data ------------------------------------------------------------------------
rows=$(printf '%s\n' "$rep" | sed -n 's/^C10-ROWS: //p' | tail -1)
facts=$(python3 - "$rows" "$first_session" "$second_session" "$unobserved_op" <<'PYROWS'
import json, sys
raw, first, second, op = sys.argv[1:5]
try:
    rows = json.loads(raw)
except Exception:
    print("rows-parsed=no"); raise SystemExit
mine = [r for r in rows if isinstance(r, dict) and r.get("worker") == first]
theirs = [r for r in rows if isinstance(r, dict) and r.get("worker") == second]
print("rows-parsed=yes")
print("all-rows=%d" % len([r for r in rows if isinstance(r, dict)]))
print("observed-rows=%d" % len(mine))
print("observed-cancellation=%s" % (mine[0].get("cancellation") if mine else "none"))
print("observed-released=%s" % (mine[0].get("released") if mine else "none"))
print("unobserved-rows=%d" % len(theirs))
print("unobserved-cancellation=%s" % (theirs[0].get("cancellation") if theirs else "none"))
print("unobserved-op=%s" % (theirs[0].get("cancel_operation") if theirs else "none"))
print("unobserved-released=%s" % (theirs[0].get("released") if theirs else "none"))
PYROWS
)
check "R2 the ledger rows were read back as data" "$facts" "rows-parsed=yes"
check "R2b one row for the observed child" "$facts" "observed-rows=1"
check "R2c and it is marked exited" "$facts" "observed-cancellation=exited"
check "R2d and it was released" "$facts" "observed-released=True"
check "R5d one row for the unobserved child" "$facts" "unobserved-rows=1"
check "R5e and it is marked attempted" "$facts" "unobserved-cancellation=attempted"
check "R5f naming the operation that recorded it" "$facts" "unobserved-op=$unobserved_op"
check "R5g and it was not released" "$facts" "unobserved-released=none"

# ---- the log's own bytes, where the red above comes from ----------------------------------------
#
# Everything R4 asked for is what the *tree is supposed to do*. What it does instead is visible in
# the parent session's own journal: the cancellation row core persisted carries an evidence string
# that says the exit was NOT observed, while the typed word `custody_inspect` answers is `exited` —
# and `exited` is the one word the plugin is required to trust (`cancel.rn`'s own rule). These two
# checks are here so the contradiction is printed rather than inferred from a reader's diff.
parent=$(printf '%s' "$rows" | python3 -c '
import json,sys
try: rows=json.load(sys.stdin)
except Exception: print(""); raise SystemExit
print(next((r.get("coordinator") for r in rows if isinstance(r,dict) and r.get("coordinator")), ""))
')
journal=$sessions/$parent.eid
evidence=$(strings -a "$journal" 2>/dev/null | grep -o 'exit unconfirmed[^"]*' | head -1)
check "R4j the persisted evidence says the exit was not observed" "$evidence" "it was not observed within"
check "R4k while the typed word the plugin must trust said exited" "$record_unobserved" '"cancellation":"exited"'
check "R4l and the host's own inspection still reports the child held" "$record_unobserved" '"observed":"held"'

# ---- teardown: not observation ------------------------------------------------------------------
#
# Everything above is the evidence. What follows is cleanup, and it is deliberately after the checks
# and in its own section, so no reader can think the `attempted` word depended on it.
say ""
say "=== teardown, not observation ==="
stopped_pid=$(cut -d' ' -f1 "$stop_marker" 2>/dev/null || true)
case "$stopped_pid" in ''|*[!0-9]*) stopped_pid="";; esac
if [ -n "$stopped_pid" ] && [ -d "/proc/$stopped_pid" ]; then
    say "     continuing the stopped child $stopped_pid, then letting it end"
    kill -CONT "$stopped_pid" 2>/dev/null || true
    n=0
    while [ "$n" -lt 100 ] && [ -d "/proc/$stopped_pid" ]; do n=$((n + 1)); sleep 0.05; done
    if [ -d "/proc/$stopped_pid" ]; then
        say "     it did not end on its own after SIGCONT; sending TERM"
        kill -TERM "$stopped_pid" 2>/dev/null || true
        n=0
        while [ "$n" -lt 100 ] && [ -d "/proc/$stopped_pid" ]; do n=$((n + 1)); sleep 0.05; done
    fi
    if [ -d "/proc/$stopped_pid" ]; then
        say "     it did not end after TERM either; sending KILL"
        kill -9 "$stopped_pid" 2>/dev/null || true
        sleep 0.2
    fi
    if [ -d "/proc/$stopped_pid" ]; then
        say "FAIL teardown: the stopped child $stopped_pid is still present"
        fail=$((fail + 1)); checks=$((checks + 1))
    else
        say "ok   teardown: the stopped child $stopped_pid is gone"
        checks=$((checks + 1))
    fi
    stopped_pid=""
else
    say "     no stopped child to clean up (the watcher never claimed one)"
fi
kill "$watcher_pid" 2>/dev/null || true

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-cancel-rows complete: $checks checks, 0 FAIL"
else
    say "live-rune-cancel-rows FAILED: $fail of $checks checks"
    exit 1
fi

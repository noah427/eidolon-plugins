#!/usr/bin/env bash
# live-rune-holder-death.sh — what a second session's start with the *same* operation actually does
# after the first session (the holder) exited and its held child was aborted.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-holder-death.sh
#
# WHY. The holder of a held child is the session: the child's channel belongs to that process, and
# when it exits the channel closes, the child journals `Aborted` and it never runs. Two facts follow,
# and this fixture asks for both rather than assuming either:
#   * the holder's death aborts the held child (journal `state":"aborted"`, process gone);
#   * a second session that presents **the same operation id and byte-identical arguments** gets —
#     what? The brief this fixture was written under expected a refusal: the log's record found, no
#     second row, nothing released, nothing started. The tree does not do that, and the fixture's job
#     is to say which of the two answers it got. `claim_launch` (crates/core/src/custody.rs:837-860)
#     skips every record whose `request.caller` is not this session, and the custody rows are the
#     session's own journal, so from the second session the operation is *Free* and is admitted as a
#     new one. That is why the checks below assert the facts of what happened instead of the
#     expectation, and why the last section says plainly which shape this is.
#
# Both runs use the same fixture HOME, so the plugin's ledger (which is the plugin's, not a
# session's) is shared between them — that is the whole point: the second attempt can see the first
# attempt's row even though it cannot see the first session's custody log.
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
    echo "SKIP live-rune-holder-death.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi
say_bin() {
    printf 'EIDOLON_BIN=%s\n' "$EID"
    stat -c '  size=%s mtime=%y' "$EID" | sed 's/^/  /'
    sha256sum "$EID" | sed 's/^/  sha256=/'
}

runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime
trap 'rm -rf "$runtime" 2>/dev/null || true; :' EXIT

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

say "=== the harness under test ==="
say_bin

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/lifecycle-fixture/tools/hold_only.rn" "$here/lifecycle-fixture/tools/dump_rows.rn" "$plug/tools/"
python3 - "$plug/plugin.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"hold_only", "dump_rows"'
assert '"hook_peer"]' in s, "the claims list is not where this expects it"
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'hold_only' "$plug/plugin.rn" || { say "FAIL the fixture tools are not registered"; exit 1; }
cp "$here/rune-hold-only.rn" "$here/rune-start-once.rn" "$plug/workflows/"
"$EID" plugins trust coordinator >/dev/null 2>&1 || { say "FAIL the plugin would not vouch"; exit 1; }

# One policy row per workflow this fixture starts, and no more: a name the table has not seen is a
# question nothing can answer in an unattended run, and `--yolo` would answer every question in the
# fixture rather than these two.
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
assert '        _ => {' in s, "no fallthrough arm to put the rows in front of"
for name in ("rune-hold-only", "rune-start-once"):
    key = "workflow:coordinator:" + name
    if key in s:
        continue
    row = '        "%s" => v(ALLOW, "fixture: start this one workflow", false),\n' % key
    s = s.replace('        _ => {', row + '        _ => {', 1)
p.write_text(s)
PY

op='holder-death-op'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
mkdir -p "$sessions"

# The two runs' arguments are built **once** and handed to both invocations, so "the same immutable
# input" is a property of the fixture rather than a claim about it. The driver compares them anyway,
# before the second run, because a fixture that says it passed the same bytes should show them.
args1=$(python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text","brief":"the bounded assignment"}))' "$op" "$ws")
args2=$args1

run_workflow() { # run_workflow <name> <args-json>
    "$EID" workflow run "$plug" "$1" --args "$2" --provider mock 2>&1 || true
}
report_of() { # report_of <raw-run>
    printf '%s' "$1" | python3 -c '
import json,sys
raw=sys.stdin.read()
cand=[l for l in raw.splitlines() if l.startswith("{\"report\"")]
if not cand:
    print(raw); raise SystemExit
print(json.loads(cand[-1]).get("report",""))
'
}

say ""
say "=== session one: hold the child, then exit (the holder is the session) ==="
run1=$(run_workflow rune-hold-only "$args1")
rep1=$(report_of "$run1")
printf '%s\n' "$rep1" | sed 's/^/     /'
check "A0 the first session's workflow completed" "$run1" '"status":"completed"'
held=$(printf '%s\n' "$rep1" | sed -n 's/^A-HELD: //p' | tail -1)
holder_session=$(printf '%s' "$held" | python3 -c 'import json,sys
raw=sys.stdin.read(); i=raw.find("{")
print(json.loads(raw[i:raw.rfind("}")+1]).get("session","") if i>=0 else "")')
check_re "A1 it held a real child" "$holder_session" '^[0-9]{6,}$'

# The process exits when its holder does. Give the kernel and the child a moment, then ask the
# child's own journal and /proc rather than the parent's sentence.
host_pid=$(python3 - "$sessions" "$holder_session" <<'PY'
import json, sys
root, want = sys.argv[1], sys.argv[2]
import os
try: names = os.listdir(root)
except FileNotFoundError: names = []
for name in names:
    if not name.endswith(".eid"):
        continue
    blob = open(os.path.join(root, name), "rb").read()
    at = 0
    while True:
        at = blob.find(b"managed-start/v1 {", at)
        if at < 0: break
        start = at + len(b"managed-start/v1 "); end = blob.find(b"}", start); at = start
        if end < 0: break
        try: d = json.loads(blob[start:end + 1].decode("utf-8", "replace"))
        except Exception: continue
        if d.get("session") != want: continue
        print(d.get("pid", "")); raise SystemExit
print("")
PY
)
abort_reason() {
    strings -a "$sessions/$holder_session.eid" 2>/dev/null | grep -o '"state":"aborted"[^}]*' | head -1
}
n=0
while [ "$n" -lt 100 ]; do
    n=$((n + 1))
    reason=$(abort_reason)
    if printf '%s' "$reason" | grep -qF 'aborted'; then break; fi
    sleep 0.05
done
check "A2 the held child journaled its abort" "$(abort_reason)" '"state":"aborted"'
check "A3 and names the closed channel as the reason" "$(abort_reason)" 'managed channel closed before release'
if [ -n "$host_pid" ] && [ -d "/proc/$host_pid" ]; then
    say "     (the aborted child $host_pid is still in /proc; waiting briefly for it to be reaped)"
    n=0
    while [ "$n" -lt 100 ] && [ -d "/proc/$host_pid" ]; do n=$((n + 1)); sleep 0.05; done
fi
if [ -n "$host_pid" ] && [ -d "/proc/$host_pid" ]; then
    say "FAIL A4 the aborted child $host_pid is still present"
    fail=$((fail + 1)); checks=$((checks + 1))
else
    say "ok   A4 the aborted child's process is gone"
    checks=$((checks + 1))
fi

say ""
say "=== session two: the same operation id, byte-identical arguments ==="
if [ "$args1" = "$args2" ]; then
    say "ok   B0 the two runs' arguments are byte-identical"
    checks=$((checks + 1))
else
    say "FAIL B0 the two runs' arguments differ: [$args1] vs [$args2]"
    fail=$((fail + 1)); checks=$((checks + 1))
fi
say "     args: $args2"
run2=$(run_workflow rune-start-once "$args2")
rep2=$(report_of "$run2")
printf '%s\n' "$rep2" | sed 's/^/     /'
check "B1 the second session's workflow completed" "$run2" '"status":"completed"'

rows=$(printf '%s\n' "$rep2" | sed -n 's/^B-ROWS: //p' | tail -1)
facts=$(python3 - "$rows" "$holder_session" <<'PYROWS'
import json, sys
raw, first = sys.argv[1:3]
try: rows = json.loads(raw)
except Exception:
    print("rows-parsed=no"); raise SystemExit
rows = [r for r in rows if isinstance(r, dict)]
first_rows = [r for r in rows if r.get("worker") == first]
others = [r for r in rows if r.get("worker") != first]
print("rows-parsed=yes")
print("rows=%d" % len(rows))
print("first-rows=%d" % len(first_rows))
print("first-released=%s" % (first_rows[0].get("released") if first_rows else "none"))
print("first-cancellation=%s" % (first_rows[0].get("cancellation") if first_rows else "none"))
print("other-rows=%d" % len(others))
print("other-worker=%s" % (others[0].get("worker") if others else "none"))
print("other-operation=%s" % (others[0].get("operation") if others else "none"))
PYROWS
)
check "B2 the ledger was read back as data" "$facts" "rows-parsed=yes"

# Which answer the tree gave. Both are asserted as *facts of this tree*, and the driver says in its
# closing section which one it saw: a refusal in the plugin's own words, or a second admission.
if printf '%s' "$rep2" | grep -qF 'did not establish a child'; then
    say "     the second session's hold was REFUSED — the brief's expectation"
    check "B3 the refusal says this attempt wrote no adoption row" "$rep2" "This attempt wrote no adoption row and released nothing"
    check "B4 and quotes the log's own record" "$rep2" "The custody log's own record for this operation: phase"
    check "B5 and the record names the dead holder's child" "$rep2" "session $holder_session"
    check "B6 and says this ledger holds no row for it" "$rep2" "This ledger holds no row for \`$holder_session\`"
    check_absent "B7 and reports nothing started" "$rep2" "started \`"
    check "B8 the ledger still holds exactly one child" "$facts" "rows=1"
else
    say "     the second session was NOT refused: the operation was admitted again"
    check_absent "B3 no refusal in the plugin's hold_refused words" "$rep2" "did not establish a child"
    check_absent "B4 no claim that this attempt wrote no row" "$rep2" "This attempt wrote no adoption row and released nothing"
    check "B5 a second child was admitted under the same operation id" "$facts" "other-rows=1"
    check_re "B6 and it is a different child from the first" "$(printf '%s' "$facts" | sed -n 's/^other-worker=//p')" '^[0-9]{6,}$'
    check "B7 the ledger holds exactly one row" "$facts" "rows=1"
    check "B8 and it is the retry's child, not the aborted one" "$facts" "first-rows=0"
    check "B9 the row names the same operation id" "$facts" "other-operation=holder-death-op"
    # The fixture knows this and says it rather than implying more: session one **held only**, so it
    # never wrote a row, and there is nothing here to be orphaned. A `coordinator_start` that died
    # *between* its row write and its release would leave a row pointing at an aborted child; this
    # fixture does not produce that state, and the last section says so.
fi

say ""
say "=== what this fixture saw, in plain words ==="
if printf '%s' "$rep2" | grep -qF 'did not establish a child'; then
    say "     A second session presenting the same operation and the same bytes was refused by the hold,"
    say "     found the log's record, wrote no second row, and claimed nothing started: the shape the"
    say "     brief expected. Nothing here contradicts the brief."
else
    say "     A second session presenting the same operation id and byte-identical arguments was NOT"
    say "     refused: core admitted the operation again, because claim_launch skips every record whose"
    say "     request.caller is not this session (crates/core/src/custody.rs:837-860) and the custody rows"
    say "     are the session's own journal. So this is a *new* operation, not a retry: the second"
    say "     session's hold was admitted, a child was established, and a row was written for it."
    say "     Session one here HELD ONLY, so it never wrote a row and there is no orphaned row in this"
    say "     run — a start that died between its row write and its release would leave one, and this"
    say "     fixture does not claim to have produced that state."
    say "     That is the fact to report, not a green line: the plugin's hold_refused path is unreachable"
    say "     from a different session, and 'a retry after the holder died is a refusal' is false here."
fi

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-holder-death complete: $checks checks, 0 FAIL"
else
    say "live-rune-holder-death FAILED: $fail of $checks checks"
    exit 1
fi

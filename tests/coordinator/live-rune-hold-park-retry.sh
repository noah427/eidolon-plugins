#!/usr/bin/env bash
# live-rune-hold-park-retry.sh — contract witness 2: the retry after a holder's death, issued from
# **the same session's resumed log**.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-hold-park-retry.sh
#
# Every directory it needs is created here; one that cannot be made or written stops the run by name
# (exit 2) before the plugin is copied, before the workflow is driven and before any check is counted.
#
# WHY. `live-rune-holder-death.sh` shows what a **different** session gets for the same operation: a
# fresh admission, because `claim_launch` filters on `request.caller` and the custody rows live in the
# holder's own journal. The lane's other sentence is about the same session — "a retry after that death
# is a refusal that claims nothing" — and nothing had run it, because the shape needs a log that is
# resumed. So this file holds a child, **parks**, and lets the holder die: the run exits parked, the
# held child's channel closes and the child aborts with that reason in its own journal. Then
# `workflow resume` on the same log re-enters past the park and issues the retry through the shipped
# `coordinator_start`.
#
# Steps are memoized by the run's own step id, so the resumed pass **replays** the hold: the report
# repeats the held line, and that repetition is a replay, not a second hold. What proves it is the
# record and the row count, which the driver reads rather than infers.
#
# What it witnesses:
#   A1  the holder died at the park and the run is `parked`, not completed;
#   A2  the held child journaled its abort, with the closed channel as the reason;
#   A3  and its process is gone;
#   B1  the resumed pass is the **same session's** log — the same path, the same run id;
#   B2  the retry is refused, and says it did not establish a child;
#   B3  and names the reason the tree gives: a bound child is not established, never a replacement;
#   B4  and says it wrote no adoption row and released nothing;
#   B5  the custody record's own read-back in that sentence is `phase bound`, the same child, observed
#       `unknown` — the log's facts, not the plugin's claim;
#   B6  `custody_inspect` after the retry still says `phase bound`, the same child, observed `unknown`;
#   B7  the ledger is still empty: no row was written for a child that never ran;
#   B8  the number of custody rows in the run's journal is the same before and after the retry;
#   B9  no second child was spawned: still one log for the run and one for the child.
#
# Local only: nothing installed, no operator config, no monitor, no compositor, headless children. The
# harness is the binary in EIDOLON_BIN and never PATH's — a refusal to find it is a SKIP (3). No
# `--yolo`, one policy row for the one workflow, and no gate is bypassed.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-hold-park-retry.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi

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

runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime
cleanup() {
    if [ -n "${host_pid:-}" ] && [ -d "/proc/$host_pid" ]; then
        printf 'teardown (early exit): stopping child %s\n' "$host_pid"
        kill -TERM "$host_pid" 2>/dev/null; sleep 0.2; kill -9 "$host_pid" 2>/dev/null
    fi
    rm -rf "$runtime" 2>/dev/null || true
}
trap cleanup EXIT

say "=== the harness under test ==="
say_bin() {
    printf 'EIDOLON_BIN=%s\n' "$EID"
    stat -c '  size=%s mtime=%y' "$EID" | sed 's/^/  /'
    sha256sum "$EID" | sed 's/^/  sha256=/'
}
say_bin
want=${EIDOLON_PIN_SHA256:-aa5ad25d5038f0bf76a82d731d14d012a2cb5cc82a695b5636f6f3876d8d8454}
got=$(sha256sum "$EID" 2>/dev/null | cut -d' ' -f1)
if [ "$got" != "$want" ]; then
    printf 'refusing: the binary at %s is not the pinned build\n' "$EID" >&2
    printf '  expected sha256 %s, read %s\n' "$want" "${got:-nothing}" >&2
    printf '  nothing was run and no check was counted; set EIDOLON_PIN_SHA256 to witness a different build knowingly.\n' >&2
    exit 2
fi
say "  pin: matches the recorded sha256 for core 537d999"

# ---- setup: every step guarded by name, none of them able to wear a FAIL -------------------------
setup_refuse() { # setup_refuse <what> <reason>
    printf 'refusing: %s %s\n' "$1" "$2" >&2
    printf '  nothing was run and no check was counted; this is the fixture'"'"'s setup, not the port.\n' >&2
    printf '  the documented invocation is: t=$(mktemp -d); HOME=$t XDG_CONFIG_HOME=$t/cfg \\\n' >&2
    printf '    XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data EIDOLON_BIN=/path/to/eidolon bash %s\n' "$0" >&2
    exit 2
}
for d in "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_DATA_HOME"; do
    mkdir -p "$d" 2>/dev/null
    [ -d "$d" ] || setup_refuse "$d" "does not exist and \`mkdir -p\` could not create it"
    if ! : >"$d/.fixture-writable" 2>/dev/null; then
        setup_refuse "$d" "exists but is not writable (this fixture writes the plugin, the ledger and journals there)"
    fi
    rm -f "$d/.fixture-writable"
done
say "setup: config=$XDG_CONFIG_HOME state=$XDG_STATE_HOME data=$XDG_DATA_HOME (created where absent)"

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows" 2>/dev/null
[ -d "$plug/workflows" ] || setup_refuse "$plug/workflows" "could not be created"
cp -r "$repo/coordinator/." "$plug/" 2>/dev/null
[ -f "$plug/plugin.rn" ] || setup_refuse "$plug" "the plugin was not copied into the fixture config"
cp "$here/lifecycle-fixture/tools/hold_only.rn" "$here/lifecycle-fixture/tools/dump_record.rn" \
   "$here/lifecycle-fixture/tools/dump_rows.rn" "$plug/tools/" 2>/dev/null
for t in hold_only dump_record dump_rows; do
    [ -f "$plug/tools/$t.rn" ] || setup_refuse "$plug/tools/$t.rn" "the fixture tool was not copied"
done
python3 - "$plug/plugin.rn" <<'PY' || setup_refuse "$plug/plugin.rn" "the fixture tools could not be added to the claims list"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"hold_only", "dump_record", "dump_rows"'
if '"hook_peer"]' not in s:
    raise SystemExit("the claims list is not where this expects it")
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'hold_only' "$plug/plugin.rn" || setup_refuse "$plug/plugin.rn" "the fixture tools are not registered"
cp "$here/rune-hold-park-retry.rn" "$plug/workflows/rune-hold-park-retry.rn" 2>/dev/null
[ -f "$plug/workflows/rune-hold-park-retry.rn" ] || setup_refuse "$plug/workflows" "the workflow was not copied"
if ! trust_out=$("$EID" plugins trust coordinator 2>&1); then
    printf '%s\n' "$trust_out" | sed 's/^/  /' >&2
    setup_refuse "plugins trust coordinator" "the plugin would not vouch"
fi
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY' || setup_refuse "$XDG_CONFIG_HOME/eidolon/policy.rn" "the policy row could not be seeded"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-hold-park-retry" => v(ALLOW, "fixture: start this one workflow", false),\n'
if '        _ => {' not in s:
    raise SystemExit("no fallthrough arm to put the row in front of")
if 'workflow:coordinator:rune-hold-park-retry' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

op='holder-death-resume-op'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
mkdir -p "$sessions" 2>/dev/null
[ -d "$sessions" ] || setup_refuse "$sessions" "the sessions directory could not be created"

args=$(python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text","brief":"the bounded assignment for the holder-death resume"}))' "$op" "$ws")
line_of() { # line_of <raw-run> — the machine line the door printed
    # The object's keys are serialized in sorted order, so the line does not begin with any one of them.
    # It is found by being a JSON object that carries `status`, which is what the door promises.
    printf '%s' "$1" | python3 -c '
import json,sys
raw=sys.stdin.read()
for line in reversed(raw.splitlines()):
    if not line.startswith("{"): continue
    try: d=json.loads(line)
    except Exception: continue
    if isinstance(d, dict) and "status" in d:
        print(line); raise SystemExit
print("{}")
'
}
field_of() { # field_of <json> <key>
    python3 -c 'import json,sys
try: d=json.loads(sys.argv[1])
except Exception: print(""); raise SystemExit
v=d.get(sys.argv[2])
print("" if v is None else v)' "$1" "$2"
}

# ---- pass one: hold, then park — which is where the holder stops being alive ----------------------
say ""
say "=== session one: hold a child, then park (the holder exits here) ==="
run1=$("$EID" workflow run "$plug" rune-hold-park-retry --args "$args" --provider mock 2>&1)
rc1=$?
line1=$(line_of "$run1")
say "     the door's line: $line1"
say "     exit code: $rc1"
if [ "$line1" = "{}" ]; then
    say "     the door printed no machine line; its whole output was:"
    printf '%s\n' "$run1" | sed 's/^/     /'
fi
run_session_path=$(field_of "$line1" "session")
run_id=$(field_of "$line1" "run")
check "A1 the first pass parked rather than completing" "$(field_of "$line1" "status")" "parked"
check_re "A1b and the line names the session the run lives in" "$run_session_path" '\.eid$'
check_re "A1c and the run id" "$run_id" '^[0-9]+$'
run_log=$run_session_path
[ -f "$run_log" ] || run_log=$sessions/$(basename "$run_session_path")
check "A1d the run's journal is on disk" "$([ -f "$run_log" ] && echo present || echo absent)" "present"

# ---- the held child, from its own receipt --------------------------------------------------------
say ""
say "=== the held child, from its own receipt ==="
receipts() { # receipts <dir> — every managed-start receipt as one JSON object per line
    python3 - "$1" <<'PY'
import json, os, sys
root = sys.argv[1]
try: names = sorted(os.listdir(root))
except FileNotFoundError: names = []
for name in names:
    if not name.endswith(".eid"): continue
    try: blob = open(os.path.join(root, name), "rb").read()
    except OSError: continue
    at = 0
    while True:
        at = blob.find(b"managed-start/v1 {", at)
        if at < 0: break
        start = at + len(b"managed-start/v1 "); end = blob.find(b"}", start); at = start
        if end < 0: break
        try: d = json.loads(blob[start:end + 1].decode("utf-8", "replace"))
        except Exception: continue
        print(json.dumps(d))
PY
}
held=$(receipts "$sessions" | python3 -c '
import json,sys
for line in sys.stdin:
    try: d=json.loads(line)
    except Exception: continue
    if d.get("operation")==sys.argv[1] and d.get("state")=="held":
        print(line.strip()); break
' "$op")
child_session=$(field_of "$held" "session")
host_pid=$(field_of "$held" "pid")
check_re "A2 the hold is recorded by the child itself, with an operation and a session" "$held" '"state": "held"'
check_re "A2b and names a real child" "$child_session" '^[0-9]{6,}$'
check_re "A2c and the pid it was given" "$host_pid" '^[0-9]+$'

# ---- the holder's death ---------------------------------------------------------------------------
abort_of() {
    receipts "$sessions" | python3 -c '
import json,sys
for line in sys.stdin:
    try: d=json.loads(line)
    except Exception: continue
    if d.get("operation")==sys.argv[1] and d.get("state")=="aborted":
        print(line.strip()); break
' "$op"
}
n=0
while [ "$n" -lt 200 ]; do
    n=$((n + 1))
    aborted=$(abort_of)
    [ -n "$aborted" ] && break
    sleep 0.05
done
check "A3 the held child journaled its abort" "$(printf '%s' "$aborted" | grep -c '"state": "aborted"')" "1"
check "A3b and names the closed channel as the reason" "$aborted" "managed channel closed before release"
if [ -d "/proc/$host_pid" ]; then
    n=0
    while [ "$n" -lt 100 ] && [ -d "/proc/$host_pid" ]; do n=$((n + 1)); sleep 0.05; done
fi
if [ -d "/proc/$host_pid" ]; then
    checks=$((checks + 1)); say "FAIL A4 the aborted child $host_pid is still present"; fail=$((fail + 1))
else
    checks=$((checks + 1)); say "ok   A4 the aborted child's process is gone"
fi

# The custody rows the holder wrote, counted from the renderer's own lines. The resumed pass appends
# tool results of its own, so counting *all* records would prove nothing; only these lines do.
rows_before=$("$EID" log "$run_log" 2>/dev/null | grep -cF -- "custody launch \`$op\`" || true)
say "     custody rows in the run's journal before the retry: $rows_before"
check_re "A5 the hold wrote custody rows the journal renders" "$rows_before" '^[1-9][0-9]*$'

# ---- pass two: the same session's log, resumed ----------------------------------------------------
say ""
say "=== the same session's log, resumed: the retry ==="
run2=$("$EID" workflow resume "$run_log" "$run_id" --plugin "$plug" --answer "retry after the holder died" --provider mock 2>&1)
line2=$(line_of "$run2")
rep2=$(field_of "$line2" "report")
say "     the door's line: $(printf '%s' "$line2" | cut -c1-200)"
if [ "$line2" = "{}" ]; then
    say "     the door printed no machine line; its whole output was:"
    printf '%s\n' "$run2" | sed 's/^/     /'
fi
printf '%s\n' "$rep2" | sed 's/^/     /'
check "B1 the resumed pass completed" "$(field_of "$line2" "status")" "completed"
check "B1b in the same session's log" "$(field_of "$line2" "session")" "$run_session_path"
check "B1c under the same run id" "$(field_of "$line2" "run")" "$run_id"

check "B2 the retry was refused" "$rep2" "R1-ERR: the hold for \`$op\` did not establish a child"
# Which arm of `the_bound_child` the retry reaches is a fact about the tree, not the lane's
# expectation, and the two arms are different sentences about the same refusal. On a **restarted** host
# `hold` registers the operation before it inspects, so the host answers `NotCreated` for a launch it
# never attempted, and a record whose phase is `Bound` meets the disagreement arm. `LaunchState`'s own
# doc says a restart lands on `Unknown` "and it is not `NotCreated`"; this tree says the other word.
# Both arms refuse and neither spawns, so the safety this witness needs is the refusal — the vocabulary
# is carried as a finding rather than fixed by reordering a production guard in a closing lane.
check "B3 the retry reaches the disagreement arm and refuses to respawn" "$rep2" "store binds a created child but host says never created; state disagreement needs repair, not respawn"
check "B4 and says it wrote no adoption row and released nothing" "$rep2" "This attempt wrote no adoption row and released nothing"
check "B5 and re-reads the record as phase bound, the same child" "$rep2" "phase bound, session $child_session"
check "B5b and quotes the host's observation rather than adopting it" "$rep2" "its current observation, quoted and not adopted as a claim by this plugin: not_created"
check_absent "B5c and never claims the child is held" "$rep2" "is still held"
check_absent "B5d and never reports a start" "$rep2" "R1-OK:"

record2=$(printf '%s\n' "$rep2" | sed -n 's/^R2-RECORD: //p' | tail -1)
check "B6 the record after the retry is still phase bound" "$record2" '"phase":"bound"'
check "B6b and still names the same child" "$record2" "\"session\":\"$child_session\""
check "B6c and the host's own word for the child it cannot see" "$record2" '"observed":"not_created"'
rows2=$(printf '%s\n' "$rep2" | sed -n 's/^R3-ROWS: //p' | tail -1)
check "B7 the ledger is still empty: no row for a child that never ran" "$rows2" "[]"

rows_after=$("$EID" log "$run_log" 2>/dev/null | grep -cF -- "custody launch \`$op\`" || true)
say "     custody rows in the run's journal after the retry: $rows_after"
check "B8 the retry wrote no custody row" "$rows_after" "$rows_before"
# And the resumed pass **replayed** the hold rather than issuing a second one: the run's step 0 appears
# once in the journal, though the program executed it twice (once per pass).
steps0=$("$EID" log "$run_log" 2>/dev/null | grep -cF -- "step 0 [w$run_id-0]" || true)
say "     journaled executions of step 0: $steps0"
check "B8b the hold was replayed, not issued again" "$steps0" "1"

logs=$(find "$sessions" -maxdepth 1 -name '*.eid' 2>/dev/null | wc -l)
say "     session logs: ${logs} (the run and the child)"
check "B9 no second child was spawned: two logs, exactly" "$logs" "2"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-hold-park-retry complete: $checks checks, 0 FAIL"
else
    say "live-rune-hold-park-retry FAILED: $fail of $checks checks"
    exit 1
fi

#!/usr/bin/env bash
# live-rune-public-start-retry.sh — contract witness 1: the **public** `coordinator_start` repeated
# after a start whose barrier opened and whose brief did not settle.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-public-start-retry.sh
#
# Every directory it needs is created here; one that cannot be made or written stops the run by name
# (exit 2) **before** the plugin is copied, before the workflow is driven and before any check is
# counted. That discipline is the whole file's: this fixture is new, so the setup steps that follow
# the guard are guarded too, and a broken setup can never wear a `FAIL` label.
#
# WHY. The lane's sentence is that repeating the same operation lands the brief the first attempt
# could not deliver, with **one** child, **one** brief, **one** delivery key and **one** epoch. Two
# existing fixtures each hold half of it: `live-rune-held-start.sh` drives the shipped tool twice and
# both attempts stop at the release (no doorbell exists, so nothing can land), and
# `live-rune-allow-start-retry.sh` lands a brief through a **fixture tool** that calls the custody
# door directly, because a workflow's Rune module cannot. What a caller actually does — the shipped
# registry tool, once, ending with the barrier open and the brief unestablished, then the same
# operation again — is what this file runs, and it runs it a third time so "no second brief" is
# measured rather than asserted.
#
# What it witnesses, and each is a way the port can be wrong:
#   P1  the first issue claims neither "held" nor "started": the row is written, the barrier is open,
#       and the brief is the thing not established — the log's own `observed` says running;
#   P2  the record after it says `phase bound, observed running`, which is where that sentence comes
#       from: the phase is the record's and the observation is the host's, and they disagree;
#   P3  the repeat is accepted, names the same child, and says the row was **found**, not written a
#       second time, in epoch 1;
#   P4  a third issue of the same operation is still accepted, still epoch 1 — and mints nothing;
#   P5  the ledger holds one row, one epoch, one worker, written by this run;
#   P6  the record settles at `phase running, observed running`, same child;
#   P7  the child's journal holds exactly one copy of the brief and exactly one 32-hex delivery key,
#       and that key is also the one the coordinator's custody journal holds;
#   P8  no second child exists: there is one log for this run and one for the child, and no third.
#
# Local only: nothing installed, no operator config, no monitor, no compositor, headless children.
# The harness is the binary in EIDOLON_BIN and never PATH's — a refusal to find it is a SKIP (3).
# No `--yolo`, no policy row beyond the one workflow this file runs, and no gate is bypassed.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-public-start-retry.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
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
child_pid=""
cleanup() {
    if [ -n "$child_pid" ] && [ -d "/proc/$child_pid" ]; then
        printf 'teardown (early exit): stopping child %s\n' "$child_pid"
        kill -TERM "$child_pid" 2>/dev/null; sleep 0.2; kill -9 "$child_pid" 2>/dev/null
    fi
    rm -rf "$runtime" 2>/dev/null || true
}
trap cleanup EXIT

# ---- the binary, named and pinned ----------------------------------------------------------------
#
# A fixture that cannot say which binary it ran is not reproducible, and one that pins a build
# artefact does not know what it ran: the gate overwrites `target/debug/eidolon` in the same tree a
# fixture points at. This pins a copy and re-hashes it before use.
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
cp "$here/lifecycle-fixture/tools/wait_for_socket.rn" "$here/lifecycle-fixture/tools/dump_record.rn" \
   "$here/lifecycle-fixture/tools/dump_rows.rn" "$here/lifecycle-fixture/tools/me.rn" "$plug/tools/" 2>/dev/null
for t in wait_for_socket dump_record dump_rows me; do
    [ -f "$plug/tools/$t.rn" ] || setup_refuse "$plug/tools/$t.rn" "the fixture tool was not copied"
done
python3 - "$plug/plugin.rn" <<'PY' || setup_refuse "$plug/plugin.rn" "the fixture tools could not be added to the claims list"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"wait_for_socket", "dump_record", "dump_rows", "me"'
if '"hook_peer"]' not in s:
    raise SystemExit("the claims list is not where this expects it")
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'wait_for_socket' "$plug/plugin.rn" || setup_refuse "$plug/plugin.rn" "the fixture tools are not registered"
cp "$here/rune-public-start-retry.rn" "$plug/workflows/rune-public-start-retry.rn" 2>/dev/null
[ -f "$plug/workflows/rune-public-start-retry.rn" ] || setup_refuse "$plug/workflows" "the workflow was not copied"
if ! trust_out=$("$EID" plugins trust coordinator 2>&1); then
    printf '%s\n' "$trust_out" | sed 's/^/  /' >&2
    setup_refuse "plugins trust coordinator" "the plugin would not vouch"
fi

# ---- the one policy row this run needs -----------------------------------------------------------
#
# A workflow's `tool_call` goes through the same policy table as a model's, and a name the table has
# not seen is a question nothing can answer unattended. One row for the one workflow, in front of the
# fallthrough: every shipped row keeps its meaning, and `--yolo` — which would answer every question
# in this fixture rather than this one — is not used.
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY' || setup_refuse "$XDG_CONFIG_HOME/eidolon/policy.rn" "the policy row could not be seeded"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-public-start-retry" => v(ALLOW, "fixture: start this one workflow", false),\n'
if '        _ => {' not in s:
    raise SystemExit("no fallthrough arm to put the row in front of")
if 'workflow:coordinator:rune-public-start-retry' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

op='public-start-retry-op'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
presence=$runtime/eidolon
mkdir -p "$sessions" "$presence" 2>/dev/null
[ -d "$sessions" ] || setup_refuse "$sessions" "the sessions directory could not be created"
[ -d "$presence" ] || setup_refuse "$presence" "the presence root could not be created"

# ---- three issues of one operation, through the shipped tool --------------------------------------
say ""
say "=== one operation, three issues of the public coordinator_start ==="
args=$(python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text","brief":sys.argv[3],"sock_dir":sys.argv[4]}))' \
    "$op" "$ws" "the bounded assignment for the public retry" "$presence")
run=$("$EID" workflow run "$plug" rune-public-start-retry --args "$args" --provider mock 2>&1 || true)
# The run prints its own [policy]/[lifecycle] lines before the answer; the answer is the last line
# that is a JSON object carrying `report`. Checking the escaped form would be checking the
# transport's spelling.
rep=$(printf '%s' "$run" | python3 -c '
import json,sys
raw=sys.stdin.read()
cand=[l for l in raw.splitlines() if l.startswith("{\"report\"")]
if not cand:
    print(raw); raise SystemExit
print(json.loads(cand[-1]).get("report",""))
')
printf '%s\n' "$rep" | sed 's/^/     /'

check "P1 the workflow run completed" "$run" '"status":"completed"'
check "P1b the first issue reached the plugin's own sentence" "$rep" "P1-ERR: the adoption row for"
check "P1c and it says the barrier is open, from the log's observation" "$rep" "and the barrier is **open**"
check "P1d and names the brief as what is not established" "$rep" "What is not established is its **brief**"
check "P1e and says the row was written by this attempt and read back" "$rep" "is written by this attempt and read back after the write"
check "P1f and names epoch 1" "$rep" "read back after the write in epoch 1"
check_absent "P1g and does not claim the child is held" "$rep" "is still held"
check "P1h and says in its own words that nothing here reports the start as complete" "$rep" "nothing here reports the start as complete"

first_record=$(printf '%s\n' "$rep" | sed -n 's/^P1-RECORD: //p' | tail -1)
child_session=$(printf '%s' "$first_record" | sed -n 's/.*"session":"\([0-9]\{6,\}\)".*/\1/p')
check_re "P2 the record names the child the start was bound to" "$child_session" '^[0-9]{6,}$'
check "P2b the record's phase is bound — the brief never settled" "$first_record" '"phase":"bound"'
check "P2c and the host observes that child running" "$first_record" '"observed":"running"'

check "P3 the workflow waited on the doorbell path, not on a guess" "$rep" "P2-SOCK: probe"
check "P3b and the path existed when it waited" "$rep" "the path exists and refuses a plain read"

first_answer=$(printf '%s\n' "$rep" | sed -n 's/^P1-ERR: //p' | tail -1)
repeat=$(printf '%s\n' "$rep" | sed -n 's/^P3-OK: //p' | tail -1)
check "P4 the repeat was accepted" "$rep" "P3-OK: started"
check "P4b and named the same child as the first issue" "$repeat" "$child_session"
check "P4c and says this ledger's row was found, not written again" "$repeat" "found from an earlier attempt of this operation, which this attempt did not write a second time"
check "P4d and in epoch 1" "$repeat" "in epoch 1"

third=$(printf '%s\n' "$rep" | sed -n 's/^P4-OK: //p' | tail -1)
check "P5 a third issue of the same operation was accepted" "$rep" "P4-OK: started"
check "P5b and named the same child" "$third" "$child_session"
check "P5c and still epoch 1" "$third" "in epoch 1"
check_absent "P5d and never a second epoch" "$third" "epoch 2"
# The same brief **record**, from the door's typed receipt: a second brief would be a second record id
# even if it landed under the same key, so this is the one assertion a key alone could not make.
second_record=$(printf '%s' "$repeat" | sed -n 's/.*journaled as record \([0-9][0-9]*\).*/\1/p')
third_record=$(printf '%s' "$third" | sed -n 's/.*journaled as record \([0-9][0-9]*\).*/\1/p')
check_re "P5e the repeat named a brief record id" "$second_record" '^[0-9]+$'
check "P5f and the third issue named that same record, not a second brief" "$third_record" "$second_record"

# ---- the ledger, as data -------------------------------------------------------------------------
rows=$(printf '%s\n' "$rep" | sed -n 's/^P5-ROWS: //p' | tail -1)
facts=$(python3 - "$rows" "$op" <<'PYROWS'
import json, sys
try:
    rows = json.loads(sys.argv[1])
except Exception:
    print("rows-parsed=no"); raise SystemExit
mine = [r for r in rows if isinstance(r, dict) and r.get("operation") == sys.argv[2]]
print("rows-parsed=yes")
print("rows-for-op=%d" % len(mine))
print("epochs=%s" % sorted(str(r.get("epoch")) for r in mine))
print("workers=%d" % len({r.get("worker") for r in mine}))
print("coordinator=%s" % (mine[0].get("coordinator") if mine else "none"))
PYROWS
)
printf '%s\n' "$facts" | sed 's/^/     /'
check "P6 the ledger rows were read back as data" "$facts" "rows-parsed=yes"
check "P6b one row for one operation, after three issues" "$facts" "rows-for-op=1"
check "P6c in one epoch" "$facts" "epochs=['1']"
check "P6d naming one child" "$facts" "workers=1"

settled=$(printf '%s\n' "$rep" | sed -n 's/^P5-RECORD: //p' | tail -1)
check "P7 the record settles at phase running" "$settled" '"phase":"running"'
check "P7b and the host observes it running" "$settled" '"observed":"running"'
check "P7c and still names the same child" "$settled" "\"session\":\"$child_session\""

run_me=$(printf '%s\n' "$rep" | sed -n 's/^P6-ME: //p' | tail -1)
run_session=$(printf '%s' "$run_me" | sed -n 's/.*"session":"\([0-9]\{6,\}\)".*/\1/p')
check_re "P8 this run's own address was read from the roster" "$run_session" '^[0-9]{6,}$'
coordinator=$(printf '%s\n' "$facts" | sed -n 's/^coordinator=//p')
check "P8b and the row was written by this run, not by a name someone typed" "$coordinator" "$run_session"

# ---- the two journals: one brief, one key, one child ---------------------------------------------
say ""
say "=== the child's journal and the coordinator's custody journal ==="
child_log=$sessions/$child_session.eid
run_log=$sessions/$run_session.eid
brief_text='the bounded assignment for the public retry'

copies=0
[ -f "$child_log" ] && copies=$(strings -a "$child_log" 2>/dev/null | grep -cF -- "$brief_text" || true)
check "P9 the child's journal exists" "$([ -f "$child_log" ] && echo present || echo absent)" "present"
check "P9b and holds exactly one copy of the brief" "$copies" "1"

# The delivery key is 32 hex — 16 random bytes — and a 64-hex binding digest is a different shape, so
# a word-boundary match on 32 hex cannot split one into two. The child's managed instance id is also
# 32 hex and is not a key, so it is excluded by value rather than by hoping it is absent.
instance=$(strings -a "$child_log" 2>/dev/null | grep -o '"instance":"[0-9a-f]*"' | head -1 | sed 's/.*:"//; s/"//')
say "     the child's managed instance id (32 hex, excluded): ${instance:-unknown}"
keys=$(strings -a "$child_log" 2>/dev/null | grep -oE '\b[0-9a-f]{32}\b' | sort -u | grep -vF -- "${instance:-@@none@@}" || true)
nkeys=$(printf '%s\n' "$keys" | grep -c . || true)
printf '%s\n' "$keys" | sed 's/^/     key: /'
check "P10 exactly one delivery key exists after three issues" "$nkeys" "1"
key=$(printf '%s\n' "$keys" | sed -n '1p')
if [ -n "$key" ]; then
    check "P10b and the coordinator's custody journal holds that same key" \
        "$(strings -a "$run_log" 2>/dev/null | grep -cF -- "$key" || true)" "1"
else
    checks=$((checks + 1)); say "FAIL P10b no key was found to look for"; fail=$((fail + 1))
fi

# ---- one child, counted rather than asserted ------------------------------------------------------
logs=$(find "$sessions" -maxdepth 1 -name '*.eid' 2>/dev/null | wc -l)
others=$((logs - 2))
say "     session logs: ${logs} (this run and the child are two of them)"
check "P11 one log for this run" "$([ -f "$run_log" ] && echo present || echo absent)" "present"
check "P11b no third session exists, so no second child was spawned" "$others" "0"

# ---- teardown, not observation -------------------------------------------------------------------
#
# The child is running after a successful repeat; end it here so nothing is left on the machine. The
# pid comes from the child's own managed-start receipt, not from a guess about the process table.
say ""
say "=== teardown, not observation ==="
child_pid=$(python3 - "$sessions" "$child_session" <<'PY'
import json, os, sys
root, want = sys.argv[1], sys.argv[2]
try: names = os.listdir(root)
except FileNotFoundError: names = []
for name in names:
    if not name.endswith(".eid"): continue
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
if [ -n "$child_pid" ] && [ -d "/proc/$child_pid" ]; then
    say "     stopping the released child $child_pid"
    kill -TERM "$child_pid" 2>/dev/null || true
    n=0; while [ "$n" -lt 100 ] && [ -d "/proc/$child_pid" ]; do n=$((n+1)); sleep 0.05; done
    [ -d "/proc/$child_pid" ] && kill -9 "$child_pid" 2>/dev/null
    sleep 0.2
    if [ -d "/proc/$child_pid" ]; then
        checks=$((checks + 1)); say "FAIL teardown: child $child_pid survived"; fail=$((fail + 1))
    else
        checks=$((checks + 1)); say "ok   teardown: the child is gone"
    fi
    child_pid=""
else
    say "     no live child to clean up"
fi

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-public-start-retry complete: $checks checks, 0 FAIL"
    say "the first issue's words, for the record:"
    printf '%s\n' "$first_answer" | sed 's/^/     /'
else
    say "live-rune-public-start-retry FAILED: $fail of $checks checks"
    exit 1
fi

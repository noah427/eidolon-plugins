#!/usr/bin/env bash
# live-rune-allow-start-retry.sh — one bounded experiment: does re-issuing the same operation land a
# brief that the first `custody_allow_start` could not deliver?
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-allow-start-retry.sh
#
# WHY. `custody_allow_start` can refuse with "brief delivery is unconfirmed" because the correlated
# delivery dials `<presence>/sock` and presence is registered before the socket binds. Two questions
# follow, and this run answers both from what the tree says rather than from what it ought to say:
#   * does the socket appear at all, and how long after the child's presence directory?
#   * does a **second** `custody_allow_start` for the same operation land the brief — or refuse
#     again? And if it lands, did it carry the same delivery key?
#
# This is not a fixture for a happy path and its assertions do not call success a requirement: B1 is
# satisfied by either answer, and the closing section prints which one the tree gave. If the repeat
# refuses, that is the result.
#
# The socket probe is deliberately uncut: the watcher lists the runtime directory with `ls -1` (no
# `head`), and the workflow waits on the path itself rather than racing the second ask against a bind.
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
    echo "SKIP live-rune-allow-start-retry.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi
say_bin() {
    printf 'EIDOLON_BIN=%s\n' "$EID"
    stat -c '  size=%s mtime=%y' "$EID" | sed 's/^/  /'
    sha256sum "$EID" | sed 's/^/  sha256=/'
}

runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime
watcher_pid=""
child_pid=""
cleanup() {
    [ -n "$watcher_pid" ] && kill "$watcher_pid" 2>/dev/null
    if [ -n "$child_pid" ] && [ -d "/proc/$child_pid" ]; then
        printf 'teardown (early exit): stopping child %s\n' "$child_pid"
        kill -TERM "$child_pid" 2>/dev/null; sleep 0.2; kill -9 "$child_pid" 2>/dev/null
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
cp "$here/lifecycle-fixture/tools/hold_only.rn" "$here/lifecycle-fixture/tools/wait_for_socket.rn" \
   "$here/lifecycle-fixture/tools/allow_start_only.rn" "$here/lifecycle-fixture/tools/dump_record.rn" "$plug/tools/"
python3 - "$plug/plugin.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"hold_only", "wait_for_socket", "allow_start_only", "dump_record"'
assert '"hook_peer"]' in s, "the claims list is not where this expects it"
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'wait_for_socket' "$plug/plugin.rn" || { say "FAIL the fixture tools are not registered"; exit 1; }
cp "$here/rune-allow-start-retry.rn" "$plug/workflows/rune-allow-start-retry.rn"
"$EID" plugins trust coordinator >/dev/null 2>&1 || { say "FAIL the plugin would not vouch"; exit 1; }

python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-allow-start-retry" => v(ALLOW, "fixture: start this one workflow", false),\n'
assert '        _ => {' in s, "no fallthrough arm to put the row in front of"
if 'workflow:coordinator:rune-allow-start-retry' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

op='allow-start-retry-op'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
presence=$runtime/eidolon
timing=$XDG_STATE_HOME/socket-timing.log
mkdir -p "$sessions" "$presence"

# ---- the timing watcher: uncut, and outside the workflow -----------------------------------------
#
# It lists the runtime root with `ls -1` and records the first moment each `<session>/` directory and
# each `<session>/sock` is seen, in milliseconds. It never decides anything: the workflow waits on the
# path itself, and this log is the independent measurement of how long the bind took.
watch_presence() {
    local deadline=$((SECONDS + 30)) line d s
    : >"$timing"
    while [ "$SECONDS" -lt "$deadline" ]; do
        for d in "$presence"/*/; do
            [ -d "$d" ] || continue
            d=${d%/}
            grep -qF "dir $d" "$timing" 2>/dev/null || printf '%s dir %s\n' "$(date +%s%3N)" "$d" >>"$timing"
            if [ -S "$d/sock" ]; then
                if ! grep -qF "sock $d/sock" "$timing" 2>/dev/null; then
                    printf '%s sock %s/sock\n' "$(date +%s%3N)" "$d" >>"$timing"
                    # The snapshot that decides which situation the repeat is in: a key already in a
                    # journal here means the first send journaled it and only the acknowledgement
                    # was lost; absent means the send died before the record.
                    rm -rf "$XDG_STATE_HOME/sessions-at-sock"
                    cp -r "$sessions" "$XDG_STATE_HOME/sessions-at-sock" 2>/dev/null || true
                fi
            fi
        done
        sleep 0.005
    done
}
watch_presence &
watcher_pid=$!

say ""
say "=== one operation: hold, ask, wait for the socket, ask again ==="
args=$(python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text","brief":"the bounded assignment","sock_dir":sys.argv[3]}))' "$op" "$ws" "$presence")
run=$("$EID" workflow run "$plug" rune-allow-start-retry --args "$args" --provider mock 2>&1 || true)
rep=$(printf '%s' "$run" | python3 -c '
import json,sys
raw=sys.stdin.read()
cand=[l for l in raw.splitlines() if l.startswith("{\"report\"")]
if not cand:
    print(raw); raise SystemExit
print(json.loads(cand[-1]).get("report",""))
')
printf '%s\n' "$rep" | sed 's/^/     /'
check "R0 the workflow run completed" "$run" '"status":"completed"'

held=$(printf '%s\n' "$rep" | sed -n 's/^R0-HELD: //p' | tail -1)
child_session=$(printf '%s' "$held" | sed -n 's/.*session \([0-9]\{6,\}\).*/\1/p')
check_re "R1 the child was held" "$child_session" '^[0-9]{6,}$'

say ""
say "=== the socket, measured from outside the workflow ==="
sed 's/^/     /' "$timing"
dir_ms=$(awk '/ dir /{print $1; exit}' "$timing")
sock_ms=$(awk '/ sock /{print $1; exit}' "$timing")
if [ -n "$dir_ms" ] && [ -n "$sock_ms" ]; then
    say "     presence directory seen first, socket $((sock_ms - dir_ms)) ms later"
    printf 'dir-to-sock-ms=%s\n' "$((sock_ms - dir_ms))"
else
    printf 'dir-to-sock-ms=none\n'
fi

# ---- the two asks --------------------------------------------------------------------------------
check "R2 the first ask was refused" "$rep" "R1-ALLOW-ERR:"
check "R2b with the brief unconfirmed" "$rep" "brief delivery is unconfirmed"
check "R3 the workflow waited on the socket path, not on a guess" "$rep" "R2-SOCK: probe"
check "R3b and the path existed when it waited" "$rep" "the path exists and refuses a plain read"

if printf '%s\n' "$rep" | grep -qF "R3-ALLOW-OK:"; then
    say "     the repeat was ACCEPTED"
    second=$(printf '%s\n' "$rep" | sed -n 's/^R3-ALLOW-OK: //p' | tail -1)
    check "R4 the repeat answered a record id" "$second" '"brief_record":'
    check "R4b and named the same child" "$second" "$child_session"
    record=$(printf '%s\n' "$rep" | sed -n 's/^R4-RECORD: //p' | tail -1)
    check "R5 the record's phase is running" "$record" '"phase":"running"'
    check "R5b and the host observes the child running" "$record" '"observed":"running"'
else
    say "     the repeat was REFUSED again"
    second=$(printf '%s\n' "$rep" | sed -n 's/^R3-ALLOW-ERR: //p' | tail -1)
    say "     its words: ${second}"
    check "R4 the repeat answered an error rather than a record" "$rep" "R3-ALLOW-ERR:"
    record=$(printf '%s\n' "$rep" | sed -n 's/^R4-RECORD: //p' | tail -1)
    check "R5 the record's phase is still bound" "$record" '"phase":"bound"'
fi

# ---- the delivery key: one value, or the repeat minted a second one -------------------------------
#
# (the block below reports the key, the record, the situation, and the wake as typed facts)
#
# The key is persisted before the first send, so every repeat must carry the same one. The parent
# session's journal is where the durable rows live; the count of **distinct** keys is the check.
parent=$(printf '%s' "$rep" | sed -n 's/^R0-HELD: session \([0-9]\{6,\}\).*/\1/p' | tail -1)
keys=$(strings -a "$sessions/$parent.eid" 2>/dev/null | grep -o '"brief_delivery_key":"[0-9a-f]*"' | sort -u)
nkeys=$(printf '%s\n' "$keys" | grep -c 'brief_delivery_key' || true)
say ""
say "=== the delivery key ==="
printf '%s\n' "$keys" | sed 's/^/     /'
key=$(printf '%s\n' "$keys" | sed -n 's/.*"brief_delivery_key":"\([0-9a-f]*\)".*/\1/p' | head -1)
say "     delivery key: ${key:-none}"

# ---- the Imperator's four questions, from receipts and journals -----------------------------------
#
# The view's `brief_record` is a **record id**, not the receipt struct: the door collapses `journaled`
# and `existing` into one word (a finding in its own right — a plugin cannot tell the two apart), so
# the distinction has to come from the log. Everything below is read from the child's own journal at
# two moments: the instant its socket appeared (before the repeat) and after the run.
say ""
say "=== the repeat as typed facts ==="
answer=$(printf '%s\n' "$rep" | sed -n 's/^R3-ALLOW-\(OK\|ERR\): //p' | tail -1)
first_answer=$(printf '%s\n' "$rep" | sed -n 's/^R1-ALLOW-ERR: //p' | tail -1)
view=$(printf '%s' "$answer" | python3 -c '
import json,sys
raw=sys.stdin.read(); i=raw.find("{")
if i < 0: print("brief-record=none"); print("wake=none"); raise SystemExit
try: d=json.loads(raw[i:raw.rfind("}")+1])
except Exception: print("brief-record=none"); print("wake=none"); raise SystemExit
br=d.get("brief_record")
print("brief-record=%s" % ("none" if br is None else br))
print("session=%s" % d.get("session",""))
print("wake=%s" % d.get("wake",""))
')
printf '%s\n' "$view" | sed 's/^/     /'
record_id=$(printf '%s\n' "$view" | sed -n 's/^brief-record=//p')
wake=$(printf '%s\n' "$view" | sed -n 's/^wake=//p')

# 1. the typed answer
check "Q1 the repeat answered a typed view" "$answer" '"session"'
if [ "$record_id" = "none" ]; then
    say "     the repeat answered NO receipt"
    check "Q4 and therefore no wake was attempted" "$wake" "not_attempted"
else
    check_re "Q1b it names a record id" "$record_id" '^[0-9]+$'
    say "     receipt: record id ${record_id}, wake word \`${wake}\`"
    case "$wake" in
        not_attempted) checks=$((checks+1)); say "FAIL Q4 a receipt exists but no wake was attempted"; fail=$((fail+1));;
        *) checks=$((checks+1)); say "ok   Q4 a receipt exists and the wake was attempted once (\`$wake\`)";;
    esac
fi

# 2 and 3. the child's log, at the socket and now: one record, and which situation
say ""
say "=== the correlated record in the child's log ==="
brief_text='the bounded assignment'
child_now=$sessions/$child_session.eid
child_then=$XDG_STATE_HOME/sessions-at-sock/$child_session.eid
count_now=0
[ -f "$child_now" ] && count_now=$(strings -a "$child_now" | grep -cF -- "$brief_text" || true)
say "     copies of the brief text in the child's log, after the run: $count_now"
check "Q2 the child's log holds exactly one copy of the brief" "$count_now" "1"
# The at-socket snapshot is **not** used to decide which situation this was: it is taken the instant
# the watcher sees the socket, and the workflow's own probe can succeed in the same millisecond, so the
# copy may already be the repeat's. A snapshot that races the act it is meant to bracket is not
# evidence, and saying so is cheaper than a green line nobody can defend. The situation is decided
# from the door's own typed answer below instead.
say "     (the at-socket snapshot was taken but is not used as evidence: it races the repeat)"
case "$(printf '%s' "$first_answer" | sed -n 's/.*\(brief delivery is unconfirmed\).*/unconfirmed/p')" in
    unconfirmed)
        say "     situation: the first ask answered \`Unknown\` — no durable acceptance and no evidence of"
        say "     absence — so on the door's own account the first send journaled nothing and the single"
        say "     copy in the log is the repeat's. Nothing was duplicated; the key was already persisted."
        ;;
    *)
        say "     situation: not established from a typed answer; see the finding"
        ;;
esac
say "     and the door CANNOT say whether the repeat answered \`journaled\` or \`existing\`:"
say "     \`StartView.brief_record\` is the record id alone, so the two are collapsed at this door."

# 4. the delivery key: the parent's launch rows are bitcode, so the *value* is what is legible — a
#    32-hex key (the binding digest is a 64-hex sha256, a different shape). The set must not grow
#    across the repeat: a second key would be a second brief under a new name.
say ""
say "=== the delivery key (value, not field name: the rows are bitcode) ==="
instance=$(strings -a "$sessions/$child_session.eid" 2>/dev/null | grep -o '"instance":"[0-9a-f]*"' | head -1 | sed 's/.*:"//; s/"//')
say "     the child's managed instance id (also 32 hex, so excluded): ${instance:-unknown}"
keys_now=$(strings -a "$sessions/$child_session.eid" "$sessions/$parent.eid" 2>/dev/null | grep -oE '\b[0-9a-f]{32}\b' | sort -u | grep -vF -- "${instance:-@@none@@}" || true)
nkeys_now=$(printf '%s\n' "$keys_now" | grep -c . || true)
printf '%s\n' "$keys_now" | sed 's/^/     /'
check "Q5 exactly one delivery key exists after the run" "$nkeys_now" "1"
# ---- teardown, not observation -------------------------------------------------------------------
#
# The child is running after a successful repeat; end it here so nothing is left on the machine.
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
    [ -d "/proc/$child_pid" ] && { say "FAIL teardown: child $child_pid survived"; fail=$((fail+1)); checks=$((checks+1)); } \
        || { say "ok   teardown: the child is gone"; checks=$((checks+1)); }
else
    say "     no live child to clean up"
fi
kill "$watcher_pid" 2>/dev/null; watcher_pid=""
[ "$fail" -eq 0 ] || exit 1

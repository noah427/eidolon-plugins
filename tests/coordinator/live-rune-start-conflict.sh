#!/usr/bin/env bash
# live-rune-start-conflict.sh — contract witness 3: the `conflict` refusal of `coordinator_start`.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-start-conflict.sh
#
# Every directory it needs is created here; one that cannot be made or written stops the run by name
# (exit 2) before the plugin is copied, before the workflow is driven and before any check is counted.
#
# WHY. `record()` refuses when this ledger's row for an operation and this session's custody log's
# child for it disagree, and the reviewer named that branch as having no witness anywhere — the gap
# that leaves C2's ruling untested. The brief allows two ways to close it: a production-realizable
# producer, or an explicit disposition that says why there is none. **This file does both, and keeps
# them apart.**
#
#   (1) The producer search, with no seeded state at all: two sessions, one operation id, one set of
#       bytes. It fails to produce a disagreement, and *why* it fails is the finding — plugin state is
#       keyed `(plugin, session, key)` (`plugin-state/coordinator/<session>/store.json`), so the second
#       session writes its own store. The ledgers are not shared, so one session's row can never be
#       read against another session's log. The fixture asserts that, including the two store files on
#       disk, so the disposition is evidence rather than an assertion.
#
#   (2) The branch itself, exercised by **labeled seeded state**: a fixture tool writes the one
#       adoption row the branch needs, in a fresh session, with the plugin's own primitive. That proves
#       the refusal refuses, names both sides and writes nothing. It is **not** producer proof and is
#       reported as such.
#
# What it witnesses:
#   A1–A4  a session's own start writes its own store: one row, its child, epoch 1;
#   B1–B6  a second session presenting the same operation id and bytes gets its **own** store and its
#          own child, and no conflict — because the ledgers are not shared. Two store files on disk.
#   C1–C10 seeded: the conflict refusal, its detail naming both children, "wrote nothing and released
#          nothing", the ledger unchanged, and the held child left held.
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
    echo "SKIP live-rune-start-conflict.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
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
    # Teardown, not observation: every managed child these runs spawned is stopped by the pid its own
    # `managed-start/v1` receipt recorded, and nothing is inferred from the process table.
    for p in ${pids:-}; do
        if [ -d "/proc/$p" ]; then kill -TERM "$p" 2>/dev/null; sleep 0.2; kill -9 "$p" 2>/dev/null; fi
    done
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
cp "$here/lifecycle-fixture/tools/dump_record.rn" "$here/lifecycle-fixture/tools/dump_rows.rn" \
   "$here/lifecycle-fixture/tools/me.rn" "$here/lifecycle-fixture/tools/seed_adoption.rn" "$plug/tools/" 2>/dev/null
for t in dump_record dump_rows me seed_adoption; do
    [ -f "$plug/tools/$t.rn" ] || setup_refuse "$plug/tools/$t.rn" "the fixture tool was not copied"
done
python3 - "$plug/plugin.rn" <<'PY' || setup_refuse "$plug/plugin.rn" "the fixture tools could not be added to the claims list"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"dump_record", "dump_rows", "me", "seed_adoption"'
if '"hook_peer"]' not in s:
    raise SystemExit("the claims list is not where this expects it")
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'seed_adoption' "$plug/plugin.rn" || setup_refuse "$plug/plugin.rn" "the fixture tools are not registered"
cp "$here/rune-start-conflict.rn" "$plug/workflows/rune-start-conflict.rn" 2>/dev/null
[ -f "$plug/workflows/rune-start-conflict.rn" ] || setup_refuse "$plug/workflows" "the workflow was not copied"
if ! trust_out=$("$EID" plugins trust coordinator 2>&1); then
    printf '%s\n' "$trust_out" | sed 's/^/  /' >&2
    setup_refuse "plugins trust coordinator" "the plugin would not vouch"
fi
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY' || setup_refuse "$XDG_CONFIG_HOME/eidolon/policy.rn" "the policy row could not be seeded"
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-start-conflict" => v(ALLOW, "fixture: start this one workflow", false),\n'
if '        _ => {' not in s:
    raise SystemExit("no fallthrough arm to put the row in front of")
if 'workflow:coordinator:rune-start-conflict' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

op='conflict-witness-op'
ws=$(mktemp -d)
sessions=$XDG_DATA_HOME/eidolon/sessions
states=$XDG_DATA_HOME/eidolon/plugin-state/coordinator
mkdir -p "$sessions" 2>/dev/null
[ -d "$sessions" ] || setup_refuse "$sessions" "the sessions directory could not be created"

args_of() { # args_of <seed-worker>
    python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text","brief":"the bounded assignment for the conflict witness","seed_worker":sys.argv[3]}))' \
        "$op" "$ws" "$1"
}
line_of() { # line_of <raw-run>
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
report_field() { # report_field <report> <prefix> — one line of the report, by prefix, verbatim
    printf '%s\n' "$1" | sed -n "s/^$2: //p" | tail -1
}
session_field() { # session_field <json> -> the `session` field, or empty
    python3 -c 'import json,sys
raw=sys.stdin.read()
try: print(json.loads(raw).get("session") or "")
except Exception: print("")' <<<"$1"
}
rows_facts() { # rows_facts <rows-json>
    python3 - "$1" "$op" <<'PYROWS'
import json, sys
try: rows=json.loads(sys.argv[1])
except Exception: print("rows-parsed=no"); raise SystemExit
mine=[r for r in rows if isinstance(r,dict) and r.get("operation")==sys.argv[2]]
print("rows-parsed=yes")
print("rows-for-op=%d" % len(mine))
print("epochs=%s" % sorted(str(r.get("epoch")) for r in mine))
print("workers=%s" % sorted({str(r.get("worker")) for r in mine}))
print("coordinators=%s" % sorted({str(r.get("coordinator")) for r in mine}))
PYROWS
}
store_facts() { # store_facts <session-id> — the store file that session wrote, as facts
    local f=$states/$1/store.json
    if [ ! -f "$f" ]; then say "     (no store file for $1)"; printf 'store=absent\n'; return; fi
    python3 - "$f" "$op" <<'PYSTORE'
import json, sys
raw = json.load(open(sys.argv[1]))
value = raw.get("value")
try: store = json.loads(value) if isinstance(value, str) else {}
except Exception: store = {}
rows = [r for r in store.get("adoptions", []) if isinstance(r, dict) and r.get("operation") == sys.argv[2]]
print("store=present revision=%s rows-for-op=%d workers=%s epochs=%s"
      % (raw.get("revision"), len(rows), sorted({str(r.get("worker")) for r in rows}),
         sorted(str(r.get("epoch")) for r in rows)))
PYSTORE
}

# ---- A: one session, its own store --------------------------------------------------------------
say ""
say "=== A: session one starts the operation ==="
line=$(line_of "$("$EID" workflow run "$plug" rune-start-conflict --args "$(args_of "")" --provider mock 2>&1)")
rep=$(field_of "$line" "report")
printf '%s\n' "$rep" | sed 's/^/     /'
check "A1 session one's run completed" "$(field_of "$line" "status")" "completed"
me_a=$(session_field "$(report_field "$rep" S4-ME)")
check_re "A1b its address was read from the roster" "$me_a" '^[0-9]{6,}$'
child_a=$(session_field "$(report_field "$rep" S3-RECORD)")
check_re "A2 its own custody log binds the operation to a real child" "$child_a" '^[0-9]{6,}$'
facts_a=$(rows_facts "$(report_field "$rep" S2-ROWS)")
printf '%s\n' "$facts_a" | sed 's/^/     /'
check "A2b one row for the operation, naming that child" "$facts_a" "workers=['$child_a']"
check "A2c in epoch 1" "$facts_a" "epochs=['1']"
store_a=$(store_facts "$me_a")
check "A3 and the store on disk is this session's own" "$store_a" "store=present"
check "A3b holding that one row" "$store_a" "rows-for-op=1 workers=['$child_a']"

# ---- B: the same operation in a second session — the producer search -----------------------------
say ""
say "=== B: a second session, same operation id, same bytes — the producer search ==="
lineb=$(line_of "$("$EID" workflow run "$plug" rune-start-conflict --args "$(args_of "")" --provider mock 2>&1)")
repb=$(field_of "$lineb" "report")
printf '%s\n' "$repb" | sed 's/^/     /'
check "B1 session two's run completed" "$(field_of "$lineb" "status")" "completed"
me_b=$(session_field "$(report_field "$repb" S4-ME)")
child_b=$(session_field "$(report_field "$repb" S3-RECORD)")
check_re "B2 it was admitted in a session of its own, with a child of its own" "$child_b" '^[0-9]{6,}$'
check_absent "B2b and the conflict branch did NOT fire" "$repb" "disagree with what this ledger already holds"
facts_b=$(rows_facts "$(report_field "$repb" S2-ROWS)")
printf '%s\n' "$facts_b" | sed 's/^/     /'
check "B3 session two sees one row: its own, naming its own child" "$facts_b" "workers=['$child_b']"
check "B3b in epoch 1 of its own ledger" "$facts_b" "epochs=['1']"
# The fact that rules the cross-session route out, read off the disk rather than inferred: plugin
# state is keyed by session, so each of these two sessions wrote a store of its own.
store_b=$(store_facts "$me_b")
say "     session one's store: $store_a"
say "     session two's store: $store_b"
check "B4 the two sessions wrote two stores, not one" "$store_b" "store=present"
check "B4b session two's store names only its own child" "$store_b" "workers=['$child_b']"
store_a_after=$(store_facts "$me_a")
check "B4c and session one's store is untouched" "$store_a_after" "$store_a"
stores=$(find "$states" -maxdepth 2 -name store.json 2>/dev/null | wc -l)
say "     store files on disk: $stores"
check_re "B5 there is more than one store: the ledger is not shared between sessions" "$stores" '^[2-9]'

say ""
say "--- the disposition this run supports, in its own words -------------------------------"
say "     The conflict branch compares THIS ledger's row for an operation with THIS session's"
say "     custody log. Plugin state is keyed (plugin, session, key) — the two store files above — so"
say "     no session can read another's row, and a row this session wrote always names the child this"
say "     session's hold bound. Arm one (a row naming a different child) therefore needs the custody"
say "     log's binding for an operation to change, which the state machine forbids: hold is idempotent"
say "     on the operation and refuses changed input, a bound launch is never re-pointed, and a"
say "     cancelled one can never start again. Arm two (a live adoption of the bound child this"
say "     operation did not write) needs an adopt of a child that exists only inside the start's own"
say "     hold. Neither has a producer in the shipped tools. Hence the seeded witness below, which is"
say "     labeled and is not offered as producer proof."

# ---- C: seeded, labeled — the branch itself ------------------------------------------------------
say ""
say "=== C: SEEDED (test-only) — one adoption row naming a worker the hold will not bind ==="
seed_worker='9999999999999'
linec=$(line_of "$("$EID" workflow run "$plug" rune-start-conflict --args "$(args_of "$seed_worker")" --provider mock 2>&1)")
repc=$(field_of "$linec" "report")
printf '%s\n' "$repc" | sed 's/^/     /'
check "C1 the seeded run completed" "$(field_of "$linec" "status")" "completed"
check "C1b the seed was written through the plugin's own primitive" "$repc" "S0-SEED: "
check_absent "C2 the start was not accepted" "$repc" "S1-OK:"
conflict=$(report_field "$repc" S1-ERR)
check "C3 the start was refused with the conflict sentence" "$conflict" "disagree with what this ledger already holds"
check "C3b naming the ledger's row and its worker" "$conflict" "this ledger's row for the operation names \`$seed_worker\`"
child_c=$(session_field "$(report_field "$repc" S3-RECORD)")
check_re "C3c and naming a child of the custody log's" "$child_c" '^[0-9]{6,}$'
check "C3d the detail names that child too" "$conflict" "the custody log bound \`$child_c\`"
if [ "$child_c" != "$seed_worker" ]; then
    checks=$((checks + 1)); say "ok   C3e and the two children differ, which is the disagreement"
else
    checks=$((checks + 1)); say "FAIL C3e the seeded worker and the bound child are the same"; fail=$((fail + 1))
fi
check "C4 and says this attempt wrote nothing and released nothing" "$conflict" "so this attempt wrote nothing and released nothing"
check "C5 and does not claim whether the child ran" "$conflict" "Whether the child ran is not this tool's to say"

record_c=$(report_field "$repc" S3-RECORD)
check "C6 the record binds the operation to the child the hold made" "$record_c" "\"session\":\"$child_c\""
check "C6b at phase bound: the refusal released nothing" "$record_c" '"phase":"bound"'
me_c=$(session_field "$(report_field "$repc" S4-ME)")
store_c=$(store_facts "$me_c")
say "     the seeded session's store: $store_c"
check "C7 the refusal wrote nothing: the store still holds exactly the seeded row" "$store_c" "rows-for-op=1"
check "C7b still naming the seeded worker" "$store_c" "workers=['$seed_worker']"
check "C7c still that one row, epoch 1" "$store_c" "epochs=['1']"

receipts() {
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
say ""
say "=== the children these runs spawned, from their own receipts ==="
receipts "$sessions" | sed 's/^/     /'
receipt_c=$(receipts "$sessions" | python3 -c '
import json,sys
for line in sys.stdin:
    try: d=json.loads(line)
    except Exception: continue
    if d.get("session")==sys.argv[1]: print(line.strip()); break
' "$child_c")
check "C8 the child the conflict bound was left held: nothing opened its barrier" "$receipt_c" '"state": "held"'
pids=$(receipts "$sessions" | python3 -c '
import json,sys
for line in sys.stdin:
    try: d=json.loads(line)
    except Exception: continue
    p=d.get("pid")
    if isinstance(p,int): print(p)
' | sort -u | tr '\n' ' ')

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-start-conflict complete: $checks checks, 0 FAIL"
    say "  A and B are the producer search: no seeded state, and no disagreement is reachable."
    say "  C is SEEDED test-only state: it shows the branch refuses, and is not producer proof."
else
    say "live-rune-start-conflict FAILED: $fail of $checks checks"
    exit 1
fi

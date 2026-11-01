#!/usr/bin/env bash
# live-rune-held-start.sh — the plugin's `start.rn` at the held door, with a real child.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-held-start.sh
#
# WHY. `coordinator_start` used to dial the coordinator monitor; it now runs three local acts —
# `custody_hold`, this ledger's own adoption row, `custody_allow_start` — in that order, and the
# order is the guarantee. A `cargo test` can prove the state machine through a fake host and cannot
# prove that the tool file compiles, registers, or that the row is durable *before* the barrier
# opens. This drives the shipped tool through `eidolon workflow run`, which compiles it before it can
# call anything, against a real child process.
#
# What it witnesses, and each is a way the port can be wrong:
#   H1  a start whose release is refused or unconfirmed claims **neither** "held" nor "running": the
#       row is written, and whether the child ran is unknown;
#   H2  the same immutable input again finds *its own* row, writes no second one, and mints no
#       second epoch — the retry is not a repair and not a duplicate;
#   H3  the ledger holds exactly one row for the child, as data rather than as a sentence;
#   H4  a cancel against a start whose release was attempted answers `unrecorded` — the log holds no
#       cancellation, which is a different answer from "the log did not answer";
#   H5  a visible launch refuses at the no-focus gate before any process exists and writes no row;
#   H6  and the ledger is still one row after that refusal.
#
# Local only: nothing installed, no operator config, no monitor, no compositor, headless children.
# The harness is the binary in EIDOLON_BIN and never PATH's — a refusal to find it is a SKIP (3).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-held-start.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi
# The pin, in the output rather than in somebody's message: a witness that cannot say which binary
# it ran is not reproducible.
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

say "=== the harness under test ==="
say_bin

# ---- the plugin, copied byte for byte, plus the fixture's own tools -------------------------------
plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/lifecycle-fixture/tools/hold_only.rn" "$here/lifecycle-fixture/tools/dump_record.rn" \
   "$here/lifecycle-fixture/tools/dump_rows.rn" "$plug/tools/"
python3 - "$plug/plugin.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
add = '"hold_only", "dump_record", "dump_rows"'
assert '"hook_ready", "hook_peer"]' in s or '"hook_peer"]' in s, "the claims list is not where this expects it"
if add not in s:
    s = s.replace('"hook_peer"]', '"hook_peer", ' + add + ']', 1)
p.write_text(s)
PY
grep -q 'hold_only' "$plug/plugin.rn" || { say "FAIL the fixture tools are not registered"; exit 1; }
cp "$here/rune-held-start.rn" "$plug/workflows/rune-held-start.rn"
"$EID" plugins trust coordinator >/dev/null 2>&1 || { say "FAIL the plugin would not vouch"; exit 1; }

# ---- the one policy row a workflow start needs ---------------------------------------------------
#
# A workflow's start goes through the same policy table as a tool call, and the harness answers a
# name the table has not seen with a question — which nothing can answer in an unattended run. The
# harness seeds its own table into this config; the fixture adds **one** row for the one workflow it
# runs, so every shipped row keeps its meaning. `--yolo` would answer every question in the fixture
# rather than this one, and is not used.
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rune-held-start" => v(ALLOW, "fixture: start this one workflow", false),\n'
assert '        _ => {' in s, "no fallthrough arm to put the row in front of"
if 'workflow:coordinator:rune-held-start' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

op='held-fixture-op'
vis='held-fixture-visible'
ws=$(mktemp -d)

say "=== driving the three acts (operation $op) ==="
run=$("$EID" workflow run "$plug" rune-held-start \
        --args "$(python3 -c 'import json,sys; print(json.dumps({"operation":sys.argv[1],"visible_operation":sys.argv[2],"workspace":sys.argv[3],"model":"mock-text","brief":"the bounded assignment"}))' "$op" "$vis" "$ws")" \
        --provider mock 2>&1 || true)
# The workflow's `report` is one JSON string; unescape it once and check against the text the tools
# actually answered. Checking the escaped form would be checking the transport's spelling.
rep=$(printf '%s' "$run" | python3 -c '
import json,sys
raw=sys.stdin.read()
# The run prints its own [policy]/[lifecycle] lines before the answer; the answer is the last line
# that is a JSON object carrying `report`.
cand=[l for l in raw.splitlines() if l.startswith("{\"report\"")]
if not cand:
    print(raw); raise SystemExit
print(json.loads(cand[-1]).get("report",""))
')
printf '%s\n' "$rep" | sed 's/^/     /'

# A run that failed outright would satisfy every absence check below, so the run itself is checked
# first, and each answer is checked for the shape it must have.
check "the workflow run completed" "$run" '"status":"completed"'
check "H0 the first start reached the plugin's own answer" "$rep" "H1-ERR: the adoption row for"
check "H0b and that answer is the partial outcome, not a refusal of the hold" "$rep" "and the barrier is **open**"
# The branch is keyed on the log's own observation, not on core's prose. On this host the release is
# acknowledged and the child is observed running, so the sentence must say so and name the brief as
# what is unconfirmed — an "unknown" here would understate a fact the log holds.
check "H1 the child is reported running, from the log's observation" "$rep" "the log's own observation for this operation is \`running\`, so the child is running"
check "H1b and the brief is what it says is unestablished" "$rep" "What is not established is its **brief**"
check "H1c and it is not reported as a completed start" "$rep" "nothing here reports the start as complete"
check "H1d and the repeat re-sends under the same delivery key" "$rep" "under the same durable delivery key rather than sending an ordinary message beside it"
check_absent "H1e and it does not claim the child was stopped" "$rep" "the child was stopped"
check_absent "H1f and it does not claim the child is held" "$rep" "is still held"
check_absent "H1g and it does not fall back to an unknown child" "$rep" "Whether the child ran is **unknown**"
check "H1h and the row it wrote is named with its epoch" "$rep" "is written by this attempt and read back after the write in epoch 1"
# The retry: its own row, no second one, no second epoch.
check "H2 the retry finds its own row rather than minting one" "$rep" "H2-ERR: the adoption row for"
check "H2b and says this attempt wrote no second one" "$rep" "found from an earlier attempt of this operation, which this attempt did not write a second time in epoch 1"
check_absent "H2c and it does not claim a second epoch" "$rep" "epoch 2"
# The ledger as data: exactly one row, for the one child.
rows=$(printf '%s\n' "$rep" | sed -n 's/^H3-ROWS: //p' | tail -1)
facts=$(python3 - "$rows" <<'PYROWS'
import json, sys
try:
    rows = json.loads(sys.argv[1])
except Exception:
    print("rows-parsed=no"); raise SystemExit
mine = [r for r in rows if isinstance(r, dict) and r.get("operation") == "held-fixture-op"]
print("rows-parsed=yes")
print("rows-for-op=%d" % len(mine))
print("epochs=%s" % sorted(str(r.get("epoch")) for r in mine))
print("workers=%d" % len({r.get("worker") for r in mine}))
print("written-by=%s" % (mine[0].get("coordinator") if mine else "none"))
PYROWS
)
check "H3 the ledger rows were read back as data" "$facts" "rows-parsed=yes"
check "H3b one row for one operation" "$facts" "rows-for-op=1"
check "H3c in one epoch" "$facts" "epochs=['1']"
check "H3d naming one child" "$facts" "workers=1"
# Row 20b from the same start record: a start whose release was attempted is not a held child, and
# the log holds no cancellation for it.
check "H4 the cancel on a release-attempted start answers unrecorded" "$rep" "H4-ERR: cancellation of \`$op\` recorded nothing"
check "H4b and says the log holds no cancellation for it" "$rep" "the custody log holds no cancellation for it, so the answer above is a refusal rather than a cancellation, and this ledger was left exactly as it was"
check "H4c and the record's typed word is empty" "$rep" '"cancellation":null'
# The visible launch: this host has no terminal configured, and that gate fires before any process
# exists and before the door is asked for anything. Both halves are checked, because "no process"
# and "no record" are different facts.
check "H5 the visible launch is refused before any process exists" "$rep" "H5-ERR: the hold for \`$vis\` did not establish a child"
check "H5b in the gate's own words" "$rep" "this host has no terminal configured, so it cannot start a visible child"
check "H5c and it wrote no adoption row and released nothing" "$rep" "This attempt wrote no adoption row and released nothing"
check "H5d and the custody log holds no record for it" "$rep" "H5-RECORD-ERR: this session has no managed start named \`$vis\`"
# And the ledger is still exactly one row.
rows2=$(printf '%s\n' "$rep" | sed -n 's/^H6-ROWS: //p' | tail -1)
facts2=$(python3 - "$rows2" <<'PYROWS2'
import json, sys
try:
    rows = json.loads(sys.argv[1])
except Exception:
    print("rows-parsed=no"); raise SystemExit
print("rows-parsed=yes")
print("rows=%d" % len([r for r in rows if isinstance(r, dict) and r.get("operation")]))
PYROWS2
)
check "H6 the ledger after the refused launch" "$facts2" "rows=1"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-held-start complete: $checks checks, 0 FAIL"
else
    say "live-rune-held-start FAILED: $fail of $checks checks"
    exit 1
fi

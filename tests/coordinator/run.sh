#!/usr/bin/env bash
# run.sh — the coordinator plugin's fixture, on the subject it has left: the tools that read the
# harness and the ledger, and a rank that is a tag on the ranked session.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/run.sh
#
# WHY THIS FILE EXISTS AT ALL. The monitor this suite used to drive is retired — its port, its
# token, its store and its own tests went with it. What a deleted test cannot do is stop covering
# behaviour that is still promised, so this fixture covers the promises on the new path:
#
#   * a rank is a tag on the ranked session, and `peers` shows it           (W1, W2)
#   * replacing the rank replaces the *family*, one rank, unrelated tags kept (W3, W4)
#   * `ranks` reads those tags; there is no second store                   (W5)
#   * the vocabulary is this plugin's and an argument does not widen it    (W6)
#   * an address with no registration is refused, nothing written for it   (W7)
#   * `unrank` removes the family and says what stands                      (W8)
#   * `unrank` again is already true, not a failure                        (W9)
#   * the tag survives a **resume**                                        (W10, shell check R1–R3)
#
# One real `eidolon`, one throwaway config, one mock session, one workflow run. No port is bound,
# no token is written, and nothing of the operator's is read or changed. The binary is pinned from
# `EIDOLON_BIN` (never from `PATH`): the fixture's subject includes which build is under test, and
# a `PATH` lookup measures whatever the operator happens to have installed.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"
# The swarm's presence lives under XDG_RUNTIME_DIR, so this fixture takes one of its own whether or
# not the caller's environment already has one: otherwise the run registers beside every real
# session on the machine and reads the operator's peers.
runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP run.sh: set EIDOLON_BIN to a built \`eidolon\` binary (this fixture does not read PATH)" >&2
    exit 3
fi
say_bin=$(sha256sum "$EID" | cut -c1-16)

fail=0
checks=0
skipped=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3] in the answer"; fail=$((fail + 1))
    fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"; fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

say "binary: $EID (sha256 $say_bin)"

# ---- one live session, parked on a FIFO ------------------------------------
# A rank is a tag on a *registration*, so the fixture needs one. A mock `eidolon chat` under this
# fixture's own HOME is that registration; nothing here needs a model.
live_dir=$runtime/live
mkdir -p "$live_dir"
mkfifo "$live_dir/in" 2>/dev/null || true
( sleep 900 >"$live_dir/in" ) & fifo_pid=$!
"$EID" chat --provider mock --cwd "$live_dir" <"$live_dir/in" >"$live_dir/out" 2>&1 & live_pid=$!
live=""
cleanup() {
    kill "$live_pid" "$fifo_pid" 2>/dev/null || true
    wait "$live_pid" "$fifo_pid" 2>/dev/null || true
    rm -rf "$runtime" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 80); do
    live=$(ls -t "$XDG_DATA_HOME"/eidolon/sessions/*.eid 2>/dev/null | head -1 | sed 's|.*/||;s|\.eid$||' || true)
    [ -n "$live" ] || { sleep 0.5; continue; }
    "$EID" peers 2>/dev/null | grep -qF -- "$live" && break
    sleep 0.5
done
if [ -z "$live" ]; then
    say "SKIP the whole witness: no registered session appeared, so there is no registration to carry a tag"
    say "0 checks run"
    exit 3
fi
live_log=$XDG_DATA_HOME/eidolon/sessions/$live.eid
say "live session: $live"

# ---- the plugin, in this fixture's own config ------------------------------
plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/rank-witness.rn" "$plug/workflows/rank-witness.rn"
# Two test-only tools, never shipped: one places a tag through the harness's door and one reads a
# row's tags, because a workflow has no `swarm_call` of its own. Registered the way the other
# fixtures register their seed tools — by editing this fixture's *copy* of the manifest.
cp "$here/rank-fixture/tools/seed_tag.rn" "$here/rank-fixture/tools/dump_tags.rn" "$plug/tools/"
sed -i 's/"rank", "unrank", "ranks", "hook_ready", "hook_peer"\]/"rank", "unrank", "ranks", "hook_ready", "hook_peer", "seed_tag", "dump_tags"]/' "$plug/plugin.rn"
grep -q 'seed_tag' "$plug/plugin.rn" || { say "FAIL the fixture tools are not registered"; exit 1; }
"$EID" plugins trust coordinator >/dev/null

# One real policy row for this one workflow, seeded the way the other fixtures seed theirs: a
# workflow's declared class is an input to the table, not an answer standing in for it, and
# `--yolo` is not this fixture's to spend.
python3 - "$XDG_CONFIG_HOME/eidolon/policy.rn" <<'PY' || { say "FAIL the policy row could not be seeded"; exit 1; }
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
row = '        "workflow:coordinator:rank-witness" => v(ALLOW, "fixture: drive the rank tools", false),\n'
if '        _ => {' not in s:
    raise SystemExit("no fallthrough arm to put the row in front of")
if 'workflow:coordinator:rank-witness' not in s:
    s = s.replace('        _ => {', row + '\n        _ => {', 1)
p.write_text(s)
PY

say "=== driving rank / ranks / unrank through the plugin's own tools ==="
absent=1793000000404
run=$("$EID" workflow run "$plug" rank-witness --provider mock \
    --args "$(printf '{"live":"%s","absent":"%s"}' "$live" "$absent")" 2>&1 || true)
printf '%s\n' "$run" | sed 's/^/     /'

# The run must have COMPLETED before any absence check means anything: a workflow that failed
# outright satisfies every check_absent below, which is how a broken run looks clean.
check "the workflow run completed" "$run" '"status":"completed"'

check "W0 an unrelated tag was placed through the harness's own door" "$run" "W0 OK"
check "W1 the rank lands as a tag on the session" "$run" "W1 OK"
check "W1 and the answer names the tag it now carries" "$run" "rank:legatus"
check "W2 the session's own row publishes it" "$run" "W2 tags"
check "W2 with the unrelated tag still beside it" "$run" "team:forge"
check "W3 a second rank replaces the family" "$run" "W3 OK"
# The report is one JSON string, so its lines are escaped: the W4 line is taken by pattern, not by
# splitting on newlines that are not there.
w4=$(printf '%s' "$run" | grep -o 'W4 tags \[[^]]*\]' | head -1)
say "     $w4"
check "W4 the row now carries the family's new member" "$w4" "rank:miles"
check_absent "W4 and no longer the old one" "$w4" "rank:legatus"
check "W4 the unrelated tag survived the replacement" "$w4" "team:forge"
check "W5 ranks reads it off the roster" "$run" "W5 OK"
check "W5 naming this session" "$run" "$live"
# The schema refuses `admiral` before the body runs, which is the stronger guard: the check is that
# the refusal names the field and the vocabulary, not that some sentence was printed.
check "W6 an unknown rank word is refused by the schema, naming the vocabulary" "$run" "\`rank\` must be one of"
check "W7 an address nobody registered is refused, naming it" "$run" "W7 ERR"
check "W7 and the refusal is core's no-such-session, not a write" "$run" "there is no session"
check "W8 the release reports the set that stands" "$run" "W8 OK"
check "W8 with no rank tag on it" "$run" "no \`rank:\` tag stands"
check "W9 a second release is already true" "$run" "W9 OK"
check "W9 and says nothing was written" "$run" "nothing was written"
check "W10 the session was ranked again for the resume check" "$run" "W10 OK"

# ---- the tag survives a resume --------------------------------------------
# This is the half the retired store used to own by reasserting a decoration on every scan. The tag
# is the session's own now, and the harness keeps it beside the log, so the only question is
# whether a *new process* on the same log publishes it — which is what this restart asks.
say "=== R: the tag across a resume ==="
kill "$live_pid" 2>/dev/null || true
wait "$live_pid" 2>/dev/null || true
resumed=0
"$EID" chat --session "$live_log" --provider mock --cwd "$live_dir" <"$live_dir/in" >"$live_dir/out2" 2>&1 & resumed_pid=$!
for _ in $(seq 1 80); do
    if "$EID" peers 2>/dev/null | grep -qF -- "$live" && "$EID" peers 2>/dev/null | grep -qF "rank:legatus"; then
        resumed=1
        break
    fi
    sleep 0.5
done
# Read the roster *before* the process goes: the row is the registration's, and a killed process has
# none. This is the same rule the fixture states elsewhere — read the thing while it exists.
report=$("$EID" peers 2>&1 | sed -n "/$live/p")
kill "$resumed_pid" 2>/dev/null || true
wait "$resumed_pid" 2>/dev/null || true
checks=$((checks + 1))
if [ "$resumed" -eq 1 ]; then
    say "ok   R1 the resumed session publishes rank:legatus for the same address"
else
    say "FAIL R1 the resumed session does not publish rank:legatus: the tag did not survive the process"
    fail=$((fail + 1))
fi
ranks_seen=$(printf '%s' "$report" | tr ' ' '\n' | grep -c '^rank:' || true)
checks=$((checks + 1))
if [ "$ranks_seen" = 1 ]; then
    say "ok   R2 the resumed row carries exactly one rank tag"
else
    say "FAIL R2 the resumed row carries $ranks_seen rank tag(s), not one: $report"
    fail=$((fail + 1))
fi

say ""
say "checks: $checks, skipped: $skipped, failures: $fail"
[ "$fail" -eq 0 ] || exit 1

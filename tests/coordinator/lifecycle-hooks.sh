#!/usr/bin/env bash
# lifecycle-hooks.sh: the two lifecycle handlers against a real plugin-state store.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/coordinator/lifecycle-hooks.sh
#
# No peer and no monitor: `peer_changed` observations are fabricated and passed to
# the handlers through `eidolon::tool_call`, and the adoption/rank rows are seeded
# by a fixture tool. The store, the state primitive, the derived notice id, the
# dedupe and the decoration call are the real ones. This is evidence about the
# handlers, not about the harness's lifecycle delivery: that a real `peer_changed`
# carries these fields is the brief's contract, and is not measured here.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

eidolon=${EIDOLON_BIN:-}
if [ -z "$eidolon" ]; then
    eidolon=$(command -v eidolon || true)
fi
if [ -z "$eidolon" ] || [ ! -x "$eidolon" ]; then
    echo "SKIP lifecycle-hooks.sh: no eidolon binary (set EIDOLON_BIN)" >&2
    exit 3
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
plug=$work/plugin
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/plugin.rn" "$plug/plugin.rn"
cp -r "$repo/coordinator/tools" "$plug/tools"
cp -r "$repo/coordinator/lib" "$plug/lib"
cp "$here"/lifecycle-fixture/tools/*.rn "$plug/tools/"
cp "$here/lifecycle-hooks.rn" "$plug/workflows/lifecycle-hooks.rn"
# The fixture verbs are shipped only to this throwaway copy, and the manifest is
# what names them; add them here rather than in the shipped plugin.
sed -i 's/"hook_ready", "hook_peer"\]/"hook_ready", "hook_peer", "seed", "read", "me"]/' "$plug/plugin.rn"

installed=$XDG_CONFIG_HOME/eidolon/plugins
mkdir -p "$installed"
rm -rf "$installed/coordinator"
cp -r "$plug" "$installed/coordinator"

# Seed the config and vouch for every verb this throwaway plugin ships. `trust`
# records the yes against each script's bytes, so this is the same act the
# operator's install uses; it is not a permission this test invents.
"$eidolon" plugins trust coordinator --yolo >"$work/trust.log" 2>&1 || {
    cat "$work/trust.log" >&2
    echo "FAIL lifecycle-hooks.sh: could not vouch for the throwaway plugin" >&2
    exit 1
}

status=0
out=$("$eidolon" workflow run "$installed/coordinator" lifecycle-hooks \
    --provider mock --yolo --args '{}' --cwd "$work" 2>&1) || status=$?
printf '%s\n' "$out"

fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3]"
        fail=$((fail + 1))
    fi
}
check_absent() {
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"
        fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

if [ "$status" -ne 0 ]; then
    say "FAIL lifecycle-hooks.sh: workflow exited $status"
    exit 1
fi
check "the workflow completed" "$out" '"status":"completed"'

report=$(printf '%s\n' "$out" | grep '^{' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("report",""))')
check "the first halt writes the service's notice id" "$report" "356845f06c836344"
check "the same occurrence on the next scan is one notice, not two" "$report" "one notice, no second"
check "a completed ending is not a halt" "$report" 'ending `completed` is not a failed'
check "a failed ending while busy is not a stall" "$report" 'while busy: this is not a stalled source'
check "unavailable writes nothing" "$report" "unavailable is not a completion; nothing is written"
check "a gap is recorded as a gap" "$report" "coverage gap recorded; a gap is not a halt"
check "the gap keeps the harness's reason" "$report" "missed two turns"
check "the gap keeps the harness's coverage" "$report" "rc:8..rc:10"
check "an unjournaled registration count is its own occurrence" "$report" "888e6961148e0362"
check "and its key kind is observed" "$report" "key_kind"
check "the watch list is the live adoption" "$report" '"watch":["w1"]'
check "hook_ready attaches the stored rank to the live session" "$report" "decoration Tribunus attached"

count_notices=$(printf '%s' "$report" | grep -o 'notice_id' | wc -l)
count_gaps=$(printf '%s' "$report" | grep -o 'coverage_gap' | wc -l)
if [ "$count_notices" -eq 2 ]; then
    say "ok   two notices were written (two occurrences, one each)"
else
    say "FAIL expected two notice rows, got $count_notices"
    fail=$((fail + 1))
fi
if [ "$count_gaps" -eq 1 ]; then
    say "ok   one gap row was written"
else
    say "FAIL expected one gap row, got $count_gaps"
    fail=$((fail + 1))
fi

# The second ready pass must not rewrite the matching decoration; and the live row
# must carry exactly the stored prefix by this plugin's owner.
second_ready=$(printf '%s' "$report" | grep '^ready second:' || true)
check_absent "a matching decoration is not rewritten" "$second_ready" "decoration Tribunus attached"
check "the live row carries our decoration" "$report" '"controller":"coordinator"'
check "and the stored prefix" "$report" '"text":"Tribunus"'

if [ "$fail" -ne 0 ]; then
    say "FAIL lifecycle-hooks.sh: $fail check(s) failed"
    exit 1
fi
say "lifecycle-hooks.sh: every check holds"

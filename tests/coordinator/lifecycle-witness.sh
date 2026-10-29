#!/usr/bin/env bash
# lifecycle-witness.sh: one witness per condition, through the real handlers.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/lifecycle-witness.sh
#
# Unlike `lifecycle-shape.sh` (pure policy) and `lifecycle-hooks.sh` (handlers, one
# shape), this drives the four readers the runner drives, against a real store and a
# real session log, once per condition C1-C3 of the review: a JSON null in a ledger row
# and in a roster row; a ledger that cannot be read; a notice journaled before its mark
# was lost; and a release racing delivery. A pass is evidence about the handlers and the
# dispatcher, not about the harness's lifecycle delivery.
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
    echo "SKIP lifecycle-witness.sh: no eidolon binary (set EIDOLON_BIN)" >&2
    exit 3
fi
echo "lifecycle-witness.sh: binary $eidolon"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
plug=$work/plugin
mkdir -p "$plug/workflows"
cp "$repo/coordinator/plugin.rn" "$plug/plugin.rn"
cp -r "$repo/coordinator/tools" "$plug/tools"
cp -r "$repo/coordinator/lib" "$plug/lib"
cp "$here"/lifecycle-fixture/tools/*.rn "$plug/tools/"
cp "$here/lifecycle-witness.rn" "$plug/workflows/lifecycle-witness.rn"
sed -i 's/"hook_ready", "hook_peer"\]/"hook_ready", "hook_peer", "seed", "seed_json", "journal", "read", "me"]/' "$plug/plugin.rn"

installed=$XDG_CONFIG_HOME/eidolon/plugins
mkdir -p "$installed"
rm -rf "$installed/coordinator"
cp -r "$plug" "$installed/coordinator"

"$eidolon" plugins trust coordinator --yolo >"$work/trust.log" 2>&1 || {
    cat "$work/trust.log" >&2
    echo "FAIL lifecycle-witness.sh: could not vouch for the throwaway plugin" >&2
    exit 1
}

status=0
out=$("$eidolon" workflow run "$installed/coordinator" lifecycle-witness \
    --provider mock --yolo --args '{}' --cwd "$work" 2>&1) || status=$?
printf '%s\n' "$out"

fail=0
say() { printf '%s\n' "$*"; }
check() { if printf '%s' "$2" | grep -qF -- "$3"; then say "ok   $1"; else say "FAIL $1: wanted [$3]"; fail=$((fail + 1)); fi; }
check_absent() { if printf '%s' "$2" | grep -qF -- "$3"; then say "FAIL $1: must not contain [$3]"; fail=$((fail + 1)); else say "ok   $1"; fi; }

if [ "$status" -ne 0 ]; then
    say "FAIL lifecycle-witness.sh: workflow exited $status"
    exit 1
fi
check "the workflow completed" "$out" '"status":"completed"'
report=$(printf '%s\n' "$out" | grep '^{' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("report",""))')
session=$(printf '%s\n' "$out" | grep '^{' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session",""))')

# ---- W1: a JSON null in a ledger row and in a roster row ----
check "W1: hook_peer reads a row written the way the plan documents it" "$report" "W1 hook_peer: "
check_absent "W1: and does not throw on the documented shape" "$report" "W1 hook_peer ERR"
check_absent "W1: hook_ready does not throw on it either" "$report" "W1 hook_ready ERR"
check_absent "W1: adoptions does not throw on it" "$report" "W1 adoptions ERR"
check "W1: adoptions reaches the live claimant — the roster's null outcome_at no longer kills it" "$report" 'live as `'
check "W1: and every row still carries the marker it can honestly make" "$report" "  store only"
check_absent "W1: notices does not throw on a migrated row with a null answered_at" "$report" "W1 notices ERR"
check "W1: the migrated row keeps the service's word" "$report" "delivered as the service recorded it"

# ---- W2: a ledger that cannot be read is never a false Ok ----
check "W2: an unreadable ledger answers Err" "$report" "W2 hook_peer ERR:"
check "W2: naming the refusal" "$report" "is not a list"
check_absent "W2: never a false terminal Fired" "$report" "W2 hook_peer OK"

# ---- W3: journaled before the mark was lost ----
check "W3: the fixture journaled the notice first" "$report" '"outcome":"written"'
check "W3: the reconcile found the log already holding it" "$report" "the log answered \`already\`"
w3id=$(printf '%s\n' "$report" | sed -n 's/^W3 id: \(.*\)$/\1/p')
record=$(printf '%s\n' "$report" | sed -n 's/^W3 journal: .*"record":\([0-9]*\).*/\1/p')
if [ -n "$w3id" ] && [ -n "$record" ]; then
    say "ok   W3: the delivery id is the ledger's own derivation ($w3id), record $record"
else
    say "FAIL W3: could not read the id and record back from the report"
    fail=$((fail + 1))
fi
check "W3: the row is marked written" "$report" '\"outcome\":\"written\"'
if printf '%s' "$report" | grep -q "\"record\":$record"; then
    say "ok   W3: and keeps the record the log named ($record), so the retry is the same delivery"
else
    say "FAIL W3: the row does not carry record $record"
    fail=$((fail + 1))
fi
if [ -n "$w3id" ]; then
    # Counted by record kind, not by the id's text: the id also appears in the call's
    # own arguments, in the seeded store, and in the replies read back.
    # The session carries other notices too (W1's halt, W4's live row), so the count is
    # of PluginNotice records carrying *this* id.
    count=$("$eidolon" log "$session" --json 2>/dev/null | grep '"PluginNotice"' | grep -c -- "$w3id" || true)
    if [ "${count:-0}" -eq 1 ]; then
        say "ok   W3: exactly one PluginNotice record in the session log — the reconcile deduplicated, no second emission"
    else
        say "FAIL W3: ${count:-0} PluginNotice record(s) in $session, wanted 1"
        fail=$((fail + 1))
    fi
fi

# ---- W4: a release racing delivery ----
check "W4: the open row of a released epoch is cancelled" "$report" '\"notice_id\":\"pendrel1\"'
check "W4: the open row of a live epoch is journaled and marked" "$report" '\"notice_id\":\"pendlive2\"'
check "W4: the notice already in the log is left exactly as it was" "$report" '\"record\":41'
check "W4: notices prints the delivered row as journaled, not retracted" "$report" "journaled — record 41"
check "W4: and the cancelled one honestly" "$report" "cancelled — the epoch was released before this row was closed as journaled"
check_absent "W4: no row claims never sent" "$report" "never sent"

if [ "$fail" -ne 0 ]; then
    say "FAIL lifecycle-witness.sh: $fail check(s) failed"
    exit 1
fi
say "lifecycle-witness.sh: every check holds"

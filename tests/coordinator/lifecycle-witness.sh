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

# ---- W5-W7: the null class, one reader per function ----
check_absent "W5: notices survives a null record — lifecycle_number" "$report" "W5 notices ERR"
check "W5: and says the record number is not stated rather than inventing record 0" "$report" "the record number is not stated here"
check_absent "W6: adoptions survives a null epoch — lifecycle_number" "$report" "W6 adoptions ERR"
check "W6: and renders it as not stated" "$report" "epoch not stated —"
check_absent "W6: never as a number-shaped zero" "$report" "epoch 0 —"
check_absent "W6: the reconcile does not throw on an unstated epoch" "$report" "W6 reconcile ERR"
check "W6: it says the row stays pending because it cannot be matched" "$report" "states no epoch, so it cannot be matched to an adoption and is not cancelled"
check "W6: and the row is still pending in the store — no phantom release" "$report" '\"notice_id\":\"nullep01\"'
# Scoped to W6's own answer: the report also carries W4's *genuine* cancellation, whose
# sentence this must not appear for an unstated epoch.
w6=$(printf '%s\n' "$report" | sed -n '/^W6 notices:/,/^W7 /p')
check "W6: notices says it is owed and unmatched instead" "$w6" "the row states no epoch"
check "W6: and that it stays pending" "$w6" "stays pending rather than being cancelled"
check_absent "W6: never calling an unstated epoch a release" "$w6" "cancelled — the epoch was released"
check_absent "W6: nor printing a number-shaped epoch" "$w6" "(epoch 0)"
check_absent "W7: hook_peer survives a null busy — lifecycle_tristate" "$report" "W7 hook_peer ERR"
check "W7: and refuses to decide a halt from an unknown" "$report" "not stated: an unknown is not a false"
check_absent "W7: so no notice is invented for a peer that is merely not reporting" "$report" "W7 hook_peer: {\"notes\":[\"w8: halted"
check "W7: while a stated busy false still halts (W1, and the hooks suite)" "$report" "W1 hook_peer: "

# ---- W8: the two render-only sentences ----
check_absent "W8: hook_peer decides the release without a fabricated epoch" "$report" "epoch 0 was released"
check "W8: and says the epoch is not stated" "$report" "epoch not stated was released"
w8=$(printf '%s\n' "$report" | sed -n '/^W8 adoptions:/,$p')
check "W8: a start row asks the custody log" "$report" "managed start op-without-epoch"
# Scoped to W8's own answer: the header explains both markers by name, which a whole-report
# grep cannot tell apart from a row claiming one.
check_absent "W8: and does not claim the log verified a start it does not know" "$w8" "  verified against the log"
check "W8: so the marker is narrowed to the log's own refusal" "$w8" "store only — the custody log does not answer for this operation:"
check_absent "W8: adoptions never prints a managed start's epoch as 0" "$w8" "adoption epoch 0"
check "W8: but as not stated" "$w8" "adoption epoch not stated"

# ---- W9: a real adoption, and the handler consuming it ----
check_absent "W9: the adoption goes through the door, not the monitor" "$report" "W9 adopt ERR"
check "W9: the tool wrote the adoption into its own ledger" "$report" "adopted 1793000000042 in epoch 1"
check "W9: and says the ledger is the record, not a mirror" "$report" "This ledger is the record"
w9=$(printf '%s\n' "$report" | sed -n '/^W9 halt:/,/^W9 store after halt:/p')
check_absent "W9: the halt handler reads the row the writer wrote" "$w9" "not adopted by this session"
check "W9: and journals the notice it owes" "$w9" "journaled in this session's log as record"
check "W9: which the ledger holds as written" "$w9" '\"outcome\":\"written\"'
check "W9: keyed on the epoch the door named, not a fabricated one" "$w9" '\"epoch\":1'

# ---- W10: the crash between the two writes, and the retry that reconciles ----
check "W10: the first adoption is durable in the harness" "$report" "in epoch 1"
check "W10: and this ledger held it before the loss" "$report" '\"worker\":\"1793000000043\"'
w10=$(printf '%s\n' "$report" | sed -n '/^W10 halt before repair:/,$p')
check "W10: with the row lost, a halt is honestly unannounced" "$w10" "not adopted by this session"
check "W10: the retry re-establishes the adoption in the ledger" "$w10" "adopted 1793000000043 in epoch 1"
check "W10: at the same epoch, because the epoch is this ledger's own next number" "$w10" "in epoch 1"
check "W10: so the repaired ledger row carries it" "$w10" '\"epoch\":1'
check_absent "W10: with no fabricated epoch anywhere" "$w10" '\"epoch\":0'
check "W10: a halt after the repair is announced" "$w10" "journaled in this session's log as record"
# The harness's own record: exactly one adoption for that worker, because the retry was refused.
# Counted in the plugin's own store on disk, where the adoption now lives: exactly one row for
# that worker after two adopt calls, so a retry cannot duplicate what it re-establishes.
# The *final* store dump, not every dump in the block: the id appears in each one.
w10last=$(printf '%s\n' "$report" | sed -n '/^W10 ledger last:/,$p')
rows=$(printf '%s' "$w10last" | grep -oF '\"worker\":\"1793000000043\",\"epoch\"' | wc -l || true)
if [ "${rows:-0}" -eq 1 ]; then
    say "ok   W10: the ledger holds exactly one row for that worker after both calls"
else
    say "FAIL W10: ${rows:-0} row(s) for that worker in the ledger, wanted 1"
    fail=$((fail + 1))
fi

# ---- W11: release-of-monitoring in the ledger ----
check_absent "W11: the release needs no core verb" "$report" "W11 release ERR"
check "W11: it says monitoring ends and names the epoch" "$report" "monitoring ends now"
check "W11: the epoch's pending notice is cancelled in the same write" "$report" "1 pending notice(s) of that epoch were cancelled"
w11=$(printf '%s\n' "$report" | sed -n '/^W11 store after release:/,$p')
check "W11: the row is marked released by a word, not a fabricated time" "$w11" '\"released\":true'
check "W11: the pending row is cancelled" "$w11" '\"outcome\":\"cancelled\"'
check "W11: the journaled row keeps its record" "$w11" '\"record\":41'
check "W11: a second release refuses rather than releasing twice" "$report" "is not adopted by this session (or was already released)"
check "W11: and a re-adoption is a new epoch" "$report" "adopted 1793000000060 in epoch 2"

if [ "$fail" -ne 0 ]; then
    say "FAIL lifecycle-witness.sh: $fail check(s) failed"
    exit 1
fi
say "lifecycle-witness.sh: every check holds"

#!/usr/bin/env bash
# fresh-ledger.sh: `coordinator_adopt` on a session whose ledger has never been written.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/fresh-ledger.sh
#
# Why this exists. `load_store` used to refuse `revision: 0, value: null` — a ledger nothing has
# written yet — as corrupt, and every test seeded a store first, so the ordinary path (a session
# whose first act is an adoption) was the one nobody ran. Fixing it changed nothing any test could
# see: reverting the fix left every suite green. This script is the witness that can fail.
#
# It assembles a plugin with **no lifecycle handlers** and one tool, `coordinator_adopt`, so
# nothing writes the store before the adoption does, and it runs twice:
#   arm 1 — the tree as it stands: the adoption must be recorded, and the store must exist;
#   arm 2 — the same plugin with the fresh-ledger branch deleted: it must fail.
# A test that only ran arm 1 would be a fix nobody could keep.
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
    echo "SKIP fresh-ledger.sh: no eidolon binary (set EIDOLON_BIN)" >&2
    exit 3
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
installed=$XDG_CONFIG_HOME/eidolon/plugins
mkdir -p "$installed"

# The plugin: one tool, no handlers. `claims` is what registers the verb; nothing else exists
# to touch the store, so the first writer really is the adoption.
assemble() { # assemble <dir> <tool>
    local dir=$1 tool=$2
    mkdir -p "$dir/tools" "$dir/workflows"
    cp "$tool" "$dir/tools/adopt.rn"
    cat >"$dir/plugin.rn" <<'EOF'
pub fn manifest() {
    #{
        name: "coordinator",
        version: "0.0.0",
        description: "fresh-ledger witness: one adoption tool and no handlers",
        claims: #{ tools: ["adopt"] },
    }
}
EOF
    cat >"$dir/workflows/adopt_once.rn" <<'EOF'
pub fn workflow() {
    #{ name: "adopt_once", description: "adopt one worker on a ledger nothing has written", approval: "read_only", params: #{ "type": "object", "properties": #{} } }
}
pub async fn run(args) {
    let out = "";
    match eidolon::tool_call("coordinator_adopt", #{ "session": "1793000000050", "name": "freshling" }).await {
        Ok(v) => out += `ADOPT OK: ${v}\n`,
        Err(e) => out += `ADOPT ERR: ${e}\n`,
    }
    Ok(out)
}
EOF
}

run_arm() { # run_arm <label> <plugin dir> -> sets ARM_REPORT and ARM_STORE
    local label=$1 dir=$2
    rm -rf "$installed/coordinator"
    cp -r "$dir" "$installed/coordinator"
    "$eidolon" plugins trust coordinator --yolo >"$work/trust-$label.log" 2>&1 || {
        cat "$work/trust-$label.log" >&2
        echo "FAIL fresh-ledger.sh: could not vouch for the throwaway plugin ($label)" >&2
        exit 1
    }
    local status=0 out
    out=$("$eidolon" workflow run "$installed/coordinator" adopt_once --provider mock --yolo \
        --args '{}' --cwd "$work" 2>&1) || status=$?
    printf '%s\n' "$out" >"$work/out-$label.txt"
    local session
    session=$(printf '%s\n' "$out" | grep '^{' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session",""))' 2>/dev/null || true)
    ARM_REPORT=$(printf '%s\n' "$out" | grep '^{' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("report",""))' 2>/dev/null || true)
    ARM_STORE=""
    if [ -n "$session" ]; then
        ARM_STORE="$XDG_DATA_HOME/eidolon/plugin-state/coordinator/$(basename "$session" .eid)/store.json"
    fi
    ARM_STATUS=$status
}

fail=0
say() { printf '%s\n' "$*"; }
dump() { # dump <label>: the raw run, so a red arm says why rather than only that
    say "     --- raw run ($1) ---"
    sed 's/^/       /' "$work/out-$1.txt" 2>/dev/null | tail -6
}

# ---- arm 1: the tree as it stands must record the adoption on a fresh ledger ----
assemble "$work/plugin" "$repo/coordinator/tools/adopt.rn"
run_arm green "$work/plugin"
say "arm 1 (fresh ledger, the tree): ADOPT answer"
printf '%s\n' "$ARM_REPORT" | head -3
if printf '%s' "$ARM_REPORT" | grep -qF "This ledger is the record"; then
    say "ok   arm 1: the adoption was recorded on a ledger nothing had written"
else
    say "FAIL arm 1: the adoption was not recorded: $(printf '%s' "$ARM_REPORT" | tr '\n' ' ' | cut -c1-200)"
    fail=$((fail + 1))
fi
if printf '%s' "$ARM_REPORT" | grep -qF "This ledger is the record"; then
    say "ok   arm 1: and the answer says the ledger is the record, not a mirror of a harness one"
else
    say "FAIL arm 1: the answer does not say the ledger is the record"
    fail=$((fail + 1))
fi
if [ -n "$ARM_STORE" ] && [ -f "$ARM_STORE" ] && grep -qF '1793000000050' "$ARM_STORE"; then
    say "ok   arm 1: the store on disk holds the adoption row"
else
    say "FAIL arm 1: no store with the adoption row at $ARM_STORE"
    fail=$((fail + 1))
fi

# ---- arm 2: the same plugin with the fresh-ledger branch deleted must fail ----
mkdir -p "$work/reverted"
python3 - "$repo/coordinator/tools/adopt.rn" "$work/reverted/adopt.rn" <<'PY'
import sys, pathlib
src = pathlib.Path(sys.argv[1]).read_text()
start = src.index("            if lifecycle_null(v) {")
end = src.index("            } else if v is String {")
# Delete the branch and keep the rest: exactly the state the fix was reverted to.
pathlib.Path(sys.argv[2]).write_text(src[:start] + "            if v is String {" + src[end + len("            } else if v is String {"):])
PY
# The branch is identified by its comment, not by `if lifecycle_null(v) {`: the generated
# `lifecycle_text` carries the same condition, so a grep for the condition alone finds code
# that is not the fix and would report a successful revert as a no-op.
if grep -q "Nothing written yet" "$work/reverted/adopt.rn"; then
    say "FAIL proof arm: the branch was not removed, so this arm proves nothing"
    fail=$((fail + 1))
else
    cp -r "$work/plugin" "$work/plugin-reverted" 2>/dev/null || true
    assemble "$work/plugin-reverted" "$work/reverted/adopt.rn"
    run_arm reverted "$work/plugin-reverted"
    if printf '%s' "$ARM_REPORT" | grep -qF "This ledger is the record"; then
        say "FAIL proof arm: the adoption was still recorded with the fix removed — the test cannot fail"
        fail=$((fail + 1))
    else
        say "ok   proof arm: with the fresh-ledger branch removed the very same run fails to record"
        printf '%s' "$ARM_REPORT" | tr '\n' ' ' | cut -c1-220 | sed 's/^/       /'
        say ""
    fi
fi

if [ "$fail" -ne 0 ]; then
    dump green
    say "FAIL fresh-ledger.sh: $fail check(s) failed"
    exit 1
fi
say "fresh-ledger.sh: every check holds, and the witness fails when the fix is removed"

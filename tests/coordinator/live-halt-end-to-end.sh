#!/usr/bin/env bash
# live-halt-end-to-end.sh — witness-map row 18's **end-to-end** half: the harness's own
# `peer_changed` observation, delivered by the real sampler to the real handler, writing the
# notice the adopted worker's halt is owed.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/.config XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-halt-end-to-end.sh
#
# WHY THIS EXISTS. Every other coordinator fixture calls a handler with an observation it
# fabricated, so what they prove is the handler. Nothing proved the *seam*: that a live session
# with this plugin loaded samples the roster, decides an adopted worker has halted, and dispatches
# `hook_peer` with the fields the handler reads. `crates/core/tests/lifecycle.rs` drives the
# sampler against a fake host and `crates/cli/src/lifecycle.rs` binds the real one; the two halves
# were never joined in one run. This joins them.
#
# The shape, and why each part is the way it is:
#
#   W  a real session that hits its iteration wall and stays alive at `stuck` — a real halt with a
#      real `last`, not a fabricated payload.
#   C  a real session with the plugin installed, which is therefore a real sampler: the launch
#      line below names the two handlers it bound.
#   C is stopped and **resumed under the same id** on purpose. The watch set is the handler's
#      answer to `plugin_ready`, so an adoption written into C's ledger after launch is not
#      watched until a handler runs again; resuming re-announces `plugin_ready` with a new
#      instance occurrence, and the handler reconciles its ledger against the world. That is the
#      documented reconcile path — a resumed session asks its handlers what to watch — rather than
#      a test reaching past it.
#   The adoption row is seeded by writing the plugin's own store file, because this fixture owns
#     the store root (`EIDOLON_PLUGIN_STATE_DIR`) and cannot mint C's id before C exists. The
#     format is `plugin_state`'s, read back below rather than assumed.
#
# What this does NOT claim: that a halt is noticed within a bound, that a wake reached a
# conversation, or that the retired monitor's transport semantics survived. It claims the
# occurrence reached a ledger row through the harness's own path.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-halt-end-to-end.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi

# The fixture owns its runtime dir whether or not the caller's environment has one: a session
# that registers beside the operator's own would make C's roster the machine's, not this run's.
runtime=$(mktemp -d); export XDG_RUNTIME_DIR=$runtime
export EIDOLON_PLUGIN_STATE_DIR=$XDG_DATA_HOME/eidolon/plugin-state
# One iteration: W's first turn is its whole budget, so it settles at `stuck` and stays alive.
mkdir -p "$XDG_CONFIG_HOME/eidolon"
printf 'max_iterations = 1\n' >"$XDG_CONFIG_HOME/eidolon/config.toml"

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug"
cp -r "$repo/coordinator/." "$plug/"
"$EID" plugins trust coordinator >/dev/null 2>&1 || { echo "FAIL live-halt-end-to-end.sh: could not vouch for the plugin" >&2; exit 1; }

say() { printf '%s\n' "$*"; }
fail=0
check() { # check <name> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then say "ok   $1"; else say "FAIL $1: wanted [$3]"; fail=$((fail + 1)); fi
}
check_absent() {
    if printf '%s' "$2" | grep -qF -- "$3"; then say "FAIL $1: must not contain [$3]"; fail=$((fail + 1)); else say "ok   $1"; fi
}

work=$(mktemp -d)
pids=()
cleanup() {
    for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$runtime" "$work" 2>/dev/null || true
    :
}
trap cleanup EXIT

# start_chat <name> [log path] — a chat session parked on its own FIFO, so it stays alive and keeps
# sampling. Nothing is written to the FIFO unless a caller wants a turn.
start_chat() {
    local name=$1 session=${2:-} dir=$work/$1
    mkdir -p "$dir"
    # A resumed session re-uses its own directory, so the FIFO from the first launch is still
    # there; the old writer keeps its deleted inode and this launch gets a fresh one.
    rm -f "$dir/in"
    mkfifo "$dir/in"
    ( sleep 900 >"$dir/in" ) & pids+=("$!")
    if [ -n "$session" ]; then
        "$EID" chat --session "$session" --provider mock --cwd "$dir" <"$dir/in" >"$dir/out" 2>&1 &
    else
        "$EID" chat --provider mock --cwd "$dir" <"$dir/in" >"$dir/out" 2>&1 &
    fi
    LAST_CHAT_PID=$!
    pids+=("$LAST_CHAT_PID")
}
# The newest session log is the one the launch just minted; a log file is not a registration, so
# every caller then waits for the roster.
newest_session() { ls -t "$XDG_DATA_HOME"/eidolon/sessions/*.eid 2>/dev/null | head -1 | sed 's|.*/||;s|\.eid$||'; }
wait_roster() { # wait_roster <session>
    local i; for i in $(seq 1 80); do
        "$EID" peers 2>/dev/null | grep -qF -- "$1" && return 0
        sleep 0.25
    done
    return 1
}

# ---- W: a real halt ------------------------------------------------------
before=$(newest_session || true)
start_chat w
w=""
for _ in $(seq 1 80); do w=$(newest_session || true); [ -n "$w" ] && [ "$w" != "$before" ] && break; w=""; sleep 0.25; done
if [ -z "$w" ]; then say "SKIP the end-to-end check: no session log appeared for W"; exit 3; fi
wait_roster "$w" || { say "SKIP the end-to-end check: W never appeared on this fixture's roster"; exit 3; }
# One turn, and only one: W's budget is one iteration, so it settles at `stuck` and lives on.
printf 'a single turn\n' >"$work/w/in" 2>/dev/null || true
stuck=0
for _ in $(seq 1 120); do
    line=$("$EID" peers 2>/dev/null | grep -F -- "$w" || true)
    if printf '%s' "$line" | grep -qF -- "stuck"; then stuck=1; break; fi
    sleep 0.25
done
if [ "$stuck" -ne 1 ]; then
    say "SKIP the end-to-end check: W never reached a stuck halt (it is a fixture fact, not a defect)"
    exit 3
fi
say "W = $w, stuck"

# ---- C: the adopting session, and the resume that arms the watch ----------
before=$(newest_session || true)
start_chat c
c=""
for _ in $(seq 1 80); do c=$(newest_session || true); [ -n "$c" ] && [ "$c" != "$before" ] && [ "$w" != "$c" ] && break; c=""; sleep 0.5; done
if [ -z "$c" ]; then say "SKIP the end-to-end check: no session log appeared for C"; exit 3; fi
wait_roster "$c" || { say "SKIP the end-to-end check: C never appeared on this fixture's roster"; exit 3; }
# A roster registration is not a loaded handler, and the registration comes first: wait for the
# sampler's own launch line and for the store its reconcile wrote, rather than sampling a moment
# that raced them.
store=$EIDOLON_PLUGIN_STATE_DIR/coordinator/$c/store.json
for _ in $(seq 1 120); do
    [ -f "$store" ] && grep -qF -- "hook_peer" "$work/c/out" 2>/dev/null && break
    sleep 0.25
done
# The seam is only a seam if the sampler bound the shipped handlers: its own launch line says so.
check "W18: C is a real sampler over the shipped plugin" "$(cat "$work/c/out" 2>/dev/null)" \
    "coordinator:peer_changed -> coordinator_hook_peer"
say "C = $c"

# ---- seed the adoption into C's own ledger, then resume C ----------------
# C is stopped first: the store is written by one writer at a time, and a write racing C's own
# compare-and-set would be a fixture defect reported as a product one.
kill "$LAST_CHAT_PID" 2>/dev/null || true
sleep 1
if [ ! -f "$store" ]; then
    say "FAIL W18: the store C wrote at launch is not where the primitive says it is: $store"
    exit 1
fi
python3 - "$store" "$c" "$w" <<'PY'
import json, sys
path, coordinator, worker = sys.argv[1], sys.argv[2], sys.argv[3]
doc = json.load(open(path))
value = json.loads(doc["value"])
value["adoptions"] = [{
    # The shape `adopt.rn` writes, and nothing else: `released_at_ms` and `release_reason` are
    # filled in by a release, so seeding them on a live adoption handed the reader fields the
    # product would not have written.
    "worker": worker, "name": "haltling", "note": "", "epoch": 1,
    "coordinator": coordinator,
}]
doc["value"] = json.dumps(value)
json.dump(doc, open(path, "w"))
print("seeded adoption of %s in epoch 1" % worker)
PY

start_chat c "$XDG_DATA_HOME/eidolon/sessions/$c.eid"
notice=0
for _ in $(seq 1 240); do
    if python3 - "$store" <<'PY' 2>/dev/null
import json, sys
value = json.loads(json.load(open(sys.argv[1]))["value"])
sys.exit(0 if value.get("notices") else 1)
PY
    then notice=1; break; fi
    sleep 0.5
done

if [ "$notice" -ne 1 ]; then
    say "FAIL W18: the harness's own halt observation never reached the adopted worker's notice row"
    say "the store after the wait: $(cat "$store" 2>/dev/null)"
    say "C's output (tail): $(tail -5 "$work/c/out" 2>/dev/null)"
    exit 1
fi

rows=$(python3 - "$store" <<'PY'
import json, sys
value = json.loads(json.load(open(sys.argv[1]))["value"])
for n in value.get("notices", []):
    print(json.dumps(n))
PY
)
say "notices: $rows"
check "W18: the notice names the worker that actually halted" "$rows" "\"worker\": \"$w\""
check "W18: and it is about that worker's halt, not a gap" "$rows" '"kind": "stuck"'
count_rows=$(printf '%s\n' "$rows" | grep -c '"worker"' || true)
if [ "${count_rows:-0}" -eq 1 ]; then
    say "ok   W18: exactly one notice row, for the one worker that halted"
else
    say "FAIL W18: ${count_rows:-0} notice row(s) for one halt"
    fail=$((fail + 1))
fi
check "W18: the handler answered the reconcile, not a fabricated payload" "$rows" '"key_kind": "outcome"'
# The commit's list of what was measured named `last`, `occurrence`, `outcome` and `record`; the
# first cut printed them and asserted none of them, so a notice carrying the wrong ending stayed 6/6
# green (the reviewer's mutation). Each one is a check now.
check "W18: it names the ending the harness actually observed" "$rows" '"last": "iteration limit"'
# **`occurrence` is shape only, and says so.** The token is the host's own counter and the only
# place it appears in this fixture is the same row this check reads, so a wrong token and a right
# one are indistinguishable here: the reviewer made it `rc:99` and the check stayed green. What it
# can still witness is that the row carries an occurrence at all rather than an empty field, which
# is what the label now claims. The value is checked where a value exists — `last` above, and
# `record` below against the id the log assigned.
check "W18: and the row carries an occurrence (shape only: the token exists nowhere else)" "$rows" '"occurrence": "rc:'
check "W18: the ledger's word for it is written, not owed" "$rows" '"outcome": "written"'
check_absent "W18: the row claims no delivery the log did not record" "$rows" '"outcome": "delivered"'
# The record, if the sink journaled it: a notice written down is what the ledger's `written`
# outcome claims, and the log is the only place that claim can be checked.
# `log` takes the session log, not its id.
plugin_notices=$("$EID" log "$XDG_DATA_HOME/eidolon/sessions/$c.eid" --json 2>/dev/null | grep -F '"PluginNotice"' || true)
logged=$(printf '%s\n' "$plugin_notices" | grep -c . || true)
# "Exactly one", as the commit says: `>= 1` would pass on a fixture that journaled the same notice
# twice, which is what the notice's dedupe exists to prevent.
if [ "${logged:-0}" -eq 1 ]; then
    say "ok   W18: exactly one PluginNotice in C's own log"
else
    say "FAIL W18: ${logged:-0} PluginNotice record(s) in C's log, wanted exactly 1"
    fail=$((fail + 1))
fi

# ---- the record, as a **value** and not as a shape ----
# The commit claimed `record` was measured; the first cut asserted only that the key was present, so
# the reviewer made the row say `51` against the log's `11` and stayed 11/11 green. The number is the
# host's: `eidolon::notice` answers with the `RecordId` the notice was appended as, and the row may
# not invent one. Read from the log's own `PluginNotice` record, whose `id` is assigned there, and
# compare the two exactly — a substring test cannot tell `"record": 1` from `"record": 11`.
# `python3 -c` with single-quoted code, not a nested heredoc: an inner `<<'PY'` cannot live inside an
# outer one, which is where the first attempt at this died.
record_id=$(printf '%s\n' "$plugin_notices" | python3 -c 'import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    record = json.loads(line)
    kind = record.get("kind")
    if isinstance(kind, dict) and "PluginNotice" in kind:
        print(record.get("id", ""))
        break
')
row_record=$(printf '%s\n' "$rows" | python3 -c 'import json, sys
for line in sys.stdin:
    line = line.strip()
    if line:
        print(json.loads(line).get("record", ""))
        break
')
if [ -n "$record_id" ] && [ "$row_record" = "$record_id" ]; then
    say "ok   W18: the row keeps the record the log named (record $record_id)"
else
    say "FAIL W18: the row's record is [${row_record:-absent}] and the log's PluginNotice is record [${record_id:-absent}]: the id is the host's and the row may not invent one"
    fail=$((fail + 1))
fi

if [ "$fail" -ne 0 ]; then
    say "FAIL live-halt-end-to-end.sh: $fail check(s) failed"
    exit 1
fi
say "live-halt-end-to-end.sh: every check holds"

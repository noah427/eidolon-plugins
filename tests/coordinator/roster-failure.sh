#!/usr/bin/env bash
# roster-failure.sh: what an adoption does when the roster itself cannot be read —
# the distinction the review asked for, at the service level rather than in a unit
# test. A stand-in `eidolon` whose `peers --json` fails must make `adopt` *refuse
# before anything is written*, and say that the roster was unreadable rather than
# that no live session carries the address. Those are different facts and a reader
# must not have to guess which one happened.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/coordinator/roster-failure.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

fail=0
say() { printf '%s\n' "$*"; }
check() {
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3] in:"
        printf '%s\n' "$2" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
}
check_absent() {
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]:"
        printf '%s\n' "$2" | sed 's/^/     /'
        fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

port=
for p in 8095 8094 8095 8096 8097 8098; do
    if ! timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$p" 2>/dev/null; then
        port=$p
        break
    fi
done
[ -n "$port" ] || { say "FAIL no free port"; exit 1; }

work=$(mktemp -d)
monitor=
cleanup() {
    [ -n "$monitor" ] && kill "$monitor" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# A harness that cannot read its roster: the scan fails, and the failure is loud.
cat >"$work/eidolon" <<'FAKE'
#!/bin/sh
case "$1" in
peers)
    echo "fake eidolon: the roster is unavailable in this fixture" >&2
    exit 3
    ;;
*)
    echo "fake eidolon: unhandled: $*" >&2
    exit 2
    ;;
esac
FAKE
chmod +x "$work/eidolon"

if [ -n "${EIDOLON_COORDINATOR_BIN:-}" ]; then
    bin=$EIDOLON_COORDINATOR_BIN
else
    say "building the monitor with cargo"
    (cd "$repo" && cargo build -q -p eidolon-coordinator)
    bin=$repo/target/debug/eidolon-coordinator
fi

root=$work/state
export EIDOLON_COORDINATOR_ROOT=$root
export EIDOLON_COORDINATOR_BIN=$work/eidolon
export EIDOLON_COORDINATOR_PORT=$port
export EIDOLON_COORDINATOR_INTERVAL_S=1
export EIDOLON_COORDINATOR_TOKEN=$XDG_CONFIG_HOME/eidolon/coordinator.token

token=$("$bin" --print-token | sed -n '1p')
[ -n "$token" ] || { say "FAIL the monitor made no token"; exit 1; }
"$bin" >"$work/monitor.log" 2>&1 &
monitor=$!
for _ in $(seq 1 50); do
    if timeout 2 curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then break; fi
    sleep 0.1
done

adopt=$(curl -sS --max-time 10 -X POST "http://127.0.0.1:$port/call" \
    -H "Authorization: Bearer $token" -H 'content-type: application/json' \
    -d '{"method":"adopt","args":{"caller":"/c.eid","session":"/w.eid"}}')
say "the answer was: $adopt"
check "an unreadable roster refuses the adoption" "$adopt" '"ok":false'
check "and it says the roster could not be read" "$adopt" "the roster could not be read"
check "and that an unread roster is not an absent worker" "$adopt" "An unread roster is not an absent worker"
check "and that nothing was written" "$adopt" "nothing was written"
check_absent "it does not claim the worker is missing" "$adopt" "No live session carries that address"
check_absent "and it is not a success" "$adopt" '"ok":true'

say "=== the store on disk after the refusal ==="
if [ -f "$root/state.json" ]; then
    python3 - "$root/state.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
print("records:", len(d.get("records",[])), "notices:", len(d.get("notices",[])))
PY
else
    say "no store file at all — nothing was written"
fi
records=$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1])).get("records",[])))' "$root/state.json" 2>/dev/null || echo "none")
if [ "$records" = "0" ] || [ "$records" = "none" ]; then
    say "ok   no adoption was recorded"
else
    say "FAIL an adoption was recorded despite the refusal"
    fail=$((fail + 1))
fi

check "the monitor is still running (a roster failure is not a poison)" \
    "$(kill -0 "$monitor" 2>/dev/null && echo alive || echo gone)" "alive"
check "and its scan said why it saw nothing" "$(cat "$work/monitor.log")" "roster could not be read"

say ""
if [ "$fail" -eq 0 ]; then
    say "roster-failure proof complete: refused before mutation, and said which failure it was"
else
    say "roster-failure proof FAILED: $fail check(s)"
fi
exit $((fail > 0))

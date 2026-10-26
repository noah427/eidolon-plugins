#!/usr/bin/env bash
# ack-failure.sh: what the *process* does when the store cannot be written after a
# send has already been handed over — the case a unit test can only describe.
#
# The stand-in `eidolon` makes the store's directory unwritable as it answers the
# send, and only then: the scan's write-ahead write has already succeeded, the notice
# has gone out, and the acknowledgement cannot be written. The live process then holds
# an answer the file does not, which is exactly the state that must not be served.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/coordinator/ack-failure.sh
#
# What it measures: the process says why and **exits non-zero**, and the file it left
# behind still holds the notice as `pending` under the same `notice_id`.
#
# What it does NOT measure: that a restart then retries it. No second monitor process is
# started against that store. The retry-after-reload half is the unit test
# `a_crash_after_the_send_and_before_the_ack_repeats_the_same_id` — a fresh store loaded
# from the file attempts the same id again, word for word — and the real process restart
# *and* retry is unverified.
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

# Fixed, like the plugin declares it — and deliberately NOT isolated with a user
# namespace: this fixture's subject is a store the process must fail to write, and
# `unshare -rn` maps the caller to uid 0, which writes into a read-only directory
# regardless. It needs a free 8095 on the host rather than a private one.
port=8095
if timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null; then
    say "FAIL 127.0.0.1:$port is already answering, and the monitor cannot use another port"
    exit 1
fi

work=$(mktemp -d)
pid=
cleanup() {
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    chmod -R u+w "$work" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

root=$work/state
worker=$work/w.eid
rows=$work/rows.json
sendlog=$work/sends.txt
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":7,
  "outcome_observed":null,"started_ms":null,"parked":null,"reached":true,
  "repo":"$work","log":"$work/w.eid"}]
EOF

cat >"$work/eidolon" <<'FAKE'
#!/bin/sh
case "$1" in
peers)
    printf '{"me":null,"sessions":'
    cat "$FAKE_ROWS"
    printf '}\n'
    ;;
send)
    text=$(cat)
    { printf 'argv: %s\n' "$*"; printf 'text: %s\n' "$text"; } >>"$FAKE_SEND_LOG"
    # The acknowledgement is about to fail: the store's directory stops being
    # writable as this answer goes out — and only now, so the write-ahead write in
    # the same scan has already succeeded.
    chmod 0555 "$FAKE_ROOT"
    printf '{"outcome":"delivered","to":"%s","delivered":[],"queued":[],"failed":[]}\n' "$FAKE_ROOT"
    ;;
*)
    echo "fake eidolon: unhandled: $*" >&2
    exit 2
    ;;
esac
FAKE
chmod +x "$work/eidolon"
export FAKE_ROWS=$rows FAKE_SEND_LOG=$sendlog FAKE_ROOT=$root

if [ -n "${EIDOLON_COORDINATOR_BIN:-}" ]; then
    bin=$EIDOLON_COORDINATOR_BIN
else
    say "building the monitor with cargo"
    (cd "$repo" && cargo build -q -p eidolon-coordinator)
    bin=$repo/target/debug/eidolon-coordinator
fi

export EIDOLON_COORDINATOR_ROOT=$root
export EIDOLON_COORDINATOR_BIN=$work/eidolon
export EIDOLON_COORDINATOR_INTERVAL_S=1
export EIDOLON_COORDINATOR_TOKEN=$XDG_CONFIG_HOME/eidolon/coordinator.token

token=$("$bin" --print-token | sed -n '1p')
[ -n "$token" ] || { say "FAIL the monitor made no token"; exit 1; }

"$bin" >"$work/monitor.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do
    if timeout 2 curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then break; fi
    sleep 0.1
done

curl -sS -X POST "http://127.0.0.1:$port/call" \
    -H "Authorization: Bearer $token" -H 'content-type: application/json' \
    -d "{\"method\":\"adopt\",\"args\":{\"caller\":\"$work/coord.eid\",\"session\":\"$worker\"}}" \
    >"$work/adopt.json" 2>&1 || true
check "the adoption was recorded before the send" "$(cat "$work/adopt.json")" '"ok":true'

# Wait for the process to stop itself.
for _ in $(seq 1 80); do
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.25
done
if kill -0 "$pid" 2>/dev/null; then
    say "FAIL the monitor was still running after an acknowledgement it could not write"
    fail=$((fail + 1))
else
    code=0
    wait "$pid" || code=$?
    say "ok   the monitor stopped itself (exit $code)"
    if [ "$code" -ne 0 ]; then
        say "ok   and not with success"
    else
        say "FAIL it exited 0, so a script would read that as fine"
        fail=$((fail + 1))
    fi
fi

log=$(cat "$work/monitor.log")
check "it says why it stopped" "$log" "a monitor that cannot write its store"
check "and it says the answer could not be persisted" "$log" "could not be persisted"
check "and it does not claim the notice is pending on disk" "$log" "not evidence either way"

# What a restart reads.
chmod 0755 "$root" 2>/dev/null || true
onfile=$(cat "$root/state.json")
check "the file still holds the notice as pending" "$onfile" '"outcome": "pending"'
check "and the send really did happen" "$(grep -c '^argv:' "$sendlog" || true)" "1"

say ""
if [ "$fail" -eq 0 ]; then
    say "ack-failure proof complete: the monitor stops rather than serving state it cannot vouch for"
else
    say "ack-failure proof FAILED: $fail check(s)"
fi
exit $((fail > 0))

#!/usr/bin/env bash
# run.sh: the coordinator plugin's fixture. NO real session, NO real peer, NO
# network beyond 127.0.0.1, and nothing of the operator's read or written.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/coordinator/run.sh
#
# Two things are checked, and they are different kinds of evidence.
#
# (1) The monitor, end to end, against a stand-in harness: a fake `eidolon`
#     answers `peers --json` from a fixture file and answers `send --json` with
#     the harness's typed outcome. The real service binary is started on a port of
#     its own with its own state root, and its four doors are driven with curl.
#     This is the whole plumbing — adoption, one notice per occurrence, no repeat,
#     the errored-ending fallback and its stated limit, release, and the typed
#     outcome stored as given. It is NOT evidence about the harness: the rows are a
#     fixture.
#
# (2) The shipped bytes fail closed. The plugin is installed into a throwaway
#     config and a workflow calls `coordinator_adoptions` against the real harness:
#     the refusal must name what the harness does not publish for this session
#     rather than answer as if nobody had been adopted.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"



fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3] in:"; printf '%s\n' "$2" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]:"
        printf '%s\n' "$2" | sed 's/^/     /'
        fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

# Every door a coordinator tool dials, the port the plugin declares, and the
# port the monitor defaults to must be the same number. A mixed pair passes
# every test that exercises only the tools on one side of the split, and fails
# in service with "not answering" against a monitor that is running fine.
declared_port=$(sed -n 's/.*readiness: #{ port: \([0-9]*\).*/\1/p' "$repo/coordinator/plugin.rn" | head -1)
default_port=$(sed -n 's/.*const DEFAULT_PORT: u16 = \([0-9]*\).*/\1/p' "$repo/coordinator/service/src/main.rs" | head -1)
for tool in "$repo"/coordinator/tools/*.rn; do
    dialed=$(sed -n 's/.*base_url: "http:\/\/127\.0\.0\.1:\([0-9]*\)".*/\1/p' "$tool" | head -1)
    [ -z "$dialed" ] && continue
    if [ "$dialed" != "$declared_port" ] || [ "$dialed" != "$default_port" ]; then
        say "FAIL: $(basename "$tool") dials $dialed, plugin declares ${declared_port:-none}, monitor defaults to ${default_port:-none}"
        fail=$((fail + 1))
    fi
done
say "every coordinator tool, the declaration and the monitor default agree on port ${declared_port:-none}"
check_count() { # check_count <name> <haystack> <expected> <pattern>
    local got
    got=$(printf '%s' "$2" | grep -c -- "$4" || true)
    if [ "$got" = "$3" ]; then
        say "ok   $1"
    else
        say "FAIL $1: wanted $3 lines matching [$4], got $got"
        printf '%s\n' "$2" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
}

# ---- the monitor's port: fixed, and this run must own it. ------------------
# 8095 is what `plugin.rn` declares and what every tool's `service()` dials, and the
# monitor cannot be told to use another — so the isolation cannot be a number this
# script picks. If something is already answering there, this run is not isolated and
# says so instead of quietly testing a split. A coordinator on the host is what that
# usually means: run this inside `unshare -rn` and 8095 is nobody's until the monitor
# below takes it (README, "The port").
service_port=8095
if timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$service_port" 2>/dev/null; then
    say "FAIL 127.0.0.1:$service_port is already answering, and the monitor cannot use another port"
    exit 1
fi
say "monitor port: $service_port"

work=$(mktemp -d)
monitor_pid=
cleanup() {
    [ -n "$monitor_pid" ] && kill "$monitor_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# ---- the stand-in harness --------------------------------------------------
worker=$work/w.eid
other=$work/other.eid
rows=$work/rows.json
sendlog=$work/sends.txt
outcomefile=$work/outcome.txt
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":7,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
echo queued >"$outcomefile"

cat >"$work/eidolon" <<'FAKE'
#!/bin/sh
# A stand-in for the harness: the rows the structured roster publishes, and the
# typed outcome `send --json` prints. Nothing else is answered.
case "$1" in
peers)
    [ "${2:-}" = "--json" ] || { echo "fake eidolon: peers without --json" >&2; exit 2; }
    # The object the harness prints: `me` is null for a scan with no session of
    # its own, and `sessions` is the same list, in the same order, as before.
    printf '{"me":null,"sessions":'
    cat "$FAKE_ROWS"
    printf '}\n'
    ;;
send)
    recipient=
    for a in "$@"; do [ "$a" = "-" ] || recipient=$a; done
    { printf 'argv: %s\n' "$*"; printf 'text: %s\n' "$(cat)"; } >>"$FAKE_SEND_LOG"
    case "$(cat "$FAKE_SEND_OUTCOME")" in
    refused)
        # The harness's refusal shape: JSON carrying its own words, and a
        # non-zero exit. A refusal is not an outcome, and it is not retried.
        printf '{"outcome":"refused","error":"`1791` is the session id of 2 live sessions (`a`, `b`) - name one of those"}\n'
        exit 1
        ;;
    *)
        printf '{"outcome":"%s","to":"%s","delivered":[],"queued":[],"failed":[]}\n' \
            "$(cat "$FAKE_SEND_OUTCOME")" "$recipient"
        ;;
    esac
    ;;
*)
    echo "fake eidolon: unhandled: $*" >&2
    exit 2
    ;;
esac
FAKE
chmod +x "$work/eidolon"
export FAKE_ROWS=$rows FAKE_SEND_LOG=$sendlog FAKE_SEND_OUTCOME=$outcomefile

if [ -n "${EIDOLON_COORDINATOR_BIN:-}" ]; then
    bin=$EIDOLON_COORDINATOR_BIN
else
    say "building the monitor with cargo"
    (cd "$repo" && cargo build -q -p eidolon-coordinator)
    bin=$repo/target/debug/eidolon-coordinator
fi
if [ ! -x "$bin" ]; then say "FAIL no monitor binary at $bin"; exit 1; fi

root=$work/state
export EIDOLON_COORDINATOR_ROOT=$root
export EIDOLON_COORDINATOR_BIN=$work/eidolon
export EIDOLON_COORDINATOR_INTERVAL_S=1
export EIDOLON_COORDINATOR_TOKEN=$XDG_CONFIG_HOME/eidolon/coordinator.token

token=$("$bin" --print-token | sed -n '1p')
if [ -z "$token" ]; then say "FAIL the monitor did not make a token"; exit 1; fi
check "the token file is 0600" "$(stat -c %a "$EIDOLON_COORDINATOR_TOKEN")" "600"

"$bin" >"$work/monitor.log" 2>&1 &
monitor_pid=$!
for _ in $(seq 1 50); do
    if timeout 2 curl -fsS "http://127.0.0.1:$service_port/health" >/dev/null 2>&1; then break; fi
    sleep 0.1
done

call() { # call <method> <json args>
    curl -sS -X POST "http://127.0.0.1:$service_port/call" \
        -H "Authorization: Bearer $token" -H 'content-type: application/json' \
        -d "{\"method\":\"$1\",\"args\":$2}"
}
result() { # result <json> — the result text, unescaped enough to grep
    python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("result") or ("ERROR: "+d.get("error","")))'
}
sends() { grep -c '^argv:' "$sendlog" 2>/dev/null || true; }

check "a call without the token is refused" \
    "$(curl -sS -X POST "http://127.0.0.1:$service_port/call" -d '{"method":"adoptions","args":{}}')" \
    "needs its token"

check "adopt without a caller is refused" \
    "$(call adopt "{\"session\":\"$worker\"}" | result)" \
    "carries no \`caller\`"

coordinator=$work/coord.eid
check "adopting a worker says so, and says what it saw" \
    "$(call adopt "{\"caller\":\"$coordinator\",\"session\":\"$worker\",\"name\":\"grinder\"}" | result)" \
    "adopted $worker in epoch 1"

sleep 2.5

body=$(cat "$sendlog" 2>/dev/null || true)
check "exactly one notice was handed to the transport" "$(sends)" "1"
check "the notice is addressed to the coordinator" "$body" "$coordinator"
check "and asked for the harness's typed outcome" "$body" "--json"
check "and it is framed as the monitor's, not the operator's" "$body" "--from coordinator"
check_absent "and it does not claim to be the operator" "$body" "operator"
check "the notice says what halted and that nothing was resumed" "$body" "Nothing was resumed"

notices=$(call notices "{\"caller\":\"$coordinator\"}" | result)
check "the notice is owed and visible" "$notices" "outcome:7"
check "the harness's own outcome word is stored" "$notices" "outcome \`queued\`"
check "and its bytes are kept verbatim" "$notices" '{"outcome":"queued"'
check_absent "and nothing is called unconfirmed any more" "$notices" "unconfirmed"

adoptions=$(call adoptions "{\"caller\":\"$coordinator\"}" | result)
check "the inventory resolves the worker to what the roster publishes" "$adoptions" "state \`stuck\`"
check_absent "and does not report it as gone" "$adoptions" "no live session carries this address"

# The same halt again: no second notice, no second send.
sleep 2.5
check "a halt already announced is not announced again" "$(sends)" "1"
check "and it is still one notice" "$(call notices "{\"caller\":\"$coordinator\"}" | result | grep -c 'attempt(s)')" "1"

# A new ending record, same ending word: a second notice, and this time the
# transport answers `delivered` — stored as delivered, and never retried.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":9,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
echo delivered >"$outcomefile"
sleep 2.5
check "a new ending record is a second notice" "$(sends)" "2"
check "a delivered outcome is stored as delivered" \
    "$(call notices "{\"caller\":\"$coordinator\"}" | result)" "outcome \`delivered\`"

# No ending record and an ending the journal may not have seen at all (`errored`
# is core's own stated case): keyed on the registration, and it says so.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":null,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
sleep 2.5
check "a halt with no ending record is not announced" "$(sends)" "2"
check "and the reason is in the scan's own words" "$(cat "$work/monitor.log")" "no \`outcome_at\`"

cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"errored","outcome_at":null,
  "outcome_observed":1,"started_ms":1764000000000,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
sleep 2.5
check "an unjournaled ending is announced by its count" "$(sends)" "3"
check "and the notice says which registration instance and count it was keyed on" \
    "$(call notices "{\"caller\":\"$coordinator\"}" | result)" "observed:eidolon-fake:1764000000000:1"

# The same registration, before the counter existed: zero is not established.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"errored","outcome_at":null,
  "outcome_observed":0,"started_ms":1764000000000,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
sleep 2.5
check "a zero count beside an errored ending is not announced" "$(sends)" "3"
check "and the reason says it cannot be this build's row" \
    "$(cat "$work/monitor.log")" "cannot come from this build"

# No count at all: not established — an older writer's row, or a registration
# that has not published an ending yet.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"errored","outcome_at":null,
  "outcome_observed":null,"started_ms":1764000000000,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
sleep 2.5
check "a missing count is not established, so nothing is announced" "$(sends)" "3"
check "and the reason says not established" "$(cat "$work/monitor.log")" "not established"

# `partial` toward one direct recipient is an anomaly, not success.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":11,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
echo partial >"$outcomefile"
sleep 2.5
check "a third occurrence is announced" "$(sends)" "4"
check "and a partial send is reported as an anomaly, not success" "$(cat "$work/monitor.log")" "anomaly"
check "and it is stored as the harness's own word" \
    "$(call notices "{\"caller\":\"$coordinator\"}" | result)" "outcome \`partial\`"

# A refusal — here, one id carried by two live sessions — is the harness's own
# words, and it is surfaced rather than retried.
cat >"$rows" <<EOF
[{"id":"eidolon-fake","session":"$worker","cwd":"$work","model":"mock","title":"a worker",
  "label":null,"tags":[],"state":"stuck","last":"iteration limit","outcome_at":13,
  "parked":null,"reached":true,"repo":"$work","log":"$work/w.eid"}]
EOF
echo refused >"$outcomefile"
sleep 2.5
check "a harness refusal is recorded as refused" \
    "$(call notices "{\"caller\":\"$coordinator\"}" | result)" "outcome \`refused\`"
check "and the harness's own words are kept verbatim" \
    "$(call notices "{\"caller\":\"$coordinator\"}" | result)" "is the session id of 2 live sessions"
check "and a refusal is not retried" "$(sends)" "5"
sleep 2.5
check "and still is not retried on the next scan" "$(sends)" "5"

# Release: monitoring ends, and what it still owed is cancelled rather than sent.
check "release ends monitoring" \
    "$(call release "{\"caller\":\"$coordinator\",\"session\":\"$worker\",\"reason\":\"fixture\"}" | result)" \
    "monitoring ends now"
check "releasing what this session never adopted is refused" \
    "$(call release "{\"caller\":\"$coordinator\",\"session\":\"$other\"}" | result)" \
    "not adopted by this session"
sleep 2.5
check "nothing is sent after a release" "$(sends)" "5"
check "and the record is history, not deleted" \
    "$(call adoptions "{\"caller\":\"$coordinator\"}" | result)" "RELEASED at"

# ---- (2) the shipped bytes, against the real harness -----------------------
eidolon=$(command -v eidolon || true)
if [ -z "$eidolon" ]; then
    say "SKIP the shipped-bytes check: eidolon is not on PATH"
else
    plug=$XDG_CONFIG_HOME/eidolon/plugins
    mkdir -p "$plug"
    rm -rf "$plug/coordinator"
    cp -r "$repo/coordinator" "$plug/coordinator"
    mkdir -p "$plug/coordinator/workflows"
    cp "$here/scenario.rn" "$plug/coordinator/workflows/scenario.rn"
    # The refusal under test is "this session has no durable seat", which is a
    # fact about the swarm, not something a tool can be asked to imagine. Make
    # it true instead of relying on the environment: with swarm disabled the
    # session registers nothing, the roster carries no row for it, and the tool
    # refuses before it ever dials. On a machine where swarm is enabled by
    # default this check used to fail — not because a boundary was missing, but
    # because the condition it asserts never held, and the run reached the
    # transport instead and was refused there by the policy gate.
    mkdir -p "$XDG_CONFIG_HOME/eidolon"
    cat >"$XDG_CONFIG_HOME/eidolon/config.toml" <<'CFG'
[swarm]
enabled = false
CFG
    "$eidolon" plugins trust coordinator >/dev/null
    "$eidolon" workflow run "$plug/coordinator" scenario --args '{}' --provider mock \
        >"$work/scenario.txt" 2>&1 || true
    out=$(cat "$work/scenario.txt")
    check "the shipped tools refuse when this session has no durable seat" \
        "$out" "no durable seat in the roster"
    check "and they name the roster call they read" "$out" 'swarm_call(\"roster\"'
    check "and they say they will not parse prose" "$out" "will not parse"
    check "and they never invent an address" "$out" "will not invent one"

    # The other side of the same boundary, proved rather than assumed: with the
    # swarm on, the session has a seat, the tool gets past the roster and is
    # refused by the policy gate because nobody granted it the token source.
    # That is a different refusal from a different layer, and it must not be
    # mistaken for the no-seat one.
    cat >"$XDG_CONFIG_HOME/eidolon/config.toml" <<'CFG'
[swarm]
enabled = true
CFG
    "$eidolon" workflow run "$plug/coordinator" scenario --args '{}' --provider mock \
        >"$work/scenario-seated.txt" 2>&1 || true
    seated=$(cat "$work/scenario-seated.txt")
    check "seated and ungranted, the same tool is refused at the policy gate" \
        "$seated" "has no grant covering it"
    check "and the refusal is not the no-seat one" \
        "$(printf '%s' "$seated" | grep -c 'no durable seat in the roster' || true)" "0"
fi

# Smoke: every verb the plugin ships reaches the monitor it declares, and the
# two new ones answer with a typed refusal rather than a dial failure. This is
# deliberately shallow — it proves the route, not the feature.
for method in adopt adoptions notices release start cancel; do
    body=$(call "$method" "{\"caller\":\"$coordinator\"}" 2>/dev/null || true)
    case $body in
    \{*\}) check "the $method door answers JSON (route and method exist)" yes yes ;;
    *) check "the $method door answers JSON (route and method exist)" "$body" "{" ;;
    esac
done
started=$(call start "{\"caller\":\"$coordinator\",\"operation\":\"smoke\",\"workspace\":\"$work\",\"model\":\"mock\",\"brief\":\"smoke\"}" | result)
check "start refuses with a typed stage, not a dial failure" "$started" "held launch unconfirmed"
cancelled=$(call cancel "{\"caller\":\"$coordinator\",\"operation\":\"absent\",\"reason\":\"smoke\"}" | result)
check "cancel refuses a missing operation by name" "$cancelled" "exactly one existing owned start operation"

say ""
if [ "$fail" -eq 0 ]; then
    say "coordinator fixture complete: every check passed"
else
    say "coordinator fixture FAILED: $fail check(s)"
fi
exit $((fail > 0))

#!/usr/bin/env bash
# live-rune-path.sh — the LIVE fixture: a real `eidolon`, disposable sessions and no
# installed state, and the adoption made **by the plugin's own tool** so the coordinator
# on the record is the harness's identity for that run rather than a string anyone typed.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-path.sh
#
# It starts its own worker session (which hits its iteration wall and stays alive at
# `stuck`), its own coordinator session (a live recipient), and the monitor on its own
# port and store root. Then, against ONE stuck worker and therefore one occurrence:
#
#   1. adopts through the **service API** with a caller this script supplies;
#   2. adopts through the **plugin's tool**, driven by `eidolon workflow run`, where
#      `coordinator_adopt` reads its own `me` row — the caller is that run's minted
#      session id, and the check below compares the record against the id the harness
#      itself reported for the run;
#   3. checks that the one occurrence produced **two notices with two ids, one addressed
#      to each recipient**, and that releasing the first coordinator left the second's
#      notice and record alone.
#
# Every claim here is load-bearing: it is one assertion inside `verify`, which exits
# non-zero when any of them is false, and the script then runs `verify` twice more with
# deliberately wrong inputs and requires it to FAIL. A check that cannot fail is not
# evidence, so the injections are the proof that these can.
#
# This is a live fixture: it needs a real binary and it starts real sessions of its own
# under a throwaway HOME. Nothing installed, no vault, no operator session.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-path.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi

fail=0
checks=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <actual> <expected>
    checks=$((checks + 1))
    if [ "$2" = "$3" ]; then say "ok   $1"; else
        say "FAIL $1: wanted [$3], got [$2]"; fail=$((fail + 1)); fi
}

port=
for p in 8095 8094 8095 8096 8097 8098; do
    if ! timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$p" 2>/dev/null; then port=$p; break; fi
done
[ -n "$port" ] || { say "FAIL no free port"; exit 1; }

w=$(mktemp -d)
mkdir -p "$w/run" "$w/.config/eidolon"
chmod 700 "$w/run"
# Two iterations: a session's first turn then hits the wall, which is a real settle and so
# a real `outcome_at`. XDG_CONFIG_HOME is `$HOME/.config` on purpose: the plugin's tools
# name their token `~/.config/eidolon/coordinator.token` and the gate resolves `~` against
# HOME, so the monitor and the tools must agree on that path or the tools' dial is
# refused for want of a grant.
printf 'max_iterations = 2\n' >"$w/.config/eidolon/config.toml"
export HOME=$w XDG_CONFIG_HOME=$w/.config XDG_STATE_HOME=$w/state XDG_DATA_HOME=$w/data XDG_RUNTIME_DIR=$w/run

pids=()
cleanup() {
    for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
    if [ -n "${COORDINATOR_FIXTURE_KEEP:-}" ]; then say "fixture work dir kept: $w"; else rm -rf "$w"; fi
}
trap cleanup EXIT

start_session() { # start_session <dir> <log>
    mkdir -p "$1"
    mkfifo "$1/in" 2>/dev/null || true
    ( sleep 600 >"$1/in" ) & pids+=("$!")
    "$EID" chat --provider mock --cwd "$1" <"$1/in" >"$2" 2>&1 & pids+=("$!")
}

# The assertions, all of them, in one place: it exits 0 only when the failures match
# `expect`, so "ok" means every claim held and "fail" means at least one did not.
verify() { # verify <store> <run_id> <service_id> <worker> <expect: ok|fail>
    python3 - "$@" <<'PY'
import json,sys
store, run_id, svc, worker, expect = sys.argv[1:6]
d=json.load(open(store)); recs=d.get("records",[]); notices=d.get("notices",[])
words={"delivered","queued","partial","failed","refused"}
def by(c): return [n for n in notices if n["coordinator"]==c]
claims = {
 "the Rune-path caller on the record is that run's own session id":
     any(r["coordinator"]==run_id and r["worker"]==worker for r in recs),
 "the record the script supplied as caller is on the store too":
     any(r["coordinator"]==svc and r["worker"]==worker for r in recs),
 "both recipients have a notice addressed to them":
     {n["coordinator"] for n in notices} == {run_id, svc},
 "the two notices have distinct ids":
     len(notices)==2 and len({n["notice_id"] for n in notices})==2,
 "both notices name the same occurrence":
     len({n["occurrence"] for n in notices})==1,
 "every notice carries the harness's own outcome word":
     all(n["outcome"] in words for n in notices),
 "the service-path recipient's notice was not refused or failed":
     all(n["outcome"] in {"delivered","queued"} for n in by(svc)),
 "the released coordinator's notice was not retracted":
     all(n["outcome"]!="cancelled" for n in by(svc)),
 "the released coordinator's record is marked released":
     any(r["coordinator"]==svc and r.get("released_at_ms") for r in recs),
 "the Rune-path record is still live":
     any(r["coordinator"]==run_id and not r.get("released_at_ms") for r in recs),
}
bad=[k for k,v in claims.items() if not v]
for k,v in claims.items(): print(("ok   " if v else "FAIL ")+k)
for n in notices: print("     notice %s -> %s | occurrence %s | outcome %s" % (n["notice_id"], n["coordinator"], n["occurrence"], n["outcome"]))
print("expect=%s failures=%d" % (expect, len(bad)))
sys.exit(0 if (expect=="ok" and not bad) or (expect=="fail" and bad) else 1)
PY
}

start_session "$w/worker" "$w/worker.log"
start_session "$w/coord" "$w/coord.log"
sleep 8
printf 'please call a tool now\n' >"$w/worker/in"
sleep 16

rows=$("$EID" peers --json)
say "=== the harness's own rows ==="
printf '%s\n' "$rows" | python3 -c 'import json,sys
for r in json.load(sys.stdin)["sessions"]: print(" ",r["session"],"|",r["id"],"|",r["state"],"|",r["last"],"| outcome_at",r["outcome_at"])'

WORKER=$(printf '%s' "$rows" | python3 -c 'import json,sys;print([r["session"] for r in json.load(sys.stdin)["sessions"] if r["state"]=="stuck"][0])')
SERVICE_COORD=$(printf '%s' "$rows" | python3 -c 'import json,sys;print([r["session"] for r in json.load(sys.stdin)["sessions"] if r["state"]!="stuck"][0])')
check "the worker's session id is a minted id, not a path" \
    "$(printf '%s' "$WORKER" | grep -cE '^[0-9]+(-[0-9]+)?$')" "1"
say "worker: $WORKER    service-path coordinator (supplied by this script): $SERVICE_COORD"

export EIDOLON_COORDINATOR_ROOT=$w/store EIDOLON_COORDINATOR_BIN=$EID EIDOLON_COORDINATOR_PORT=$port EIDOLON_COORDINATOR_INTERVAL_S=1 EIDOLON_COORDINATOR_TOKEN=$HOME/.config/eidolon/coordinator.token
COORD=$repo/target/debug/eidolon-coordinator
[ -x "$COORD" ] || (cd "$repo" && cargo build -q -p eidolon-coordinator)
token=$("$COORD" --print-token | sed -n '1p')
say "monitor port: $port | token file present: $([ -f "$EIDOLON_COORDINATOR_TOKEN" ] && echo yes || echo no)"
"$COORD" >"$w/monitor.log" 2>&1 & pids+=("$!")
for _ in $(seq 1 50); do timeout 2 curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break; sleep 0.2; done
call() { curl -sS --max-time 10 -X POST "http://127.0.0.1:$port/call" -H "Authorization: Bearer $token" -H 'content-type: application/json' -d "{\"method\":\"$1\",\"args\":$2}"; }
res() { python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("result") or ("ERROR: "+d.get("error","")))'; }

say ""
say "=== 1. service-path adoption (the caller is mine, not a tool's) ==="
a1=$(call adopt "{\"caller\":\"$SERVICE_COORD\",\"session\":\"$WORKER\",\"name\":\"service path\"}" | res)
say "$a1"
check "the service-path adoption was recorded against the real worker" \
    "$(printf '%s' "$a1" | grep -c "adopted $WORKER in epoch 1")" "1"

say ""
say "=== 2. Rune-path adoption: the plugin's own tool, driven by a workflow run ==="
plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/rune-adopt.rn" "$plug/workflows/rune-adopt.rn"
"$EID" plugins trust coordinator >/dev/null
for v in adopt release adoptions notices; do
    "$EID" plugins grant "coordinator_$v" "file:$HOME/.config/eidolon/coordinator.token" >/dev/null
done
run=$("$EID" workflow run "$plug" rune-adopt --args "{\"session\":\"$WORKER\"}" --provider mock 2>&1 || true)
printf '%s\n' "$run" | tail -2
RUN_SESSION=$(printf '%s\n' "$run" | python3 -c 'import json,sys
last=[l for l in sys.stdin.read().splitlines() if l.startswith("{")]
print(json.loads(last[-1])["session"] if last else "")')
RUN_ID=$(basename "$RUN_SESSION" .eid)
say "the workflow run's own session id, as the harness reported it: $RUN_ID"
check "the plugin's tool adopted the worker" \
    "$(printf '%s' "$run" | grep -c "ADOPT-OK: adopted $WORKER")" "1"
check "and it read its own roster row (no seat complaint)" \
    "$(printf '%s' "$run" | grep -c 'no durable seat')" "0"

sleep 4
say ""
say "=== 4. the recipient, from the coordinator session's own transcript ==="
sleep 1; printf 'anything for me?\n' >"$w/coord/in"; sleep 12
check "the notice reached the service coordinator's transcript" \
    "$(grep -c 'coordinator (monitor)' "$w/coord.log")" "1"

say ""
say "=== 5. release the service-path coordinator, then assert everything ==="
call release "{\"caller\":\"$SERVICE_COORD\",\"session\":\"$WORKER\",\"reason\":\"live fixture\"}" | res
sleep 3
store=$w/store/state.json

say "--- (a) the real run's assertions (must all hold) ---"
out=$(verify "$store" "$RUN_ID" "$SERVICE_COORD" "$WORKER" ok) && st=0 || st=$?
printf '%s\n' "$out" | grep -v '^     notice'
check "verify exits 0 when every claim holds" "$st" "0"
check "FAILURES in the real run's assertions" "$(printf '%s' "$out" | grep -c '^FAIL')" "0"

say "--- (b) fault injection: a wrong expected caller id (must FAIL) ---"
out=$(verify "$store" "9999999" "$SERVICE_COORD" "$WORKER" fail) && st=0 || st=$?
printf '%s\n' "$out" | grep -E '^(FAIL|expect=)'
check "the injected wrong caller produces failures as expected" "$st" "0"
check "and the caller claim is one of them" \
    "$(printf '%s' "$out" | grep -c '^FAIL the Rune-path caller')" "1"

say "--- (c) fault injection: the two identities swapped (must FAIL the release claims too) ---"
out=$(verify "$store" "$SERVICE_COORD" "$RUN_ID" "$WORKER" fail) && st=0 || st=$?
printf '%s\n' "$out" | grep -E '^(FAIL|expect=)'
check "the swapped identities produce failures as expected" "$st" "0"
check "and both the release claims are among them" \
    "$(printf '%s' "$out" | grep -cE '^FAIL (the released coordinator|the Rune-path record)')" "2"

say ""
say "=== monitor scan log, tail ==="; tail -2 "$w/monitor.log"
say "=== evidence kept at $w (worker/coord logs, store, monitor log) ==="
say ""
checks=$((checks + 1))
if [ "$fail" -eq 0 ]; then say "live-rune-path complete: $checks checks, 0 FAIL"; else say "live-rune-path FAILED: $fail of $checks checks"; fi
exit $((fail > 0))

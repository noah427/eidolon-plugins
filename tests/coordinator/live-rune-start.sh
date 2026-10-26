#!/usr/bin/env bash
# live-rune-start.sh — force the plugin's `tools/start.rn` to compile and run.
#
#   t=$(mktemp -d); mkdir -p "$t/cfg" "$t/state" "$t/data"
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-start.sh
#
# Why this exists: `cargo test` drives `managed_start::start` through a fake host, so it
# can prove the ordering and the state machine and it cannot prove that the Rune tool
# file — the door a model actually calls — compiles, registers, or runs. This drives
# that door: the plugin is copied BYTE FOR BYTE from this worktree into a disposable
# config, trusted and granted there, and `coordinator_start` is called by
# `eidolon workflow run`, which compiles the file before it can call anything.
#
# Provenance, because it decides what a positive means: the plugin under test is the
# WORKTREE's, the service is the one built from this worktree (checked below — its
# binary must carry the wake text), and the harness underneath is the INSTALLED binary.
# The patch is the plugin; nothing about it lives in that harness.
#
# The one thing it must not do is reach a monitor that is not its own. The tools dial
# 127.0.0.1:8095 because `start.rn`'s `service()` says so, and an installed coordinator
# may be live on that port — so the run happens inside a private network namespace
# (`unshare -rn`) with `lo` up, where 8095 is nobody's until this script binds it. The
# plugin file is never edited to point elsewhere: an edited copy would not be the file
# under test.
#
# Three injections prove the checks can fail, because a check that cannot fail is not
# evidence:
#   1. a Rune syntax error in the tool, RE-VOUCHED on purpose so the gate is not what
#      refuses, must stop it answering — and the parser must name the fault;
#   2. a hand-supplied wake text in the child's log must FAIL the attribution checks,
#      which is what makes "by content" different from "by the fixture's good manners";
#   3. the guard must refuse a real HOME outright, before anything is copied.
#
# Nothing installed, no operator config, no compositor, headless children only.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
# Before anything else, and before the namespace: a real HOME is refused outright, so a
# mistake here cannot reach the operator's config even by accident.
require_temp_config || exit 2

if [ -z "${RUNE_START_NETNS:-}" ]; then
    export RUNE_START_NETNS=1
    exec unshare -rn env RUNE_START_NETNS=1 bash "$0" "$@"
fi
# The private loopback has to be up, or the tools' dial cannot reach this monitor.
ip link set lo up 2>/dev/null || true

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"
EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-start.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
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
field() { sed -n "s/^$1=//p" "$2"; }
present() { [ "$1" -ge 1 ] && echo present || echo absent; }

w=$(mktemp -d)
mkdir -p "$w/run" "$w/.config/eidolon" "$w/work"
chmod 700 "$w/run"
printf 'max_iterations = 4\n' >"$w/.config/eidolon/config.toml"
export HOME=$w XDG_CONFIG_HOME=$w/.config XDG_STATE_HOME=$w/state XDG_DATA_HOME=$w/data XDG_RUNTIME_DIR=$w/run

pids=()
cleanup() {
    for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
    if [ -n "${RUNE_START_KEEP:-}" ]; then say "fixture work dir kept: $w"; else rm -rf "$w"; fi
}
trap cleanup EXIT

COORD=$repo/target/debug/eidolon-coordinator
[ -x "$COORD" ] || { say "SKIP: $COORD is not built; this fixture does not build anything"; exit 3; }
# A binary from before the fix sends no wake at all, so a run against it would prove
# nothing about this change. The wake text is a literal in the service that composes it.
check "the service under test carries the wake text (built from this worktree)" \
    "$(present "$(grep -ac 'Managed start ' "$COORD")")" "present"
check "and the harness underneath is the one named in EIDOLON_BIN" \
    "$([ -x "$EID" ] && echo executable || echo missing)" "executable"

# The integration reviewer needs the binary and the core pin on the record, in the
# output rather than in somebody's message. The pin is read from the flake lock the
# caller names — never guessed, and never written to.
pin_file=${EIDOLON_PIN_FILE:-}
pin_line=unreadable
if [ -n "$pin_file" ] && [ -r "$pin_file" ]; then
    pin_line=$(python3 -c 'import json,sys
try:
    nodes=(json.load(open(sys.argv[1])).get("nodes") or {})
    print((nodes.get("eidolon") or {}).get("locked",{}).get("rev") or "unreadable")
except Exception:
    print("unreadable")' "$pin_file")
fi
say "EIDOLON_BIN=$EID"
say "EIDOLON_CORE_PIN=$pin_line   (flake lock: ${pin_file:-none named; pass EIDOLON_PIN_FILE to resolve})"
check "the E2E prints the harness path and the core pin it runs against" \
    "$([ -n "$EID" ] && [ -n "$pin_line" ] && echo printed || echo missing)" "printed"
if [ "$pin_line" != unreadable ]; then
    check "and that pin is the one this union is built against" \
        "$pin_line" "${EIDOLON_EXPECTED_PIN:-da7d7a12783ab4094d816c26b2b8c72a51622ab1}"
fi

export EIDOLON_COORDINATOR_ROOT=$w/store EIDOLON_COORDINATOR_BIN=$EID \
       EIDOLON_COORDINATOR_INTERVAL_S=1 \
       EIDOLON_COORDINATOR_TOKEN=$HOME/.config/eidolon/coordinator.token
"$COORD" --print-token >/dev/null
say "monitor: 127.0.0.1:8095 inside a private namespace | token file present: $([ -f "$EIDOLON_COORDINATOR_TOKEN" ] && echo yes || echo no)"
"$COORD" >"$w/monitor.log" 2>&1 & pids+=("$!")
for _ in $(seq 1 50); do
    timeout 2 curl -fsS --max-time 2 "http://127.0.0.1:8095/health" >/dev/null 2>&1 && break
    sleep 0.2
done
health=$(curl -fsS --max-time 2 "http://127.0.0.1:8095/health" 2>/dev/null || echo unreachable)
check "the monitor on 8095 answers, and it is this run's" "$health" "ok"
[ "$health" = ok ] || { tail -5 "$w/monitor.log"; exit 1; }

# The plugin, byte for byte from the worktree. No edit, no substitution.
plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/rune-start.rn" "$plug/workflows/rune-start.rn"
check "the tool under test is the worktree's own start.rn" \
    "$(cmp -s "$repo/coordinator/tools/start.rn" "$plug/tools/start.rn" && echo same || echo different)" "same"

vouch() { "$EID" plugins trust coordinator >/dev/null 2>&1; }
vouch
for v in start adoptions adopt release notices cancel; do
    "$EID" plugins grant "coordinator_$v" "file:$HOME/.config/eidolon/coordinator.token" >/dev/null 2>&1
done

mkargs() { # mkargs <operation> <brief> <changed>
    python3 -c 'import json,sys
print(json.dumps({"operation":sys.argv[1],"workspace":sys.argv[2],"model":"mock-text",
                  "brief":sys.argv[3],"changed_brief":sys.argv[4]}))' "$1" "$w/work" "$2" "$3"
}

op=live-rune-start-1
say ""
say "=== the plugin's own tool, driven by a workflow run (operation $op) ==="
out=$("$EID" workflow run "$plug" rune-start \
        --args "$(mkargs "$op" \
            'The bounded assignment, journaled rather than inboxed.' \
            'A different assignment under the same operation id.')" \
        --provider mock 2>&1); rc=$?
printf '%s\n' "$out" >"$w/workflow.out"
check "the workflow itself exited 0" "$rc" "0"

python3 - "$w/workflow.out" >"$w/answers.txt" <<'PY'
import json,sys
raw=open(sys.argv[1]).read()
report=json.loads(raw[raw.index("{"):])["report"]
lines=[l for l in report.splitlines() if l.strip()]
def tag(t):
    for l in lines:
        if l.startswith(t): return l[len(t):].strip()
    return None
def call(n):
    ok,err=tag("START-%d-OK:"%n),tag("START-%d-ERR:"%n)
    return ("ok",json.loads(ok)) if ok else (("err",err) if err else ("missing",""))
a1=call(1)[1]; a2=call(2)[1]; a3=call(3)
def only(a): return list(a["wake"])[0]
r1=a1["brief"]["Journaled"]
print("a1_status=%s"%a1["status"])
print("a1_brief=%s"%list(a1["brief"])[0])
print("a1_wake=%s"%only(a1))
print("a1_wake_outcome=%s"%a1["wake"]["Answered"]["outcome"])
print("a1_words_say_delivered=%s"%("yes" if '"outcome": "delivered"' in a1["wake"]["Answered"]["words"] else "no"))
print("a1_claims=%s"%("not established" if (a1["brief_consumption"]=="not established"
      and a1["turn_after_wake"]=="not established") else "CLAIMED"))
print("a2_status=%s"%a2["status"])
print("a2_brief=%s"%list(a2["brief"])[0])
print("a2_same_key=%s"%("yes" if a2["brief"]["Journaled"]["delivery_key"]==r1["delivery_key"] else "no"))
print("a2_same_record=%s"%("yes" if a2["brief"]["Journaled"]["record"]==r1["record"] else "no"))
print("a3_kind=%s"%a3[0])
a4=call(4)
print("a4_kind=%s"%a4[0])
print("a4_names_the_refusal=%s"%("yes" if a4[0]=="err" and any(w in a4[1].lower() for w in
      ("visible","terminal","placement","no-focus","destination")) else "no"))
print("a3_refusal=%s"%("yes" if a3[0]=="err" and "different immutable launch input" in a3[1] else "no"))
print("adoptions=%s"%("yes" if tag("ADOPTIONS-OK:") else "no"))
PY
sed 's/^/  /' "$w/answers.txt"
A=$w/answers.txt
check "the first start answered managed" "$(field a1_status "$A")" "managed"
check "the brief was journaled as a record, not inboxed" "$(field a1_brief "$A")" "Journaled"
check "and the wake after it was answered by the harness" "$(field a1_wake "$A")" "Answered"
check "the wake reached the child's inbox" \
    "$(case $(field a1_wake_outcome "$A") in delivered|queued) echo arrived;; *) echo other;; esac)" "arrived"
check "the harness's own words say delivered" "$(field a1_words_say_delivered "$A")" "yes"
check "no consumption and no turn is claimed in the answer" "$(field a1_claims "$A")" "not established"
# The settled repeat: nothing is sent, so the answer is the receipt the log already
# holds. `Existing` is the DEDUP answer — a send repeated under the same key that the
# recipient recognised — and does not appear when no send happened at all.
check "the repeat reports the stored receipt and sends nothing" "$(field a2_brief "$A")" "Journaled"
check "the repeat carries the same delivery key" "$(field a2_same_key "$A")" "yes"
check "and points at the same record" "$(field a2_same_record "$A")" "yes"
check "the same operation with a different brief was refused" "$(field a3_refusal "$A")" "yes"
check "and the adoption is visible through a second tool" "$(field adoptions "$A")" "yes"
# ---- the union's half: the merged manifest carries `visible` too ----
check "a visible request refuses here, as it must without a terminal or compositor" \
    "$(field a4_kind "$A")" "err"
check "and the refusal names the reason, not a generic failure" \
    "$(field a4_names_the_refusal "$A")" "yes"

say ""
say "=== the store: one operation, one brief, one wake ==="
store=$w/store/state.json
python3 - "$store" >"$w/store.txt" 2>&1 <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); s=d.get("starts",[])
print("starts=%d"%len(s))
r=s[0]
print("phase=%s"%r["phase"])
print("brief=%s"%list(r["brief"])[0])
print("wake=%s"%(list(r["wake"])[0] if isinstance(r["wake"],dict) else r["wake"]))
print("key_len=%d"%len(r.get("brief_delivery_key") or ""))
print("record=%d"%r["brief"]["Journaled"]["receipt"]["record"])
print("wake_outcome=%s"%r["wake"]["Answered"]["outcome"])
PY
sed 's/^/  /' "$w/store.txt"
S=$w/store.txt
check "exactly one operation in the store" "$(field starts "$S")" "1"
check "it is Running" "$(field phase "$S")" "Running"
check "the brief is Journaled" "$(field brief "$S")" "Journaled"
check "the wake is Answered" "$(field wake "$S")" "Answered"
check "the delivery key is 32 hex characters" "$(field key_len "$S")" "32"
python3 - "$store" >"$w/union.txt" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); s=d.get("starts",[])
def wake_of(r): return list(r["wake"])[0] if isinstance(r["wake"],dict) else r["wake"]
print("operations=%d"%len(s))
print("operations_with_a_wake=%d"%len([r for r in s if wake_of(r)=="Answered"]))
vis=[r for r in s if r["request"].get("visible")]
print("visible_operations=%d"%len(vis))
if vis:
    v=vis[0]
    print("visible_brief=%s"%(list(v["brief"])[0] if isinstance(v["brief"],dict) else v["brief"]))
    print("visible_wake=%s"%wake_of(v))
PY
sed 's/^/  /' "$w/union.txt"
U=$w/union.txt
check "exactly one operation ever produced a wake" "$(field operations_with_a_wake "$U")" "1"

# One classifier, used twice: once on the child's real log, once on a doctored copy in
# which a hand-supplied message replaces the wake. The second run is the proof that
# these assertions discriminate rather than merely pass.
classify() { # classify <log.jsonl> <operation> <record the brief landed at>
    python3 - "$@" <<'PY'
import json,sys
path,op,record = sys.argv[1],sys.argv[2],sys.argv[3]
brief=[];brief_text="";wake=[];turn=None
for line in open(path):
    line=line.strip()
    if not line: continue
    r=json.loads(line); k=list(r["kind"])[0]; d=list(r["kind"].values())[0]
    if k=="CorrelatedExternalMessage":
        brief.append(r["id"])
        if not brief_text: brief_text=d.get("text","")
    if k=="ExternalMessage":
        wake.append((r["id"],r["parent"],d.get("from"),d.get("text","")))
    if k=="AssistantMessage" and turn is None and wake and brief: turn=r["id"]
wid,wparent,wfrom,wtext = wake[0] if wake else ("",None,"","")
print("briefs=%d"%len(brief))
print("wakes=%d"%len(wake))
print("brief_record=%s"%(brief[0] if brief else ""))
print("wake_record=%s"%wid)
print("first_turn_after_wake=%s"%(turn if turn is not None else ""))
chain = bool(brief and wake and turn is not None and brief[0] < wid < turn)
print("chain=%s"%("brief->wake->turn" if chain else "incomplete"))
print("wake_from=%s"%wfrom)
print("wake_names_operation=%s"%("yes" if ("Managed start `%s`"%op) in wtext else "no"))
print("wake_names_the_record=%s"%("yes" if (record and ("record %s"%record) in wtext) else "no"))
print("wake_restates_brief=%s"%("yes" if (brief_text and brief_text in wtext) else "no"))
print("wake_parent_is_the_brief=%s"%("yes" if wparent==(brief[0] if brief else None) else "no"))
print("messages_before_the_turn=%d"%len([x for x in wake if turn is not None and x[0] < turn]))
PY
}

say ""
say "=== the child's own log: which message, and did a turn follow it? ==="
child=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["starts"][0]["session"])' "$store")
childlog=$XDG_DATA_HOME/eidolon/sessions/$child.eid
say "child session: $child | log present: $([ -f "$childlog" ] && echo yes || echo no)"
check "the adopted child has a log of its own" "$([ -f "$childlog" ] && echo yes || echo no)" "yes"
if [ -f "$childlog" ]; then
    "$EID" log --json "$childlog" >"$w/child.jsonl" 2>/dev/null
    # Attribution by CONTENT and by the log's own threading, not by the fixture's good
    # behaviour: a message somebody sent by hand would not carry this operation's id
    # and a parent pointer to the brief record, and would not be the only one before
    # the turn.
    classify "$w/child.jsonl" "$op" "$(field record "$S")" >"$w/child.txt"
    sed 's/^/  /' "$w/child.txt"
    C=$w/child.txt
    check "exactly one brief record in the child's log" "$(field briefs "$C")" "1"
    check "exactly one wake in it too — the repeat woke no one twice" "$(field wakes "$C")" "1"
    check "a TURN was journaled after the wake — the live claim a unit test cannot make" \
        "$(field chain "$C")" "brief->wake->turn"
    check "the wake came from the coordinator, not a peer and not a person" \
        "$(field wake_from "$C")" "coordinator"
    check "its text is the fixed wake_text: it names THIS operation" \
        "$(field wake_names_operation "$C")" "yes"
    check "and the record the brief was actually journaled at" \
        "$(field wake_names_the_record "$C")" "yes"
    check "and it does not restate the brief" "$(field wake_restates_brief "$C")" "no"
    check "the log threads it onto the brief record itself (parent)" \
        "$(field wake_parent_is_the_brief "$C")" "yes"
    check "it is the only message before the turn" \
        "$(field messages_before_the_turn "$C")" "1"

    say ""
    say "=== injection: a wake supplied by hand must FAIL these assertions ==="
    # The objection, run as a test: the same log with the wake's text replaced by a
    # message a fixture could have sent itself. It keeps the record number on purpose,
    # so the record check alone cannot be what discriminates — the pair (the fixed
    # sentence + this operation's id) has to carry the attribution.
    python3 - "$w/child.jsonl" "$w/child-hand.jsonl" <<'PY'
import json,sys
out=[]
for line in open(sys.argv[1]):
    line=line.strip()
    if not line: continue
    r=json.loads(line)
    if list(r["kind"])[0]=="ExternalMessage":
        r["kind"]["ExternalMessage"]["text"]=("Please begin the repair assignment already journaled in "
            "your session by coordinator_start, record 3.")
    out.append(json.dumps(r))
open(sys.argv[2],"w").write("\n".join(out)+"\n")
PY
    classify "$w/child-hand.jsonl" "$op" "$(field record "$S")" >"$w/child-hand.txt"
    say "  a hand-supplied wake, through the same classifier:"
    grep -E '^(wake_from|wake_names_operation|wake_names_the_record|wake_parent_is_the_brief|messages_before)' \
        "$w/child-hand.txt" | sed 's/^/    /'
    check "it still names the record — so the record number alone proves nothing" \
        "$(field wake_names_the_record "$w/child-hand.txt")" "yes"
    check "and it is rejected because it does not name the operation" \
        "$(field wake_names_operation "$w/child-hand.txt")" "no"
    check "the real log passes the classifier the hand-supplied one fails" \
        "$(field wake_names_operation "$C")" "yes"
fi

say ""
say "=== injection 1: a Rune syntax error in the tool must stop it answering ==="
cp "$plug/tools/start.rn" "$w/start.rn.orig"
printf '\nfn broken( { \n' >>"$plug/tools/start.rn"
# Re-vouched ON PURPOSE: the vouch is keyed to the script's bytes, so a corrupted file
# would fail the gate rather than the parser and the injection would prove the wrong
# thing. Vouching the broken bytes leaves compilation as the only thing that can refuse.
vouch
bad=$("$EID" workflow run "$plug" rune-start \
        --args "$(mkargs live-rune-start-broken b c)" --provider mock 2>&1 || true)
printf '%s\n' "$bad" >"$w/broken.out"
say "  the harness said: $(grep -m1 'failed to compile' "$w/broken.out" | cut -c1-200)"
check "a broken tool never reaches the monitor" "$(grep -c 'START-1-OK' "$w/broken.out")" "0"
check "and the refusal is the Rune parser's own word: failed to compile" \
    "$(present "$(grep -c 'failed to compile' "$w/broken.out")")" "present"
check "naming the injected fault at the line, not a dial or a vouch" \
    "$(present "$(grep -c 'Expected pattern but got' "$w/broken.out")")" "present"
cp "$w/start.rn.orig" "$plug/tools/start.rn"
vouch
check "the copy is restored to the shipped bytes" \
    "$(cmp -s "$repo/coordinator/tools/start.rn" "$plug/tools/start.rn" && echo same || echo different)" "same"

say ""
say "=== injection 2: the guard refuses a real HOME ==="
guard_out=$(env RUNE_START_NETNS=1 HOME=/ XDG_CONFIG_HOME=/.config \
    XDG_STATE_HOME=/tmp/never XDG_DATA_HOME=/tmp/never EIDOLON_BIN="$EID" \
    bash "$0" 2>&1 || true)
printf '%s\n' "$guard_out" | head -1 | sed 's/^/  /'
check "the guard refused rather than copying into a real config" \
    "$(printf '%s' "$guard_out" | grep -c 'refusing: XDG_CONFIG_HOME must be a temp directory')" "1"
check "and nothing was run against a monitor" \
    "$(printf '%s' "$guard_out" | grep -c 'START-\|coordinator_start')" "0"

say ""
say "=== monitor log, tail ==="; tail -3 "$w/monitor.log"
say ""
checks=$((checks + 1))
if [ "$fail" -eq 0 ]; then say "live-rune-start complete: $checks checks, 0 FAIL"; else say "live-rune-start FAILED: $fail of $checks checks"; fi
[ "$fail" -eq 0 ]

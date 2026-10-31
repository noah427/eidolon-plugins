#!/usr/bin/env bash
# live-rune-guards.sh — the guard fixture: every refusal `coordinator_adopt` and
# `coordinator_release` now answer with, driven through the plugin's own shipped tools.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-guards.sh
#
# WHY THIS EXISTS. Core used to witness these guards itself: `adopt_refuses_an_empty_worker_address`,
# `a_session_cannot_adopt_itself`, `a_second_live_adoption_is_refused_naming_the_epoch`,
# `re_adoption_after_a_release_takes_the_next_epoch`, `releasing_a_worker_this_session_does_not_hold_says_so`
# and a release that could not be written down. Slice 2 removed the verbs, so those tests left with
# them and the guards moved here — written in `coordinator/tools/adopt.rn` and `release.rn`, and
# until this fixture existed, unwitnessed at the layer that owns them. Each check below names the
# sentence the tool answers, not merely that it answered.
#
# No monitor, no port, no service: adoption and release are this ledger's acts. One real `eidolon`,
# one throwaway config, one workflow run, nothing of the operator's read or written.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"
# The swarm's presence lives under XDG_RUNTIME_DIR, so this fixture takes one of its own **whether or
# not the caller's environment already has one**: otherwise the run registers beside every real
# session on the machine — which it did, on the first cut of this block, because `XDG_RUNTIME_DIR` is
# in the operator's inherited environment — and G21 would be reading the operator's peers rather
# than this fixture's. Same rule as `guard.sh` applies to the config: throwaway or refuse.
runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-guards.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi

fail=0
checks=0
skipped=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3] in the tool's answer"; fail=$((fail + 1))
    fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"; fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

# One live session, parked on a FIFO, so the adoption answer's live-session branch (G21, witness map
# row 17) has a real address carrying it. It is an ordinary `eidolon chat` under this fixture's own
# HOME: no service, no monitor, no port.
live_dir=$XDG_RUNTIME_DIR/live
mkdir -p "$live_dir"
mkfifo "$live_dir/in" 2>/dev/null || true
( sleep 600 >"$live_dir/in" ) & fifo_pid=$!
"$EID" chat --provider mock --cwd "$live_dir" <"$live_dir/in" >"$live_dir/out" 2>&1 & live_pid=$!
cleanup() {
    kill "$live_pid" "$fifo_pid" 2>/dev/null || true
    wait "$live_pid" "$fifo_pid" 2>/dev/null || true
    rm -rf "$runtime" 2>/dev/null || true
    : # a trap must not end on a failing test: under `set -e` that turns a green fixture into exit 1
}
trap cleanup EXIT
live=""
for _ in $(seq 1 60); do
    live=$(ls -t "$XDG_DATA_HOME"/eidolon/sessions/*.eid 2>/dev/null | head -1 | sed 's|.*/||;s|\.eid$||' || true)
    [ -n "$live" ] && break
    sleep 0.5
done
if [ -z "$live" ]; then
    say "SKIP the live-session check: no session log appeared for the chat process"
    live=""
else
    # A log file is not a registration: the roster is what `live_sentence` reads, so wait for the
    # roster and not for the file. A session that never registers is a SKIP, counted, not a pass.
    registered=0
    for _ in $(seq 1 60); do
        if "$EID" peers 2>/dev/null | grep -qF -- "$live"; then registered=1; break; fi
        sleep 0.5
    done
    if [ "$registered" -eq 0 ]; then
        say "SKIP the live-session check: $live never appeared on this fixture's roster"
        live=""
    fi
fi

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
# The seed fixture (test-only, never shipped) is the only way to put a row carrying
# `cancellation: attempted` in front of `coordinator_release` without a real child process.
cp "$here/lifecycle-fixture/tools/seed_json.rn" "$here/lifecycle-fixture/tools/dump_rows.rn" "$plug/tools/"
sed -i 's/"rank", "unrank", "ranks", "hook_ready", "hook_peer"\]/"rank", "unrank", "ranks", "hook_ready", "hook_peer", "seed_json", "dump_rows"]/' "$plug/plugin.rn"
grep -q 'seed_json' "$plug/plugin.rn" || { say "FAIL the seed fixture is not registered"; exit 1; }
cp "$here/rune-guards.rn" "$plug/workflows/rune-guards.rn"
"$EID" plugins trust coordinator >/dev/null

say "=== driving the guards through the plugin's own tools ==="
run=$("$EID" workflow run "$plug" rune-guards --provider mock --yolo \
    --args "$(printf '{"live":"%s"}' "$live")" 2>&1 || true)
printf '%s\n' "$run" | sed 's/^/     /'

# The run must have COMPLETED before any absence check below means anything: a workflow that failed
# outright satisfies every check_absent in this file, which is how a broken run can look clean.
check "the workflow run completed" "$run" '"status":"completed"'

worker=1793000004242

# G1 — nothing named. The tool's JSON schema refuses this before the body runs, which is a stronger
# guard than the sentence below it: the check is that the refusal names the missing field.
check "G1 an adoption with no session names the missing field" "$run" \
    "G1 ERR \`coordinator_adopt\` was called with invalid arguments: missing required field \`session\` (string)"
# G2 — self-adoption. A run whose roster names no row skips; that is reported, not passed.
if printf '%s' "$run" | grep -qF 'G2 SKIP'; then
    checks=$((checks + 1))
    skipped=$((skipped + 1))
    say "ok   G2 SKIPPED (counted): this run's roster named no row for it, so the guard itself was not exercised"
else
    check "G2 a session cannot adopt itself" "$run" "G2 ERR a session cannot adopt itself"
fi
# G3 — the first adoption.
check "G3 the first adoption lands in epoch 1" "$run" "G3 OK adopted $worker in epoch 1"
# G4 — the second live adoption is refused, naming the epoch and pointing at release.
# One needle carrying the label and both clauses: a bare pointer clause ("\`coordinator_release\` ends
# it first") is not anchored to this answer, and the review found it unlabelled the same way G18's
# first cut was. The epoch and the pointer are one sentence because the tool writes one.
check "G4 a second live adoption is refused, naming its epoch and the release that ends it" "$run" \
    "G4 ERR $worker is already adopted by this session, in epoch 1 and still monitored; \`coordinator_release\` ends it first"
# G5 — release ends it, and says the record stays.
check "G5 a release ends monitoring and keeps the record" "$run" "G5 OK released $worker (epoch 1); monitoring ends now"
# G6 — a re-adoption is the next epoch, never a reused one.
check "G6 a re-adoption after a release takes epoch 2" "$run" "G6 OK adopted $worker in epoch 2"
# G7 — a release of something never held says so.
check "G7 a release of an unheld worker says there is nothing to release" "$run" \
    "G7 ERR 1793000009494 is not adopted by this session (or was already released); there is nothing to release"
# G9 — an empty address. The schema cannot refuse it (`""` is a string), so the tool must. This is
# the guard core's `adopt_refuses_an_empty_worker_address` kept, and until this fixture was written
# the plugin adopted the empty address into a live row: guard 1 of its own list ("worker non-empty")
# was prose with no code behind it.
check "G9 an empty address is refused, naming what is missing" "$run" \
    "G9 ERR coordinator_adopt needs a durable address: \`session\` is empty or blank"
check_absent "G9b and no live row naming nobody was written" "$run" "G9 OK adopted"
check "G10 a blank (spaces-only) address is refused too" "$run" \
    "G10 ERR coordinator_adopt needs a durable address: \`session\` is empty or blank"
check_absent "G10b and it wrote no row either" "$run" "G10 OK adopted"
check "G11 an address carrying a real newline is refused" "$run" \
    "G11 ERR \`session\` carries a control character"
check_absent "G11b and it wrote no row either" "$run" "G11 OK adopted"

# G18/G18c/G18d — a padded address. Refused, never trimmed into the address it resembles: the
# sentence must say the address carries padding and tell the caller where the exact spelling is.
# The needle is the whole G18 sentence, label included, because a bare "Copy it from `eidolon
# peers`" passes on G20's answer: a check may not reach wider than the answer it is about. (The
# first cut of this pair had exactly that defect, and the guard-deleted arm exposed it by
# staying green on a suite where G18 itself was red.)
check "G18 a space-padded address is refused, naming the padding and where the spelling comes from" "$run" \
    "G18 ERR \`session\` carries padding: an address is the exact spelling the harness minted, with nothing before it or after it. Copy it from \`eidolon peers\`"
check_absent "G18b and no row was written under the padded spelling" "$run" "G18 OK adopted"
# NBSP is the case no other guard sees: not a control character, not a space, and `trim()` closes it.
check "G18c an NBSP before the address is refused" "$run" \
    "G18c ERR \`session\` carries padding"
check "G18d an NBSP after the address is refused" "$run" \
    "G18d ERR \`session\` carries padding"
# G18e — the trim-then-write arm. A refusal that trimmed and wrote would leave a live row for
# 1793000005555, and this clean adoption would then be refused as a second live adoption. It must
# instead be that worker's FIRST adoption, in epoch 1.
check "G18e the refusal wrote nothing: the clean address is still this ledger's first adoption of it" "$run" \
    "G18e OK adopted 1793000005555 in epoch 1"
# G18f — the residual, asserted rather than left unstated: an interior space is unreachable by
# minting, and the rule does not claim to cover it.
check "G18f an interior space is still accepted, and the fixture says so (residual)" "$run" \
    "G18f OK adopted 179300000 7777 in epoch 1"
# G19 — `release.rn`'s control-character guard: no check reached it before, since G5/G7/G13 all pass
# clean addresses. This also proves the guard runs before the ledger lookup: the address is unknown
# and the answer is still about the character, not about the worker.
check "G19 a control character in release's session is refused" "$run" \
    "G19 ERR \`session\` carries a control character"
check_absent "G19b and it was not answered as a release of the trimmed worker" "$run" "G19 OK released"
# G20 — release's padded rule. The worker is adopted and live at this point, so an answer of "not
# adopted by this session" would be the wrong sentence for the right reason; the check refuses that.
check "G20 a padded release is refused for its padding" "$run" \
    "G20 ERR \`session\` carries padding"
check_absent "G20b and not answered as the unheld string's release" "$run" "G20 OK"

# ---- the ledger's rows as data: row 14, and the review's F4 ----
# G18h is `coordinator_dump_rows`, a test-only reader that returns the adoption rows themselves. The
# previous cut asked a RENDERING whether a row existed, and a rendering omits `(called …)` when a row
# has no name — so a defect that dropped the name left the checks green while the row sat in the
# ledger (review findings m9, m10, m11). These assertions are on workers, epochs and the released
# flag, none of which a presentation decision can drop.
rows=$(python3 - "$run" <<'PYROWS'
import json,re,sys
t=sys.argv[1]
m=re.search(r'"report":("(?:[^"\\]|\\.)*")', t, re.S)
rep=json.loads(m.group(1)) if m else ""
line=[l for l in rep.split('\n') if l.startswith('G18h ')]
print(line[0][len('G18h '):] if line else "")
PYROWS
)
rowfacts=$(python3 - "$rows" <<'PYFACTS'
import json,sys
try:
    rows=json.loads(sys.argv[1])
except Exception:
    rows=None
if not isinstance(rows,list):
    print("rows-parsed=no"); raise SystemExit
def w(r): return r.get("worker") if isinstance(r.get("worker"),str) else ""
pads=[r for r in rows if w(r) and w(r)!=w(r).strip()]
blank=[r for r in rows if w(r).strip()==""]
same=[r for r in rows if w(r)=="1793000004242"]
e1=[r for r in same if str(r.get("epoch"))=="1"]
e2=[r for r in same if str(r.get("epoch"))=="2"]
clean=[r for r in rows if w(r)=="1793000005555"]
print("rows-parsed=yes")
print("padded-rows=%d" % len(pads))
print("blank-rows=%d" % len(blank))
print("epoch1-row-present=%d" % len(e1))
print("epoch1-row-released=%s" % ("true" if e1 and e1[0].get("released") is True else "false"))
print("epoch1-row-reason=%s" % (e1[0].get("release_reason") if e1 else ""))
print("epoch2-rows=%d" % len(e2))
print("clean-5555-rows=%d" % len(clean))
print("clean-5555-released=%s" % ("true" if any(r.get("released") is True for r in clean) else "false"))
print("dashed-rows=%d" % len([r for r in rows if w(r)=="1793000005555-1"]))
PYFACTS
)
check "the ledger's rows were read back as data" "$rowfacts" "rows-parsed=yes"
# F4a/F4b/F4c, restated as one assertion no field can dodge: no row's worker address is padded. This
# catches the write-then-refuse (m6), the same with the name dropped (m10), and both NBSP spellings.
check "F4a no row in this ledger carries a padded worker address" "$rowfacts" "padded-rows=0"
# m9: the empty and blank doors are named as row-absence claims, so they are checked as row absence.
check "G9c/G10c no row carries an empty or blank worker" "$rowfacts" "blank-rows=0"
# Row 14: an adoption and its release are two durable records.
check "row14a the released adoption is still a row, in epoch 1" "$rowfacts" "epoch1-row-present=1"
check "row14b and it is marked released, with its reason" "$rowfacts" "epoch1-row-released=true"
check "row14c the re-adoption is a SECOND row, not a rewrite of the first" "$rowfacts" "epoch2-rows=1"
# m11: a release that was REFUSED for padding must not have marked the row it was refused over. The
# read-back runs after G20 now; before it ran before G19/G20, so nothing could see this.
check "G20c the refused padded release marked no row" "$rowfacts" "clean-5555-released=false"
check "G20d and that row is still the only one for its address" "$rowfacts" "clean-5555-rows=1"
# Claim 1 of the brief, witnessed rather than asserted: a dashed, same-millisecond id is a real row.
check "G18i a dashed id is adopted, so the rule is whitespace and not id shape" "$rowfacts" "dashed-rows=1"

# G12 — a release over an unobserved exit is refused, and the refusal says which operation and why.
check "G12 a release is refused while a cancellation exit is unobserved" "$run" \
    "G12 ERR the exit of 1793000007777 was never observed"
check "G12 and the refusal names the operation whose exit is in doubt" "$run" \
    "G12 ERR the exit of 1793000007777 was never observed: \`coordinator_cancel\` recorded a durable cancellation intent (operation \`op-x\`"
check "G13 the same release goes through when the row carries no unobserved exit" "$run" \
    "G13 OK released 1793000008888 (epoch 1); monitoring ends now"
# G14 must assert what the rendering SAYS, not that the tool returned Ok: `"G14 OK"` alone stayed
# green with the epoch deleted from `adoptions.rn`, which the independent review caught.
check "G14 the adoption rendering names the row's epoch and what the coordinator calls it" "$run" \
    "epoch 1 — 1793000008888 (called \`seeded ok\`)"

# G15 — `cancel.rn`'s control-character refusal: its only witness, and the arm that proves the
# literal fix in `6ea681e` reaches this file too (before it, a newline in `reason` was accepted).
check "G15 a control character in cancel's reason is refused" "$run" \
    "G15 ERR \`reason\` carries a control character"
# G16 — an operation the log holds nothing for: the answer claims nothing and writes nothing down,
# which is a different answer from "no cancellation was ever recorded".
check "G16 cancelling an operation the log does not know claims nothing" "$run" \
    "G16 ERR cancellation of \`op-that-never-existed\`: **nothing is claimed and nothing is written down**"
check_absent "G16b and it was not answered as a stop" "$run" "G16 OK"
# G17 — no operation named: refused by the schema, naming the field.
check "G17 a cancel with no operation names the missing field" "$run" \
    "G17 ERR \`coordinator_cancel\` was called with invalid arguments: missing required field \`operation\`"

# G21 — the LIVE-SESSION branch (witness map row 17): an address a live session actually carries is
# answered as live, with the state and the last outcome the roster publishes, rather than as a record
# nobody carries. Skipped and counted when the fixture could not start one.
if printf '%s' "$run" | grep -qF 'G21 SKIP'; then
    checks=$((checks + 1)); skipped=$((skipped + 1))
    say "ok   G21 SKIPPED (counted): no live session was available to carry an address"
else
    check "G21 an adoption of a live session is answered as live" "$run" \
        "G21 OK adopted $live in epoch 1. A live session carries that address now: state"
    check_absent "G21b and it is not answered as an address nobody carries" "$run" \
        "G21 OK adopted $live in epoch 1. No live session carries that address"
fi
# G8 — a release with nothing named; refused by the schema, naming the field.
check "G8 a release with no session names the missing field" "$run" \
    "G8 ERR \`coordinator_release\` was called with invalid arguments: missing required field \`session\` (string)"
# The refusals must be refusals: none of them may be answered as a success line.
# F3: the name reaches exactly as far as the assertion does — this looks at G4's answer only.
check_absent "G4 was not answered as an OK" "$run" "G4 OK"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-guards complete: $checks checks ($skipped skipped), 0 FAIL"
else
    say "live-rune-guards FAILED: $fail of $checks checks ($skipped skipped)"
    exit 1
fi

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

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
# The seed fixture (test-only, never shipped) is the only way to put a row carrying
# `cancellation: attempted` in front of `coordinator_release` without a real child process.
cp "$here/lifecycle-fixture/tools/seed_json.rn" "$plug/tools/"
sed -i 's/"rank", "unrank", "ranks", "hook_ready", "hook_peer"\]/"rank", "unrank", "ranks", "hook_ready", "hook_peer", "seed_json"]/' "$plug/plugin.rn"
grep -q 'seed_json' "$plug/plugin.rn" || { say "FAIL the seed fixture is not registered"; exit 1; }
cp "$here/rune-guards.rn" "$plug/workflows/rune-guards.rn"
"$EID" plugins trust coordinator >/dev/null

say "=== driving the guards through the plugin's own tools ==="
run=$("$EID" workflow run "$plug" rune-guards --provider mock --yolo 2>&1 || true)
printf '%s\n' "$run" | sed 's/^/     /'

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
check "G4 a second live adoption is refused, naming its epoch" "$run" \
    "G4 ERR $worker is already adopted by this session, in epoch 1 and still monitored"
check "G4 and the refusal points at the release that ends it" "$run" "\`coordinator_release\` ends it first"
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

# G8 — a release with nothing named; refused by the schema, naming the field.
check "G8 a release with no session names the missing field" "$run" \
    "G8 ERR \`coordinator_release\` was called with invalid arguments: missing required field \`session\` (string)"
# The refusals must be refusals: none of them may be answered as a success line.
check_absent "no refusal was answered as an OK" "$run" "G4 OK"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-guards complete: $checks checks ($skipped skipped), 0 FAIL"
else
    say "live-rune-guards FAILED: $fail of $checks checks ($skipped skipped)"
    exit 1
fi

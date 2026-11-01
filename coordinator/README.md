# coordinator

Explicit adoption of worker sessions by a coordinator, and one notice per observed
halt. A coordinator session says "I adopt that session"; this plugin keeps that
ledger, and its `hook_peer` lifecycle handler sends the coordinator **one notice per
observed halt**, with the worker's address, the name the coordinator gave it, the
state, the last outcome and the ending record it was keyed on. Nothing scans from
outside a session: there is no monitor, no port and no service.

Adoption is an act, not a label. No tag, no name and no shared directory produces
one, and none of them routes anything: a `coordinator` tag is a string the harness
stores and prints, and this plugin decides nothing from it. A notice tells a
coordinator something happened; it never resumes, steers or cancels the worker, and
nothing about adoption gives a coordinator authority over anybody.

| tool | file | does |
|---|---|---|
| `coordinator_cancel` | `tools/cancel.rn` | separately admitted cancellation of an owned child before any release attempt; exit observation, not retirement |
| `coordinator_start` | `tools/start.rn` | one admitted held launch + durable adoption + release; explicit partial/ambiguous outcomes |
| `coordinator_adopt` | `tools/adopt.rn` | adopt one worker, by durable address, as this session |
| `coordinator_release` | `tools/release.rn` | end one adoption: monitoring stops, unsent notices are cancelled, the record stays as history |
| `coordinator_adoptions` | `tools/adoptions.rn` | what this session has adopted, and what each worker is doing now |
| `coordinator_notices` | `tools/notices.rn` | every notice owed to this session, and whether its row is journaled or still open |

The tools are thin doors onto what the harness already stores. **There is no service:**
the monitor that used to hold adoption records and watch workers is retired, and rank is a
tag on the ranked session rather than a word this plugin keeps. What remains is the ledger
the tools read and write through plugin state, the custody doors a managed start goes
through, and two lifecycle handlers.

## Managed start (isolated implementation; activation is separate)

`coordinator_start` takes an operation id, existing workspace, explicit model,
optional persona and brief. The registry dispatcher admits the call; its Rune
wrapper derives the caller from the roster, never from model input. The door reaches
core's custody API — the same admitted tool path as adoption, and no second admission
mechanism — and there is no service on the other side of it: nothing here takes a
token, and `EIDOLON_COORDINATOR_BIN`, which older installs may still set, is read by
nothing. The child is persistent headless chat, not a desktop-visibility claim.

Durable intent precedes launch; durable adoption precedes release. The brief is
one **correlated delivery**: a key is minted once, stored on the operation before
the transport is given anything, and the record that key names in the worker's own
log is the whole of the delivery — there is no inbox message beside it. The store
keeps the answer as itself: `Journaled` (this call appended the record), `Existing`
(an earlier call did, and nothing was sent again), `Refused` with the recipient's
own kind, `Unknown` (no durable acceptance and no evidence of absence), or
`Unsupported` (the worker's build does not know the op — nothing was sent and no
legacy message was sent in its place). None of them is consumption or execution
proof, and **a correlated delivery does not wake the recipient**: the record is in
its log and it reads that at its next turn. A repeat of the same immutable
operation re-sends under the same key, which the recipient deduplicates, and
retrying an operation whose record is already journaled sends nothing at all.

Because a record in a log starts no turn, the start path does not stop there: once
the brief is journaled and the child is running, the door sends **one ordinary
wake** — inbox mail carrying no brief bytes and no delivery key, whose text names
the operation and the record in the worker's own log. Its answer is kept as
`Answered` (`delivered`/`queued` have arrived, and are never sent again;
`failed`/`refused`/`partial` are re-attempted) or `Undeliverable`, which says the
record is in the log and the accepted work has still not begun — repeating the
operation re-attempts the wake and never a second brief. `delivered`/`queued` mean
the mail is in the worker's inbox; none of these words means a turn started. After a
restart the wake's state is read back from the launch's own durable row (`WakeState`),
and a receipt the row does not hold is Unknown — never permission to respawn. The
child's own stdin is the thing that ends a headless run: a child whose owner goes away
exits at its input boundary and journals the closure. No automatic recovery budget,
context compaction, retirement policy or live deployment is implied.

### Current operational limits

The registry tool derives the caller from the roster; the door behind it authenticates
transport, not the caller's identity, and the plugin's own admission is what keeps an
agent's start on the admitted tool path. A start waits on the child it is holding: one
start can delay the other custody acts of the same session while it awaits one.

`coordinator_cancel` is narrower than termination: it closes only the private
channel/stdin of this service instance's still-owned child, and refuses any release
attempt (including an uncertain ACK), a reached session, changed/inactive adoption,
or lost ownership after restart. No PID reacquisition, signal, force-kill or automatic
retry occurs. Cancellation intent is durable before pipe closure and permanently
blocks further start/release of this operation. An observation window of two seconds
bounds the call; timeout or probe failure means exit unconfirmed, never success.

The durable cancellation distinguishes Attempted from Exited, with caller in the
owning request, intent time/reason, and observed exit time/status/code/signal. Roster
evidence is pre-close, not atomic with the exit. Only an owned Child's reaped status
establishes exit. A durable Exited receipt is historical, not a current-liveness claim;
even reading it through cancel after restart refuses when host ownership is lost.
An explicit retry must use the same reason and re-enters the dispatcher gate.

Cancellation never closes adoption. Pending/unconfirmed cancellation blocks the
separate `coordinator_release` adoption verb; after durable Exited, that existing
admitted verb can close it. Until then supervision remains, including on failed
cleanup or failed receipt persistence. This does not add general retirement policy.
Stopping a whole monitor still closes all its owned pipes; it is not a substitute
for this scoped operation or permission to restart a live fleet.

A brief delivery that ended `Unknown`, or whose send was cut off before any answer
(the durable write-ahead `Attempted`), is resumed by repeating the same immutable
operation: the delivery key and binding digest were durable before the first send,
so the repeat carries them and the recipient answers `Existing` rather than taking
a second brief. That is correlation, not a claim: nothing here reports delivery on
`Unknown`, and a `Refused` or `Unsupported` answer is never retried or downgraded
to the legacy inbox path. A store written before correlated delivery has an
`Attempted` with no key, and there the ambiguity is real — it is not resent, do not
repair it by silently editing the store or treating transcript absence as proof of
non-delivery. A legacy `Accepted` receipt likewise stays readable and is never
reported as a correlated record. Neither claims exactly-once delivery.

## Rank blocks: what a child is told beside its persona

A managed start may name a rank: `imperator`, `legatus`, `tribunus`, `miles`, or
optionally `centurio`. The names are the operator's convention — this plugin adds,
renames and infers none of them, and writes no doctrine into a block.

The rank's text lives in one file per rank under the plugin's own `ranks/`
directory (`<plugin>/ranks/<rank>.md`, override the directory with
`EIDOLON_COORDINATOR_RANKS`). Those are ordinary files an operator edits and
reviews, and they ship as placeholders that say they are unwritten: **nothing is
appended while a block holds only whitespace and its placeholder comment, and a
rank with no file at all appends nothing either.** A rank without text is a
session told no rank text, never a failed launch.

The addition is delivered through the CLI's generic flag:

```
eidolon … chat --model <model> --system-append <launches>/<instance>.system-append
```

That file holds **the exact bytes that were hashed** for this launch: written 0600
into the launch's own 0700 directory, flushed, made read-only (0400), re-read and
re-hashed against the recorded digest before the spawn, and left there with the log.
It is appended to
the resolved system prompt before the session exists, so the harness's
`SessionStart` record captures the prompt the child actually had. It sits beside the
persona; it is **not** a persona rewrite (never folded into `--persona`, whose
semantics are the persona note's), and it enforces nothing.

The binding is the part that matters. When the launch is prepared the door reads
the block's bytes, hashes them (SHA-256), and puts **the rank name and that digest**
into the operation's immutable request before the intent is durably recorded. A
caller names the rank and never the digest: a digest the door did not compute
from the file is refused. The block is read again immediately before the child is
spawned, and bytes that no longer hash to the recorded digest refuse the spawn with
`the <rank> rank block … changed after this launch was prepared`. A retry of the
same operation after somebody edited the block is therefore refused, not re-told
different text; changing a rank's text means a new operation. A refusal to start
leaves the host reporting `NotCreated`, because no child was made — it is not an
attempt whose outcome is unknown.

**The digest travels with the text.** The block's *path* is never handed onward: a
path promises that the file will still say the same thing when the CLI reads it, and
that promise is exactly what a digest replaces. The host writes the hashed bytes to
a launch-private file and passes that, so the text the CLI composes is the text the
record names. The remaining exposure is a deliberate same-user rewrite of the
launch's own read-only copy in the moment between that final re-read and the child's
open — the same user could rewrite the store just as easily. It is not the operator
editing a block after a launch was prepared, which is the case the binding exists for,
and which is refused by name.

`--system-append` is the core/CLI lane's flag, and it is **per subcommand**. Before
spawning a ranked child the door asks the harness (`chat --help`) whether that
subcommand carries the flag, and a harness whose `chat` does not is refused by name
rather than started unranked:

```
this harness's `chat` does not accept `--system-append`, so the `miles` rank block
cannot be appended to the child's resolved system prompt: `eidolon chat --help`
exited 0 and its help does not list the flag. Refusing to start — …
```

The installed harness at this plugin's ref (`c66e430`) has no `--system-append` on
any subcommand, and the core lane's current head (`c78dc43`) carries it on `run`
only — not on `chat`. So **a ranked start refuses on today's harness**, by the
sentence above, before anything is created: the plugin's half is complete and
exercised against a stand-in child, and the child half needs `chat --system-append`
in the CLI. Nothing here folds rank text into `--persona` to work around it, and a
ranked start is never reported as managed when it cannot be delivered.

## What a notice is, and what it is not

**Read this as the rules the retired monitor worked out, not as a description of a
running process.** The subject moved with the subject, not away: `hook_peer` decides
the same things, in `lib/lifecycle.rn`, and journals the notice through
`eidolon::notice` into this session's own log. There is no transport, no attempt count
and no `delivered`/`queued` word on that path — a row is `journaled` or `pending`, and
`notices.rn`'s own sentences are the authority on what each means. What follows is
kept because the reasons still bind the handler.

- **One per occurrence.** A notice is keyed on the worker, the adoption epoch and the
  harness's ending evidence. For a turn the journal settled, that is **`outcome_at`** —
  the record that ended it. Two halts ending with the same word are two halts, because
  the records differ; a *count* of records would be the wrong evidence, since a peer's
  mail advances it while the same ending stands.
- **Unjournaled endings have their own key.** An `errored` ending may leave no journal
  record at all — only the driver that ran the turn saw it. The harness counts those per
  **registration instance**: `outcome_observed` is 1 for the first, 2 for the second,
  and `started_ms` names the instance, because a registration's *name* is reused across
  a restart. Two identical errored endings are therefore two notices — not one, which
  is what keying on the registration alone would have given. The count is a bare
  ordinal on purpose: **the state filters the kind, the count only tells two of the same
  kind apart.** `state: stuck` is derived from the harness's halting endings, of which
  there are exactly three (an error, an iteration limit, a truncated reply), and a
  cancellation is deliberately not one of them — it is idle with `last: cancelled`. So
  this trigger cannot fire on a cancellation, and the count is never read as an error
  signal.
- **No evidence, no notice.** The count has three states and they are not the same:
  absent/null is *not established* (an older writer's row, or a registration that has
  not published an ending yet), `0` is *established and empty* (its last ending came
  from a journaled settle), and `>= 1` is an unjournaled ending. The harness writes the
  ending word and this count together, out of one projection, so an `errored` ending
  with `0` or with no count at all cannot come from this build: the handler reports that
  as an older writer's row and announces nothing, rather than guessing which halt it
  was. The count is an occurrence ordinal, **not an error signal** — it also covers a
  cancellation that journaled no settle — so it is `last` that names the kind. Keying on
  the ending word alone would repeat on every scan and wake a coordinator forever.
- **At least once, never "exactly once".** The order is in the code, because the order is
  the claim: what a scan writes down is **persisted before anything is sent**, and the
  answer is persisted **after** the attempt. A store that cannot be written stops the
  scan before a single send — a halt that cannot be recorded is not delivered — and a
  monitor whose state may contradict the disk **stops** rather than keep serving it:
  after a send whose answer could not be written, the live copy holds the answer while
  the file still holds the notice as pending, and only a restart that re-reads the file
  can say what really landed. A restart then attempts the same `notice_id` rather than
  inventing a second notice — that is the intended behaviour, and the retry-after-reload
  half of it is what the lifecycle fixtures show; a real process restart and retry has
  not been run. A notice that arrived, was queued, or was partly delivered is never sent
  again.

## The store itself

**This is the store the monitor kept, and nothing reads it now.** It is described here
because it is what the uninstall step deletes and because the rules below shaped the
ledger the tools keep today; the tools' durable state is plugin state
(`plugin_state_read`/`plugin_state_write`, one row per key, compare-and-set), not this
file.

One JSON file under `${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/coordinator/`,
replaced by writing a temporary file beside it, **flushing it to the disk**, renaming it
over the target, and flushing the directory entry too. So a reader sees the old store or
the new one and never a half-written one, and the rename survives a power loss rather
than only a process death. Three rules beside that:

- **A store that exists and cannot be read is a refusal, not a fresh start.** A missing
  file starts empty; a corrupt or unreadable one stops the monitor at startup, with the
  bytes left exactly where they are — starting empty would replace a coordinator's
  history on the next write.
- **One writer per store.** The lock is taken non-blocking at startup, so a second
  `--once` or a second service refuses to run rather than interleaving writes with the
  first.
- **A mutation that cannot be persisted is refused.** `adopt` and `release` write to a
  copy and move the live record only once the write succeeded: an adoption that could
  not be written down is not an adoption, and the answer says so instead of claiming
  success.
- **A failure is classified, because "failed" is not one thing.** Anything before the
  rename — creating the file, writing it, flushing it, renaming it — leaves the target
  exactly as it was, and that is the only case a caller may treat as "nothing happened".
  A failure *after* the rename (opening the directory to flush the entry, or that flush
  itself) means the file on disk is **probably the new content already** and only its
  durability is unknown: `adopt` and `release` then re-read the store, take their state
  from the file, and say the outcome is unknown rather than pretending the write never
  landed.
- **A monitor that cannot write its store stops.** Every door refuses from that moment
  and the process exits, because state that cannot be vouched for must not be served; a
  restart reads the file and finds out what really landed.
- **The harness's own outcome word, and no more.** `eidolon send --json` reports
  `delivered`/`queued`/`partial`/`failed`/`refused`; the monitor stores that word and
  the transport's bytes verbatim. **None of them means the coordinator has read the
  notice.** `delivered` says a door took it; `queued` is not consumption — the
  recipient's inbox goes with its registration, so a queued notice can disappear with
  it. `partial` toward this monitor's single direct recipient is an anomaly, reported
  as one and never smoothed into success.
- **Retries are bounded and cannot duplicate.** A `failed` notice is retried up to
  three attempts; `delivered`, `queued` and `partial` notices are never sent again.
- **A release cannot unwrite.** Notices already answered keep their answer; a release
  cancels what that epoch still owes and stops the watching.

## Limits, stated plainly

- **The address is a name the harness mints, and it is opaque here.** `session` is the
  id core mints at session creation — the log's **file stem**, `<epoch_ms>` or
  `<epoch_ms>-N` when a sibling claimed that millisecond — and `log` beside it is the
  locator; they are different fields on purpose. This plugin echoes the id verbatim,
  never derives one from a path, never shortens or joins it, and never matches a name
  or a tag to find one. `id` is the ephemeral registration locator and keys nothing
  (only, as above, half of an unjournaled ending's key). It is null only when there is
  no log to name, and null means *not established*: then the verb refuses, naming that,
  rather than substituting a path or a label.
- **Renaming the log renames the id.** A resume, an import and a `--at` fork keep it
  (the fork is a head move inside one file), but a renamed log is a renamed session —
  so an adoption made under the old id stops resolving. It is reported as unresolved
  and never silently rebound; the adoption has to be released and made again.
- **Two live sessions can carry one id** (files that arrived from another sessions
  directory or machine). The harness refuses that, naming both, rather than picking
  one. The handler surfaces that refusal verbatim — the row stays `pending` with the
  log's own words, which is an open obligation and not a delivery — and it is never
  retried as a second notice under a different id, because the id is stored opaquely.
- **Release and offline-adoption semantics are provisional.** Ending an epoch,
  cancelling what it still owed, keeping the record as history and starting a new epoch
  on re-adoption are implemented as described above, but they are **not operator-ruled
  yet** — nor is whether an address with no live session may be adopted at all (this
  build records it and says "no live session carries this address"). Treat them as
  unfinished until they are ruled.
- **The caller is bound to the adopting session by this plugin's own door, not by a
  credential.** The binding holds only because `tools/adopt.rn` reads this session's own
  row from the roster and never from its input — a model cannot name a coordinator.
  There is no token any more: older installs may still carry
  `~/.config/eidolon/coordinator.token`, and nothing reads it. What is enforceable is
  this plugin's door; that is what is claimed.
- **An address is opaque and provisional.** Echoed verbatim, never canonicalised;
  `id` is the ephemeral registration locator and is never used to match a worker
  (only, as above, to key an errored ending). If the harness's spelling of the
  durable address changes, an adoption stops resolving and is reported as unresolved
  — never silently rebound to whatever now looks similar.

## What this plugin runs

**Nothing.** There is no monitor, no port, no token file and no service to approve or
start. The process that used to watch adopted workers on `127.0.0.1:8095` is retired,
and the reason is worth keeping: it was the second store of a word that belongs to the
session. Rank is now `rank:<word>` in the ranked session's own tags, written through the
harness's tag door (`swarm_call("tag", …)`), so the only store is the session's, the
harness keeps it beside the session's log, and it survives a resume without anyone
reasserting it.

What is left runs inside a session, as plugin tools and two lifecycle handlers:

- `adopt`, `release`, `adoptions`, `notices`, `start`, `cancel` — the ledger, the watch
  list, and the custody doors a managed start goes through.
- `rank`, `unrank`, `ranks` — a tag on a session, and a roster read.
- `hook_ready`, `hook_peer` — the lifecycle handlers. `hook_ready` reconciles the watch
  list against the roster; **it no longer reasserts a rank**, because a tag does not need
  reasserting. `hook_peer` produces one notice per observed halt.

One sharp edge, because it is the only way a rank disappears with no act that names a
rank: a session's own `label` call writes its **whole** tag set. Setting or clearing a
cosmetic tag there therefore drops a `rank:` tag it was given — the ruling allows it (a
session may change its own tags), and nothing on this side can stop it. The rank tools
never go through `label`: `rank` replaces the `rank:` family, `unrank` removes it, and
neither touches any other tag.

## Prerequisites

- eidolon with the plugin runtime, the structured roster and **the tag door**
  (`swarm_call("tag")`). Without it the rank tools refuse and say so; nothing else in
  the plugin needs it.
- `eidolon chat` with the generic `--system-append <path>` flag, if any start names a
  rank. A harness without it fails a ranked launch rather than starting the session
  without the rank text (see **Rank blocks**).
- No Rust toolchain: there is no crate here to build any more.

## Install

```bash
eidolon plugins install <owner>/<repo> coordinator
eidolon plugins trust coordinator
```

`trust` vouches the verbs. There is **no `grant`** and no `service approve`/`service
start`: no tool spends a credential or dials an endpoint, so there is nothing to grant
and nothing to start. An older install of this plugin may still carry
`~/.config/eidolon/coordinator.token` and a policy grant naming it; both are inert now
and are removed in **Uninstall** below.

## Verify

```bash
eidolon plugins --dir <checkout>        # the plugin lists; every tool's manifest reads
python3 tests/coordinator/lifecycle-copies.py --check   # 123 copied functions across 8 handlers
bash tests/coordinator/lifecycle-shape.sh               # the library's own policy
bash tests/coordinator/lifecycle-drift.sh
EIDOLON_BIN=<a built eidolon> bash tests/coordinator/run.sh   # 22 checks; the rank witness
```

The monitor's own fixtures (`ack-failure.sh`, `roster-failure.sh`, `live-rune-path.sh`,
`live-rune-start.sh`, `rank-launch.py`, `rank-prefix.py`, `managed-start.py`,
`managed-cancel.py`) measured behaviour that no longer exists; they are removed with it
rather than left red. `run.sh` is not one of them: it was rewritten onto the harness-only
subject and is the fixture that exercises `rank`/`ranks`/`unrank` against a live
registration — a green listing alone proves only what the manifests compile to.

## Uninstall

```bash
eidolon plugins uninstall coordinator
rm -f ~/.config/eidolon/coordinator.token          # inert if an older install made one
rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/coordinator"   # the retired monitor's store
```

The state directory is the retired monitor's and nothing reads it: deleting it is what
ends the old adoption records, and no rank lives there. A rank lives in the ranked
session's own tags, beside that session's log — uninstall this plugin and the words it
set stay where they were put, because they were never the plugin's to keep.

The rank blocks live inside the plugin folder (`ranks/`), because that folder is the
plugin directory. `eidolon plugins uninstall` removes it, so copy any block you wrote
before uninstalling: the operator's own words are not something the plugin can
recreate.

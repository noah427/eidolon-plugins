# coordinator

Explicit adoption of worker sessions by a coordinator, and one notice per observed
halt. A coordinator session says "I adopt that session"; the monitor watches it
from then on and sends the coordinator **one notice per observed halt**, with the
worker's address, the name the coordinator gave it, the state, the last outcome and
the ending record it was keyed on.

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
| `coordinator_notices` | `tools/notices.rn` | every notice owed to this session, and what the transport reported for each |

The tools are thin doors. The `coordinator` service — `eidolon-coordinator`,
built by `package.nix` — holds the records, does the watching and sends the notices,
so no session has to be awake for a halt to be noticed.

## Managed start (isolated implementation; activation is separate)

`coordinator_start` takes an operation id, existing workspace, explicit model,
optional persona and brief. The registry dispatcher admits the call; its Rune
wrapper derives the caller from the roster, never from model input. The service
uses the same authenticated API boundary as adoption, not a second admission
mechanism. Token possession is transport auth, not proof of a caller's role.
`EIDOLON_COORDINATOR_BIN` must name a harness with the enforced startup hold.
The current child is persistent headless chat, not a desktop-visibility claim.

Durable intent precedes launch; durable adoption precedes release. Brief send has
its own write-ahead Attempted/Accepted record. Accepted is the transport's word,
not consumption or execution proof. An ambiguous attempt is never blindly resent.
Retry the same immutable operation to reconcile; never change its input under the
same id. After monitor restart, missing process/channel receipts mean Unknown,
not permission to respawn. Monitor death closes the chat's owned stdin; the child
exits at its input boundary and journals the closure. No automatic recovery budget,
context compaction, retirement policy or live deployment is implied.

### Current operational limits

The registry tool derives the caller from the roster; the service authenticates
transport, not the caller's identity. Its API must remain behind the admitted tool
path for agent use. Starts are serialized under the store Gate and process-host
lock: one start can delay other mutations and monitor scans while awaiting a child.

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

Attempted-without-Accepted brief delivery remains ambiguous and is not resent.
There is currently no supported resolution verb: do not repair it by silently
editing the store or treating transcript absence as proof of non-delivery. A future
explicitly admitted resolution must preserve who decided, when, observed evidence
and the chosen action. It must not claim exactly-once delivery. Until that surface
exists, this operation remains blocked/ambiguous rather than declared complete.

## What a notice is, and what it is not

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
  with `0` or with no count at all cannot come from this build: the monitor reports that
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
  half of it is what the monitor's tests show; a real process restart and retry has not
  been run. A notice that arrived, was queued, or was partly delivered is never sent
  again.

## The store itself

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
  one. The monitor surfaces that refusal verbatim as the notice's outcome (`refused`)
  and does not retry it — the record is safe either way, because the id is stored
  opaquely.
- **Release and offline-adoption semantics are provisional.** Ending an epoch,
  cancelling what it still owed, keeping the record as history and starting a new epoch
  on re-adoption are implemented as described above, but they are **not operator-ruled
  yet** — nor is whether an address with no live session may be adopted at all (this
  build records it and says "no live session carries this address"). Treat them as
  unfinished until they are ruled.
- **The token is transport auth, not role auth.** The caller is bound to the adopting
  session only because `tools/adopt.rn` reads this session's own row from the roster
  and never from its input — a model cannot name a coordinator. A process that reads
  the token can speak to the service directly, and the service takes the `caller` it
  is given. What is enforceable is this plugin's door; that is what is claimed.
- **An address is opaque and provisional.** Echoed verbatim, never canonicalised;
  `id` is the ephemeral registration locator and is never used to match a worker
  (only, as above, to key an errored ending). If the harness's spelling of the
  durable address changes, an adoption stops resolving and is reported as unresolved
  — never silently rebound to whatever now looks similar.

## Prerequisites

- eidolon with the plugin runtime and the structured roster, and `eidolon` on `PATH`.
- Rust (or Nix) to build the monitor.

## Install

```bash
eidolon plugins install <owner>/<repo> coordinator
nix build .#coordinator            # or: cargo build -p eidolon-coordinator
eidolon plugins trust coordinator
for v in adopt release adoptions notices start; do
    eidolon plugins grant "coordinator_$v" file:~/.config/eidolon/coordinator.token
done
eidolon plugins service approve coordinator
eidolon plugins service start coordinator
```

`trust` vouches the verbs and `grant` records the credential each of them spends.
`service approve` records the operator's yes against the service declaration by
hash, so editing `plugin.rn` asks again; the monitor makes
`~/.config/eidolon/coordinator.token` (0600) on first run.

## Verify

```bash
eidolon plugins service status coordinator          # the /health probe
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/coordinator/run.sh                     # the fixture: 43 checks
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/coordinator/ack-failure.sh             # the process stops when it cannot write: 8 checks
cargo test -p eidolon-coordinator                   # the monitor's own 37
```

The fixture drives the real monitor against a stand-in harness — its own port, its
own state root, a fake `eidolon` answering `peers --json` and `send --json` — and
then checks that the shipped tools refuse, naming what is missing, against the real
harness. It proves the plumbing, not the harness.

## Uninstall

```bash
eidolon plugins service stop coordinator
eidolon plugins uninstall coordinator
rm -f ~/.config/eidolon/coordinator.token
rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/coordinator"
```

The state directory holds the adoption records and the notices; deleting it is what
ends them, and there is nothing else to clean up.

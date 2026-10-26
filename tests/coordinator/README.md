# tests/coordinator

The `coordinator` plugin's fixture. It touches nothing of the operator's: its own
port, its own state root, its own token, a throwaway `HOME`.

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/coordinator/run.sh
```

`guard.sh` refuses unless `HOME` and `XDG_CONFIG_HOME` are temp directories. The
monitor is rebuilt from the worktree first, so the fixture never measures a stale
binary; set `EIDOLON_COORDINATOR_BIN` to point it at a build of your own.

## What it is evidence of

**The monitor, end to end, against a stand-in harness.** A fake `eidolon` answers
`peers --json` from a fixture file and `send --json` with a typed outcome, appending
every send to a log. The real service binary runs on a port of its own with its own
`--root`, and its doors are driven with curl. What that pins (37 checks):

- a call without the token is refused before anything else;
- an adoption with no `caller` is refused — nobody may name the adopting session but
  the tool that reads it from the roster;
- adopting says what it saw, and the inventory resolves the worker to what the roster
  publishes, without reporting it as gone;
- one halt means **one** send, addressed to the coordinator, asked for the typed
  outcome (`--json`), framed as the monitor's and **not** as the operator's;
- the same halt again sends nothing; a **new ending record** with the same ending
  word sends a second notice;
- `delivered` and `queued` are stored as the harness's own words, with its bytes
  verbatim, and nothing is called `unconfirmed`;
- a halt with **no ending record**, in a case that is not the errored fallback, sends
  nothing and the scan's own words say why;
- an `errored` ending is announced by the harness's own count for that registration
  instance (`outcome_observed`, with `started_ms`), so two identical errored endings are
  two notices. The count is an occurrence ordinal and not an error signal — it also
  covers a cancellation that journaled no settle — so `last` is what names the kind;
- a count of `0`, or no count at all, beside an `errored` ending is not announced, and
  the reason says which of the two it is (this build publishes the word and the count
  together, so a zero beside `errored` is an older writer's row);
- a `partial` send is reported as an anomaly rather than smoothed into success;
- `release` ends monitoring, refuses a worker this session never adopted, cancels what
  the epoch still owes, sends nothing afterwards, and leaves the record as history.

It is the plumbing that is being measured — the service, the record format, the dedup
and the transport call. It is **not** evidence about the harness: the rows are a
fixture.

**The shipped bytes fail closed.** The plugin is installed into the throwaway config,
vouched for there, and a workflow calls `coordinator_adoptions` against the *real*
harness with `--provider mock`. The real harness publishes the structured roster, but
this session has no durable seat in it (`session` is null, and no row marks the
caller), so the tool must refuse and name that — and say it will not invent an
address and will not parse prose — instead of answering as if nobody had been
adopted. `scenario.rn` is the workflow, copied into the temp plugin and not shipped.

## `ack-failure.sh` — the process, when it cannot write what it just did

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/coordinator/ack-failure.sh
```

The stand-in `eidolon` makes the store's directory unwritable **as it answers the
send**, and only then: the write-ahead write in that scan has already succeeded, the
notice has gone out, and the acknowledgement cannot be written. That leaves the live
process holding an answer the file does not, which is the state that must not be
served. What it measures, at the process level rather than the unit level: the monitor
prints why and **exits non-zero**, does not claim the notice is pending on disk, and
leaves the file holding the notice as `pending` under the same id.

What it is **not**: proof that a restart then retries it. No second monitor process is
started against that store. The retry-after-reload half is the unit test in
`coordinator/service/src/store.rs` (a fresh `Store` loaded from the file attempts the
same id again, word for word); the real process restart *and* retry is unverified.

## `roster-failure.sh` — the roster that cannot be read

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/coordinator/roster-failure.sh
```

A stand-in `eidolon` whose `peers --json` fails. An unreadable roster must make `adopt`
**refuse before anything is written** and say so, rather than answering "no live session
carries that address" — a reader must not have to guess which of the two happened. Nine
checks: the refusal's wording, that nothing was recorded on disk, that the answer is not a
success and does not claim the worker is missing, and that the monitor is still running —
a roster failure is not a persistence failure and must not poison it.

## `live-rune-path.sh` — the plugin's own tool, against a real harness

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  EIDOLON_BIN=<a built eidolon> COORDINATOR_FIXTURE_KEEP=1 \
  bash tests/coordinator/live-rune-path.sh
```

The live one: it needs a real `eidolon` and starts real sessions of its own under a
throwaway HOME (a worker that hits its iteration wall and stays alive at `stuck`, and a
coordinator to receive a notice), then makes **two** adoptions of that one stuck worker —
one through the service API with a caller the script supplies, one through the plugin's
own tool driven by `eidolon workflow run`, where `coordinator_adopt` reads its own `me` row
and the coordinator on the record is the identity the harness minted for that run. It
checks the Rune-path caller against the id the harness itself printed for the run, not
against anything the tool said.

Every claim is one assertion inside the script's `verify`, which exits non-zero when any is
false, and the script then runs `verify` twice more with deliberately wrong inputs — a
wrong expected caller, and the two identities swapped — and requires it to fail. Those
injections are the proof the checks can fail; without them a passing fixture would only be
a claim. Twelve checks, 0 FAIL. Skip (exit 3) without `EIDOLON_BIN`.

## What it is NOT evidence of

- **Anything about a real worker.** No session is adopted, no session halts, no notice
  reaches a person. The only `send` is to a fake binary.
- **That a durable address is missing.** The opposite now: `session` is populated (the
  harness mints it from the log's file stem), and the shipped-bytes check drives the
  refusal path only because the temp session it runs in has no log to name. Re-run the
  check against a harness whose `session` is populated and the tool proceeds.
- **That the harness really queued or delivered anything.** The fake prints the
  outcome; the fixture asserts that the word is stored as given, not that it is true.
- **A retry path.** `failed`'s bounded retry is covered by the unit tests in
  `coordinator/service/src/lib.rs`, not here — and so is the whole persistence story:
  the lock refusing a second writer, a corrupt store refusing startup with its bytes
  left alone, nothing being sent when the write-ahead write fails, and the crash window
  between the send and the acknowledgement. Those are `coordinator/service/src/store.rs`
  tests, and they go through the real ordering with an injected write failure rather than
  mocking the cycle.
- **The operator's own state.** The fixture's port is chosen from a free-port list,
  its token and state are under `HOME=$t`, and it never reads
  `~/.config/eidolon/coordinator.token` or the operator's state root.

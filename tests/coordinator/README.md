# The coordinator fixtures that remain

The monitor this directory used to measure is retired, and its fixtures went with it:
`run.sh`, `ack-failure.sh`, `roster-failure.sh`, `live-rune-path.sh`, `live-rune-start.sh`,
`rank-launch.py`, `rank-prefix.py`, `managed-start.py` and `managed-cancel.py` all built or
dialled `eidolon-coordinator` on `127.0.0.1:8095`, with its token and its own store. None of
that exists now — rank is a tag on the ranked session and the tools reach only plugin state
and the custody doors — so a green run of them would have been a green run of nothing.

What is here still measures the plugin itself, and needs no port, no token and no service:

```bash
python3 tests/coordinator/lifecycle-copies.py --check   # the handler copies match the library
bash tests/coordinator/lifecycle-shape.sh               # the library's own policy
bash tests/coordinator/lifecycle-drift.sh               # the same, as the guard the repo runs
bash tests/coordinator/guard.sh                         # the guard's own arm
```

and the live-custody witnesses, each a session-shaped run against a pinned binary:

```bash
bash tests/coordinator/live-rune-hold-park-retry.sh
bash tests/coordinator/live-rune-held-start.sh
bash tests/coordinator/live-rune-cancel-rows.sh
bash tests/coordinator/live-rune-uncertain-write.sh
bash tests/coordinator/live-rune-allow-start-retry.sh
bash tests/coordinator/live-rune-public-start-retry.sh
bash tests/coordinator/live-rune-start-conflict.sh
bash tests/coordinator/live-rune-guards.sh
bash tests/coordinator/live-halt-end-to-end.sh
bash tests/coordinator/lifecycle-witness.sh
```

## The rank witness

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  EIDOLON_BIN=/path/to/a/built/eidolon bash tests/coordinator/run.sh
```

`run.sh` is the suite's entrypoint on the subject the plugin has left. It pins the binary from
`EIDOLON_BIN` (never `PATH` — this fixture's subject includes *which* build is under test), starts
one mock session as the registration a tag lives on, seeds one real policy row for one workflow
(no `--yolo`), and drives `coordinator_rank`, `coordinator_ranks` and `coordinator_unrank` through
22 checks: the tag lands, the row publishes it, a second rank **replaces the family** and spares an
unrelated tag, `ranks` reads it off the roster, the vocabulary is refused by the schema, an address
nobody registered is refused by core as no-such-session, a release reports the set that stands, a
second release is already true, and the tag **survives a resume** (the shell restarts the session on
the same log and reads the row).

Two test-only tools beside it (`rank-fixture/tools/seed_tag.rn`, `dump_tags.rn`) place and read a
tag through `swarm_call`, because a workflow has no `swarm_call` of its own; they are copied in and
registered the way the other fixtures register their seed tools, and are never shipped.

The pinned `bin/eidolon-537d999` predates the tag door and can only prove the tools compile.

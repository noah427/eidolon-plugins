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

**Not here yet:** the witness for `coordinator_rank`/`unrank`/`ranks` against a harness that
carries the tag door (`swarm_call("tag")`). It lands with the integrated binary that has it;
the pinned `537d999` predates the door and can only prove the tools compile.

# The coordinator's fixtures

What is left measures **roles only**, and needs no port, no token and no service:
`run.sh` (the rank witness, 22 checks against one throwaway config and one mock
session), the two test-only tools it copies in (`rank-fixture/`), and `guard.sh`,
which `run.sh` sources to refuse anything but a temp config.

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  EIDOLON_BIN=/path/to/a/built/eidolon bash tests/coordinator/run.sh
```

The binary is pinned from `EIDOLON_BIN`, never from `PATH`: the fixture's subject
includes *which* build is under test.

## What went, and why

The monitor this directory used to measure was retired first, and then the
supervision around it moved into the harness: starts, holds, cancellations,
retirements and transfers are core's custody doors, and the words a session can
be called by are the operator's `roles.rn`. The fixtures that drove the plugin's
own machinery therefore measured nothing that exists any more, so they went with
it:

- `lifecycle-copies.py`, `lifecycle-shape.sh`, `lifecycle-drift.sh`,
  `lifecycle-hooks.rn`, `lifecycle-hooks.sh`, `lifecycle-witness.rn`,
  `lifecycle-witness.sh`, `lifecycle.rn`, `lifecycle-fixture/` — the two
  lifecycle handlers (`hook_ready`, `hook_peer`) are gone: nothing in the plugin
  is invoked by the harness any more.
- `live-rune-*.sh`, `rune-*`, `scenario.rn`, `fresh-ledger.sh`,
  `live-halt-end-to-end.sh` — the managed start, cancel, release and adoption
  paths are gone: a launch is core's custody door, and adoption is not a concept
  this plugin has.

What those paths still promise — that a launch names a rank, is held, is
cancelled, is retired, and that a child is told its rank's text — is core's own
subject now and is measured in `eidolon`'s suite (`crates/core/src/custody.rs`,
`crates/cli/src/custody_doors.rs`, `crates/cli/src/custody_host.rs`), where the
rank blocks are read from `<plugin root>/ranks`.

# Rank permission presets — proposal, and the seam that is missing

Status: **proposal for review**, written 2026-11-17 on branch
`feat/coordinator-roles-only`. Nothing here is installed, and the operator has not ruled
on the matrix. The parent session owns the vault's final specification; this file is the
technical proposal and the evidence behind it.

## What is real today

**Prompt delivery is real and already wired.** The custody doors read
`<the asking plugin's admitted root>/ranks/<rank>.md`, hash the exact bytes into the
launch's immutable record, write a 0600 copy beside the child's log, and pass it to the
child as `--system-append <copy>` (`crates/cli/src/custody_host.rs`, `rank_block`,
`verify_rank`, `write_rank_copy`; `crates/cli/src/custody_doors.rs`, `launch_request`,
`rank_dir`). A file holding only HTML comments counts as *unwritten* and appends nothing
(`custody_host.rs::unwritten`, tested at `custody_host.rs:2098`). The five files in
`ranks/` therefore used to be inert placeholders; they now carry the rank text and are
delivered by that path, with no manual copy and no `~/.config` hand-editing. The bytes
that are appended are the bytes in this repository: a launch is refused if the file
changed after admission, and a launch admitted with "no written block" is refused if one
appears.

**Permissions are not wired at all.** No part of the launch path can carry a preset:

| question | where the answer would be | today |
| --- | --- | --- |
| does a start ask for capabilities? | `eidolon_core::custody_doors::StartAsk` | **no field** — `operation, workspace, model, persona, brief, rank, shape` only |
| does the child get a role or a holds set? | `custody_host::launch_command` | **no** — `chat/tui --model … [--persona …] [--system-append …]`, no `--role`, no `--rank`, no holds |
| can a script set a worker's holds? | Rune host surface | **no** — `eidolon_swarm::api::set_holds_at` exists in Rust and is called only by the TUI peers page |
| is `subset_of` enforced? | `eidolon_core::capability` | **no** — `CapabilitySet::subset_of` is used by core's own tests and by nothing else |
| does a rank tag select a holds set? | `crates/cli/src/main.rs` build | **no** — holds come from the worn **role**'s `holds` in `roles.rn`, else the owner preset `all_v1` |

Two consequences follow, and they are the honest limits of this proposal.

1. A plugin-launched child today is started with **no role**, so it wears `roles.rn`'s
   `default_role` (or nothing) and publishes that role's holds — the same set for every
   rank. A `miles` and an `imperator` launched through the custody doors hold identical
   capabilities unless the operator has written per-role sets and passed `--role`.
2. The seven-class grant is a **published fact and a delegation ceiling**, not tool
   gating. Nothing in the dispatcher refuses a call because its class is unheld; the
   roster bar draws the set, and a supervisor's power to delegate is expressed as
   "new ⊆ mine". Shipping a preset file today would be a dead config file: nothing would
   read it. So this repository ships **none**, and `rank.rn`/`ranks.rn`/`unrank.rn` are
   unchanged.

## The matrix proposed for the operator's ruling

The seven classes are the frozen v1 vocabulary (`crates/core/src/capability.rs`). A
supervisor can only delegate what it holds, so the sets must be monotone down the chain,
and a preset is a **ceiling that a brief may narrow**: never a floor, never a widening.

| rank | fs.read | fs.write | shell.run | net.reach | peers.message | workers.spawn | workers.edit | distinct from |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| imperator | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | legatus/tribunus |
| legatus | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | — (coincides) |
| tribunus | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | — (coincides) |
| centurio | ✓ | ✓ | ✓ | — | ✓ | ✓ | ✓ | tribunus, miles |
| miles | ✓ | ✓ | ✓ | — | ✓ | — | — | centurio |

Justification, class by class:

- **fs.read / fs.write / shell.run** — every rank produces or verifies work in the tree.
  A Miles writes code; every layer above must be able to inspect and test what it reviews,
  and must hold these to be allowed to delegate them.
- **peers.message** — every rank must report up and answer a blocker. A worker with no
  message class cannot deliver its result at all.
- **workers.spawn / workers.edit** — the *only* class that separates "does the work" from
  "runs a team". Miles holds neither, so "Miles does not recruit" is a missing capability
  rather than a sentence in a prompt. Centurio and above hold both: a leader that can
  start a worker but cannot change or retire it would strand it.
- **net.reach** — the one coarse class that can honestly be withheld from a bounded
  executor: a Miles's task is inside the tree it was given, and egress is the class most
  worth an explicit grant. Centurio and above hold it because they may need to look
  something up while integrating, and because a supervisor cannot delegate a class it
  lacks.

**Honest limitation.** With seven classes and a monotone rule, at most three distinct
sets exist: `miles`, `centurio`, and one officer set shared by tribunus, legatus and
imperator. That coincidence is not a design failure to paper over, and inventing a fourth
distinction (e.g. taking `shell.run` from the Imperator while demanding it of a Legatus)
would break the subset rule and be gratuitous. Above Tribunus the rank's difference is
*scope of brief and who it reports to*, which is prompt text and reporting structure, not
a capability. Splitting `net.reach` into `net.fetch`/`net.publish` or `shell.run` into
`shell.run`/`shell.confine` would create real rank-level distinctions, but the operator's
powers-first model is explicit that shell cannot be confined by these classes, and adding
vocabulary is core's decision, not this plugin's.

Also explicit: **grants, not titles.** A rank never adds a class. The matrix is what the
operator *may* grant to a session of that rank; with no operator decision, a session holds
what its role says and nothing more.

## Minimum seam, if the operator wants presets to be real

Rank-agnostic and small enough to review as one change; none of it names a rank.

1. **Payload.** `ranks/<word>.holds` beside the block, holding exactly
   `CapabilitySet::canonical_bytes` — the sorted wire spellings, newline-terminated — so
   the file's SHA-256 is `CapabilitySet::digest()` and one hashing path serves both.
   Absent file = no preset (the rank adds no permissions), which is the same shape as an
   unwritten `.md`.
2. **Door.** `StartAsk.holds: Option<Vec<String>>` is *not* what a caller supplies. The
   door reads the preset from the asking plugin's admitted root exactly as it reads the
   block, and binds `holds_digest` into the immutable launch record next to
   `rank_digest`. A retry that reads different bytes is a different launch and is refused.
3. **Subset check at the door**, using the existing function and no new vocabulary:
   the asking session's published set must be a superset of the preset it asks for
   (`preset.subset_of(caller)`), and where the ask changes an existing child, the child's
   *current* set must also be a subset (`child_now.subset_of(caller)`) so a supervisor can
   neither widen a worker past itself nor edit a session more privileged than itself. The
   operator is above this rule and is not subject to it.
4. **Child.** One generic flag, `--holds fs.read,shell.run`, parsed with
   `CapabilitySet::parse_set`; `build()` in `crates/cli/src/main.rs` publishes it in place
   of the role/owner preset, and never widens a set the operator already wrote (the
   existing "only when nothing stands" rule stays).
5. **Tests that would count**, mirroring the ones core already has for rank blocks:
   a preset file with `miles`' bytes makes a launched child publish exactly those four
   names; a changed preset between admission and spawn refuses the launch; a caller
   asking for a class it does not hold is refused; an edit naming a target holding a
   class the caller lacks is refused; and the `--system-append` copy plus `--holds` both
   appear in `launch_command` for a ranked start.

Until items 1–4 land, a per-rank preset file in this directory would be exactly the dead
configuration this proposal exists to avoid.

## The prompt blocks as delivered

`ranks/imperator.md`, `legatus.md`, `tribunus.md`, `centurio.md`, `miles.md` now carry the
duties the operator's requirements name: Imperator is the operator-facing delegate of
undertakings, Legatus owns one undertaking, Tribunus one workstream, Centurio a bounded
team, Miles bounded execution with no recruitment; upper ranks return a finished v1 rather
than per-patch ceremonies; reporting is to the immediate superior; no invented tasks; no
stop on a mere progress report; a real blocker plus an armed wait; and every block says
its text is not a permission and that rank never adds a lane.

The exact text is for the parent to put to the operator; the repository file is the
payload core loads, so an edit here is what a newly launched session is told, and no edit
rewrites an existing session's captured prompt.

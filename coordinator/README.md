# coordinator

The coordinator's **rank vocabulary**, and nothing else: five words, the tag that
puts one on a session, and the text a session of that rank is told.

There is no service, no ledger, no lifecycle handler, no monitor, no port, no
token and no supervision here. Those moved into the harness, where they belong:

| what | where it lives now |
|---|---|
| starting, holding, cancelling, retiring and transferring sessions; the custody state machine and its journal | `eidolon_core::custody` / `eidolon_core::custody_doors`, reached through the custody doors every script can call |
| the words a session can be called by — hue, persona, held set, rank | the operator's `~/.config/eidolon/roles.rn` (`crates/tui/src/roles.rs`), drawn by the roster chips |
| publishing a role's rank at start | core, from a role's `rank:` field, and only when no `rank:` tag already stands |
| the text appended to a session launched at a rank | `ranks/<word>.md` **in this directory** — the custody doors read `<plugin root>/ranks` at launch and nothing else does |

What is left is small on purpose. A rank is a word, a word is a tag on the
session it names, and a tag is storage and display: it is not a permission, not
an adoption, not a reporting line, and the harness reads `rank:` as a string and
attaches no meaning to it. The five words are already core's own role vocabulary:
core's role table ships them and draws each one's chip, so no file has to be
copied anywhere for that presentation to work.

| tool | file | does |
|---|---|---|
| `coordinator_rank` | `tools/rank.rn` | put one of the five words on a session, as `rank:<word>` among its own tags |
| `coordinator_ranks` | `tools/ranks.rn` | the ranks that stand now, read off the live roster — there is no second store to disagree with |
| `coordinator_unrank` | `tools/unrank.rn` | take the `rank:` family off a session, idempotently, and report the set that stands |

## Prerequisites

- A built `eidolon` new enough that `eidolon plugins` knows the plugin and core
  knows `rank:` tags (any build from the tag door onwards, `ab66e0a` or later on
  `eidolon`'s `main`).
- `nix` on `PATH`, or a `cargo` — neither is needed for *this* plugin: it is
  Rune and Markdown, with no Rust service and no build step.

## Install

```bash
eidolon plugins install noah427/eidolon-plugins coordinator
eidolon plugins trust coordinator
```

`install` copies the directory and writes its row in `plugins.toml`; `trust`
vouches the three verbs. Installing is not trusting and trusting is not
installing — the listing says which of the two happened.

## Verify

```bash
eidolon plugins --dir ~/.config/eidolon/plugins
```

The listing must show `coordinator 0.2.0`, `payloads  tools`, the three
verbs `coordinator_rank`, `coordinator_ranks`, `coordinator_unrank`, and **no**
`service` line and **no** `lifecycle` line. Then, from a session:

```text
coordinator_ranks                       # "no live session carries a rank" on a fresh machine
coordinator_rank  session=<id>  rank=tribunus
coordinator_ranks                       # the row, with its label, carrying tribunus
coordinator_unrank session=<id>
```

The fixture in `tests/coordinator/run.sh` drives all three through a throwaway
config and one mock session (22 checks, no port and no service):

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  EIDOLON_BIN=/path/to/a/built/eidolon bash tests/coordinator/run.sh
```

## The rank blocks

`ranks/<word>.md` is the **text** a session of that rank is told. The custody
doors read it from this plugin's own root at launch, hash the exact bytes into
the launch's immutable record, and append them through the CLI's generic
`--system-append`; a rank with no written block appends nothing and is not an
error. The five files ship as placeholders that say they are unwritten. See
`ranks/README.md`.

## What this plugin does not decide

- **Which rank a session is.** A caller names one and the harness stores it; the
  plugin refuses a word outside its five and adds no sixth.
- **What a rank holds.** `holds` and `persona` belong in the operator's
  `roles.rn`, where a person writes them, not in a payload that would publish a
  capability nobody chose.
- **Who coordinates whom.** There is no adoption here any more. A worker is
  started, retired and transferred through the harness's custody doors; a
  coordinator is a session that happens to be doing that, which is a fact about
  its work and not about this plugin.

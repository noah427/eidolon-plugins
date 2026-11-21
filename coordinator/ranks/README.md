# Rank blocks

One file per rank: `imperator.md`, `legatus.md`, `tribunus.md`, `miles.md`, and
optionally `centurio.md`. The names are the operator's convention and match the five words
`tools/rank.rn` accepts; the coordinator does not add, rename or infer a rank, and
this directory holds no doctrine of its own.
Core's own rank vocabulary (`crates/cli/src/custody_host.rs`, `RANKS`) is the same
five words, and it refuses an unknown one by name rather than treating it as a rank.

A block's **bytes** are what a session of that rank is told. The harness's custody
doors read them — `<plugin root>/ranks/<name>.md`, for the plugin that owns the
launch — when a launch names a rank, and pass the bytes through the CLI's generic
`--system-append`, so the block is appended to the resolved system prompt before the
session exists and the harness's `SessionStart` record captures it. The rank addition
sits *beside* the persona; it is not a persona rewrite, and it enforces nothing.
Nothing in this plugin launches a session any more: the launcher is core's custody
door, and this directory is the text it reads.

The five blocks in this directory are **written**: they carry the rank's duties, its
autonomy, the delegation rule and its reporting line, and they are what the custody
door appends. Nothing in this plugin launches a session any more: the launcher is
core's custody door, and this directory is the text it reads.

A file that holds only whitespace and HTML comments is treated as unwritten and
appends nothing, so an unwritten block is not an error either — it is a session
that was told no rank text.

The SHA-256 of the block's exact bytes is put into the launch's immutable record
when the launch is prepared, and read again immediately before the child is
spawned. Editing a block after preparing a launch makes that operation's retry a
refusal (`the … rank block changed after this launch was prepared`), never a child
told different text. To change a rank's text, start a new operation.

The whole file is appended verbatim, comments included, so do not leave a comment
beside the words.


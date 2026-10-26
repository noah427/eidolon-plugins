# Rank blocks

One file per rank: `imperator.md`, `legatus.md`, `tribunus.md`, `miles.md`, and
optionally `centurio.md`. The names are the operator's convention; the coordinator
does not add, rename or infer a rank, and this directory holds no doctrine of its
own.

A block's **bytes** are what a session of that rank is told: `coordinator_start`
with `rank: "<name>"` passes the file to the CLI as `--system-append <path>`, which
appends it to the resolved system prompt before the session exists, so the
harness's `SessionStart` record captures it. The rank addition sits *beside* the
persona; it is not a persona rewrite, and it enforces nothing.

Each block ships as a placeholder that says it is unwritten. Nothing is appended
while a file holds only whitespace and the placeholder comment — and a rank with no
file at all appends nothing either. **A rank with no written block is not an
error**: it is a session that was told no rank text.

The SHA-256 of the block's exact bytes is put into the launch's immutable record
when the launch is prepared, and read again immediately before the child is
spawned. Editing a block after preparing a launch makes that operation's retry a
refusal (`the … rank block changed after this launch was prepared`), never a child
told different text. To change a rank's text, start a new operation.

Replace the placeholder comment with the rank's words; the whole file is appended
verbatim, comments included, so do not leave the comment beside them.

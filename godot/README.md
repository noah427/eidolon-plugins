# godot

Work on the scene a **Godot editor** is editing: `godot_status`, `godot_tree`,
`godot_node`, `godot_set`, `godot_call`, `godot_scene`, `godot_run`.

The plugin has two halves, and the direction between them is the point. **Godot
starts nothing for eidolon** — an engine is a window somebody is using, not a
daemon — so it is the editor that dials out: an addon enabled in *your* project
connects to the `eidolon-godot` host, and the tools come in the other side of that
host. Nothing here can open a project for you, and a verb that lands on no editor
answers with that instead of failing obscurely.

| tool | does |
|---|---|
| `godot_status` | what the bridge is in front of, with or without an editor: the project, the scene being edited, whether it is playing, which nodes are selected |
| `godot_tree` | the edited scene's nodes — path, name, class, script, owner, and the fingerprint a mutating verb needs |
| `godot_node` | one node in full: class, script, groups, its script's methods, its signals and their connections, and its exported properties |
| `godot_set` | set one property, through the editor's own undo history, against a fingerprint |
| `godot_call` | call one method on one node, against a fingerprint |
| `godot_scene` | open a scene in the editor, or save the one being edited |
| `godot_run` | play or stop the project |

## What it needs from the session

- **Godot 4** (`godot --version`), and the project you want worked on open in the
  editor with the addon enabled. The plugin was written and checked against
  4.7.2; the addon reads `EditorInterface`, `EditorUndoRedoManager` and
  `StreamPeerTCP`, which are the stable editor API, but only 4.7.2 is what
  "verified" means here.
- **`eidolon-godot` on `PATH`** — the host the tools dial and the addon dials into.
  `nix profile install <this repo>#godot`, or run it from a checkout.
- **A token both halves can read**, at `~/.config/eidolon/godot.token`, mode 0600.
  The service makes it on its first run and the addon reads it on every connect, so
  the order you start things in does not matter. The verbs read it too — that is
  what `eidolon plugins grant` records below.
- **Two loopback ports.** 8092 is the tools' (the tool files name it; they are the
  endpoint's own declaration, so a caller cannot move it). 8094 is the editor's;
  the addon reads `EIDOLON_GODOT_BRIDGE_PORT` if you set it in the environment the
  *editor* was started from, and the host reads the same variable. Nothing is
  exposed beyond loopback.

## Prerequisites

```bash
godot --version                  # 4.7.x, and the project you want open in the editor
nix profile install .#godot      # puts eidolon-godot on PATH
command -v eidolon-godot         # answers with a path
```

Then, **in the project**, once:

```bash
cp -r godot/addon/eidolon_bridge <project>/addons/eidolon_bridge
```

and enable it in the editor: **Project > Project Settings > Plugins > Eidolon
Bridge > Enable**. The editor's Output panel says
`[eidolon] bridge addon enabled; dialling the eidolon-godot host`. The addon is a
few hundred lines of GDScript in `addon/eidolon_bridge/`; read it before you enable
it — it is inside your project's process and it reads the token file named above.

Start the host:

```bash
eidolon plugins service approve godot   # once: records your yes against plugin.rn's service block
eidolon plugins service start godot
```

`service start` is what the plugin's `service:` block is for. It asks once, by hash
of that block, and an edit to the command asks again.

## Install

```bash
eidolon plugins install dxcently/eidolon-plugins godot
eidolon plugins trust godot
for v in status tree node set call scene run; do
    eidolon plugins grant "godot_$v" "file:~/.config/eidolon/godot.token"
done
```

Installing does not trust. `trust godot` is what answers the gate for this plugin's
verbs, and it is written in the permission store — the commands above are the whole
install, and a person should be able to see them afterwards. The `grant` loop is not
optional the way it is for a plugin with no secret: every verb resolves the token
file to reach the host, so each one needs the grant recorded against that path. The
verbs and their classes are:

```
godot_status   read_only
godot_tree     read_only
godot_node     read_only
godot_set      mutating
godot_call     mutating
godot_scene    mutating
godot_run      mutating
```

## Verify

Against a **real Godot editor**, headless, in a throwaway home. It builds the
service, writes a project with one scripted node, starts the host, opens the project
in `godot --headless --editor` with the addon enabled, and runs the plugin's own
`selftest` workflow through eidolon:

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/godot/run.sh
```

Every check prints `ok <name>`, then the last line is `selftest: N checks hold`. The
checks cover the status of both halves, the tree and the fingerprints in it, one
node's exports, signals and script methods, a property set and read back through the
editor's undo history, a stale fingerprint refused, a method called and its return,
the save and open guards, and the refusals of `godot_run`. `tests/godot/README.md`
says what the harness deliberately leaves alone and why.

The service's own half — the handshake, the token, one editor at a time, and every
way an editor can leave — is checked without Godot at all, against a fake editor on
the real wire:

```bash
cargo test -p eidolon-godot
```

## Uninstall

```bash
eidolon plugins service stop godot
eidolon plugins uninstall godot
```

and, in the project: **Project > Project Settings > Plugins > Eidolon Bridge >
Disable**, then delete `<project>/addons/eidolon_bridge`. Disabling is what takes the
editor back; `uninstall` removes the tools and the row in the record, and leaves the
vouches and grants in place, the same trade every plugin's uninstall makes. The token
file at `~/.config/eidolon/godot.token` is nobody's to delete but yours.

## Limits, said in the answer of every call

- **One editor at a time, and the second one is refused by name.** A call that could
  land in either of two projects is a call that lands in the wrong one, so the host
  serves one attachment and says so rather than guessing.
- **The check and the act happen in the same frame.** `godot_set` and `godot_call`
  re-resolve the node at the call and compare the fingerprint `godot_node`
  reported — instance id, class and script — and refuse if it is not the same node.
  Unlike a compositor, where the client asking and the process doing are separate
  and a person can move the window in between, both halves of this run on the
  editor's main thread inside one `_process`: nothing in the editor runs between the
  check and the write. What *can* still differ is what the call meant — a property
  whose setter clamps is reported as `matched: false` with both readings, rather than
  hidden.
- **`godot_call` is the one verb that can do almost anything, and it is not
  undoable.** `free`, `queue_free` and `replace_by` are refused, because they leave a
  hole nothing repairs; everything else the node has is callable, including engine
  methods `godot_node` does not list, and a method call is not an editor action, so
  the operator cannot take it back with Ctrl-Z. The answer says so every time.
- **`godot_scene` will not throw away unsaved work.** Opening a scene is refused
  while the editor holds unsaved changes in any scene — a person gets a dialog in the
  same situation, and a tool that silently answered "yes" to it would be answering
  for them. Saving is refused for a scene that has never been written to a file,
  because the editor would then have to ask somebody where to put it.
- **What is edited, not what is running.** Every verb reads the *editor's* scene.
  `godot_run play` starts the project — a second process, a window on your screen, or
  a headless run if the editor is headless — and reports whether the editor agrees it
  is playing, but nothing here reaches into that running game: its tree, its state and
  its input are the project's own business.
- **`godot_run` with `action: "play"` is not covered by `tests/godot/run.sh`.** It
  starts a process, and a test must not be able to open a window on somebody's screen.

# tests/librewolf

Three harnesses for the `librewolf` plugin, with different evidence and different
limits. None of them touches the operator's browser, profile or session.

## `run.sh` — no browser at all

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/librewolf/run.sh
```

The plugin is copied from the repo unchanged into a throwaway config, vouched for
there, and given the token file its verbs resolve — so each call gets as far as
the dial. The test refuses to run at all if something answers on
`127.0.0.1:8091`: it is about a bridge that is not there, and a live one belongs
to a real session.

What it checks: every verb answers with what the operator has to do — the browser
runs this host, eidolon cannot — instead of a bare connection refusal. This is the
shipped bytes the test measures; the workflow it runs (`not-connected.rn`) is
copied into the temp plugin and is not shipped.

## `throwaway.sh` — a disposable headless browser

```bash
t=$(mktemp -d)
HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/librewolf/throwaway.sh      # needs LibreWolf (or EIDOLON_LIBREWOLF_BROWSER)
```

It builds the host with cargo if it is not built, makes its own `$HOME`, its own
profile, its own HTTP server, and its own port, installs the native messaging host
manifest at `$HOME/.librewolf/native-messaging-hosts/eidolon_librewolf.json` (the
path home-manager's `programs.librewolf.nativeMessagingHosts` writes on Linux),
sideloads the extension into the profile, and launches LibreWolf with
`-headless`. Nothing about the operator's browser is read or started.

### What it is evidence of

- The browser spawns the host named by the manifest, and the host makes its token
  file.
- The extension connects with the add-on id the host expects, over the real
  native messaging wire, and the host's loopback endpoint answers.
- `read` returns the enabled page's visible text out of the live DOM, and
  `structure` returns a DOM-derived outline (a heading and a link, in the fixture),
  with refs.
- The inventory lists every tab by title and id with the enabled one marked, and
  nothing else about it.
- A ref names the snapshot it came from, so a ref from an outline taken before a
  navigation is refused rather than acted on.
- A new document on the *same origin* — the fixture pages leave for one another on
  their own — keeps the enablement and moves the document's identity, so a ref
  taken before the navigation is refused rather than acted on.
- A read of a page whose visible text is empty comes back empty, and a script's
  secret does not come with it.
- `detach` disables that tab by id, and later reads say why.
- Stopping the browser takes the host with it: the port stops answering and no
  host process is left behind.

### What it is NOT evidence of

**Cross-origin.** A tab that *lands* on another origin is asserted: its
enablement is revoked, the origin it left is named, and a read against it refused.
A redirect chain that leaves the origin and comes *back* is asserted too — the run
fails if the tab is still enabled — and it was the measurement that found the
problem: before the commit latch, one run ended the enablement and another left it
enabled (a race between the page and the `onUpdated` URL); after it, every run
ended it. The *copy* this harness runs carries `webNavigation` and records every
navigation event (into the host's revocation reason) so a failure prints what the
browser reported rather than a claim about it. Not covered: a server-side redirect
chain with no foreign document load or commit (the boundary the operator set; see
the plugin README), what a foreign *host* does — this fixture's second origin is
another port on the same host, because Firefox match patterns cannot name a port —
and what a person's toolbar click does.

**The toolbar click, the site-permission doorhanger it opens, and the `activeTab` grant.** A headless browser has
no toolbar, so there is no click to make. The harness *substitutes* the click: it
appends a test-only script to a **copy** of the extension, which attaches the
active tab when asked, and it gives that copy a host permission for the local test
origin (`http://127.0.0.1/*`) so the read path can run without a person. The
shipped extension keeps `activeTab` and the click alone, and none of this run says
anything about whether the click works. That acceptance step needs a visible
browser and a person and is deliberately not simulated here.

The same-URL reload case is covered where it can be covered exactly — in
`service/tests/fake_extension.rs`, at the pipe, where the reply can be made to
carry another document's time origin on demand. In a real browser the revocation
event usually arrives first and the read is refused for that reason, which is the
belt; the document check is the braces.

## `js/run.sh` — the extension's browser path, no browser

```bash
bash tests/librewolf/js/run.sh
```

The **shipped** `librewolf/extension/background.js` is loaded into a Node context
with a fake `browser` and a fake DOM, and the code strings it injects (`readCode`,
`structureCode`, `identityCode`) are actually run against that DOM — so this is the
browser half under test, not a Rust stand-in and not a mock of the extraction.

Its checks and regression witnesses cover: the outline's names and what it must never contain (a password
value, a hidden CSRF token, a value or a link inside `display:none`, a
`hidden`-attribute link, a `visibility:hidden` link, a textarea's content, an
ordinary input's value); `read` returning the rendered text and *not* the
`textContent` a person cannot see; a **same-URL reload refused through the document nonce** even when the URL and the
clock both read the same, and the attachment let go; a detach during the click's confirmation cancelling it; a newer click winning
over an older probe; a document that changes under the probe; and the command
generation checks in both directions.

The harness models the platform faithfully where it matters: a **per-document sandbox**
that survives between injections and dies with the document — which is what the
disposable browser was measured to do, and what the document nonce rests on. A
navigation in a test is a new sandbox, so the same-URL case can be scripted exactly
rather than gestured at.

**Every one of those has a witness beside it** — the pre-fix logic, run against the
same fixture and asserted to fail — so each regression test is shown to be capable of
failing. The witnesses are the original `inject` guard, the original `label` that read
`el.value`, the original `readCode` with its `textContent` fallback, the original
`inject` guard without the document nonce, the original click handler without its
sequence checks, and the original listeners that saw only an already-attached tab. If
the shipped code moves so far that a witness no longer applies, that witness fails
loudly instead of passing quietly.

### Measured while writing this

- Firefox starts a native host with two arguments: the manifest path, then the
  add-on id. Checking the *first* argument as if it were the id refuses every real
  launch.
- LibreWolf 154 reads `~/.librewolf/native-messaging-hosts/`, not
  `~/.mozilla/native-messaging-hosts/`. Verified by the host being spawned.
- A match pattern carries no port: `http://127.0.0.1:1234/*` is not a way to grant
  one origin; `http://127.0.0.1/*` is.
- Firefox **rewrites its own argv** shortly after startup, so finding the browser
  by grepping for `--profile` finds only the wrapper script. The harness finds it
  as the parent of a content process that did not exist before the run.
- A **throwaway profile is not an offline profile**: the LibreWolf package's
  `policies.json` force-installs uBlock Origin, DarkReader, SponsorBlock and the
  charter/lyric-hud add-ons into any profile it opens, and the browser fetches the
  AMO ones. The harness neither reads nor keeps any of the operator's data, but a
  proof machine with no network will see those installs fail in the browser log.
- A host that holds a port and outlives its browser is a real failure mode here,
  not a hypothetical: it happened, and the cause was `eprintln!` panicking on the
  browser's broken stderr pipe. See the plugin README's Limits.
- **`globalThis` in an injected script is not the page's window** (`global-distinct`),
  **a value put there survives between two `executeScript` calls in one document**
  (`fresh: true`, then `fresh: false`), and **the page's own script sees nothing** —
  the three measurements the document nonce rests on, now asserted in the run.
- `performance.timeOrigin` in this LibreWolf is not visibly coarsened by
  `privacy.resistFingerprinting` (three documents of one tab reported sub-millisecond
  values), which is a measurement and not a guarantee — see the plugin README.
- The host binary under test is rebuilt from the sources beside it before every run,
  and its digest and mtime are printed. `EIDOLON_LIBREWOLF_BIN` skips the rebuild, says
  so, and is the only way to test a binary that is not the working tree's.

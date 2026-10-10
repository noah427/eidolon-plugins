# librewolf

Work with the tabs a person **enables** in their **own** LibreWolf session, by
clicking a toolbar button on each one: an inventory of every tab, and on an
enabled one read, outline, click, type, scroll, navigate and go back. No browser
of our own, no JS injection, and no browsing on the assistant's behalf.

| tool | does |
|---|---|
| `librewolf_tabs` | every tab of the browser, by title and id, marked enabled or not |
| `librewolf_status` | the enabled tabs, the document each is showing, and why the last one ended |
| `librewolf_read` | one enabled tab's visible text |
| `librewolf_structure` | a DOM-derived outline of one enabled tab, with refs |
| `librewolf_click` | click one element of an enabled tab, by ref |
| `librewolf_type` | set the text of one input of an enabled tab, by ref |
| `librewolf_scroll` | scroll an enabled tab's document or one element of it |
| `librewolf_navigate` | move an enabled tab to another address *on its own origin* |
| `librewolf_back` | go back one entry in an enabled tab's history |
| `librewolf_detach` | disable one enabled tab |

```
eidolon ──tools/*.rn──api_request──▶ eidolon-librewolf ──stdio──▶ extension ──▶ an enabled tab
   (the endpoint, 127.0.0.1:8091,      (LibreWolf starts this:      (in the operator's
    token: ~/.config/eidolon/          it is the native messaging   real profile)
    librewolf.token)                   host the extension names)
```

This is the other half of what [`browser/`](../browser/) does. That plugin owns
one headless Chromium of its own and never sees the operator's cookies; this one
never owns a browser at all — it works with the operator's, and only the tabs
they enable, for as long as this bridge's own record says so.

`activeTab` alone is not enough for that, and the plugin does not pretend
otherwise: Firefox's temporary `activeTab` grant dies with the next document, and
control has to survive a same-origin navigation. So the click asks, **inside the
person's gesture**, for the narrowest optional site permission the clicked tab
needs (`https://example.test/*`), and the enablement is what this bridge keeps.
A Firefox match pattern cannot carry a port (bug 1362809), so that permission is
wider than the boundary the bridge enforces: the bridge's origin is scheme, host
**and port**, and a tab that moves to another port of the same host has left it.

## Prerequisites

- Linux, with **LibreWolf** (a Firefox fork; this was written against LibreWolf
  154). Firefox should work the same way, but nothing here was checked against it.
- eidolon with `api_request` and the **plugin runtime**.
- The host binary, either way:

  ```bash
  nix build .#librewolf          # or: nix profile install github:dxcently/eidolon-plugins#librewolf
  cargo install --path librewolf/service
  ```

- **The extension, loaded in LibreWolf.** This is an operator's act, not a
  plugin's, and there is no tool that can do it. The smallest way, and the one
  these checks were written against:

  > `about:debugging` → **This Firefox** → **Load Temporary Add-on…** →
  > pick `librewolf/extension/manifest.json`

  A temporary add-on is gone when LibreWolf exits; load it again next session. A
  permanent install is the operator's call — the force-install policy pattern is
  already used in `~/.config/nix/detail/librewolf/main.nix` for the charta
  new-tab page, and its comment there warns that a Nix rebuild under a running
  LibreWolf wedges the live instance.

- **The native messaging host manifest**, where LibreWolf looks for it:

  ```bash
  mkdir -p ~/.librewolf/native-messaging-hosts
  eidolon-librewolf --print-host-manifest > ~/.librewolf/native-messaging-hosts/eidolon_librewolf.json
  ```

  On Linux home-manager's `programs.librewolf.nativeMessagingHosts` writes to
  `~/.librewolf/native-messaging-hosts/`; a stock Firefox reads
  `~/.mozilla/native-messaging-hosts/`. The manifest's `path` must be absolute,
  and `--print-host-manifest` prints it that way for the binary that ran.

## Install

1. The host binary — see Prerequisites.

2. The plugin directory:

   ```bash
   eidolon plugins install noah427/eidolon-plugins librewolf --ref librewolf-v0.2
   eidolon plugins trust librewolf
   for v in status tabs read structure click type scroll navigate back detach; do
     eidolon plugins grant librewolf_$v file:~/.config/eidolon/librewolf.token
   done
   ```

   **There is no `service approve` / `service start` step.** This plugin declares
   no `service:` block: the process that has to be running is the one LibreWolf
   spawns, and eidolon cannot start a browser. Everything else an install does —
   the vouch for the verbs, the grant for the token file — is the same as any
   other plugin.

3. Load the extension and install the host manifest — see Prerequisites.

## Verify

```bash
curl -s http://127.0.0.1:8091/health
# {"enabled_tabs":0,"browser_connected":true,"status":"ok"}
eidolon tools | grep -c '"name": "librewolf_'
# 10
```

`browser_connected: false` means no browser has the extension loaded and talking
to this host. `enabled_tabs: 0` means nobody has clicked the toolbar button on a
tab yet — and nothing the tools can say changes that.

The extension's own browser path is driven against the shipped `background.js`
with a fake `browser` and a fake DOM, and every defect it pins has the pre-fix logic
beside it as a witness that must still fail:

```bash
bash tests/librewolf/js/run.sh      # no browser, no network
```

That harness is a *fake browser*, not a mocked function: it has a tab list, a
document per tab, optional site permissions a click asks for, and the events a
real one fires. So the shipped browser half is driven the way a browser drives
it — the toolbar click, a navigation, a tab closing — and the answers are read
off the native port the extension posts to.

The checks that need no browser but do need the real binary — the native framing,
the token file's rules, the launch arguments, and the generation/document binding
including a same-URL reload and a revocation that races a read — are a fake
extension on the other end of a pipe:

```bash
cargo test -p eidolon-librewolf
```

Two harnesses, and what each one is evidence of:

```bash
# no browser: every verb's answer when the bridge is not running
t=$(mktemp -d); HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/librewolf/run.sh

# a disposable headless LibreWolf, its own profile and temp HOME, local pages
t=$(mktemp -d); HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
  bash tests/librewolf/throwaway.sh
```

The second one drives a real browser end to end: the browser spawns this host,
the extension connects, a page's text and outline come back, same-origin navigation keeps enablement
with a new document identity, cross-origin document commits revoke it,
`detach` ends it, and stopping the browser
takes the host with it. **It does not cover the toolbar click.** A headless
browser has no toolbar, so that harness *substitutes* the click with a test-only
trigger and a host permission for the local test origin, in a copy of the
extension. **The real click and the `activeTab` grant it creates are not covered
by any automated check here**; they need a person and a visible browser, and that
acceptance step is separate on purpose.

## The trust boundary, said exactly

There are three different things here and it is easy to let one stand in for
another. Say which is which:

- **The vouch is for the verb, not for the page.** `eidolon plugins trust
  librewolf` records a standing yes for `librewolf_read` on these exact bytes.
  After that, a read runs with no person in the loop — which is the intended
  shape, because the person in the loop is the click, not a prompt. `tabs`,
  `status`, `read` and `structure` are declared `read_only` because they change
  nothing in the browser; `click`, `type`, `scroll`, `navigate`, `back` and
  `detach` are `mutating` because they change the page's state or the bridge's.
  A tool that is *sensitive* is not the same as a tool that *mutates*, and
  labelling a read `mutating` would only buy a prompt, not a check. If an operator
  would rather be asked on every read, the class is a one-line change and the gate
  will ask — but that is a choice about prompts, not about exposure.
- **The click is the consent for the content.** A click enables exactly one tab,
  and nothing in this plugin can enable, renew, or re-use one: there is no verb
  that enables a tab, `librewolf_status` reports what the clicks created, and every
  other verb refuses when a tab is not enabled. `activeTab` is granted to that
  click and to nothing else; the site permission the click asks for is what lets
  control survive a same-origin navigation. Firefox documents `activeTab` as tied
  to the document it was given for, and this plugin does not depend on that: **it
  withdraws the document itself** the moment the browser says the tab is loading,
  ends the enablement on a cross-origin navigation, on the tab's close, on a
  browser restart, on a host that goes away, and on any navigation that arrives
  while a click is still being confirmed. A second click on the same tab is the
  revocation, and it releases the site permission when no other enabled tab needs
  it.
- **The token is the transport's authentication, and nothing more.** The
  endpoint is loopback-only and wants `Bearer <token>` from
  `~/.config/eidolon/librewolf.token` (mode 0600), exactly as `eidolon-browser`
  does. Any process running as the same user that can read that file can call
  `/call`; `allowed_extensions` in the host manifest is the *browser's*
  enforcement of which add-on may spawn this host, not an authentication of
  whoever is on the other end of stdin. The host checks both the add-on id the
  browser passes and the one the extension's `hello` carries, because a
  page-facing component is not something to take on trust — but neither check
  makes the loopback endpoint safe against a same-user process, and nothing here
  pretends otherwise.

What the assistant can therefore see: exactly what is rendered in the tabs a
person clicked on, including anything those tabs display from an authenticated
session — and only while those documents are loaded. It cannot see cookies,
history, or the contents of a tab nobody enabled. It *can* list every tab's title
and id (`librewolf_tabs`), which is metadata the operator asked for and which
grants nothing: a listed tab still refuses every verb until a person clicks its
toolbar button.

## Limits, in the answer of every call

- **The outline carries names, never contents.** A name comes from `aria-label`,
  `alt`, `title`, `placeholder`, an associated `<label>`, the `name` attribute, or
  the element's rendered text — and for a `textarea` or a `select` not even the
  rendered text, because there it *is* the value. Nothing reads `.value`. An element
  that is not rendered is not in the outline and the walk stops at it, so a
  `display:none`, `hidden`, `visibility:hidden` or `opacity:0` subtree keeps its
  contents to itself, and `input[type=hidden]` is skipped by type rather than by
  trusting the stylesheet. The cost of the last rule is over-skipping: content a page
  animates in from `opacity:0` is absent until it is visible.
- **`read` returns rendered text only.** `innerText`, with no `textContent`
  fallback: a page whose visible text is empty reads as empty rather than handing
  back the hidden and `<script>` text a person cannot see. A form control's value is
  not in `innerText` either, a password field's included.
- **The token file is checked, not assumed.** A token is used only if it is a
  regular file, owned by this user, mode 0600, opened without following a symlink;
  otherwise the host refuses and says what to do. A missing one is created with
  `create_new` — never truncating, never clobbering a concurrent start, which is
  re-read instead — and an *empty* file is a refusal, not something to overwrite.
- **The launch arguments are read as positions.** Firefox starts a native host with
  the manifest path and then the add-on id; both are checked as such, so a wrong id
  that happens to lack an `@` is refused rather than waved through.
- **DOM-derived, not an accessibility tree.** WebExtensions have no
  accessibility API, so `librewolf_structure` walks the DOM for headings, links,
  buttons, form controls and landmark elements and reports roles read from tags
  and `role` attributes. It is not the tree `browser_snapshot` prints.
- **No `confine` and no network guarantee.** `browser_open`'s `confine` works
  because the browser is ours and every request leaves through a proxy that
  refuses other origins. There is no such boundary here: the page was loaded by
  the operator's browser, with their cookies, and it goes where it goes. The
  boundary this plugin has is *which document* it will read, not where the
  document may travel.
- **The document dies with the next document; the enablement does not.** A reload,
  a same-origin link, a form post and a same-origin redirect each load a new
  document: the identity moves at once — the moment the browser says the tab is
  loading, before anything knows what is coming, so nothing is read or acted on
  from a document that is on its way out — and every request and every ref issued
  for the old one is refused. The tab stays enabled while the new document is on
  the same origin, and the new document is identified by a probe when it settles.
  A navigation that *leaves* the origin is latched, and the **boundary is the
  operator's**: a **foreign-origin document loading or committing in an enabled tab
  revokes control**. The latch leans on the commit — `webNavigation.onCommitted` for
  the tab's own frame is dispatched when a document becomes the tab's, before that
  document's own script can navigate away, so it cannot be outrun — with
  `tabs.onUpdated`'s URL as the cheap first line. That matters because measured in
  `tests/librewolf/throwaway.sh`, a 0-second meta-refresh chain out to a second
  local origin and straight back was latched in one run and left enabled in another
  — a race between the page and the `onUpdated` URL — and runs after the commit
  latch ended it every time. The URL comes from the event and, when the platform
  withholds it, from `tabs.get` (`tabs` is the permission that makes a tab's URL
  visible for every tab).
  **Server-side redirects are the separate case, and are not treated as a blocker:**
  a 302 chain that goes out and comes back *without a foreign document ever
  loading or committing* is not covered by this latch. The documented Firefox
  `webNavigation` event list carries no `onBeforeRedirect` (MDN, *webNavigation*:
  onBeforeNavigate, onCommitted, onDOMContentLoaded, onCompleted,
  onCreatedNavigationTarget, onErrorOccurred, onHistoryStateUpdated,
  onReferenceFragmentUpdated, onTabReplaced), so no extension is *documented* a hop
  event to listen for; whether some other seam could see one is not something this
  repo has established. Closing it would take blocking `webRequest` over all hosts.
  A commit whose URL the browser does not supply — or supplies as something other
  than an http(s) address — **fails closed**: the URL describes a moment that has
  passed, and a `tabs.get` taken afterwards cannot reconstruct it, so rather than
  reading a tab that has already returned as "same origin" the enablement ends, with
  a reason saying the origin could not be verified. The cost of that direction is
  stated and not hidden: on a platform that dispatched commits with no URL, a
  same-origin navigation would end control as well. Measured here, LibreWolf 156
  supplies the URL for every commit, so same-origin continuity is unaffected.
- **An injected run cannot be recalled, and a refusal is not an undo.** Once a read
  or an action has been handed to the content process, that process runs it; what
  the extension refuses afterwards is the *answer*. For a read or an outline that
  costs nothing — nothing comes back. For `librewolf_click`, `type`, `scroll`,
  `navigate` and `back` it means the page may already have changed: if a tab's
  enablement ends while such a request is in flight, the answer says exactly that
  ("the page may already have been changed by it — an injected run cannot be
  recalled once it is dispatched — so this is an answer refused, not an action
  undone") rather than claiming nothing ran. `tests/librewolf/js/run.sh` pins this
  with the answer held open after the action ran, then the tab disabled: the click lands *and*
  the answer is refused, and both are asserted. Emptying that window would take a
  serialization point between revocation and dispatch — a queue that delays the
  revocation until an in-flight action has answered, or one that refuses to dispatch
  while a revocation is being processed — which is a design decision for an
  operator, not something this file does on its own.
- **A document is named by a nonce, not by its address and not by its clock.** The
  identity is a random value the extension keeps in the content script's **own
  world** — on the sandbox's global, not on the page's window and not in the DOM. It
  is created the first time this extension looks at a document, so a reload at the
  *same URL* gets a fresh one, whether or not any revocation event arrived and
  whatever the clock reads. Both halves compare it for equality, and the host
  refuses an attachment that does not name its document.

  That rests on two things about the platform, and both are **checked on every run**
  of `tests/librewolf/throwaway.sh` rather than assumed: the injected script's
  `globalThis` is not the page's window (`global-distinct`), a value there survives
  between two `executeScript` calls in one document (`fresh: true` then
  `fresh: false`), and the page's own script sees `nothing`. If a future LibreWolf
  changed either, that run fails and says so.

  `performance.timeOrigin` is kept beside the nonce as a second, independent check.
  It is a **discriminator, not an identity document**: nothing promises it is unique,
  and LibreWolf ships `privacy.resistFingerprinting = true`, which is exactly the
  sort of setting that coarsens clocks. Measured here it is not coarsened — three
  documents of one tab reported `…994.752`, `…378.333` and `…428.654` — but that is a
  measurement, not a guarantee, which is why the nonce carries the weight and a
  mismatch on either is a refusal.
- **Refs are a table in the content script's own world, and a ref names one
  snapshot of it.** `librewolf_structure` hands out refs of the form
  `e12-ab12cd34`: an index into an element table kept on the sandbox's own global,
  next to the document nonce, and the random id of the snapshot that filled it.
  Nothing is written into the page — not even a marker attribute — so the page
  cannot see or forge a ref, the table dies with the document that made it, and a
  snapshot replaces the table the previous one made. Both halves are checked where
  the action happens: a ref whose element has left the document, whose snapshot is
  not the current table's, or whose table belongs to another document is refused
  rather than guessed at. The snapshot id is what keeps an index from *aliasing*:
  without it an old `e1` would address whatever element a later snapshot put in
  that slot — the same document, a different element.
- **Acting is synthesized, not a person's input.** `librewolf_click` calls
  `HTMLElement.click()`, `librewolf_type` sets `value` and dispatches `input` and
  `change`, `librewolf_scroll` moves a scroll position, `librewolf_back` calls
  `history.back()`. No trusted input event is delivered, so a widget that insists
  on one (some editors, some drag targets) will not take it — and every answer says
  what was done rather than claiming more.
- **`librewolf_navigate` is same-origin only.** A different origin is refused by
  the page-facing half *and* by the tool: a cross-origin navigation suspends the
  enablement and needs a person to click again, so no call can do it.
- **No JS verb, and no arbitrary script.** The extension runs the code strings it
  ships and nothing else: the four injected payloads are the read, the outline,
  the identity probe and the acting run. There is no `script` argument anywhere in
  the tools, and no verb that would take one. The reason is the execution context,
  not the approval: in this MV2 extension the only place `tabs.executeScript` can
  run is the content-script sandbox, which the browser hands a `browser` object
  with extension APIs on it (`runtime.sendMessage`/`connect`/`onMessage`,
  `extension.getURL`, `i18n`, `storage`, `menus.getTargetElement` — MDN,
  *Content scripts* §WebExtension APIs). Shadowing those names inside the injected
  run is not a guarantee (sloppy-mode `this` and `Function("return this")()`
  escape it), and nothing here has verified an execution context in this platform
  that has no extension API surface at all. So no JS verb ships rather than one
  shipping weakened. If arbitrary script is ever wanted, it needs *both* a
  per-call operator approval seam **and** a context that cannot reach those APIs,
  and that is a decision with a migration in it, not a flag.
- **One bridge per machine.** The host binds 127.0.0.1:8091; a second LibreWolf
  profile that spawns a second host finds the port taken and **exits** with the
  reason, rather than attaching to the instance that holds it.
- **Page text is untrusted data.** Every answer says so. Nothing in it is
  interpreted, stored, or put back into a page.
- **This host outlives nothing.** It exits when the browser closes the native
  messaging port, or when the extension has said nothing for 30 s
  (`EIDOLON_LIBREWOLF_SILENCE_S` to change it). Both are needed: end of stream is
  *not* a reliable "the browser is gone", because a content process can hold the
  write end of that pipe, and reparenting is not one either — measured on this
  machine, a host whose browser was still running had already been reparented to
  init. A host that outlived its browser would hold :8091 and answer the tools
  about a session that is no longer there. The extension sends a heartbeat; the
  host treats silence as death.
- **No diagnostic may kill the shutdown path.** The browser collects this
  process's stderr into its console, and that pipe breaks the moment the browser
  dies. `eprintln!` panics on a broken pipe — which killed the thread trying to
  end the host, and is how a host came to outlive its browser during this work.
  Nothing here uses it; the rule is pinned by a test in
  `service/tests/fake_extension.rs`.

## Uninstall

```bash
eidolon plugins uninstall librewolf
rm -f ~/.librewolf/native-messaging-hosts/eidolon_librewolf.json
rm -f ~/.config/eidolon/librewolf.token
```

Then remove the extension in LibreWolf (`about:debugging` → This Firefox →
Remove) if it was loaded as a temporary add-on, and stop the browser: the host
process exits with the native messaging port. The vouch rows stay in
`~/.config/eidolon/policy.permits.rn`; `eidolon plugins untrust librewolf` drops
them.

## Settings

| variable | default | |
|---|---|---|
| `EIDOLON_LIBREWOLF_PORT` | `8091` | change `base_url` in every `tools/librewolf_*.rn` to match |
| `EIDOLON_LIBREWOLF_TOKEN_FILE` | `~/.config/eidolon/librewolf.token` | change `token_file` in the tools to match |

## How it works

`service/` is the Rust crate, and one process with two faces. LibreWolf starts it
because the extension's host manifest names it, and it speaks the native
messaging wire on stdio; at the same time it binds 127.0.0.1:8091 and serves the
tools. `src/frame.rs` is the length-prefixed wire, `src/state.rs` is the record of
what the browser told us — the enabled tabs, each with the document it is showing —
and the rule that an answer is returned only for the tab *and the document* it was
asked about, and `src/main.rs` is the endpoint and the stdio channel. The
extension is `extension/`: an MV2 persistent background page with
`nativeMessaging`, `activeTab` and `tabs`, and one optional host permission it
asks for per click and releases when the last tab on that host is disabled.

The extension sends a heartbeat every five seconds, and anything at all from it
resets the host's clock (see the last two Limits). `cargo test -p eidolon-librewolf`
runs the unit tests and `tests/fake_extension.rs`, which drives the real binary
over a real pipe and a real socket with a scripted extension on the other end —
including a same-URL reload, a revocation racing a read, a silence that must end
the host, and a broken stderr that must not.

"use strict";

// The librewolf bridge, browser side. The README beside this file is the whole
// picture; this is the half that runs inside the browser.
//
// What this half owns, and what it deliberately does not:
//
//   * The toolbar click is the *only* way a tab is ever enabled. No message,
//     command or API can enable one, and no path here enables one on its own.
//     The click is also the only place a site permission is asked for, so the
//     ask happens inside the person's gesture and names the narrowest origin
//     the clicked tab needs. Clicking again revokes. Each tab is enabled
//     independently and a browser restart enables nothing: this page's state is
//     memory, and a memory dies with its browser.
//   * The inventory (`op: "tabs"`) lists every tab with its title, url and
//     whether it is enabled. It is metadata and nothing else: the op never
//     injects into any tab and never enables one.
//   * Every request names a tab id *and* the document it was issued for. The
//     document's identity is a nonce kept in the content script's own world; it
//     is created the first time this extension looks at a document, so a reload
//     at the same URL gets a fresh one, whatever the clock reads and whether or
//     not a revocation event arrived. Read-only ops compare it; the ops that
//     *act* compare it inside the same injected run that acts, so a stale
//     request can never move a mouse or type into a replacement document.
//   * Control continues across a same-origin navigation and stops at a
//     cross-origin one. A same-origin navigation loads a new document, which is
//     why the identity — and every ref taken from the old document — dies with
//     it, while the tab stays enabled and the site permission stays in force. A
//     cross-origin navigation ends the enablement before any read or action,
//     and a new enablement needs another click.
//   * Refs are an index into an element table kept on the sandbox's own global,
//     next to the document nonce. Nothing is written into the page — not even a
//     marker attribute — so the page cannot see the table, and the table dies
//     with the document that made it.
//   * Page text is data. Nothing read from a page is interpreted, stored,
//     marked up, or put back into a page.
//   * Secrets are what this must not carry out of a page. The outline names
//     elements; it never contains a form control's *value*, and an element that
//     is not rendered is not in it at all. See `structureCode`.

const HOST_NAME = "eidolon_librewolf";
const ADDON_ID = "librewolf-bridge@eidolon.local";
const EXT_VERSION = "0.2.0";

// The wire between this half and the host. The two ship together, and the host
// refuses a hello that does not name this version rather than guessing at a
// different shape of `state`.
const PROTOCOL_V = 2;

const DEFAULT_MAX_CHARS = 8000;
const MAX_CHARS = 200000;
const DEFAULT_MAX_NODES = 400;
const MAX_NODES = 4000;

let port = null;
let heartbeat = null;
let reconnectDelayMs = 2000;

/// tab_id -> {tab_id, url, title, origin, pattern, generation, nonce,
/// time_origin, confirmed, enabled_at_ms}. The whole of what a click bought, and
/// nothing that outlives this background page. `confirmed` is false from the
/// moment the browser says the tab is loading something until a probe has read
/// the new document's identity: an unconfirmed tab has no usable document, and
/// `nonce`/`time_origin` are then only what the last one had.
const enabled = new Map();
/// Monotonic; every enablement, every re-identification of a tab's document and
/// every step into "loading, not identified yet" takes the next value. A request
/// that names an older one is refused, which is how a same-origin navigation
/// invalidates everything issued before it — refs included, because a ref is
/// only meaningful in the document that handed it out.
let generation = 0;

// Bumped by every click and by everything that ends one. A confirmation still
// awaiting an answer belongs to a consent that may since have been replaced or
// withdrawn, and it must not enable anything when it comes back: see the click
// listener. One counter covers all of it, because "this consent is no longer the
// newest one" and "something has since been withdrawn" have the same answer.
let consentSeq = 0;

// The click whose probes are in flight, if any: {tab_id, seq}. The revocation
// listeners have to know about it, because a tab whose *click* is still being
// confirmed is not yet enabled — and a navigation while a probe is out would
// otherwise let both probes run in the replacement document and enable that.
let pendingClick = null;

// Per-tab refresh chains. A navigation produces several `onUpdated` events and
// they must not interleave their probes: each refresh reads the tab and the
// document, decides, and records, in that order, one tab at a time.
const refreshChains = new Map();

/// Withdraw the click being confirmed, if it is on `tabId` (or on any tab when
/// `tabId` is null). Returns whether anything was withdrawn.
function cancelPendingClick(tabId, reason) {
    if (!pendingClick) return false;
    if (tabId !== null && pendingClick.tab_id !== tabId) return false;
    consentSeq += 1;
    log("withdrawing the click on tab", pendingClick.tab_id, "because", reason);
    pendingClick = null;
    return true;
}

/// Let go of the record of a click, when its own confirmation concludes.
function concludePendingClick(seq) {
    if (pendingClick && pendingClick.seq === seq) pendingClick = null;
}

function log(...args) {
    console.debug("librewolf bridge:", ...args);
}

function isHttp(url) {
    return /^https?:\/\//.test(String(url || ""));
}

function originOf(url) {
    try {
        return new URL(url).origin;
    } catch (e) {
        return null;
    }
}

/// The narrowest match pattern the origin of `url` needs. Firefox match patterns
/// cannot carry a port (bug 1362809), so a pattern names a host and covers every
/// port on it; the per-tab boundary is this file's own record, and the site
/// permission is only the floor `executeScript` needs.
function patternOf(url) {
    try {
        const u = new URL(url);
        if (u.protocol !== "http:" && u.protocol !== "https:") return null;
        return u.protocol + "//" + u.hostname + "/*";
    } catch (e) {
        return null;
    }
}

// ---- the native port -------------------------------------------------------

function connect() {
    if (port) return port;
    try {
        port = browser.runtime.connectNative(HOST_NAME);
    } catch (e) {
        log("connectNative failed:", e && e.message);
        port = null;
        scheduleReconnect();
        return null;
    }
    port.onMessage.addListener(onCommand);
    port.onDisconnect.addListener(() => {
        const err = browser.runtime.lastError;
        log("the native host is gone", err && err.message);
        port = null;
        // Without the host there is nowhere for a read to go, so every
        // enablement is over as far as the assistant is concerned, whatever the
        // browser still thinks it granted. The site permissions are released
        // with them: nothing here is worth holding open for a host that is not
        // there.
        const was = list();
        enabled.clear();
        cancelPendingClick(null, "the native host is gone");
        for (const entry of was) releaseOrigin(entry.pattern);
        scheduleReconnect();
    });
    // The host serves one add-on and checks this id as well as the one the
    // browser put on its command line.
    send({ v: PROTOCOL_V, type: "hello", addon: ADDON_ID, ext_version: EXT_VERSION });
    // The host treats our silence as the browser's death — end of stream is not a
    // reliable signal, and this is: this page and the browser go together. Thirty
    // seconds of quiet ends the host, so say something well inside that.
    if (heartbeat === null) heartbeat = setInterval(() => send({ v: PROTOCOL_V, type: "heartbeat" }), 5000);
    return port;
}

function scheduleReconnect() {
    const wait = reconnectDelayMs;
    reconnectDelayMs = Math.min(reconnectDelayMs * 2, 60000);
    setTimeout(connect, wait);
}

function send(msg) {
    if (!port) return false;
    try {
        port.postMessage(msg);
        reconnectDelayMs = 2000;
        return true;
    } catch (e) {
        log("post failed:", e && e.message);
        port = null;
        scheduleReconnect();
        return false;
    }
}

function list() {
    return Array.from(enabled.values());
}

/// Push the whole enablement map, so the host never has to patch a record from a
/// diff. `revoked` says why the last change happened, when one just did.
function announce(revoked) {
    send({ v: PROTOCOL_V, type: "state", tabs: list(), revoked: revoked || null });
}

/// A click that enabled nothing, with the reason. It is not a revocation —
/// nothing was enabled — but the host's "why is nothing there" answer is exactly
/// where this belongs.
function refuse(tabId, reason, url) {
    log("refusing tab", tabId, ":", reason);
    announce({ tab_id: tabId, reason: reason, url: url || null });
}

async function holdsOrigin(pattern) {
    try {
        return await browser.permissions.contains({ origins: [pattern] });
    } catch (e) {
        return false;
    }
}

/// Let go of a site permission once no enabled tab needs it any more. Best
/// effort: a failure here leaves a permission the operator granted, which the
/// next click on that host reuses without a prompt.
function releaseOrigin(pattern) {
    if (!pattern) return;
    for (const entry of enabled.values()) if (entry.pattern === pattern) return;
    try {
        Promise.resolve(browser.permissions.remove({ origins: [pattern] })).catch((e) =>
            log("could not release", pattern, ":", e && e.message),
        );
    } catch (e) {
        log("could not release", pattern, ":", e && e.message);
    }
}

// ---- the click is the enablement -------------------------------------------

browser.browserAction.onClicked.addListener(async (tab) => {
    if (!tab || typeof tab.id !== "number") return;
    const id = tab.id;
    const url = tab.url || "";

    const already = enabled.get(id);
    if (already) {
        // A second click on an enabled tab is a revocation, and it is the whole
        // of the toggle: the person's newest act on that tab is the one that
        // counts.
        revoke(id, "the toolbar button was clicked again on this tab", url);
        return;
    }

    if (!isHttp(url)) {
        refuse(id, "this is not an http(s) page, and the bridge reads http(s) pages only", url);
        return;
    }
    const pattern = patternOf(url);
    const origin = originOf(url);
    if (!pattern || !origin) {
        refuse(id, "this page's address could not be read as an origin", url);
        return;
    }

    // The permission ask comes first, while the click is still the operator's
    // gesture — `permissions.request` refuses a call that no gesture is behind.
    // The record of the click is in place first so that a click on another tab,
    // or on this one again, or the tab closing, all land as a withdrawal.
    const mine = ++consentSeq;
    pendingClick = { tab_id: id, seq: mine };
    let granted = false;
    try {
        granted = await browser.permissions.request({ origins: [pattern] });
    } catch (e) {
        log("the permission ask failed:", e && e.message);
        granted = false;
    }
    if (mine !== consentSeq) {
        concludePendingClick(mine);
        log("a later click or a revocation superseded this one; nothing enabled");
        return;
    }
    if (!granted) {
        concludePendingClick(mine);
        refuse(
            id,
            "the site permission for " + pattern + " was not granted, so nothing was enabled",
            url,
        );
        return;
    }

    const first = await probe(id);
    if (mine !== consentSeq) {
        concludePendingClick(mine);
        log("a later click or a revocation superseded this one; nothing enabled");
        return;
    }
    if (!first) {
        concludePendingClick(mine);
        refuse(id, "this page could not be read (a restricted or privileged page?)", url);
        return;
    }

    // Confirming the document. The probe's answer describes the document it ran
    // in, and an await is a window in which the tab can navigate — a same-URL
    // reload included — so the enablement may not be built on it unverified.
    // Two reads of the same identity with no await between the second and the
    // assignment is what this permission set can honestly offer; every path that
    // reads or acts checks the document again.
    const again = await probe(id);
    if (mine !== consentSeq) {
        concludePendingClick(mine);
        log("a later click or a revocation superseded this one; nothing enabled");
        return;
    }
    if (
        !again ||
        again.href !== first.href ||
        again.time_origin !== first.time_origin ||
        again.nonce !== first.nonce
    ) {
        concludePendingClick(mine);
        refuse(id, "the document changed while the click was being confirmed", again && again.href);
        return;
    }
    if (originOf(again.href) !== origin) {
        // The permission was asked for the origin that was clicked; a document
        // that arrived anywhere else is one nothing was granted for.
        concludePendingClick(mine);
        refuse(
            id,
            "the tab navigated away from " + origin + " while the click was being confirmed",
            again.href,
        );
        return;
    }
    concludePendingClick(mine);

    generation += 1;
    enabled.set(id, {
        tab_id: id,
        url: again.href || url,
        title: tab.title || "",
        origin: origin,
        pattern: pattern,
        generation: generation,
        nonce: again.nonce,
        time_origin: again.time_origin,
        confirmed: true,
        enabled_at_ms: Date.now(),
    });
    log("enabled tab", id, "on", origin, "as generation", generation);
    connect();
    announce(null);
});

/// End one tab's enablement, with the reason. Nothing is announced when there
/// was nothing to end, so the host's record of the last revocation stays a fact
/// about something that happened.
function revoke(tabId, reason, url) {
    const was = enabled.get(tabId) || null;
    const hadClick = cancelPendingClick(tabId, reason);
    if (!was && !hadClick) return false;
    if (was) enabled.delete(tabId);
    const why = reason + (url && url !== (was && was.url) ? ": " + url : "");
    if (was) log("ending tab", tabId, ":", why);
    announce({ tab_id: tabId, reason: why, url: url || (was && was.url) || null });
    if (was) releaseOrigin(was.pattern);
    return true;
}

// ---- revocation, and what a navigation does --------------------------------

// A new document being fetched changes what the enabled tab is showing: a
// reload, a same-origin link, a cross-origin jump, a redirect, a form post. Two
// things happen here, and the order matters.
//
//   * A URL the browser *reports* as leaving the origin ends the enablement on
//     the spot — latched, one way. A redirect that goes out to another origin
//     and comes back is a document no click enabled, and nothing here treats the
//     return as a fresh consent. When a hop is not reported at all the browser
//     gives us nothing to latch on; what is left is the check on every request
//     and on the tab's own URL, and the README says so.
//   * A tab that begins loading has no usable document until a probe has read
//     the new one, so the identity it had is withdrawn *now* rather than when
//     the probe comes back. Every request issued against it is refused from here
//     on, and so is every ref it handed out.
browser.tabs.onUpdated.addListener((tabId, changeInfo) => {
    const entry = enabled.get(tabId);
    if (
        entry &&
        typeof changeInfo.url === "string" &&
        originOf(changeInfo.url) !== entry.origin
    ) {
        cancelPendingClick(tabId, "the tab left " + entry.origin);
        revoke(tabId, "the tab left " + entry.origin + " (the browser reported " + changeInfo.url + ")", changeInfo.url);
        return;
    }
    const starting = changeInfo.status === "loading" || typeof changeInfo.url === "string";
    if (starting) {
        // The tab's *click* may still be being confirmed, with nothing enabled yet.
        cancelPendingClick(tabId, "its document began loading");
        unconfirm(tabId, "it began loading another document");
    }
    // `complete` on its own is not a navigation — a subframe can finish — but it
    // is the moment a settled document can be identified, so it refreshes.
    if (starting || changeInfo.status === "complete") {
        if (enabled.has(tabId)) scheduleRefresh(tabId, "its document changed");
    }
});

// The commit is the one navigation signal a page cannot race: it is dispatched when
// a document becomes the tab's document, before that document's own script can
// navigate away. `tabs.onUpdated` carries the same URL for a committed document,
// but it can arrive *after* a fast page has already left again — measured in
// tests/librewolf/throwaway.sh: a 0-second meta-refresh chain out to another origin
// and back was latched in some runs and not in others, and the only event that
// arrived in every run was this one. A foreign commit ends the enablement on the
// spot, before any read or action can reach the replacement.
if (browser.webNavigation && browser.webNavigation.onCommitted) {
    browser.webNavigation.onCommitted.addListener((details) => {
        if (!details || details.frameId !== 0) return; // a frame is not the tab
        latchCommit(details.tabId, details.url);
    });
}

/// End an enablement whose tab has committed a document outside the origin the
/// click enabled.
///
/// The event's URL is the **only** evidence about which document committed: it
/// describes a moment that has passed, and a `tabs.get` taken afterwards cannot
/// reconstruct it — by the time such a call answers, a page that went out and came
/// straight back is home again, and the commit that crossed would read as
/// same-origin. So a commit whose URL is absent, or is not an http(s) address this
/// bridge works with, **fails closed**: control ends rather than continuing on a
/// document whose origin could not be verified. The cost of that direction is
/// stated rather than hidden: on a platform that dispatched commits with no URL, a
/// same-origin navigation would end control too.
function latchCommit(tabId, url) {
    const entry = enabled.get(tabId);
    if (!entry) return;
    cancelPendingClick(tabId, "the tab committed a document");
    if (typeof url !== "string" || !isHttp(url)) {
        revoke(
            tabId,
            "the tab committed a document whose origin could not be verified (the browser supplied " +
                (typeof url === "string" ? JSON.stringify(url) : "no url for the commit") +
                "), so its enablement ended rather than continuing on a document that cannot be checked",
            typeof url === "string" ? url : null,
        );
        return;
    }
    if (originOf(url) !== entry.origin) {
        revoke(tabId, "the tab left " + entry.origin + " (it committed " + url + ")", url);
    }
}

browser.tabs.onRemoved.addListener((tabId) => {
    cancelPendingClick(tabId, "the tab was closed");
    if (enabled.has(tabId)) revoke(tabId, "the tab was closed", null);
});

/// Withdraw the document a tab was holding, before anything knows what the tab
/// is loading. The enablement (and the site permission) stay; the document does
/// not. Idempotent: a tab already unconfirmed is left as it is.
function unconfirm(tabId, why) {
    const entry = enabled.get(tabId);
    if (!entry || entry.confirmed === false) return false;
    generation += 1;
    enabled.set(tabId, Object.assign({}, entry, { confirmed: false, generation: generation }));
    log("tab", tabId, "has no identified document until a probe reads it:", why);
    announce(null);
    return true;
}

function scheduleRefresh(tabId, why) {
    const prev = refreshChains.get(tabId) || Promise.resolve();
    const next = prev
        .then(() => refreshTab(tabId, why))
        .catch((e) => log("refreshing tab", tabId, "failed:", e && e.message));
    refreshChains.set(tabId, next);
    next.then(() => {
        if (refreshChains.get(tabId) === next) refreshChains.delete(tabId);
    });
}

/// Re-read an enabled tab after a navigation and keep the record true: end the
/// enablement if the tab left the origin, keep it and take the new document's
/// identity if it did not. The exact origin — port included, because the site
/// permission Firefox asks for cannot name a port and is therefore wider than
/// the boundary this keeps.
async function refreshTab(tabId, why) {
    const entry = enabled.get(tabId);
    if (!entry) return;
    let tab = null;
    try {
        tab = await browser.tabs.get(tabId);
    } catch (e) {
        revoke(tabId, "the tab is gone", null);
        return;
    }
    const url = tab && typeof tab.url === "string" ? tab.url : "";
    if (!isHttp(url) || originOf(url) !== entry.origin) {
        revoke(tabId, why + " and left " + entry.origin, url);
        return;
    }
    const id = await probe(tabId);
    if (enabled.get(tabId) !== entry) return; // superseded while the probe was out
    if (!id) {
        revoke(tabId, why + " and the new document could not be identified", url);
        return;
    }
    if (originOf(id.href) !== entry.origin) {
        revoke(tabId, why + " and left " + entry.origin, id.href);
        return;
    }
    if (id.nonce === entry.nonce && id.time_origin === entry.time_origin) {
        // The same document: nothing was loaded after all (a subframe finished,
        // a pushState moved the address). It is usable again, and the generation
        // does not have to move because nothing about the document changed.
        if (entry.confirmed !== false && id.href === entry.url) return;
        enabled.set(tabId, Object.assign({}, entry, { url: id.href, confirmed: true }));
        if (entry.confirmed === false) announce(null);
        return;
    }
    // A new document at the origin the click enabled: control continues, the
    // identity does not. The generation moves, so every request issued against
    // the old document — and every ref taken from it — is refused from here on.
    generation += 1;
    enabled.set(
        tabId,
        Object.assign({}, entry, {
            url: id.href,
            title: (tab && tab.title) || entry.title,
            generation: generation,
            nonce: id.nonce,
            time_origin: id.time_origin,
            confirmed: true,
        }),
    );
    log("tab", tabId, "is still enabled on", entry.origin, "as generation", generation);
    announce(null);
}

// ---- commands from the host ------------------------------------------------

async function onCommand(msg) {
    if (!msg || msg.type !== "command") return;
    const id = msg.id;
    if (msg.v !== PROTOCOL_V) {
        reply(id, null, false, null, "this extension speaks protocol v" + PROTOCOL_V + "; the host asked in v" + msg.v);
        return;
    }
    const op = msg.op;

    // The inventory names no tab and grants none: it is a listing, and it runs
    // with nothing enabled.
    if (op === "tabs") return runTabs(id);

    const tabId = typeof msg.tab_id === "number" ? msg.tab_id : null;
    const entry = tabId === null ? null : enabled.get(tabId);
    if (!entry) {
        reply(
            id,
            null,
            false,
            null,
            "tab " + tabId + " is not enabled — the extension's toolbar button on that tab is what enables it, and nothing here can do it for you",
        );
        return;
    }

    if (op === "detach") return runDetach(id, msg, entry);

    if (entry.confirmed === false) {
        reply(
            id,
            entry,
            false,
            null,
            "tab " + tabId + " is loading a new document: the document this tab had is not available any more, and the new one is identified when it settles. Nothing was read or acted on",
        );
        return;
    }

    if (
        msg.generation !== entry.generation ||
        msg.nonce !== entry.nonce ||
        msg.time_origin !== entry.time_origin
    ) {
        reply(
            id,
            entry,
            false,
            null,
            "this request was issued for a different document than the one this tab has now; nothing from the old document is returned",
        );
        return;
    }

    // The origin is checked against the browser's own record of where the tab
    // is, before anything is injected: a cross-origin navigation must end the
    // enablement *before* an action, not after one.
    let tab = null;
    try {
        tab = await browser.tabs.get(tabId);
    } catch (e) {
        revoke(tabId, "the tab is gone", null);
        reply(id, null, false, null, "tab " + tabId + " is gone");
        return;
    }
    const now = tab && typeof tab.url === "string" ? tab.url : "";
    if (!isHttp(now) || originOf(now) !== entry.origin) {
        revoke(tabId, "the tab left " + entry.origin, now);
        reply(id, null, false, null, "the tab is no longer on " + entry.origin + "; its enablement ended before anything ran");
        return;
    }
    if (enabled.get(tabId) !== entry) {
        // Something ended or re-identified this tab while the tab lookup was out.
        reply(id, null, false, null, "this tab's enablement changed while the request was being prepared; nothing ran");
        return;
    }

    if (op === "read") return runRead(id, entry, msg.args || {});
    if (op === "structure") return runStructure(id, entry, msg.args || {});
    if (op === "click") return runClick(id, entry, msg.args || {});
    if (op === "type") return runType(id, entry, msg.args || {});
    if (op === "scroll") return runScroll(id, entry, msg.args || {});
    if (op === "navigate") return runNavigate(id, entry, msg.args || {});
    if (op === "back") return runBack(id, entry);
    reply(id, entry, false, null, "unknown op " + op);
}

async function runTabs(id) {
    let tabs;
    try {
        tabs = await browser.tabs.query({});
    } catch (e) {
        reply(id, null, false, null, "the browser would not list its tabs: " + (e && e.message ? e.message : e));
        return;
    }
    const listed = (tabs || [])
        .filter((t) => t && typeof t.id === "number")
        .map((t) => ({
            tab_id: t.id,
            title: t.title || "",
            enabled: enabled.has(t.id),
        }));
    reply(id, null, true, {
        tabs: listed,
        note: "an inventory of this browser's tabs — title and id, and whether each one is enabled — and nothing more: listing a tab enables nothing, reads nothing, and grants nothing",
    });
}

function runDetach(id, msg, entry) {
    if (
        msg.generation !== entry.generation ||
        msg.nonce !== entry.nonce ||
        msg.time_origin !== entry.time_origin
    ) {
        // A detach aimed at a document this tab no longer has. Honouring it would
        // end a *newer* document's enablement, and that enablement is a later and
        // separate state: the command is refused, and nothing is ended.
        reply(
            id,
            entry,
            false,
            null,
            "that detach was issued for a document this tab no longer has; nothing was disabled",
        );
        return;
    }
    const was = entry.url;
    const tabId = entry.tab_id;
    revoke(tabId, "the assistant disabled the tab", was);
    reply(id, entry, true, { detached: true, tab_id: tabId, was: was });
}

async function runRead(id, entry, args) {
    const max = clampInt(args.max, DEFAULT_MAX_CHARS, MAX_CHARS);
    const got = await inject(entry, readCode(max), "read");
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (typeof got.page.text !== "string") {
        return reply(id, entry, false, null, "the page yielded nothing readable");
    }
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, {
        url: got.page.href,
        title: got.page.title,
        text: got.page.text,
        truncated: !!got.page.truncated,
    });
}

async function runStructure(id, entry, args) {
    const maxNodes = clampInt(args.max_nodes, DEFAULT_MAX_NODES, MAX_NODES);
    const got = await inject(entry, structureCode(maxNodes), "structure");
    if (!got.ok) return reply(id, entry, false, null, got.error);
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, {
        url: got.page.href,
        title: got.page.title,
        nodes: got.page.nodes || [],
        truncated: !!got.page.truncated,
    });
}

/// A ref as the outline minted it: `e<index>-<snapshot>`. The snapshot id is
/// what keeps an index from aliasing another element after a later snapshot, so a
/// bare `e12` — the shape this deliberately no longer accepts — is refused.
function refOf(ref) {
    const m = /^e(\d+)-([a-z0-9]{4,16})$/.exec(String(ref === undefined || ref === null ? "" : ref));
    if (!m) return null;
    const n = Number(m[1]);
    if (!Number.isFinite(n) || n < 1 || n > MAX_NODES) return null;
    return { index: n - 1, snapshot: m[2] };
}

const REF_HINT = "a ref like e12-ab12cd34 from librewolf_structure for this tab, taken against its current document";

async function runClick(id, entry, args) {
    const ref = refOf(args.ref);
    if (ref === null) {
        return reply(id, entry, false, null, "click needs " + REF_HINT + "; " + JSON.stringify(String(args.ref)) + " is not one");
    }
    const got = await inject(entry, clickCode(entry, ref), "click", true);
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (got.page.error) return reply(id, entry, false, null, String(got.page.error));
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, { tab_id: entry.tab_id, ref: String(args.ref), clicked: true, url: got.page.href, untrusted: true });
}

async function runType(id, entry, args) {
    const ref = refOf(args.ref);
    if (ref === null) {
        return reply(id, entry, false, null, "type needs " + REF_HINT);
    }
    if (typeof args.text !== "string") {
        return reply(id, entry, false, null, "type needs text to type");
    }
    const got = await inject(entry, typeCode(entry, ref, args.text), "type", true);
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (got.page.error) return reply(id, entry, false, null, String(got.page.error));
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, {
        tab_id: entry.tab_id,
        ref: String(args.ref),
        typed: args.text.length,
        url: got.page.href,
        untrusted: true,
    });
}

async function runScroll(id, entry, args) {
    const dy = Number(args.dy);
    const dx = Number(args.dx || 0);
    if (!Number.isFinite(dy) || !Number.isFinite(dx)) {
        return reply(id, entry, false, null, "scroll needs a dy (and optionally a dx) that are numbers");
    }
    let ref = null;
    if (args.ref !== undefined && args.ref !== null && args.ref !== "") {
        ref = refOf(args.ref);
        if (ref === null) {
            return reply(id, entry, false, null, "scroll's ref must be " + REF_HINT + ", or left out to scroll the document");
        }
    }
    const got = await inject(entry, scrollCode(entry, ref, dy, dx), "scroll", true);
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (got.page.error) return reply(id, entry, false, null, String(got.page.error));
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, {
        tab_id: entry.tab_id,
        scroll: { dy: dy, dx: dx, ref: ref === null ? null : String(args.ref) },
        url: got.page.href,
        untrusted: true,
    });
}

/// Navigation is same-origin only, and it is the *background* that performs it:
/// the injected run validates the target against the document the request names
/// and returns the absolute URL, nothing navigates inside that run, and the
/// answer is sent before the tab is moved. That keeps a reply from being lost to
/// the teardown the navigation causes.
async function runNavigate(id, entry, args) {
    if (typeof args.url !== "string" || args.url.length === 0) {
        return reply(id, entry, false, null, "navigate needs a url");
    }
    const got = await inject(entry, navigateCode(entry, args.url), "navigate", true);
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (got.page.error) return reply(id, entry, false, null, String(got.page.error));
    const target = got.page.target;
    reply(id, entry, true, {
        tab_id: entry.tab_id,
        navigating_to: target,
        url: got.page.href,
        note: "the tab is being moved to " + target + "; a same-origin navigation keeps the enablement and gives the document a new identity, so refs and documents taken now are stale next call",
        untrusted: true,
    });
    try {
        await browser.tabs.update(entry.tab_id, { url: target });
    } catch (e) {
        log("the navigation could not be started:", e && e.message);
    }
}

async function runBack(id, entry) {
    const got = await inject(entry, backCode(entry), "back", true);
    if (!got.ok) return reply(id, entry, false, null, got.error);
    if (got.page.error) return reply(id, entry, false, null, String(got.page.error));
    noteLiveUrl(entry, got.page.href);
    reply(id, entry, true, {
        tab_id: entry.tab_id,
        went_back: true,
        url: got.page.href,
        note: "the tab is going back one entry; if that is a different document the enablement continues only while it is the same origin",
        untrusted: true,
    });
}

/// The URL a page reports may have moved with `pushState` without a new
/// document. The record follows it — the identity is what says which document
/// this is, not the address.
function noteLiveUrl(entry, href) {
    if (typeof href === "string" && href !== entry.url && originOf(href) === entry.origin) {
        entry.url = href;
    }
}

// Inject, then decide whether anything from it may be used. Two things must hold:
// the document it ran in must be the one the request named — the same *nonce*,
// not merely the same URL, because a reload at the same URL is a new document —
// and the enablement must still be the one the request named. The acting ops
// check the same nonce *inside* the injected run, before they act.
async function inject(entry, code, what, acting) {
    let value;
    try {
        const out = await browser.tabs.executeScript(entry.tab_id, {
            code: code,
            runAt: "document_idle",
        });
        value = out && out[0];
    } catch (e) {
        // Restricted or privileged pages, a tab that closed, a permission that
        // was withdrawn in the browser's own UI. Fail closed and say so.
        const why =
            "the page could not be reached for this " +
            what +
            " (restricted or privileged page, a withdrawn permission, or the tab is gone): " +
            (e && e.message ? e.message : e);
        if (!(await holdsOrigin(entry.pattern))) {
            revoke(entry.tab_id, "the site permission for " + entry.pattern + " is not held any more", entry.url);
        }
        return { ok: false, error: why };
    }
    if (!value || typeof value !== "object") {
        return { ok: false, error: "the page returned nothing; nothing from it is returned" };
    }
    if (value.stale === true) {
        return {
            ok: false,
            error: "the document changed: this request was issued for another document than the one the tab has now, and nothing ran",
        };
    }
    if (
        typeof value.nonce !== "string" ||
        typeof value.time_origin !== "number" ||
        value.nonce !== entry.nonce ||
        value.time_origin !== entry.time_origin ||
        originOf(value.href) !== entry.origin
    ) {
        return {
            ok: false,
            error: "the document changed while this " + what + " was in flight; nothing from it is returned",
        };
    }
    if (enabled.get(entry.tab_id) !== entry) {
        return {
            ok: false,
            error: acting
                ? "this tab's enablement changed while the " +
                  what +
                  " was in flight; the page may already have been changed by it — an injected run cannot be recalled once it is dispatched — so this is an answer refused, not an action undone"
                : "this tab's enablement changed while the " + what + " was in flight; nothing from it is returned",
        };
    }
    // Named `page` and not `value`, so that a grep for a DOM value read in this
    // file finds nothing: the outline and the read never take one out of a
    // document.
    return { ok: true, page: value };
}

function reply(id, expected, ok, result, error) {
    const msg = {
        v: PROTOCOL_V,
        type: "result",
        id: id,
        ok: !!ok,
        tab_id: expected ? expected.tab_id : null,
        generation: expected ? expected.generation : null,
        time_origin: expected ? expected.time_origin : null,
        nonce: expected ? expected.nonce : null,
    };
    if (ok) msg.result = result;
    else msg.error = error || "the extension refused";
    send(msg);
}

// ---- reading a document, without touching it -------------------------------

function clampInt(value, fallback, cap) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 1) return fallback;
    return Math.min(Math.floor(n), cap);
}

// The document's identity as it is now. Used at click time (twice, so that an
// await cannot slip a navigation between the check and the record), after every
// navigation, and by every request, so that "the same URL" can never stand in
// for "the same document".
//
// The nonce is the identity. It lives on the *sandbox's own global*, not on the page's
// window and not in the DOM, and it is created the first time this extension looks at
// a document. Measured in a disposable LibreWolf, that global is distinct from the page
// window (`globalThis === window` is false), the value survives between two
// `executeScript` calls in one document, and the page's own script cannot see it. So a
// reload at the same URL gets a fresh sandbox and therefore a fresh nonce, whether or
// not any revocation event arrived, and whether or not the clock happens to read the
// same. `performance.timeOrigin` is kept beside it as a second, independent signal:
// LibreWolf ships privacy.resistFingerprinting, and a timestamp is not an identity.
const IDENTITY = `(function () {
    var g = globalThis;
    var nonce = g.__eidolon_doc_nonce;
    if (typeof nonce !== "string") {
        nonce = "n" + Math.random().toString(36).slice(2, 12) + Date.now().toString(36);
        g.__eidolon_doc_nonce = nonce;
    }
    return { href: location.href, time_origin: performance.timeOrigin, nonce: nonce };
})()`;

function identityCode() {
    return `(() => { try { return ${IDENTITY}; } catch (e) { return null; } })()`;
}

// The table a snapshot leaves behind, in the same sandbox as the nonce: one
// element per ref, and the document nonce the table was made for. Nothing is
// written into the page, so the page cannot see or forge a ref; the table dies
// with the document, so a ref from before a navigation is not in the new one.
const REFS = `(function () {
    var g = globalThis;
    var t = g.__eidolon_refs;
    if (!t || typeof t !== "object") { t = { nonce: "", snapshot: "", els: [] }; g.__eidolon_refs = t; }
    if (!Array.isArray(t.els)) t.els = [];
    if (typeof t.snapshot !== "string") t.snapshot = "";
    return t;
})()`;

/// Run `body` in the page only when the page's own world still holds the nonce
/// the request named. The check is in the same injected run as the action, with
/// no await between them, so a stale request cannot act on a replacement
/// document: it returns `stale` instead and the caller refuses.
function actionCode(entry, body) {
    return `(() => {
        const identity = ${IDENTITY};
        if (identity.nonce !== ${JSON.stringify(entry.nonce)}) {
            return Object.assign({}, identity, { stale: true });
        }
        try {
${body}
        } catch (e) {
            return Object.assign({}, identity, { error: String(e) });
        }
    })()`;
}

/// The ref's element, or an explanation of why it cannot be used — the same rule
/// in every acting op.
///
/// A ref is an index *and the snapshot it came from*, and both have to still
/// hold: the table the index counts into is rebuilt by every snapshot, so an
/// index on its own would alias whatever a later snapshot put in that slot — the
/// same document with a different element in it. The snapshot id is random and
/// the page cannot see it (the table is in the sandbox's own world), so a ref
/// cannot be forged and an old one cannot be aimed at a new element.
function refLookup(index, ref, snapshot) {
    return `        const table = ${REFS};
        if (table.nonce !== identity.nonce) {
            return Object.assign({}, identity, { error: "there is no outline for this document: take one with librewolf_structure first, because a new document has no refs" });
        }
        if (table.snapshot !== ${JSON.stringify(snapshot)}) {
            return Object.assign({}, identity, { error: "ref ${ref} is from an outline that is not this document's current one; take a fresh outline with librewolf_structure" });
        }
        const el = table.els[${index}];
        if (!el) {
            return Object.assign({}, identity, { error: "no element ${ref} in the outline this document has; take a fresh snapshot" });
        }
        if (el.isConnected === false) {
            return Object.assign({}, identity, { error: "element ${ref} is no longer in the document; take a fresh snapshot" });
        }
`;
}

async function probe(tabId) {
    try {
        const out = await browser.tabs.executeScript(tabId, {
            code: identityCode(),
            runAt: "document_idle",
        });
        const got = out && out[0];
        if (!got || typeof got.time_origin !== "number" || typeof got.nonce !== "string") return null;
        return got;
    } catch (e) {
        log("the document could not be identified:", e && e.message);
        return null;
    }
}

// `read` returns what the page renders, which is what a person sitting there
// would read: `innerText` leaves out hidden text and does not include a form
// control's value, a password field's included. Values never appear here.
function readCode(max) {
    return `(() => {
        const identity = ${IDENTITY};
        try {
            // Only rendered text. innerText leaves out hidden text, script text and a
            // form control's value. There is deliberately no textContent fallback: it
            // would hand back exactly the hidden and script text a person cannot see,
            // which is how a page's secrets leave with a read that looks empty. A
            // document with no rendered text yields nothing, and says so.
            const source = document.body || document.documentElement;
            const text = source && typeof source.innerText === "string" ? source.innerText : "";
            return Object.assign({}, identity, {
                title: document.title || "",
                text: text.slice(0, ${max + 1}),
                truncated: text.length > ${max},
            });
        } catch (e) {
            return Object.assign({}, identity, { error: String(e) });
        }
    })()`;
}

// The outline names elements; it must not carry their contents out of the page.
// Two rules, and both are load-bearing:
//
//   * A *label* comes from attributes or rendered text, never from a value. The
//     earlier version read `el.value`, which exported a password field's value and
//     a hidden CSRF token as node names; `innerText` is a label for most elements
//     but is the *value* for a `textarea` or a `select`, so it is not read there.
//   * An element that is not rendered is not in the outline, and the walk stops at
//     it — which is how a `display:none` ancestor keeps its whole subtree,
//     including any secret sitting in it, out of the answer. `input[type=hidden]`
//     is skipped by type rather than by trusting the UA stylesheet.
//
// The refs it hands out are indices into a table it rebuilds here, so this
// snapshot invalidates the refs of the previous one.
function structureCode(maxNodes) {
    return `(() => {
        const identity = ${IDENTITY};
        const LIMIT = ${maxNodes};
        const VISIT_LIMIT = 20000;
        const nodes = [];
        const table = ${REFS};
        table.nonce = identity.nonce;
        table.snapshot = "s" + Math.random().toString(36).slice(2, 10);
        table.els.length = 0;
        let visited = 0;
        const collapse = (s) => String(s === undefined || s === null ? "" : s)
            .replace(/\\s+/g, " ").trim().slice(0, 120);

        const rendered = (el, style) => {
            if (el.hidden) return false;
            if (!style) return false;
            if (style.display === "none") return false;
            if (style.visibility === "hidden" || style.visibility === "collapse") return false;
            if (style.opacity === "0" || style.opacity === 0) return false;
            return true;
        };

        const label = (el) => {
            const tag = (el.localName || "").toLowerCase();
            let out = "";
            if (el.getAttribute) {
                out = el.getAttribute("aria-label")
                    || el.getAttribute("alt")
                    || el.getAttribute("title")
                    || el.getAttribute("placeholder")
                    || "";
            }
            // What a person sees beside the control beats the developer's name attribute.
            if (!out && el.labels && el.labels.length) out = el.labels[0].innerText || "";
            if (!out && el.getAttribute) out = el.getAttribute("name") || "";
            // Not for a textarea or a select: their innerText is what a person
            // typed or chose, which is a value however it is spelled.
            if (!out && tag !== "textarea" && tag !== "select" && el.innerText) out = el.innerText;
            return collapse(out);
        };

        const roleOf = (el) => {
            if (!el || !el.tagName) return null;
            const tag = el.tagName.toLowerCase();
            if (el.getAttribute) {
                const role = el.getAttribute("role");
                if (role) return collapse(role);
            }
            if (tag === "a") return el.hasAttribute("href") ? "link" : null;
            if (tag === "button") return "button";
            if (tag === "input") return "input:" + (el.getAttribute("type") || "text");
            if (tag === "select") return "select";
            if (tag === "textarea") return "textarea";
            if (tag === "form") return "form";
            if (tag === "nav") return "navigation";
            if (tag === "main") return "main";
            if (tag === "header") return "banner";
            if (tag === "footer") return "contentinfo";
            if (tag === "img") return "img";
            if (/^h[1-6]$/.test(tag)) return tag;
            return null;
        };

        const walk = (el, depth) => {
            if (nodes.length >= LIMIT || visited >= VISIT_LIMIT) return;
            if (!el || !el.tagName) return;
            visited += 1;
            const tag = (el.localName || el.tagName).toLowerCase();
            if (tag === "input" && (el.getAttribute("type") || "text").toLowerCase() === "hidden") return;
            let style = null;
            try {
                style = window.getComputedStyle(el);
            } catch (e) {
                return;
            }
            if (!rendered(el, style)) return;
            const role = roleOf(el);
            if (role) {
                table.els.push(el);
                nodes.push({
                    ref: "e" + table.els.length + "-" + table.snapshot,
                    role: role,
                    name: label(el),
                    depth: depth,
                });
            }
            const kids = el.children || [];
            for (let i = 0; i < kids.length; i++) {
                if (nodes.length >= LIMIT || visited >= VISIT_LIMIT) return;
                walk(kids[i], depth + 1);
            }
        };

        try {
            if (document.body) walk(document.body, 0);
            return Object.assign({}, identity, {
                title: document.title || "",
                nodes: nodes,
                truncated: nodes.length >= LIMIT || visited >= VISIT_LIMIT,
            });
        } catch (e) {
            return Object.assign({}, identity, { error: String(e) });
        }
    })()`;
}

// A click that the page's own event loop handles: `HTMLElement.click()` is a
// synthesized, *untrusted* activation, not a person's mouse. It is enough for a
// button, a link and a checkbox; a widget that insists on trusted pointer events
// will not take it, and nothing here pretends otherwise.
function clickCode(entry, ref) {
    const label = "e" + (ref.index + 1) + "-" + ref.snapshot;
    return actionCode(
        entry,
        `${refLookup(ref.index, label, ref.snapshot)}        if (typeof el.scrollIntoView === "function") {
            el.scrollIntoView({ block: "center", inline: "nearest" });
        }
        if (typeof el.click !== "function") {
            return Object.assign({}, identity, { error: "element ${label} is not something this can click" });
        }
        el.click();
        return Object.assign({}, identity, { clicked: "${label}" });`,
    );
}

// Typing is a value set and the events a page listens for, not a person's
// keystrokes: `el.value = text` plus `input` and `change`. Nothing here can
// deliver a trusted key event, so a page that requires one (some editors) will
// not take this, and the answer says what was done rather than claiming more.
function typeCode(entry, ref, text) {
    const label = "e" + (ref.index + 1) + "-" + ref.snapshot;
    const literal = JSON.stringify(text);
    return actionCode(
        entry,
        `${refLookup(ref.index, label, ref.snapshot)}        const tag = (el.localName || "").toLowerCase();
        if (tag !== "input" && tag !== "textarea") {
            return Object.assign({}, identity, { error: "element ${label} is a " + tag + ", and type only sets the value of an input or a textarea" });
        }
        if (el.disabled) {
            return Object.assign({}, identity, { error: "element ${label} is disabled" });
        }
        if (typeof el.focus === "function") el.focus();
        el.value = ${literal};
        if (typeof el.dispatchEvent === "function") {
            el.dispatchEvent(new Event("input", { bubbles: true }));
            el.dispatchEvent(new Event("change", { bubbles: true }));
        }
        return Object.assign({}, identity, { typed: ${literal}.length });`,
    );
}

function scrollCode(entry, ref, dy, dx) {
    const body =
        ref === null
            ? `        window.scrollBy({ top: ${dy}, left: ${dx}, behavior: "instant" });
        return Object.assign({}, identity, { scrolled: true });`
            : `${refLookup(ref.index, "e" + (ref.index + 1) + "-" + ref.snapshot, ref.snapshot)}        if (typeof el.scrollBy !== "function") {
            return Object.assign({}, identity, { error: "element e${ref.index + 1}-${ref.snapshot} cannot be scrolled" });
        }
        el.scrollBy({ top: ${dy}, left: ${dx}, behavior: "instant" });
        return Object.assign({}, identity, { scrolled: true });`;
    return actionCode(entry, body);
}

/// The target is resolved and checked *here*, against the document this request
/// names, and nothing navigates in this run: the background moves the tab after
/// the answer is sent.
function navigateCode(entry, url) {
    return actionCode(
        entry,
        `        const target = new URL(${JSON.stringify(url)}, location.href);
        if (target.protocol !== "http:" && target.protocol !== "https:") {
            return Object.assign({}, identity, { error: "only http(s) addresses can be navigated to" });
        }
        if (target.origin !== new URL(location.href).origin) {
            return Object.assign({}, identity, { error: "that is a different origin (" + target.origin + ") from the one this tab is enabled on (" + new URL(location.href).origin + "); a cross-origin navigation needs a person to enable the tab again, so this cannot do it" });
        }
        return Object.assign({}, identity, { target: target.href });`,
    );
}

function backCode(entry) {
    return actionCode(
        entry,
        `        history.back();
        return Object.assign({}, identity, { went_back: true });`,
    );
}

// ---- start -----------------------------------------------------------------

log("librewolf bridge extension", EXT_VERSION, "starting");
connect();

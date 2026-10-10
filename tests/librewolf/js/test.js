"use strict";

// The extension's browser path, driven for real: the *shipped* background.js is
// loaded into a context with a fake `browser` and a fake DOM, and the code strings
// it injects are actually run against that DOM. So these are tests of the browser
// half, not of a Rust stand-in.
//
//   node tests/librewolf/js/test.js        (or: bash tests/librewolf/js/run.sh)
//
// The fake browser is a *browser*, not a mock of one function: it has a tab list,
// a document per tab, optional site permissions that a click asks for, and the
// events a real one fires. A test therefore drives the shipped code the way the
// browser does — the toolbar click, a navigation, a tab closing — and the answers
// are read off the native port the extension posts to.
//
// Every finding this file pins has a *witness* beside it: the pre-fix logic, kept
// here verbatim, run against the same fixture, asserted to fail. A regression test
// that cannot fail proves nothing, so the bug is reproduced in the same run that
// shows the fix closing it. If the witness stops applying (because the shipped code
// moved), the witness test says so instead of quietly passing.

const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const EXT = path.join(__dirname, "..", "..", "..", "librewolf", "extension", "background.js");
const SOURCE = fs.readFileSync(EXT, "utf8");
const ADDON = "librewolf-bridge@eidolon.local";
const PROTOCOL_V = 2;

let failures = 0;
const ok = (name) => console.log("ok   " + name);
const fail = (name, detail) => {
    failures += 1;
    console.log("FAIL " + name + (detail === undefined ? "" : ": " + detail));
};
const check = (name, cond, detail) => (cond ? ok(name) : fail(name, detail));
const has = (haystack, needle) => String(haystack).indexOf(needle) !== -1;

// ---- a small fake DOM ------------------------------------------------------

function node(tag, attrs = {}, opts = {}) {
    const style = Object.assign({ display: "block", visibility: "visible", opacity: "1" }, opts.style || {});
    const events = [];
    const el = {
        tagName: tag.toUpperCase(),
        localName: tag,
        children: opts.children || [],
        hidden: !!opts.hidden,
        disabled: !!opts.disabled,
        innerText: opts.text === undefined ? "" : opts.text,
        labels: opts.labels,
        isConnected: true,
        // Deliberately present on every element: the shipped code must never read it.
        value: opts.value,
        _attrs: attrs,
        _style: style,
        _events: events,
        clicks: 0,
        scrolled: [],
        focused: false,
        hasAttribute(n) {
            return Object.prototype.hasOwnProperty.call(attrs, n);
        },
        getAttribute(n) {
            return Object.prototype.hasOwnProperty.call(attrs, n) ? String(attrs[n]) : null;
        },
        click() {
            el.clicks += 1;
        },
        focus() {
            el.focused = true;
        },
        scrollIntoView() {
            el.scrolled.push("into-view");
        },
        scrollBy(o) {
            el.scrolled.push(o);
        },
        dispatchEvent(e) {
            events.push(e);
            return true;
        },
    };
    return el;
}

function pageState(overrides = {}) {
    return Object.assign(
        {
            href: "https://example.test/page-one",
            timeOrigin: 1000,
            title: "Bridge test page",
            body: node("body"),
            // The content script's own world for this document. It survives between
            // injections and it dies with the document — which is exactly what the
            // measured browser behaviour is, and what the document nonce and the ref
            // table both rest on.
            sandbox: {},
        },
        overrides,
    );
}

// A new document at the address given: a fresh sandbox, whatever the clock says.
function navigate(page, { href = page.href, timeOrigin = page.timeOrigin, body = page.body } = {}) {
    page.href = href;
    page.timeOrigin = timeOrigin;
    page.body = body;
    page.sandbox = {};
    return page;
}

/// What an injected run did to the document, kept on the document: a scroll and a
/// history step are the page's, not the harness's.
function pageScrolls(page) {
    if (!Array.isArray(page.scrolls)) page.scrolls = [];
    return page.scrolls;
}

function pageWentBack(page) {
    if (!Array.isArray(page.wentBack)) page.wentBack = [];
    return page.wentBack;
}

class FakeEvent {
    constructor(type, opts) {
        this.type = type;
        Object.assign(this, opts || {});
    }
}

function runInPage(code, page) {
    const globals = Object.assign({}, page.sandbox);
    globals.document = { title: page.title, body: page.body, documentElement: page.body };
    globals.location = { href: page.href };
    globals.performance = { timeOrigin: page.timeOrigin };
    // Measured in the disposable browser: `globalThis` in an injected script is *not*
    // the page's window.
    globals.window = { getComputedStyle: (el) => el._style, scrollBy: (o) => pageScrolls(page).push(o) };
    globals.URL = URL;
    globals.Event = FakeEvent;
    globals.history = { back: () => pageWentBack(page).push(true) };
    const out = vm.runInNewContext(code, globals);
    page.sandbox = globals;
    return out;
}

// ---- the extension, loaded as it ships (or as patched for a witness) -------

function load({ pages = null, extraTabs = [], script = null, patches = [], patchAll = [], refusePermission = false } = {}) {
    let source = SOURCE;
    for (const [before, after] of patchAll) {
        const count = source.split(before).length - 1;
        if (count === 0) {
            throw new Error("a witness patch (all) found nothing to replace; the shipped code moved:\n" + before);
        }
        source = source.split(before).join(after);
    }
    for (const [before, after] of patches) {
        const count = source.split(before).length - 1;
        if (count !== 1) {
            throw new Error(
                "a witness patch did not apply exactly once (" + count + " matches); the shipped code moved:\n" + before,
            );
        }
        source = source.replace(before, after);
    }

    const sent = [];
    const injected = [];
    const permissionCalls = { request: [], remove: [] };
    const tabUpdates = [];
    const listeners = { click: [], updated: [], removed: [], committed: [] };

    const env = {
        // tab id -> the document that tab is showing
        pages: pages || new Map([[7, pageState()]]),
        // tabs with no document here at all: a background tab, an about: page
        extraTabs,
        grants: new Set(),
        refusePermission,
        permissionGate: null,
        script: script || null,
    };
    env.pageOf = (tabId) => env.pages.get(tabId);
    env.tabOf = (tabId) => {
        const p = env.pages.get(tabId);
        if (p) return { id: tabId, title: p.title || "", url: p.href };
        const e = env.extraTabs.find((t) => t.id === tabId);
        if (e) return { id: e.id, title: e.title || "", url: e.url || "" };
        return null;
    };
    env.tabList = () =>
        [...env.pages.keys()].map(env.tabOf).concat(env.extraTabs.map((t) => ({ id: t.id, title: t.title || "", url: t.url || "" })));

    const port = {
        postMessage: (m) => sent.push(JSON.parse(JSON.stringify(m))),
        onMessage: { addListener: () => {} },
        onDisconnect: { addListener: (fn) => { env.onDisconnect = fn; } },
    };
    const browser = {
        runtime: { connectNative: () => port, lastError: undefined },
        browserAction: { onClicked: { addListener: (fn) => listeners.click.push(fn) } },
        permissions: {
            async request(req) {
                permissionCalls.request.push(JSON.parse(JSON.stringify(req)));
                if (env.permissionGate) await env.permissionGate.promise;
                if (env.refusePermission) return false;
                for (const o of req.origins || []) env.grants.add(o);
                return true;
            },
            async contains(req) {
                return (req.origins || []).every((o) => env.grants.has(o));
            },
            async remove(req) {
                permissionCalls.remove.push(JSON.parse(JSON.stringify(req)));
                for (const o of req.origins || []) env.grants.delete(o);
                return true;
            },
        },
        tabs: {
            async query() {
                return env.tabList();
            },
            async get(tabId) {
                const t = env.tabOf(tabId);
                if (!t) throw new Error("Invalid tab ID: " + tabId);
                return t;
            },
            async update(tabId, props) {
                tabUpdates.push({ tab_id: tabId, props });
                const t = env.tabOf(tabId);
                if (t && props && props.url) {
                    const p = env.pages.get(tabId);
                    if (p) p.href = props.url;
                    else t.url = props.url;
                }
                return t;
            },
            async executeScript(tabId, opts) {
                injected.push({ tab_id: tabId, code: opts.code });
                const page = env.pages.get(tabId);
                if (!page) throw new Error("Missing host permission for the tab");
                if (env.script) return [await env.script(tabId, opts.code, injected.length)];
                return [runInPage(opts.code, page)];
            },
            onUpdated: { addListener: (fn) => listeners.updated.push(fn) },
            onRemoved: { addListener: (fn) => listeners.removed.push(fn) },
        },
        // A browser with webNavigation: the commit event the shipped extension
        // leans on to latch a document that a page would otherwise race away from.
        webNavigation: { onCommitted: { addListener: (fn) => listeners.committed.push(fn) } },
    };
    const ctx = vm.createContext({
        browser,
        // The globals a background page has that this file uses. `URL` is one of
        // them: the extension reads origins with it.
        URL: URL,
        console: { debug() {}, log() {}, error() {} },
        setTimeout: (fn) => 0,
        setInterval: () => 0,
        clearInterval: () => {},
    });
    vm.runInContext(source, ctx, { filename: "background.js" });

    const get = (name) => vm.runInContext(name, ctx);
    const call = (name, ...args) => vm.runInContext(name, ctx)(...args);
    const handle = {
        sent,
        injected,
        listeners,
        env,
        permissionCalls,
        tabUpdates,
        get,
        call,
        run: (code) => vm.runInContext(code, ctx),
        /// The tab id 7 document, for the many tests that only use one tab.
        page: () => env.pages.get(7),
        /// tab id -> the record the extension holds for it, or undefined.
        entry: (tabId) => get("enabled").get(tabId),
        entries: () => [...get("enabled").values()],
        generation: () => get("generation"),
        consentSeq: () => get("consentSeq"),
        /// The whole enabled map as the host would read it off the port.
        pushed: () => handle.states().filter((m) => m.tabs).pop(),
        states: () => sent.filter((m) => m.type === "state"),
        results: () => sent.filter((m) => m.type === "result"),
        replyFor: (id) => sent.filter((m) => m.type === "result" && m.id === id).pop(),
        command: (msg) => call("onCommand", msg),
        /// A command exactly as the host builds one, from the record the extension
        /// holds for that tab — so a test never has to remember the nonce by hand.
        commandFor(tabId, op, args = {}, id = 1) {
            const e = handle.entry(tabId);
            return handle.command({
                v: PROTOCOL_V,
                type: "command",
                id,
                op,
                tab_id: tabId,
                generation: e ? e.generation : 0,
                time_origin: e ? e.time_origin : 0,
                nonce: e ? e.nonce : "",
                args,
            });
        },
        click: (tabId) => {
            const t = handle.env.tabOf(tabId);
            return handle.listeners.click[0](t);
        },
        updated: (tabId, changeInfo) => handle.listeners.updated[0](tabId, changeInfo || {}),
        committed: (tabId, url, frameId = 0) =>
            handle.listeners.committed.forEach((fn) => fn({ tabId, url, frameId })),
        removed: (tabId) => handle.listeners.removed[0](tabId),
        /// The identity of one tab's document as it is now, read the way the
        /// extension reads it — nonce included.
        identity(tabId) {
            const identityOf = vm.runInContext("identityCode", ctx);
            return runInPage(identityOf(), env.pages.get(tabId));
        },
        /// Enable a tab the way a person's click does, and wait for it to land.
        async enable(tabId) {
            await handle.click(tabId);
            await settle();
            return handle.entry(tabId);
        },
    };
    return handle;
}

function deferred() {
    let resolve;
    const promise = new Promise((r) => (resolve = r));
    return { promise, resolve };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// Let every promise the extension has in flight settle: the click handler's
/// awaits, and the per-tab refresh chains a navigation starts.
async function settle() {
    for (let i = 0; i < 8; i += 1) await Promise.resolve();
    await sleep(5);
}

// ---- 1. the outline: labels, never values; nothing unrendered --------------

function secretsPage() {
    return pageState({
        body: node("body", {}, {
            children: [
                node("h1", {}, { text: "Bridge test page" }),
                node("a", { href: "/page-two" }, { text: "to page two" }),
                // Bare: no aria-label, no title. The pre-fix label() fell through to
                // el.value for exactly this shape, which is the common one.
                node("input", { type: "password", name: "password" }, { value: "hunter2-CORRECT-HORSE" }),
                node("input", { type: "hidden", name: "csrf_token" }, { value: "CSRF-TOKEN-abc123" }),
                node("div", { style: "display:none" }, {
                    style: { display: "none" },
                    children: [
                        node("p", {}, { text: "HIDDEN-BLOCK-TEXT" }),
                        node("a", { href: "/hidden" }, { text: "HIDDEN-LINK-TEXT" }),
                        node("input", { type: "text", name: "note" }, { value: "HIDDEN-INPUT-VALUE" }),
                    ],
                }),
                node("div", {}, { hidden: true, children: [node("p", {}, { text: "HIDDEN-ATTR-TEXT" })] }),
                node("div", {}, { style: { visibility: "hidden" }, children: [node("p", {}, { text: "VIS-HIDDEN-TEXT" })] }),
                node("div", {}, { style: { opacity: "0" }, children: [node("p", {}, { text: "OPACITY-ZERO-TEXT" })] }),
                node("input", { type: "text", name: "email", placeholder: "Your email" }, { value: "noah@example.invalid" }),
                node("input", { type: "checkbox", name: "remember", "aria-label": "" }, { labels: [{ innerText: "Remember me" }] }),
                node("textarea", {}, { text: "TYPED-IN-TEXTAREA" }),
                node("button", {}, { text: "Send" }),
            ],
        }),
    });
}

const SECRETS = [
    "hunter2-CORRECT-HORSE",
    "CSRF-TOKEN-abc123",
    "HIDDEN-BLOCK-TEXT",
    "HIDDEN-LINK-TEXT",
    "HIDDEN-INPUT-VALUE",
    "HIDDEN-ATTR-TEXT",
    "VIS-HIDDEN-TEXT",
    "OPACITY-ZERO-TEXT",
    "noah@example.invalid",
    "TYPED-IN-TEXTAREA",
];

// The pre-fix outline, kept verbatim as the witness: `el.value` as a name, and no
// filter for rendering or for hidden inputs.
const ORIGINAL_STRUCTURE = `(() => {
    const identity = { href: location.href, time_origin: performance.timeOrigin };
    const LIMIT = 400;
    const nodes = [];
    const collapse = (s) => String(s === undefined || s === null ? "" : s)
        .replace(/\\s+/g, " ").trim().slice(0, 120);
    const label = (el) => {
        let out = "";
        if (el.getAttribute) {
            out = el.getAttribute("aria-label") || el.getAttribute("alt") || el.getAttribute("title") || "";
        }
        if (!out && el.value) out = el.value;
        if (!out && el.innerText) out = el.innerText;
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
        if (/^h[1-6]$/.test(tag)) return tag;
        return null;
    };
    const walk = (el, depth) => {
        if (nodes.length >= LIMIT) return;
        const role = roleOf(el);
        if (role) nodes.push({ role: role, name: label(el), depth: depth });
        const kids = el.children || [];
        for (let i = 0; i < kids.length; i++) walk(kids[i], depth + 1);
    };
    if (document.body) walk(document.body, 0);
    return Object.assign({}, identity, { title: document.title || "", nodes: nodes, truncated: false });
})()`;

function oneTab(page) {
    return new Map([[7, page]]);
}

async function structureOf(handle, tabId = 7, id = 1) {
    await handle.enable(tabId);
    await handle.commandFor(tabId, "structure", {}, id);
    return handle.replyFor(id);
}

async function testOutline() {
    const page = secretsPage();
    const reply = await structureOf(load({ pages: oneTab(page) }));
    const json = JSON.stringify(reply);
    check("structure: the reply is ok", reply && reply.ok === true, json);
    const names = (reply.result.nodes || []).map((n) => n.name).join(" | ");
    check("structure: the heading and its text are there", has(names, "Bridge test page"), names);
    check("structure: a link is there", has(names, "to page two"), names);
    check("structure: a button is there", has(names, "Send"), names);
    check("structure: a labelled control is named by its label", has(names, "Remember me"), names);
    check("structure: an ordinary input is named, not valued", has(names, "Your email"), names);
    for (const secret of SECRETS) {
        check("structure: " + secret + " is not in the outline", !has(json, secret), json);
    }
    const roles = (reply.result.nodes || []).map((n) => n.role);
    check("structure: the password field is still described", roles.indexOf("input:password") !== -1, JSON.stringify(roles));
    check("structure: and named by its field name, not its contents", has(names, "password"), names);
    check("structure: no node for a hidden input", roles.indexOf("input:hidden") === -1, JSON.stringify(roles));
    check(
        "structure: every node carries a ref that names its snapshot",
        (reply.result.nodes || []).every((n) => /^e\d+-[a-z0-9]{4,16}$/.test(n.ref)),
        JSON.stringify(reply.result.nodes),
    );

    // The witness: the same fixture against the pre-fix outline leaks exactly what
    // this test says the shipped one does not.
    const witness = runInPage(ORIGINAL_STRUCTURE, page);
    const witnessJson = JSON.stringify(witness);
    check("witness: the pre-fix outline exported the password", has(witnessJson, "hunter2-CORRECT-HORSE"));
    check("witness: the pre-fix outline exported the hidden CSRF token", has(witnessJson, "CSRF-TOKEN-abc123"));
    check("witness: the pre-fix outline exported a hidden link's text", has(witnessJson, "HIDDEN-LINK-TEXT"));
    check("witness: the pre-fix outline exported a hidden input's value", has(witnessJson, "HIDDEN-INPUT-VALUE"));
    check("witness: the pre-fix outline exported a textarea's typed value", has(witnessJson, "TYPED-IN-TEXTAREA"));
}

// ---- 2. read: rendered text only -------------------------------------------

async function testRead() {
    const page = pageState({ body: node("body", {}, { text: "hello people" }) });
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);
    await handle.commandFor(7, "read", {}, 1);
    let reply = handle.replyFor(1);
    check("read: returns the rendered text", reply.ok === true && reply.result.text === "hello people", JSON.stringify(reply));
    // The marker that says the text is untrusted page data is added by the host
    // (state.rs), which is what the tool's answer is built from.
    check("read: the answer names the document it came from", reply.result.url === "https://example.test/page-one" && reply.tab_id === 7, JSON.stringify(reply));

    // A body whose rendered text is empty, with the text a person cannot see sitting
    // in textContent. The old code fell back to it; the shipped code must not.
    const hidden = pageState({ body: node("body", {}, { text: "" }) });
    hidden.body.textContent = "SCRIPT-ONLY-SECRET-zzz";
    const h2 = load({ pages: oneTab(hidden) });
    await h2.enable(7);
    await h2.commandFor(7, "read", {}, 1);
    reply = h2.replyFor(1);
    const json = JSON.stringify(reply);
    check("read: an empty rendered body stays empty", reply.ok === true && reply.result.text === "", json);
    check("read: hidden/script text is not returned", !has(json, "SCRIPT-ONLY-SECRET-zzz"), json);

    const witness = runInPage(
        `(() => { const body = document.body;
           const text = body ? (body.innerText || body.textContent || "") : "";
           return { text: text }; })()`,
        hidden,
    );
    check("witness: the pre-fix fallback returned the hidden/script text", has(String(witness.text), "SCRIPT-ONLY-SECRET-zzz"), String(witness.text));
}

// ---- 3. a click enables exactly one tab, and a second click disables it -----

async function testTheClickIsTheToggle() {
    const page = pageState();
    const handle = load({ pages: oneTab(page) });
    await handle.click(7);
    await settle();
    const e = handle.entry(7);
    check("click: the tab is enabled", !!e, JSON.stringify(handle.entries()));
    check("click: with the document's own nonce", e && typeof e.nonce === "string" && e.nonce.length > 0, JSON.stringify(e));
    check("click: confirmed, with the origin and a site permission", e && e.confirmed === true && e.origin === "https://example.test", JSON.stringify(e));
    check(
        "click: the narrowest site permission was asked for, in the gesture",
        JSON.stringify(handle.permissionCalls.request) === JSON.stringify([{ origins: ["https://example.test/*"] }]),
        JSON.stringify(handle.permissionCalls.request),
    );
    check(
        "click: and the host is told the whole enabled set",
        handle.states().some((m) => Array.isArray(m.tabs) && m.tabs.length === 1 && m.tabs[0].tab_id === 7),
        JSON.stringify(handle.states()),
    );

    // The second click is the revocation, and it releases the site permission.
    await handle.click(7);
    await settle();
    check("click: a second click on the same tab disables it", handle.entry(7) === undefined);
    check(
        "click: and the site permission it asked for is released",
        JSON.stringify(handle.permissionCalls.remove) === JSON.stringify([{ origins: ["https://example.test/*"] }]),
        JSON.stringify(handle.permissionCalls.remove),
    );
    check(
        "click: the host is told, with the reason",
        handle.states().some((m) => m.revoked && has(m.revoked.reason, "clicked again")),
        JSON.stringify(handle.states()),
    );

    // A page that is not http(s) is refused at the click, and nothing is asked for.
    const other = load({ pages: new Map([[9, pageState({ href: "about:config" })]]) });
    const before = other.permissionCalls.request.length;
    await other.click(9);
    await settle();
    check("click: a non-http(s) page never enables", other.entry(9) === undefined);
    check("click: and no site permission is asked for it", other.permissionCalls.request.length === before);
    check(
        "click: the refusal is pushed with its reason",
        other.states().some((m) => m.revoked && has(m.revoked.reason, "not an http(s) page")),
        JSON.stringify(other.states()),
    );

    // A person who declines the site permission gets nothing enabled.
    const declined = load({ pages: oneTab(pageState()), refusePermission: true });
    await declined.click(7);
    await settle();
    check("click: a declined site permission enables nothing", declined.entry(7) === undefined);
    check(
        "click: and the host is told which permission was declined",
        declined.states().some((m) => m.revoked && has(m.revoked.reason, "was not granted")),
        JSON.stringify(declined.states()),
    );
}

// ---- 4. multiple tabs, independently enabled -------------------------------

async function testMultipleTabs() {
    const a = pageState({ href: "https://example.test/one", body: node("body", {}, { text: "PAGE ONE TEXT" }) });
    const b = pageState({ href: "https://other.test/two", timeOrigin: 2000, body: node("body", {}, { text: "PAGE TWO TEXT" }) });
    const handle = load({ pages: new Map([[7, a], [8, b]]) });

    await handle.enable(7);
    await handle.enable(8);
    check("multi-tab: both tabs are enabled", handle.entries().length === 2, JSON.stringify(handle.entries()));
    check(
        "multi-tab: each with its own origin and document",
        handle.entry(7).origin === "https://example.test" && handle.entry(8).origin === "https://other.test"
            && handle.entry(7).nonce !== handle.entry(8).nonce,
        JSON.stringify(handle.entries()),
    );

    const beforeInjections = handle.injected.length;
    await handle.commandFor(8, "read", {}, 2);
    const reply = handle.replyFor(2);
    check("multi-tab: a read for tab 8 returns tab 8's text", reply.ok === true && reply.result.text === "PAGE TWO TEXT", JSON.stringify(reply));
    const used = handle.injected.slice(beforeInjections);
    check(
        "multi-tab: and it was injected into tab 8 alone",
        used.length === 1 && used[0].tab_id === 8,
        JSON.stringify(used.map((i) => i.tab_id)),
    );

    // A command whose tab field names another tab than the document it carries is
    // its own case, and the extension refuses it by name.
    const e7 = handle.entry(7);
    await handle.command({
        v: PROTOCOL_V, type: "command", id: 3, op: "read", tab_id: 7,
        generation: e7.generation, time_origin: e7.time_origin, nonce: handle.entry(8).nonce, args: {},
    });
    const wrong = handle.replyFor(3);
    check("multi-tab: tab 7 with tab 8's document is refused", wrong.ok === false, JSON.stringify(wrong));

    // Disabling one leaves the other alone, permission included.
    const before = handle.injected.length;
    await handle.click(7);
    await settle();
    check("multi-tab: disabling tab 7 leaves tab 8 enabled", handle.entry(7) === undefined && !!handle.entry(8));
    check(
        "multi-tab: and it does not release tab 8's permission",
        handle.permissionCalls.remove.length === 1 && handle.permissionCalls.remove[0].origins[0] === "https://example.test/*",
        JSON.stringify(handle.permissionCalls.remove),
    );
    await handle.commandFor(8, "read", {}, 4);
    check("multi-tab: tab 8 still reads", handle.replyFor(4).ok === true, JSON.stringify(handle.replyFor(4)));
    await handle.commandFor(7, "read", {}, 5);
    const gone = handle.replyFor(5);
    check("multi-tab: tab 7 now refuses", gone.ok === false && has(gone.error, "not enabled"), JSON.stringify(gone));
    check("multi-tab: and nothing was injected for it", handle.injected.length === before + 1, String(handle.injected.length));
}

// ---- 5. the inventory is metadata, and grants nothing ----------------------

async function testInventory() {
    const page = pageState();
    const handle = load({
        pages: oneTab(page),
        extraTabs: [
            { id: 9, title: "A background tab", url: "https://elsewhere.test/x" },
            { id: 10, title: "A page nothing was granted for", url: "https://locked.test/y" },
        ],
    });
    await handle.enable(7);

    await handle.command({ v: PROTOCOL_V, type: "command", id: 11, op: "tabs", args: {} });
    const reply = handle.replyFor(11);
    check("inventory: it answers with every tab", reply.ok === true && reply.result.tabs.length === 3, JSON.stringify(reply));
    const byId = Object.fromEntries(reply.result.tabs.map((t) => [t.tab_id, t]));
    check("inventory: the enabled tab is marked enabled", byId[7].enabled === true, JSON.stringify(byId[7]));
    check("inventory: a background tab is listed and not enabled", byId[9] && byId[9].enabled === false, JSON.stringify(byId[9]));
    check("inventory: including one no permission covers", byId[10] && byId[10].enabled === false, JSON.stringify(byId[10]));
    check("inventory: titles come with the ids", byId[9].title === "A background tab", JSON.stringify(byId[9]));
    check(
        "inventory: it carries no url and no page content",
        reply.result.tabs.every((t) => Object.keys(t).sort().join(",") === "enabled,tab_id,title"),
        JSON.stringify(reply.result.tabs),
    );
    check("inventory: it says what it is not", has(reply.result.note, "nothing more"), JSON.stringify(reply.result.note));

    // Listing does not enable: a command for a listed tab is still refused, and the
    // inventory itself injected nothing.
    const before = handle.injected.length;
    await handle.commandFor(9, "read", {}, 12);
    const read = handle.replyFor(12);
    check("inventory: a listed tab is not enabled by being listed", read.ok === false && has(read.error, "not enabled"), JSON.stringify(read));
    check("inventory: and no injection happened", handle.injected.length === before, String(handle.injected.length));

    // And the inventory works with nothing enabled at all.
    const fresh = load({ pages: oneTab(page), extraTabs: [{ id: 9, title: "Background", url: "https://x.test/" }] });
    await fresh.command({ v: PROTOCOL_V, type: "command", id: 1, op: "tabs", args: {} });
    const cold = fresh.replyFor(1);
    check("inventory: it answers with nothing enabled", cold.ok === true && cold.result.tabs.length === 2, JSON.stringify(cold));
    check("inventory: and enables nothing", fresh.entries().length === 0);
}

// ---- 6. the document is the identity, not the address ----------------------

async function readInto(handle, tabId, id) {
    await handle.commandFor(tabId, "read", {}, id);
    return handle.replyFor(id);
}

async function testSameUrlReload() {
    const page = pageState({ body: node("body", {}, { text: "OLD DOCUMENT TEXT" }) });
    const handle = load({ pages: oneTab(page) });
    const record = await handle.enable(7);
    check(
        "same-url reload: the click enabled the tab, with a document nonce",
        record && typeof record.nonce === "string" && record.nonce.length > 0,
        JSON.stringify(record),
    );

    // The document is replaced at the same address, with no event at all and the
    // clock reading the same: only the content script's own world — and so the
    // nonce — is new. This is the case that neither a URL nor a timestamp can see.
    navigate(page, { body: node("body", {}, { text: "NEW DOCUMENT TEXT" }) });
    const reply = await readInto(handle, 7, 7);
    const json = JSON.stringify(reply);
    check("same-url reload: the read is refused", reply && reply.ok === false, json);
    check("same-url reload: no text from the replacement document is returned", !has(json, "NEW DOCUMENT TEXT"), json);

    // The second line, on its own: with the nonce comparison patched out of the
    // guard, the clock is what refuses a document whose time origin moved. This is
    // not the mechanism — it is the check that stands behind it.
    {
        const page2 = pageState({ body: node("body", {}, { text: "OLD DOCUMENT TEXT" }) });
        const h2 = load({
            pages: oneTab(page2),
            // The nonce comparison goes, and the `||` with it: what is left is the clock.
            patches: [[
                `        value.nonce !== entry.nonce ||
        value.time_origin !== entry.time_origin ||
        originOf(value.href) !== entry.origin
`,
                `        value.time_origin !== entry.time_origin
`,
            ]],
        });
        await h2.enable(7);
        navigate(page2, { body: node("body", {}, { text: "NEW DOCUMENT TEXT" }), timeOrigin: 9999 });
        const reply2 = await readInto(h2, 7, 8);
        check("second line: the clock alone refuses a moved document", reply2 && reply2.ok === false, JSON.stringify(reply2));
        check("second line: and its text does not come back", !has(JSON.stringify(reply2), "NEW DOCUMENT TEXT"), JSON.stringify(reply2));
    }

    // The witness: the guard as it was — the field's presence and the URL, nothing
    // more — hands the replacement document's text back under the old identity.
    {
        const page3 = pageState({ body: node("body", {}, { text: "OLD DOCUMENT TEXT" }) });
        const h3 = load({
            pages: oneTab(page3),
            patches: [[
                `        typeof value.nonce !== "string" ||
        typeof value.time_origin !== "number" ||
        value.nonce !== entry.nonce ||
        value.time_origin !== entry.time_origin ||
        originOf(value.href) !== entry.origin`,
                `        typeof value.nonce !== "string" ||
        typeof value.time_origin !== "number" ||
        value.href !== entry.url`,
            ]],
        });
        await h3.enable(7);
        navigate(page3, { body: node("body", {}, { text: "NEW DOCUMENT TEXT" }) });
        const reply3 = await readInto(h3, 7, 9);
        const witnessJson = JSON.stringify(reply3);
        check("witness: the pre-fix guard accepted the replacement document", reply3 && reply3.ok === true, witnessJson);
        check("witness: the pre-fix reply carried the replacement's text", has(witnessJson, "NEW DOCUMENT TEXT"), witnessJson);
        check("witness: and labelled it with the enabled document's nonce", reply3.nonce === h3.entry(7).nonce, witnessJson);
    }
}

// ---- 7. a same-origin navigation keeps control, and invalidates the document -

async function testSameOriginNavigation() {
    const page = pageState({
        body: node("body", {}, {
            children: [node("button", {}, { text: "press me" }), node("a", { href: "/next" }, { text: "next" })],
        }),
    });
    const handle = load({ pages: oneTab(page) });
    const first = await handle.enable(7);
    await handle.commandFor(7, "structure", {}, 1);
    const outline = handle.replyFor(1);
    check("same-origin: the snapshot has refs", outline.ok && outline.result.nodes.length >= 2, JSON.stringify(outline));

    // The tab starts loading another document. The old document is withdrawn *now*,
    // before anything knows what is coming: a request issued for it refuses without
    // even being injected.
    const beforeInjections = handle.injected.length;
    handle.updated(7, { status: "loading", url: "https://example.test/next" });
    let e = handle.entry(7);
    check("same-origin: the tab is still enabled while it loads", !!e, JSON.stringify(e));
    check("same-origin: but its document is withdrawn at once", e && e.confirmed === false, JSON.stringify(e));
    check("same-origin: and the generation moved, so old requests are stale", e && e.generation > first.generation, JSON.stringify(e));

    await handle.commandFor(7, "read", {}, 2);
    const during = handle.replyFor(2);
    check(
        "same-origin: a request during the load is refused for a loading document",
        during.ok === false && has(during.error, "loading a new document"),
        JSON.stringify(during),
    );
    check("same-origin: and nothing was injected for it", handle.injected.length === beforeInjections, String(handle.injected.length));

    // The witness: without the withdrawal, that same request is answered from the
    // document that is on its way out.
    {
        const p2 = pageState({ body: node("body", {}, { text: "OLD DOCUMENT TEXT" }) });
        const h2 = load({
            pages: oneTab(p2),
            patches: [[
                `        cancelPendingClick(tabId, "its document began loading");
        unconfirm(tabId, "it began loading another document");
`,
                `        cancelPendingClick(tabId, "its document began loading");
`,
            ]],
        });
        await h2.enable(7);
        h2.updated(7, { status: "loading", url: "https://example.test/next" });
        const witness = await readInto(h2, 7, 1);
        check("witness: without the withdrawal the request read the departing document", witness.ok === true, JSON.stringify(witness));
        check("witness: and handed its text back", has(JSON.stringify(witness), "OLD DOCUMENT TEXT"), JSON.stringify(witness));
    }

    // The new document commits. The probe identifies it, the enablement continues,
    // the generation moves again — and every ref from before is refuseable.
    navigate(page, { href: "https://example.test/next", timeOrigin: 4242, body: node("body", {}, { text: "THE NEW DOCUMENT" }) });
    handle.updated(7, { status: "complete" });
    await settle();
    e = handle.entry(7);
    check("same-origin: the tab is still enabled after the navigation", !!e, JSON.stringify(e));
    check("same-origin: with a new generation", e && e.generation > first.generation, JSON.stringify(e));
    check("same-origin: and the new document's own identity", e && e.nonce !== first.nonce && e.confirmed === true, JSON.stringify(e));
    check("same-origin: and the same origin, so no new permission was asked for", e && e.origin === "https://example.test" && handle.permissionCalls.request.length === 1, JSON.stringify(handle.permissionCalls.request));

    const fresh = await readInto(handle, 7, 3);
    check("same-origin: a read against the new document is answered", fresh.ok === true && has(fresh.result.text, "THE NEW DOCUMENT"), JSON.stringify(fresh));

    const ref = outline.result.nodes[0].ref;
    await handle.commandFor(7, "click", { ref: ref }, 4);
    const staleRef = handle.replyFor(4);
    check(
        "same-origin: a ref taken from the old document is refused",
        staleRef.ok === false && (has(staleRef.error, "outline") || has(staleRef.error, "issued for a different document")),
        JSON.stringify(staleRef),
    );
    check(
        "same-origin: and it did not act on the replacement document",
        handle.page().body.children.every((c) => !c.clicks || c.clicks === 0),
        JSON.stringify(handle.page().body.children.map((c) => c.clicks)),
    );

    // A ref that is not in this document's table at all is refused by name.
    await handle.commandFor(7, "structure", {}, 5);
    const second = handle.replyFor(5);
    check("same-origin: a fresh snapshot works on the new document", second.ok === true, JSON.stringify(second));
    await handle.commandFor(7, "click", { ref: "e99" }, 6);
    const missing = handle.replyFor(6);
    check(
        "same-origin: a ref that is not the shape an outline mints is refused",
        missing.ok === false && has(missing.error, "is not one"),
        JSON.stringify(missing),
    );
}

// ---- 8. an acting op acts, and only on the document it named ---------------

async function testActing() {
    const button = node("button", {}, { text: "press me" });
    const text = node("input", { type: "text", name: "field" }, { value: "" });
    const page = pageState({ body: node("body", {}, { children: [button, text] }) });
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);
    await handle.commandFor(7, "structure", {}, 1);
    const nodes = handle.replyFor(1).result.nodes;
    const buttonRef = nodes.find((n) => n.role === "button").ref;
    const inputRef = nodes.find((n) => n.role === "input:text").ref;

    await handle.commandFor(7, "click", { ref: buttonRef }, 2);
    const clicked = handle.replyFor(2);
    check("acting: click says what it clicked", clicked.ok === true && clicked.result.clicked === true, JSON.stringify(clicked));
    check("acting: the element was clicked once", button.clicks === 1, String(button.clicks));
    check("acting: and it was scrolled into view first", button.scrolled.length === 1, JSON.stringify(button.scrolled));
    check("acting: and the answer names the ref it acted on", clicked.result.ref === buttonRef, JSON.stringify(clicked.result));

    await handle.commandFor(7, "type", { ref: inputRef, text: "hello" }, 3);
    const typed = handle.replyFor(3);
    check("acting: type sets the value", typed.ok === true && text.value === "hello", JSON.stringify(typed));
    check(
        "acting: and dispatches the events a page listens for",
        text._events.map((e) => e.type).join(",") === "input,change",
        JSON.stringify(text._events.map((e) => e.type)),
    );
    await handle.commandFor(7, "type", { ref: buttonRef, text: "nope" }, 4);
    const wrongTarget = handle.replyFor(4);
    check("acting: type refuses a button", wrongTarget.ok === false && has(wrongTarget.error, "only sets the value"), JSON.stringify(wrongTarget));

    await handle.commandFor(7, "scroll", { dy: 120 }, 5);
    check("acting: scroll scrolls the document", handle.replyFor(5).ok === true, JSON.stringify(handle.replyFor(5)));
    check("acting: by what was asked", pageScrolls(page).length === 1 && pageScrolls(page)[0].top === 120, JSON.stringify(pageScrolls(page)));
    await handle.commandFor(7, "scroll", { dy: 5, ref: inputRef }, 6);
    check("acting: scroll can name an element", text.scrolled.some((s) => s && s.top === 5), JSON.stringify(text.scrolled));

    await handle.commandFor(7, "back", {}, 7);
    check("acting: back went back", handle.replyFor(7).ok === true && pageWentBack(page).length === 1, JSON.stringify(handle.replyFor(7)));

    // Navigation: same origin only, and the background moves the tab.
    await handle.commandFor(7, "navigate", { url: "/elsewhere" }, 8);
    const nav = handle.replyFor(8);
    check("acting: navigate answers with the resolved target", nav.ok === true && nav.result.navigating_to === "https://example.test/elsewhere", JSON.stringify(nav));
    await settle();
    check(
        "acting: and the background moves the tab there",
        handle.tabUpdates.length === 1 && handle.tabUpdates[0].props.url === "https://example.test/elsewhere",
        JSON.stringify(handle.tabUpdates),
    );

    await handle.commandFor(7, "navigate", { url: "https://other.test/elsewhere" }, 9);
    const cross = handle.replyFor(9);
    check(
        "acting: navigate refuses a different origin",
        cross.ok === false && has(cross.error, "different origin"),
        JSON.stringify(cross),
    );
    check("acting: and nothing was moved", handle.tabUpdates.length === 1, JSON.stringify(handle.tabUpdates));
}

/// The acting ops check the document inside the run that acts. That check is what
/// stands between a queued click and a replacement document, and it is worth its
/// own witness: the ref index resolves in *both* documents — they have the same
/// shape — so the only thing that refuses the stale request is the identity.
async function testActingWitness() {
    const oldRecord = { tab_id: 7, nonce: "a-nonce-from-the-document-that-was-here", url: "https://example.test/one" };

    // Doc A and doc B: same origin, same shape, one button each, and a ref table
    // built in whichever one the request lands in.
    const build = () => {
        const button = node("button", {}, { text: "press me" });
        const page = pageState({ href: "https://example.test/one", body: node("body", {}, { children: [button] }) });
        return { button, page };
    };

    /// The ref an outline in this document just minted, as the acting ops take it.
    const mintedRef = (handle, page) => {
        const out = runInPage(handle.run("structureCode")(400), page);
        const m = /^e(\d+)-([a-z0-9]{4,16})$/.exec(out.nodes[0].ref);
        return { index: Number(m[1]) - 1, snapshot: m[2] };
    };

    // The shipped run: the ref resolves, `isConnected` is true, and the nonce is
    // not this document's — so nothing is clicked.
    {
        const { button, page } = build();
        const handle = load({ pages: oneTab(page) });
        await handle.enable(7);
        navigate(page, { href: "https://example.test/two", timeOrigin: 4242 });
        const ref = mintedRef(handle, page); // doc B's own table, doc A's identity
        const out = runInPage(handle.run("clickCode")(oldRecord, ref), page);
        check("acting witness: a stale request is refused with `stale`", out.stale === true, JSON.stringify(out));
        check("acting witness: and the shipped run did not click", button.clicks === 0, String(button.clicks));
    }

    // The pre-fix run — actionCode with the nonce comparison taken out — clicks the
    // replacement document's element with the old document's ref.
    {
        const { button, page } = build();
        const handle = load({
            pages: oneTab(page),
            patches: [[
                `        const identity = \${IDENTITY};
        if (identity.nonce !== \${JSON.stringify(entry.nonce)}) {
            return Object.assign({}, identity, { stale: true });
        }
`,
                `        const identity = \${IDENTITY};
`,
            ]],
        });
        navigate(page, { href: "https://example.test/two", timeOrigin: 4242 });
        const ref = mintedRef(handle, page); // doc B's table: the ref resolves, the identity is doc A's
        const out = runInPage(handle.run("clickCode")(oldRecord, ref), page);
        check("witness: the pre-fix acting run accepted the stale document", out.stale !== true, JSON.stringify(out));
        check("witness: and clicked the replacement document's element", button.clicks === 1, String(button.clicks));
    }
}

// ---- 8b. a ref names one snapshot, and nothing else -------------------------

/// A ref is an index *and the snapshot it came from*. Without the snapshot id an
/// index aliases whatever the next snapshot put in that slot — the same document,
/// a different element — so both cases are pinned here, fail-before included.
async function testRefIdentity() {
    // (a) the same document, snapshotted twice, with the element at index 0
    //     changing between them.
    {
        const first = node("button", {}, { text: "first" });
        const second = node("button", {}, { text: "second" });
        const page = pageState({ body: node("body", {}, { children: [first, second] }) });
        const handle = load({ pages: oneTab(page) });
        await handle.enable(7);
        await handle.commandFor(7, "structure", {}, 1);
        const refOld = handle.replyFor(1).result.nodes.find((n) => n.role === "button").ref;
        check("refs: the first outline names its snapshot", /^e1-[a-z0-9]{4,16}$/.test(refOld), String(refOld));

        // The first button stops being rendered, so the next snapshot's index 0 is
        // the *other* button. An index-only ref would now point at it.
        first._style.display = "none";
        await handle.commandFor(7, "structure", {}, 2);
        const refNew = handle.replyFor(2).result.nodes.find((n) => n.role === "button").ref;
        check("refs: the second snapshot reuses the index", refNew.startsWith("e1-"), String(refNew));
        check("refs: with a different snapshot id", refNew !== refOld, refNew + " vs " + refOld);

        await handle.commandFor(7, "click", { ref: refOld }, 3);
        const old = handle.replyFor(3);
        check("refs: the older snapshot's ref is refused", old.ok === false && has(old.error, "not this document's current one"), JSON.stringify(old));
        check("refs: and the element that took that index was not clicked", second.clicks === 0, String(second.clicks));

        await handle.commandFor(7, "click", { ref: refNew }, 4);
        check("refs: the current snapshot's ref works", handle.replyFor(4).ok === true, JSON.stringify(handle.replyFor(4)));
        check("refs: and clicked the element it names", second.clicks === 1, String(second.clicks));

        // The witness: with the snapshot comparison taken out of the lookup, the
        // older ref lands on the element that took its index.
        const wFirst = node("button", {}, { text: "first" });
        const wSecond = node("button", {}, { text: "second" });
        const wPage = pageState({ body: node("body", {}, { children: [wFirst, wSecond] }) });
        const w = load({
            pages: oneTab(wPage),
            patches: [[
                `        if (table.snapshot !== \${JSON.stringify(snapshot)}) {
            return Object.assign({}, identity, { error: "ref \${ref} is from an outline that is not this document's current one; take a fresh outline with librewolf_structure" });
        }
`,
                ``,
            ]],
        });
        await w.enable(7);
        await w.commandFor(7, "structure", {}, 1);
        const wOld = w.replyFor(1).result.nodes.find((n) => n.role === "button").ref;
        wFirst._style.display = "none";
        await w.commandFor(7, "structure", {}, 2);
        await w.commandFor(7, "click", { ref: wOld }, 3);
        check("witness: without the snapshot id the old ref was accepted", w.replyFor(3).ok === true, JSON.stringify(w.replyFor(3)));
        check("witness: and it clicked the element that took its index", wSecond.clicks === 1, String(wSecond.clicks));
    }

    // (b) a ref from before a navigation, after the new document has its own
    //     outline: the record was re-announced, so the extension accepts the
    //     command for the current document — and the ref is still refused.
    {
        const page = pageState({ body: node("body", {}, { children: [node("button", {}, { text: "old" })] }) });
        const handle = load({ pages: oneTab(page) });
        await handle.enable(7);
        await handle.commandFor(7, "structure", {}, 1);
        const refBefore = handle.replyFor(1).result.nodes[0].ref;

        const next = node("button", {}, { text: "new" });
        navigate(page, { href: "https://example.test/next", timeOrigin: 5150, body: node("body", {}, { children: [next] }) });
        handle.updated(7, { status: "loading", url: "https://example.test/next" });
        handle.updated(7, { status: "complete" });
        await settle();
        await handle.commandFor(7, "structure", {}, 2);
        const fresh = handle.replyFor(2);
        check("refs: the new document has its own outline", fresh.ok === true, JSON.stringify(fresh));
        const refAfter = fresh.result.nodes[0].ref;
        check("refs: and its own snapshot id", refAfter !== refBefore, refAfter + " vs " + refBefore);

        await handle.commandFor(7, "click", { ref: refBefore }, 3);
        const stale = handle.replyFor(3);
        check(
            "refs: a ref from before the navigation is refused",
            stale.ok === false && (has(stale.error, "not this document's current one") || has(stale.error, "outline")),
            JSON.stringify(stale),
        );
        check("refs: and the new document's element was not clicked", next.clicks === 0, String(next.clicks));
        await handle.commandFor(7, "click", { ref: refAfter }, 4);
        check("refs: the new document's ref works", handle.replyFor(4).ok === true, JSON.stringify(handle.replyFor(4)));
        check("refs: and clicked in the new document", next.clicks === 1, String(next.clicks));
    }
}

// ---- 8c. the commit event is the signal a page cannot race -------------------

/// A page can leave the origin and come back between two `onUpdated` events: the
/// URL for a committed document arrives in an `onUpdated` that a fast page can
/// outrun. The *commit* event cannot be outrun — it is dispatched when the document
/// becomes the tab's — so it is what the latch leans on.
async function testCommitLatch() {
    const page = pageState();
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);
    const before = handle.injected.length;

    handle.committed(7, "https://other.test/out.html");
    await settle();
    check("commit: a foreign document committing ends the enablement", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    check(
        "commit: and the reason names the origin it left",
        handle.states().some((m) => m.revoked && has(m.revoked.reason, "left https://example.test")),
        JSON.stringify(handle.states()),
    );
    check("commit: nothing was injected into it", handle.injected.length === before, String(handle.injected.length));

    // A commit on the origin the click enabled is not a crossing: the enablement
    // stays, and the document is re-identified by the probe as before.
    const same = load({ pages: oneTab(pageState()) });
    await same.enable(7);
    same.committed(7, "https://example.test/next");
    await settle();
    check("commit: a same-origin commit does not end it", !!same.entry(7), JSON.stringify(same.entries()));

    // A commit in a *frame* is not the tab's document.
    const frame = load({ pages: oneTab(pageState()) });
    await frame.enable(7);
    frame.committed(7, "https://other.test/frame.html", 7);
    await settle();
    check("commit: a subframe's commit is ignored", !!frame.entry(7), JSON.stringify(frame.entries()));

    // A commit the browser gave no URL for. The URL describes a moment that has
    // passed, so a `tabs.get` taken now cannot reconstruct it: the tab may already
    // be back on the enabled origin, and that must not be read as "same origin".
    // Fail closed.
    const back = load({ pages: oneTab(pageState({ href: "https://example.test/home" })) });
    await back.enable(7);
    back.committed(7, undefined);
    await settle();
    check("commit: a commit with no url ends the enablement (fail closed)", back.entry(7) === undefined, JSON.stringify(back.entries()));
    check(
        "commit: and the reason says the origin could not be verified",
        back.states().some((m) => m.revoked && has(m.revoked.reason, "could not be verified")),
        JSON.stringify(back.states()),
    );

    // The witness: the pre-fix fallback asked `tabs.get` instead, and the tab is
    // back on the enabled origin by then — so the crossing was read as same-origin
    // and control survived a commit nobody could check.
    const wf = load({
        pages: oneTab(pageState({ href: "https://example.test/home" })),
        patches: [[
            `    if (typeof url !== "string" || !isHttp(url)) {
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
    }`,
            `    if (typeof url === "string" && originOf(url) !== entry.origin) {
        revoke(tabId, "the tab left " + entry.origin + " (it committed " + url + ")", url);
        return;
    }
    browser.tabs
        .get(tabId)
        .then((tab) => {
            const now = tab && typeof tab.url === "string" ? tab.url : "";
            const still = enabled.get(tabId);
            if (!still || still !== entry) return;
            if (!isHttp(now) || originOf(now) !== still.origin) {
                revoke(tabId, "the tab left " + still.origin + " (it committed " + now + ")", now);
            }
        })
        .catch(() => {});`,
        ]],
    });
    await wf.enable(7);
    wf.committed(7, undefined);
    await settle();
    check(
        "witness: the pre-fix fallback read the returned tab as same-origin and kept control",
        !!wf.entry(7),
        JSON.stringify(wf.entries()),
    );

    // The witness: with the commit listener gone, the same foreign commit is not
    // seen at all — which is the racy path the measurement caught leaving a tab
    // enabled after a chain out and back.
    const w = load({
        pages: oneTab(pageState()),
        patches: [["if (browser.webNavigation && browser.webNavigation.onCommitted) {", "if (false) {"]],
    });
    await w.enable(7);
    w.committed(7, "https://other.test/out.html");
    await settle();
    check(
        "witness: without the commit listener the foreign commit is not seen",
        !!w.entry(7),
        JSON.stringify(w.entries()),
    );
}

// ---- 8d. a revocation that lands while an action is already dispatched --------

/// An injected run cannot be recalled once it is dispatched: the content process
/// has it, and what the extension refuses afterwards is the *answer*. This test
/// pins the difference between "refused" and "did not happen", because a caller
/// told the latter about the former would be told something false.
async function testActionInFlightRevocation() {
    const button = node("button", {}, { text: "press me" });
    const page = pageState({ body: node("body", {}, { children: [button] }) });
    // A second enabled tab on the same origin, so the site permission is still held
    // when the first tab is disabled — which is exactly the case where the injected
    // run can still reach the page.
    const other = pageState({ href: "https://example.test/other" });
    const handle = load({ pages: new Map([[7, page], [8, other]]) });
    await handle.enable(7);
    await handle.enable(8);
    await handle.commandFor(7, "structure", {}, 1);
    const ref = handle.replyFor(1).result.nodes.find((n) => n.role === "button").ref;

    const gate = deferred();
    // The run is executed first — as it is in a content process — and its answer is
    // held, which is the window a revocation can land in.
    let held = false;
    handle.env.script = async (tabId, code) => {
        const out = runInPage(code, handle.env.pages.get(tabId));
        if (!held && code.indexOf("el.click()") !== -1) {
            held = true;
            await gate.promise;
        }
        return out;
    };

    const acting = handle.commandFor(7, "click", { ref: ref }, 2);
    await sleep(0);
    check("in-flight: the run reached the page before the revocation", button.clicks === 1, String(button.clicks));

    handle.call("revoke", 7, "the assistant disabled the tab", null);
    await settle();
    check(
        "in-flight: disabling one tab does not release a permission another held",
        handle.permissionCalls.remove.length === 0,
        JSON.stringify(handle.permissionCalls.remove),
    );
    check("in-flight: the tab is no longer enabled", handle.entry(7) === undefined, JSON.stringify(handle.entries()));

    gate.resolve();
    await acting;
    const answer = handle.replyFor(2);
    check("in-flight: the answer is refused", answer && answer.ok === false, JSON.stringify(answer));
    check(
        "in-flight: and it does not claim the page was left untouched",
        has(answer.error, "may already have been changed") && !has(answer.error, "nothing from it is returned"),
        JSON.stringify(answer.error),
    );
    check("in-flight: the click on the page stands", button.clicks === 1, String(button.clicks));
}

// ---- 9. a cross-origin navigation ends it, and a return does not restore it -

async function testCrossOrigin() {
    const page = pageState();
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);
    const before = handle.injected.length;

    // The browser reports the tab leaving the origin. That is latched before
    // anything can be read or acted on.
    handle.updated(7, { status: "loading", url: "https://other.test/away" });
    check("cross-origin: the tab is no longer enabled", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    check(
        "cross-origin: the reason names the origin it left",
        handle.states().some((m) => m.revoked && has(m.revoked.reason, "left https://example.test")),
        JSON.stringify(handle.states()),
    );

    // The redirect comes back to the origin it started on. A crossing was observed,
    // and the return is not a fresh consent: the tab stays disabled.
    page.href = "https://example.test/back";
    handle.updated(7, { status: "complete" });
    await settle();
    check("cross-origin: a redirect back does not re-enable anything", handle.entry(7) === undefined, JSON.stringify(handle.entries()));

    await handle.commandFor(7, "read", {}, 1);
    const reply = handle.replyFor(1);
    check("cross-origin: a read after it is refused", reply.ok === false && has(reply.error, "not enabled"), JSON.stringify(reply));
    check("cross-origin: and nothing was injected", handle.injected.length === before, String(handle.injected.length));

    // The witness: without the latch, the return is taken for a same-origin
    // navigation and the tab is re-identified rather than left disabled.
    {
        const p2 = pageState();
        const h2 = load({
            pages: oneTab(p2),
            patches: [[
                `    if (
        entry &&
        typeof changeInfo.url === "string" &&
        originOf(changeInfo.url) !== entry.origin
    ) {`,
                `    if (false) {`,
            ]],
        });
        await h2.enable(7);
        h2.updated(7, { status: "loading", url: "https://other.test/away" });
        p2.href = "https://example.test/back";
        h2.updated(7, { status: "complete" });
        await settle();
        const witness = h2.entry(7);
        check("witness: without the latch the tab is enabled again after the return", !!witness, JSON.stringify(h2.entries()));
        check("witness: and its document is the returned one", witness && witness.url === "https://example.test/back", JSON.stringify(witness));
    }

    // A port is part of the origin here, even though it cannot be part of the site
    // permission Firefox asks for (match patterns cannot carry one): a tab that
    // moves to another port of the same host has left the origin the click enabled.
    {
        const p = pageState({ href: "http://127.0.0.1:8123/page" });
        const h = load({ pages: oneTab(p) });
        await h.enable(7);
        check(
            "cross-origin: the site permission for a ported host cannot name the port",
            h.permissionCalls.request[0].origins[0] === "http://127.0.0.1/*",
            JSON.stringify(h.permissionCalls.request),
        );
        h.updated(7, { status: "loading", url: "http://127.0.0.1:8124/other" });
        check("cross-origin: another port of the same host ends the enablement", h.entry(7) === undefined, JSON.stringify(h.entries()));
        check(
            "cross-origin: and the reason names the ported origin it left",
            h.states().some((m) => m.revoked && has(m.revoked.reason, "left http://127.0.0.1:8123")),
            JSON.stringify(h.states()),
        );
    }

    // A tab whose URL the browser reports as another origin *at request time* is
    // refused the same way, with the enablement ended before the injection.
    {
        const p3 = pageState();
        const h3 = load({ pages: oneTab(p3) });
        await h3.enable(7);
        const injectedBefore = h3.injected.length;
        p3.href = "https://other.test/away";
        await h3.commandFor(7, "read", {}, 1);
        const v = h3.replyFor(1);
        check("cross-origin: a request to a tab that has moved is refused", v.ok === false, JSON.stringify(v));
        check("cross-origin: before anything is injected", h3.injected.length === injectedBefore, String(h3.injected.length));
        check("cross-origin: and the enablement is over", h3.entry(7) === undefined);
    }
}

// ---- 10. the click, and what may cancel it ---------------------------------

async function testClickRaces() {
    // (a) a revocation lands while the click's probe is still out.
    {
        const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
        const gate = deferred();
        let first = true;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                if (first) {
                    first = false;
                    return gate.promise;
                }
                return runInPage(code, page);
            },
        });
        const pending = handle.click(7);
        await sleep(0);
        handle.call("revoke", 7, "the assistant disabled the tab", null);
        gate.resolve(handle.identity(7));
        await pending;
        check("click: a revocation during the probe cancels the attach", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
        check(
            "click: no enablement is announced",
            !handle.states().some((m) => m.tabs && m.tabs.length),
            JSON.stringify(handle.states()),
        );
    }

    // (b) a second click lands while the first is still waiting; the newer one wins.
    {
        const a = pageState({ href: "https://example.test/one" });
        const b = pageState({ href: "https://example.test/two", timeOrigin: 3000 });
        const gate = deferred();
        let first = true;
        const handle = load({
            pages: new Map([[7, a], [8, b]]),
            script: async (tabId, code) => {
                if (first) {
                    first = false;
                    return gate.promise;
                }
                return runInPage(code, handle.env.pages.get(tabId));
            },
        });
        const older = handle.click(7);
        await sleep(0);
        await handle.click(8);
        const afterNewer = handle.entries().map((e) => e.tab_id);
        gate.resolve(handle.identity(7));
        await older;
        const after = handle.entries().map((e) => e.tab_id);
        check("click: the newer click enables its own tab", afterNewer.indexOf(8) !== -1, JSON.stringify(afterNewer));
        check("click: the older probe does not add its tab", after.length === afterNewer.length, JSON.stringify(after));
    }

    // (c) the document changes between the probe and the confirmation.
    {
        const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
        let calls = 0;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                calls += 1;
                if (calls === 2) page.timeOrigin = 4242;
                return runInPage(code, page);
            },
        });
        await handle.click(7);
        await settle();
        check("click: a document that changes under the probe is not enabled", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
        check(
            "click: and the reason says the confirmation failed",
            handle.states().some((m) => m.revoked && has(m.revoked.reason, "confirmed")),
            JSON.stringify(handle.states()),
        );
    }

    // (d) a tab that navigates away from the clicked origin while the click is
    // being confirmed is refused: the permission was asked for the other one.
    {
        const page = pageState();
        let calls = 0;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                calls += 1;
                if (calls === 2) {
                    page.href = "https://other.test/away";
                    page.timeOrigin = 7777;
                }
                return runInPage(code, page);
            },
        });
        await handle.click(7);
        await settle();
        check("click: a tab that left the clicked origin is not enabled", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    }

    // Witness: without the confirmation and the sequence, the awaited probe enables
    // the tab even though the consent was withdrawn while it waited.
    {
        const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
        const gate = deferred();
        let first = true;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                if (first) {
                    first = false;
                    return gate.promise;
                }
                return runInPage(code, page);
            },
            patchAll: [["if (mine !== consentSeq) {", "if (false) {"]],
        });
        const pending = handle.click(7);
        await sleep(0);
        handle.call("revoke", 7, "the assistant disabled the tab", null);
        gate.resolve(handle.identity(7));
        await pending;
        check(
            "witness: the pre-fix click resurrected an enablement after a revocation",
            handle.entry(7) !== undefined,
            JSON.stringify(handle.entries()),
        );
    }
}

// ---- 11. a click whose confirmation is still out ---------------------------

async function testPendingClickLifecycle() {
    // (a) the clicked tab navigates while the first probe is still out. Nothing is
    // enabled at that point, so an `enabled`-only listener would miss it entirely
    // and the replacement document would be enabled.
    {
        const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
        const gate = deferred();
        let calls = 0;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                calls += 1;
                if (calls === 1) return gate.promise;
                return runInPage(code, page);
            },
        });
        const pending = handle.click(7);
        await sleep(0);
        page.href = "https://example.test/replacement";
        page.timeOrigin = 7777;
        handle.updated(7, { status: "loading" });
        gate.resolve(handle.identity(7));
        await pending;
        await settle();
        check("pending click: a navigation on the clicked tab withdraws it", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
        check(
            "pending click: and no enablement is announced",
            !handle.states().some((m) => m.tabs && m.tabs.length),
            JSON.stringify(handle.states()),
        );
    }

    // (b) the clicked tab closes while the first probe is out.
    {
        const page = pageState();
        const gate = deferred();
        let first = true;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                if (first) {
                    first = false;
                    return gate.promise;
                }
                return runInPage(code, page);
            },
        });
        const pending = handle.click(7);
        await sleep(0);
        handle.removed(7);
        gate.resolve(handle.identity(7));
        await pending;
        check("pending click: a closed tab withdraws it", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    }

    // (c) the native host goes away while the first probe is out: there is nowhere
    // for an enablement to be served, so it must not be made.
    {
        const page = pageState();
        const gate = deferred();
        let first = true;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                if (first) {
                    first = false;
                    return gate.promise;
                }
                return runInPage(code, page);
            },
        });
        const pending = handle.click(7);
        await sleep(0);
        handle.env.onDisconnect();
        gate.resolve(handle.identity(7));
        await pending;
        check("pending click: a lost host withdraws it", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    }

    // (d) two clicks in flight, and the *older* one's tab closes: the newer one is
    // unaffected and still completes.
    {
        const a = pageState();
        const b = pageState({ href: "https://example.test/two", timeOrigin: 4000 });
        const gA = deferred();
        const gB = deferred();
        const gates = [gA, gB];
        const handle = load({
            pages: new Map([[7, a], [8, b]]),
            script: async (tabId, code) =>
                gates.length ? gates.shift().promise : runInPage(code, handle.env.pages.get(tabId)),
        });
        const first = handle.click(7);
        await sleep(0);
        const second = handle.click(8);
        await sleep(0);
        handle.removed(7); // the older click's tab closes
        gB.resolve(handle.identity(8));
        await second;
        gA.resolve(handle.identity(7));
        await first;
        const ids = handle.entries().map((e) => e.tab_id);
        check("pending click: closing the older click's tab leaves the newer one alone", ids.join(",") === "8", JSON.stringify(ids));
    }

    // Witness: with the listeners as they were — returning unless a tab is already
    // enabled — the navigation of a tab whose click is pending is not seen at all,
    // and the replacement document is enabled.
    {
        const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
        const gate = deferred();
        let calls = 0;
        const handle = load({
            pages: oneTab(page),
            script: async (tabId, code) => {
                calls += 1;
                if (calls === 1) return gate.promise;
                return runInPage(code, page);
            },
            patches: [[
                `browser.tabs.onUpdated.addListener((tabId, changeInfo) => {
    const entry = enabled.get(tabId);`,
                `browser.tabs.onUpdated.addListener((tabId, changeInfo) => {
    const entry = enabled.get(tabId);
    if (!entry) return;`,
            ]],
        });
        const pending = handle.click(7);
        await sleep(0);
        page.href = "https://example.test/replacement";
        page.timeOrigin = 7777;
        handle.updated(7, { status: "loading" });
        gate.resolve(handle.identity(7));
        await pending;
        await settle();
        const witness = handle.entry(7);
        check(
            "witness: the pre-fix listeners let the replacement document attach",
            witness !== undefined && witness.url === "https://example.test/replacement",
            JSON.stringify(witness),
        );
    }
}

// ---- 12. commands: the tab and the document are checked, both ways ---------

async function testCommandGenerations() {
    const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);
    const record = handle.entry(7);
    // The click itself probes the document twice; what these checks are about is
    // that a refused request adds no injection of its own.
    const before = handle.injected.length;

    await handle.command({
        v: PROTOCOL_V, type: "command", id: 1, op: "read", tab_id: 7,
        generation: record.generation - 1, time_origin: record.time_origin, nonce: record.nonce, args: {},
    });
    let reply = handle.replyFor(1);
    check("commands: a read issued for an older generation is refused", reply.ok === false, JSON.stringify(reply));
    check("commands: and nothing was injected", handle.injected.length === before, String(handle.injected.length));

    await handle.command({
        v: PROTOCOL_V, type: "command", id: 2, op: "read", tab_id: 7, args: {},
        generation: record.generation, time_origin: 7.5, nonce: record.nonce,
    });
    reply = handle.replyFor(2);
    check("commands: a read issued for another clock is refused", reply.ok === false, JSON.stringify(reply));

    await handle.command({
        v: PROTOCOL_V, type: "command", id: 6, op: "read", tab_id: 7, args: {},
        generation: record.generation, time_origin: record.time_origin,
        nonce: "a-nonce-from-another-document",
    });
    reply = handle.replyFor(6);
    check("commands: a read issued for another document is refused", reply.ok === false, JSON.stringify(reply));
    check("commands: and nothing was injected for it", handle.injected.length === before, String(handle.injected.length));

    // A command for a tab nobody enabled is refused here, without a word to the page.
    const before2tabs = handle.injected.length;
    await handle.commandFor(99, "read", {}, 7);
    const un = handle.replyFor(7);
    check("commands: a tab that is not enabled is refused", un.ok === false && has(un.error, "not enabled"), JSON.stringify(un));
    check("commands: and nothing was injected", handle.injected.length === before2tabs, String(handle.injected.length));

    // A protocol the extension does not speak is refused by name.
    await handle.command({ v: 1, type: "command", id: 8, op: "read", tab_id: 7, args: {} });
    const version = handle.replyFor(8);
    check("commands: another protocol version is refused by name", version.ok === false && has(version.error, "protocol"), JSON.stringify(version));

    // A detach issued for a *previous* document, but well-formed for this tab:
    // the refusal must be about the document, not about a missing nonce.
    const before2 = JSON.stringify(handle.entries());
    await handle.command({
        v: PROTOCOL_V, type: "command", id: 3, op: "detach", tab_id: 7, args: {},
        generation: record.generation - 1, time_origin: record.time_origin, nonce: record.nonce,
    });
    reply = handle.replyFor(3);
    check("commands: a stale detach is refused", reply.ok === false, JSON.stringify(reply));
    check("commands: a stale detach does not disable the tab", JSON.stringify(handle.entries()) === before2, JSON.stringify(handle.entries()));

    await handle.commandFor(7, "detach", {}, 4);
    reply = handle.replyFor(4);
    check("commands: a matching detach is honoured", reply.ok === true, JSON.stringify(reply));
    check("commands: and the tab is gone from the enabled set", handle.entry(7) === undefined);

    await handle.commandFor(7, "read", {}, 5);
    reply = handle.replyFor(5);
    check("commands: a read after detach is refused", reply.ok === false && has(reply.error, "not enabled"), JSON.stringify(reply));
}

// ---- 13. a page that cannot be injected into -------------------------------

async function testInjectionFailure() {
    const page = pageState();
    // The click's own two probes work; every read after it fails the way a
    // restricted page or a withdrawn permission does.
    const handle = load({
        pages: oneTab(page),
        script: async (tabId, code, n) => {
            if (n <= 2) return runInPage(code, page);
            throw new Error("Missing host permission for the tab");
        },
    });
    await handle.enable(7);
    check("refusal: the click enabled the tab", !!handle.entry(7), JSON.stringify(handle.entries()));
    await handle.commandFor(7, "read", {}, 1);
    const reply = handle.replyFor(1);
    check("refusal: an uninjectable page is reported, not guessed at", reply.ok === false && has(reply.error, "could not be reached"), JSON.stringify(reply));
    check(
        "refusal: and the enablement stands while the site permission is still held",
        !!handle.entry(7),
        JSON.stringify(handle.entries()),
    );
    check("refusal: with nothing released", handle.permissionCalls.remove.length === 0, JSON.stringify(handle.permissionCalls.remove));

    // The permission gone — withdrawn in the browser's own UI, or never granted for
    // this origin any more — ends the enablement rather than refusing forever.
    handle.env.grants.clear();
    await handle.commandFor(7, "read", {}, 2);
    const second = handle.replyFor(2);
    check("refusal: a withdrawn permission is reported", second.ok === false, JSON.stringify(second));
    check("refusal: and the enablement is let go", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
}

// ---- 14. queued requests and a revoke racing an action ---------------------

async function testQueuedAndRacingRequests() {
    const page = pageState({ body: node("body", {}, { text: "TEXT" }) });
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);

    // Two requests for one tab, the first still in flight: both are answered, and
    // neither is answered from the other's document.
    const gate = deferred();
    let gated = true;
    const handle2 = load({
        pages: oneTab(page),
        script: async (tabId, code) => {
            if (gated) {
                gated = false;
                return gate.promise;
            }
            return runInPage(code, page);
        },
    });
    await handle2.enable(7);
    handle2.env.script = async (tabId, code, n) => {
        if (n === 2) {
            // The first read is out; the second request arrives while it waits.
            return gate.promise;
        }
        return runInPage(code, page);
    };
    const first = handle2.commandFor(7, "read", {}, 21);
    await sleep(0);
    const second = handle2.commandFor(7, "read", {}, 22);
    await sleep(0);
    // The tab's second click lands while both are out: the enablement is over, and
    // neither request may be answered from the document it named.
    await handle2.click(7);
    await settle();
    gate.resolve(runInPage("(() => null)()", page));
    await Promise.all([first, second]);
    const one = handle2.replyFor(21);
    const two = handle2.replyFor(22);
    check("queued: the first request is not answered after the revocation", one.ok === false, JSON.stringify(one));
    check("queued: and neither is the second", two.ok === false, JSON.stringify(two));
    check(
        "queued: both say the enablement changed under them, not that they succeeded",
        has(one.error, "enablement changed") || has(one.error, "not enabled"),
        JSON.stringify(one),
    );

    // An action whose injection is in flight when the tab is disabled: the answer
    // is refused rather than returned, because the record it named is gone.
    const gate2 = deferred();
    const h3 = load({
        pages: oneTab(page),
        script: async (tabId, code) => {
            const out = runInPage(code, page);
            // The click lands in the document, and the enablement ends right after
            // it: the answer is what must not be believed.
            await gate2.promise;
            return out;
        },
    });
    await h3.enable(7);
    await h3.commandFor(7, "structure", {}, 1);
    const ref = h3.replyFor(1).result.nodes[0] ? h3.replyFor(1).result.nodes[0].ref : "e1";
    const acting = h3.commandFor(7, "click", { ref: ref }, 2);
    await sleep(0);
    h3.call("revoke", 7, "the assistant disabled the tab", null);
    gate2.resolve();
    await acting;
    const after = h3.replyFor(2);
    check(
        "races: an action that landed while the tab was disabled is not reported as done",
        after.ok === false,
        JSON.stringify(after),
    );
}

// ---- 15. restart, close, and the host going away ---------------------------

async function testClosingAndRestart() {
    const page = pageState();
    const handle = load({ pages: oneTab(page) });
    await handle.enable(7);

    // The tab closes: the enablement ends and the permission goes with it.
    handle.removed(7);
    check("close: the tab is no longer enabled", handle.entry(7) === undefined, JSON.stringify(handle.entries()));
    check(
        "close: the host is told why",
        handle.states().some((m) => m.revoked && has(m.revoked.reason, "closed")),
        JSON.stringify(handle.states()),
    );
    check("close: and the site permission is released", handle.permissionCalls.remove.length === 1, JSON.stringify(handle.permissionCalls.remove));

    // A browser restart is a new background page: the enabled set is memory, so it
    // starts empty, and the site permission is not consulted to make one.
    const restarted = load({ pages: oneTab(page) });
    check("restart: nothing is enabled after a restart", restarted.entries().length === 0);
    await restarted.commandFor(7, "read", {}, 1);
    const reply = restarted.replyFor(1);
    check("restart: and a read is refused until a person clicks", reply.ok === false && has(reply.error, "not enabled"), JSON.stringify(reply));
    check("restart: no permission was asked for on the way", restarted.permissionCalls.request.length === 0);

    // The host going away ends every enablement, and releases every permission it
    // was holding: there is nowhere for a read to go.
    const gone = load({ pages: new Map([[7, pageState()], [8, pageState({ href: "https://other.test/x" })]]) });
    await gone.enable(7);
    await gone.enable(8);
    gone.env.onDisconnect();
    check("host gone: every tab is disabled", gone.entries().length === 0, JSON.stringify(gone.entries()));
    check("host gone: and both permissions are released", gone.permissionCalls.remove.length === 2, JSON.stringify(gone.permissionCalls.remove));
}

// ---- run -------------------------------------------------------------------

async function main() {
    console.log("# the librewolf extension's browser path, against the shipped background.js");
    await testOutline();
    await testRead();
    await testTheClickIsTheToggle();
    await testMultipleTabs();
    await testInventory();
    await testSameUrlReload();
    await testSameOriginNavigation();
    await testActing();
    await testActingWitness();
    await testRefIdentity();
    await testCommitLatch();
    await testActionInFlightRevocation();
    await testCrossOrigin();
    await testClickRaces();
    await testPendingClickLifecycle();
    await testCommandGenerations();
    await testInjectionFailure();
    await testQueuedAndRacingRequests();
    await testClosingAndRestart();
    if (failures) {
        console.log("\n" + failures + " check(s) failed");
        process.exit(1);
    }
    console.log("\nok   every browser-path check holds");
}

main().catch((e) => {
    console.error(e);
    process.exit(2);
});

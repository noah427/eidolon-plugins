//! What the browser has told this host, and what a request is allowed to act on.
//!
//! The extension owns the browser side — the toolbar click, the site
//! permission, which tabs are enabled, which document each one is showing. This
//! module owns the *record* of that, and the rule that a request is answered
//! only when the tab it names is still enabled, against the **document** the
//! request was issued for.
//!
//! Document identity is a nonce the extension keeps in the content script's own
//! world, with `performance.timeOrigin` beside it as a second, independent
//! signal. The nonce is created the first time the extension looks at a
//! document, so it changes on every new document — including a reload at the
//! *same URL*, which a URL comparison cannot see and which a revocation event
//! may not have reached us about yet. Both checks are here and both fail closed:
//!
//! 1. the reply must name the tab, the generation and the document we issued
//!    against;
//! 2. our own record must still be that same enablement and document at the
//!    moment the reply lands — a revocation that raced the reading wins.
//!
//! Neither check is a substitute for the extension's own check inside the page;
//! this is the half that can be tested without a browser, and the extension
//! repeats it before it will send anything back. The acting ops check it *in*
//! the injected run that acts, so no request can move a mouse or type into a
//! replacement document.
//!
//! ## The enabled set is a snapshot, and the extension's is the truth
//!
//! The extension pushes the whole map after every change (`type: "state"`,
//! `tabs: [...]`), so this host never patches a record from a diff. The
//! generation rule below is what keeps a snapshot from *lowering* anything: a
//! record that moves backwards, or that wears another document at the same
//! generation, is refused and the record already held is kept. That is the
//! fail-closed direction — a stale record makes the extension refuse our
//! requests, while a lowered one would let a request through.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_MAX_CHARS: usize = 8000;
pub const MAX_CHARS: usize = 200_000;
pub const DEFAULT_MAX_NODES: usize = 400;
pub const MAX_NODES: usize = 4000;

/// The wire this host speaks with the extension. It is checked on the `hello`,
/// so a half of a different vintage is refused by name instead of being read as
/// a shape it does not have.
pub const PROTOCOL_VERSION: u64 = 2;

/// How long a command may take before the tool is told it did not answer. The
/// extension is a page read, not a download; a read that takes this long is a
/// bug or a wedged browser, and waiting longer only holds the service's lock.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A detach is a message to the extension and nothing more — it does no page work —
/// so the caller is not made to wait a read's timeout for the acknowledgement. The
/// record is cleared either way; this only bounds how long the caller waits to hear
/// that the page-facing half let go.
pub const DETACH_TIMEOUT: Duration = Duration::from_secs(10);

/// A tab the operator enabled with the extension's toolbar button.
#[derive(Clone, Debug, PartialEq)]
pub struct EnabledTab {
    pub tab_id: i64,
    pub url: String,
    pub title: String,
    /// Scheme, host and port, exactly as the extension read it from the tab.
    /// The site permission Firefox is asked for cannot name a port (bug
    /// 1362809), so the permission is wider than this; the boundary the bridge
    /// keeps is this string, and [`origin_matches`] is how it is checked.
    pub origin: String,
    /// The extension's monotonic counter for this tab. Every enablement, every
    /// new document and every step into "loading, not identified yet" takes the
    /// next value.
    pub generation: u64,
    /// `performance.timeOrigin` of the document that was enabled — a second,
    /// independent signal beside the nonce, kept because LibreWolf ships
    /// `privacy.resistFingerprinting` and a timestamp is not an identity.
    pub time_origin: f64,
    /// The per-document nonce the extension keeps in its own content-script
    /// world. This is the identity: a reload at the same URL gets a new one, and
    /// the page cannot read or forge it. Opaque here — this host only checks that
    /// the answer names the one the request was issued with. While the tab is
    /// loading a new document this is the *last* document's nonce and
    /// `confirmed` is false: there is no usable document until a probe reads it.
    pub nonce: String,
    pub confirmed: bool,
    pub enabled_at_ms: u64,
}

impl EnabledTab {
    /// The record `librewolf_status` is answered with.
    pub fn record(&self) -> Value {
        json!({
            "tab_id": self.tab_id,
            "url": self.url,
            "title": self.title,
            "origin": self.origin,
            "generation": self.generation,
            "time_origin": self.time_origin,
            "nonce": self.nonce,
            "confirmed": self.confirmed,
            "enabled_at_ms": self.enabled_at_ms,
        })
    }

    /// What has to match for a record to be the same *document* rather than the
    /// same tab.
    fn document(&self) -> (&str, f64, bool) {
        (self.nonce.as_str(), self.time_origin, self.confirmed)
    }

    fn invalid(&self) -> Option<String> {
        if self.tab_id < 0 {
            return Some("a tab id below zero".into());
        }
        if !is_http(&self.url) {
            return Some(format!("only http(s) pages can be enabled, and this one is {}", quote(&self.url)));
        }
        if self.origin.is_empty() {
            return Some("no origin".into());
        }
        if !origin_matches(&self.url, &self.origin) {
            return Some(format!(
                "the url {} is not on the origin {}",
                quote(&self.url),
                quote(&self.origin)
            ));
        }
        if self.confirmed && self.nonce.is_empty() {
            return Some("a confirmed document with no nonce".into());
        }
        if self.confirmed && !self.time_origin.is_finite() {
            return Some("a confirmed document with no time origin".into());
        }
        None
    }
}

/// Why an enablement ended, or why a click did not enable one. Kept after the
/// fact so the next call can say what happened instead of only that nothing is
/// enabled.
#[derive(Clone, Debug, PartialEq)]
pub struct Revocation {
    pub tab_id: Option<i64>,
    pub reason: String,
    pub url: Option<String>,
}

#[derive(Default)]
pub struct State {
    /// Set by the extension's `hello`. False again when the port closes.
    pub connected: bool,
    pub addon: Option<String>,
    /// The tabs a click has enabled, by tab id. Empty is the normal state: a
    /// browser restart, a closed tab, a cross-origin navigation and the
    /// assistant's own detach all take entries out of it, and nothing in this
    /// host ever puts one in.
    pub tabs: BTreeMap<i64, EnabledTab>,
    pub last_revocation: Option<Revocation>,
}

/// Only http(s) pages are read or acted on: the bridge has no business in
/// `about:`, `file:`, `view-source:`, or another extension's pages, and a click
/// that lands on one is refused rather than half-served.
pub fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The origin of `url` as scheme://authority, without a path, a query or a
/// fragment — and with the port, when there is one. `None` when the string is
/// not an absolute http(s) URL this host is willing to name an origin.
///
/// Deliberately not a URL parser: it does not resolve, normalise or decode
/// anything, so two spellings of one origin compare unequal and the record is
/// refused rather than trusted. That is the fail-closed direction, and
/// [`origin_matches`] is the only user.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// Whether `url` is on *exactly* the origin `origin` — scheme, host and port.
/// A wider match would be a bug with consequences: the site permission the
/// extension asks for covers every port on a host, so `http://host:8080` and
/// `http://host:9090` are two different boundaries that the permission cannot
/// tell apart and this function must.
pub fn origin_matches(url: &str, origin: &str) -> bool {
    match origin_of(url) {
        Some(parsed) => parsed == origin.to_ascii_lowercase(),
        None => false,
    }
}

impl State {
    /// Replace the enabled set with the snapshot the extension just pushed,
    /// keeping whatever the ordering rules say must not be lowered, and say in
    /// the returned notes what was refused.
    pub fn sync(&mut self, incoming: &[EnabledTab]) -> Vec<String> {
        let mut notes = Vec::new();
        let mut next: BTreeMap<i64, EnabledTab> = BTreeMap::new();
        for tab in incoming {
            if let Some(why) = tab.invalid() {
                notes.push(format!("tab {} was refused: {why}", tab.tab_id));
                // A refused record for a tab this host already holds keeps the
                // record it verified: dropping it would turn a bad push into a
                // disabled tab, and keeping it makes the extension refuse our
                // requests, which is the safe direction.
                if let Some(held) = self.tabs.get(&tab.tab_id) {
                    next.insert(tab.tab_id, held.clone());
                }
                continue;
            }
            if let Some(held) = self.tabs.get(&tab.tab_id) {
                if tab.generation < held.generation {
                    notes.push(format!(
                        "tab {} offered generation {} where {} is held; the older record stands",
                        tab.tab_id, tab.generation, held.generation
                    ));
                    next.insert(tab.tab_id, held.clone());
                    continue;
                }
                if tab.generation == held.generation && tab.document() != held.document() {
                    notes.push(format!(
                        "tab {} offered a second document at generation {}; the record already held stands",
                        tab.tab_id, tab.generation
                    ));
                    next.insert(tab.tab_id, held.clone());
                    continue;
                }
            }
            next.insert(tab.tab_id, tab.clone());
        }
        for (id, held) in &self.tabs {
            if !next.contains_key(id) {
                notes.push(format!("tab {id} ({}) is no longer enabled", held.url));
            }
        }
        self.tabs = next;
        notes
    }

    /// Record why the last enablement ended (or why a click enabled nothing).
    pub fn note_revocation(&mut self, tab_id: Option<i64>, reason: &str, url: Option<String>) {
        self.last_revocation = Some(Revocation {
            tab_id,
            reason: reason.to_string(),
            url,
        });
    }

    /// End one tab's enablement, recording why. A no-op when that tab was not
    /// enabled.
    pub fn revoke(&mut self, tab_id: i64, reason: &str, url: Option<String>) -> Option<Revocation> {
        let ended = self.tabs.remove(&tab_id)?;
        let rev = Revocation {
            tab_id: Some(tab_id),
            reason: reason.to_string(),
            url: url.or_else(|| Some(ended.url.clone())),
        };
        self.last_revocation = Some(rev.clone());
        Some(rev)
    }

    /// The sentence a call gets when the tab it named is not enabled. It names
    /// the one act that can change that, because no call this host serves can.
    pub fn no_tab_reason(&self, tab_id: i64) -> String {
        if !self.connected {
            return "the LibreWolf bridge extension has not connected to this host: open LibreWolf with the librewolf bridge extension enabled".into();
        }
        match &self.last_revocation {
            Some(rev) => format!(
                "tab {tab_id} is not enabled — the last enablement ended because {}; click the extension's toolbar button on the tab you want the assistant to work with, which also asks for that site's permission{}",
                rev.reason,
                rev.url
                    .as_deref()
                    .map(|u| format!(" (it was {})", quote(u)))
                    .unwrap_or_default()
            ),
            None => format!(
                "tab {tab_id} is not enabled — click the librewolf bridge extension's toolbar button on that tab; the click is what enables it, and nothing here can do it for you. Use librewolf_tabs for the ids and titles"
            ),
        }
    }

    /// The sentence for a request that names no tab at all.
    pub fn no_tab_named(&self) -> String {
        "which tab? pass the tab_id of an enabled tab — librewolf_status lists the enabled ones and librewolf_tabs lists every tab in the browser".into()
    }

    pub fn status(&self) -> Value {
        let tabs: Vec<Value> = self.tabs.values().map(EnabledTab::record).collect();
        let empty = tabs.is_empty();
        json!({
            "tabs": tabs,
            "revocation": self.last_revocation.as_ref().map(|r| json!({
                "tab_id": r.tab_id,
                "reason": r.reason,
                "url": r.url,
            })),
            // Only when there is nothing to report: the sentence names the act
            // that enables a tab, and repeating it beside a list of enabled ones
            // would be noise.
            "hint": if empty { Value::String(self.no_tab_reason(0)) } else { Value::Null },
        })
    }

    pub fn health(&self) -> Value {
        json!({
            "status": "ok",
            "browser_connected": self.connected,
            "enabled_tabs": self.tabs.len(),
        })
    }
}

/// The host's side of the bridge: the record, the channel to the extension, and
/// the requests waiting on it.
pub struct Bridge {
    inner: Mutex<State>,
    to_ext: mpsc::UnboundedSender<Value>,
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    expected_addon: String,
    /// When anything last arrived from the extension. The extension and the
    /// browser die together, so this is how the host tells a live browser from a
    /// corpse that is still holding the pipes.
    last_seen: Mutex<Instant>,
}

impl Bridge {
    pub fn new(expected_addon: &str, to_ext: mpsc::UnboundedSender<Value>) -> Bridge {
        Bridge {
            inner: Mutex::new(State::default()),
            to_ext,
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            expected_addon: expected_addon.to_string(),
            last_seen: Mutex::new(Instant::now()),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn health(&self) -> Value {
        self.lock().health()
    }

    /// How many commands are waiting on an answer. Test-only: the rule it checks
    /// is that no exit from a command leaves a waiter behind for the next caller.
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// How long since anything arrived from the extension.
    pub fn idle(&self) -> Duration {
        self.last_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    /// The `librewolf_status` answer: the tabs the toolbar clicks enabled, with
    /// the document each one is showing, and why the last enablement ended.
    /// There is no path through this function that enables anything.
    pub fn status(&self) -> Result<Value, String> {
        Ok(self.lock().status())
    }

    /// Handle one message from the extension.
    pub fn deliver(&self, msg: Value) {
        *self.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        match msg.get("type").and_then(Value::as_str) {
            Some("hello") => {
                let addon = msg.get("addon").and_then(Value::as_str).unwrap_or("");
                let version = msg.get("v").and_then(Value::as_u64);
                let mut st = self.lock();
                if addon != self.expected_addon {
                    crate::note!(
                        "native host: refusing a hello from {addon:?}; this host serves {}",
                        self.expected_addon
                    );
                    st.connected = false;
                    return;
                }
                if version != Some(PROTOCOL_VERSION) {
                    crate::note!(
                        "native host: refusing a hello speaking v{:?}; this host speaks v{PROTOCOL_VERSION}",
                        version
                    );
                    st.connected = false;
                    return;
                }
                st.connected = true;
                st.addon = Some(addon.to_string());
            }
            Some("state") => self.record_state(&msg),
            // The extension says so at a fixed interval; anything at all resets
            // the clock, and this is the cheapest of those.
            Some("heartbeat") => {}
            Some("result") => {
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    crate::note!("native host: a result arrived with no id");
                    return;
                };
                let waiter = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                match waiter {
                    Some(tx) => {
                        let _ = tx.send(msg);
                    }
                    None => {
                        crate::note!("native host: a result for unknown command {id} was dropped")
                    }
                }
            }
            other => crate::note!("native host: ignoring a message of type {other:?}"),
        }
    }

    fn record_state(&self, msg: &Value) {
        let Some(listed) = msg.get("tabs").and_then(Value::as_array) else {
            // A state with no list is a shape this host does not know. The
            // record already held stands: a stale record makes the extension
            // refuse our requests, which is the safe direction.
            crate::note!("native host: a state with no tabs list; the record held stands");
            return;
        };
        let incoming: Vec<EnabledTab> = listed.iter().map(tab_from_json).collect();
        let revoked = msg.get("revoked").and_then(Value::as_object).map(|r| {
            (
                r.get("tab_id").and_then(Value::as_i64),
                r.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the extension ended it")
                    .to_string(),
                r.get("url").and_then(Value::as_str).map(str::to_string),
            )
        });
        let mut st = self.lock();
        for note in st.sync(&incoming) {
            crate::note!("native host: {note}");
        }
        if let Some((tab_id, reason, url)) = revoked {
            st.note_revocation(tab_id, &reason, url);
        }
    }

    /// The native messaging port closed, or the writer failed: nothing can be
    /// served any more. Every enablement is ended (their site permissions died
    /// with the browser side) and every waiting request is failed at once, by
    /// dropping the sender each is waiting on.
    pub fn disconnect(&self) {
        let mut st = self.lock();
        st.connected = false;
        st.addon = None;
        st.tabs.clear();
        st.note_revocation(
            None,
            "the browser closed the native messaging port",
            None,
        );
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// The inventory: every tab of the browser, by title and id, and whether
    /// each is enabled. It names no tab, so it is answered with nothing enabled
    /// — and it enables nothing.
    pub async fn tabs(&self) -> Result<Value, String> {
        let out = self.unbound_command("tabs", json!({})).await?;
        Ok(json!({
            "tabs": out.get("tabs").cloned().unwrap_or_else(|| json!([])),
            "note": out.get("note").cloned().unwrap_or(Value::Null),
            "untrusted": true,
        }))
    }

    /// Issue a command that names no tab, and hand back its result. Only the
    /// inventory is this: it reads the browser's tab list and nothing else, so
    /// there is no document to bind it to.
    async fn unbound_command(&self, op: &str, args: Value) -> Result<Value, String> {
        {
            let st = self.lock();
            if !st.connected {
                return Err(st.no_tab_reason(0));
            }
        }
        let msg = json!({ "v": PROTOCOL_VERSION, "type": "command", "op": op, "args": args });
        let reply = self.ask(msg, None, op).await?;
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Issue a command bound to one enabled tab and to the document the tab is
    /// showing, and refuse the answer unless both the answer and our own record
    /// still name them. See the module comment for the two checks and why
    /// neither alone is enough.
    async fn bounded_command(&self, op: &str, args: Value) -> Result<Value, String> {
        let expected = {
            let st = self.lock();
            if !st.connected {
                return Err(st.no_tab_reason(0));
            }
            let Some(tab_id) = args
                .get("tab_id")
                .and_then(Value::as_i64)
                .filter(|id| *id >= 0)
            else {
                return Err(st.no_tab_named());
            };
            let Some(tab) = st.tabs.get(&tab_id) else {
                return Err(st.no_tab_reason(tab_id));
            };
            if !tab.confirmed {
                return Err(format!(
                    "tab {tab_id} is loading a new document: the document it had is not available any more, and the new one is identified when it settles. Nothing was read or acted on — retry in a moment"
                ));
            }
            tab.clone()
        };
        let msg = json!({
            "v": PROTOCOL_VERSION,
            "type": "command",
            "op": op,
            "tab_id": expected.tab_id,
            "generation": expected.generation,
            "time_origin": expected.time_origin,
            "nonce": expected.nonce,
            "args": args,
        });
        let reply = self.ask(msg, Some(&expected), op).await?;
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send one command and wait for the extension's answer to it, with the
    /// timeout and the waiter bookkeeping in one place.
    async fn ask(
        &self,
        msg: Value,
        expected: Option<&EnabledTab>,
        op: &str,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let mut msg = msg;
        msg["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if self.to_ext.send(msg).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err("this host has lost its connection to the browser".into());
        }
        let reply = match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                return Err(format!(
                    "the bridge dropped this {op} before it was answered (the browser closed, or the host restarted)"
                ));
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(format!(
                    "the extension did not answer this {op} within {}s",
                    COMMAND_TIMEOUT.as_secs()
                ));
            }
        };
        if let Some(expected) = expected {
            self.check_reply(&reply, expected, op)?;
        }
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the extension refused without a reason")
                .to_string());
        }
        Ok(reply)
    }

    /// The answer must be about the tab and the document we asked about, and our
    /// own record must still be that same document. The extension checks this
    /// too; a host that only trusted the extension's `ok` would be trusting a
    /// bug in a page-facing component.
    fn check_reply(&self, reply: &Value, expected: &EnabledTab, op: &str) -> Result<(), String> {
        if reply.get("tab_id").and_then(Value::as_i64) != Some(expected.tab_id) {
            return Err(format!(
                "the extension answered about tab {}, not {}",
                reply
                    .get("tab_id")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "nothing".into()),
                expected.tab_id
            ));
        }
        if reply.get("generation").and_then(Value::as_u64) != Some(expected.generation) {
            return Err(format!(
                "the extension answered about generation {}, not {}",
                reply
                    .get("generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                expected.generation
            ));
        }
        if reply.get("nonce").and_then(Value::as_str) != Some(expected.nonce.as_str()) {
            return Err(
                "the extension answered from a different document than the one enabled; nothing from it is returned"
                    .into(),
            );
        }
        if reply.get("time_origin").and_then(Value::as_f64) != Some(expected.time_origin) {
            return Err(format!(
                "the extension answered about a different document than the one enabled (time origin {}, not {})",
                reply
                    .get("time_origin")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN),
                expected.time_origin
            ));
        }
        let st = self.lock();
        let Some(now) = st.tabs.get(&expected.tab_id) else {
            return Err(format!(
                "the tab was disabled while this {op} was in flight; nothing from it is returned ({})",
                st.no_tab_reason(expected.tab_id)
            ));
        };
        if now.generation != expected.generation {
            return Err(format!(
                "the tab's document changed while this {op} was in flight (generation {} to {}); nothing from it is returned",
                expected.generation, now.generation
            ));
        }
        if now.nonce != expected.nonce || now.time_origin != expected.time_origin {
            return Err(format!(
                "the document changed while this {op} was in flight (a different document is on that tab now); nothing from it is returned"
            ));
        }
        Ok(())
    }

    /// An enabled tab's visible text, as the page renders it.
    pub async fn read(&self, args: &Value) -> Result<Value, String> {
        let max = clamp(
            args.get("max").and_then(Value::as_i64),
            DEFAULT_MAX_CHARS,
            MAX_CHARS,
        );
        let out = self.bounded_command("read", json!({ "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null), "max": max })).await?;
        let text = out.get("text").and_then(Value::as_str).unwrap_or("");
        let (text, cut) = truncate_chars(text, max);
        Ok(json!({
            "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null),
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "title": out.get("title").cloned().unwrap_or(Value::Null),
            "text": text,
            "truncated": cut || out.get("truncated").and_then(Value::as_bool).unwrap_or(false),
            "untrusted": true,
        }))
    }

    /// An enabled tab's DOM-derived outline, with the refs a later request can
    /// name. The refs are the extension's; this host passes them through and
    /// never invents one.
    pub async fn structure(&self, args: &Value) -> Result<Value, String> {
        let max_nodes = clamp(
            args.get("max_nodes").and_then(Value::as_i64),
            DEFAULT_MAX_NODES,
            MAX_NODES,
        );
        let out = self
            .bounded_command(
                "structure",
                json!({ "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null), "max_nodes": max_nodes }),
            )
            .await?;
        let nodes: Vec<Value> = out
            .get("nodes")
            .and_then(Value::as_array)
            .map(|v| v.iter().take(max_nodes).cloned().collect())
            .unwrap_or_default();
        Ok(json!({
            "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null),
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "title": out.get("title").cloned().unwrap_or(Value::Null),
            "nodes": nodes,
            "truncated": out.get("truncated").and_then(Value::as_bool).unwrap_or(false),
            "note": "derived from the DOM (tags and role attributes), not an accessibility tree",
            "untrusted": true,
        }))
    }

    /// Every acting op takes the same shape through the bridge and differs only
    /// in what the extension does; the host passes the fields through and lets
    /// the extension refuse a ref or a target it does not have.
    async fn act(&self, op: &str, args: &Value, extra: Value) -> Result<Value, String> {
        let mut call = json!({ "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null) });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            call[k] = v;
        }
        let out = self.bounded_command(op, call).await?;
        let mut result = out.clone();
        if let Some(map) = result.as_object_mut() {
            map.insert("tab_id".into(), args.get("tab_id").cloned().unwrap_or(Value::Null));
            map.insert("untrusted".into(), json!(true));
            map.insert("note".into(), json!(ACT_NOTE));
        }
        Ok(result)
    }

    pub async fn click(&self, args: &Value) -> Result<Value, String> {
        let ref_ = args.get("ref").cloned().unwrap_or(Value::Null);
        self.act("click", args, json!({ "ref": ref_ })).await
    }

    pub async fn type_text(&self, args: &Value) -> Result<Value, String> {
        let text = args.get("text").cloned().unwrap_or(Value::Null);
        self.act("type", args, json!({ "ref": args.get("ref").cloned().unwrap_or(Value::Null), "text": text }))
            .await
    }

    pub async fn scroll(&self, args: &Value) -> Result<Value, String> {
        self.act(
            "scroll",
            args,
            json!({
                "ref": args.get("ref").cloned().unwrap_or(Value::Null),
                "dy": args.get("dy").cloned().unwrap_or(Value::Null),
                "dx": args.get("dx").cloned().unwrap_or(Value::Null),
            }),
        )
        .await
    }

    pub async fn navigate(&self, args: &Value) -> Result<Value, String> {
        let url = args.get("url").cloned().unwrap_or(Value::Null);
        self.act("navigate", args, json!({ "url": url })).await
    }

    pub async fn back(&self, args: &Value) -> Result<Value, String> {
        self.act("back", args, json!({})).await
    }

    /// Disable one tab. The extension is told first so it can drop the site
    /// permission it holds; the local record is cleared either way, because a
    /// refusal from the page-facing half must not leave this host believing it
    /// still has a tab.
    pub async fn detach(&self, args: &Value) -> Result<Value, String> {
        let expected = {
            let st = self.lock();
            if !st.connected {
                return Err(st.no_tab_reason(0));
            }
            let Some(tab_id) = args.get("tab_id").and_then(Value::as_i64) else {
                return Err(st.no_tab_named());
            };
            st.tabs
                .get(&tab_id)
                .cloned()
                .ok_or_else(|| st.no_tab_reason(tab_id))?
        };

        // Tell the extension first, so it stops using the tab. (No API withdraws
        // the browser's own permission; what ends here is the bridge's
        // enablement, and every path refuses without one.)
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let msg = json!({
            "v": PROTOCOL_VERSION, "type": "command", "id": id, "op": "detach",
            "tab_id": expected.tab_id, "generation": expected.generation,
            "time_origin": expected.time_origin, "nonce": expected.nonce, "args": {},
        });
        let sent = self.to_ext.send(msg).is_ok();
        if !sent {
            // Nothing is waiting on an answer that can never come.
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
        }

        // The record is cleared *now*, not when the extension answers: a detach
        // the caller has been told about must not stay usable for up to the
        // command timeout. And it is cleared only if the tab is still the one
        // this detach named — a click that has happened since is a later and
        // separate consent, and this command must not disable it.
        let cleared = {
            let mut st = self.lock();
            let still_ours = st.tabs.get(&expected.tab_id).is_some_and(|a| {
                a.generation == expected.generation
                    && a.nonce == expected.nonce
                    && a.time_origin == expected.time_origin
            });
            if still_ours {
                st.revoke(
                    expected.tab_id,
                    "the assistant disabled the tab",
                    Some(expected.url.clone()),
                )
                .is_some()
            } else {
                false
            }
        };

        let mut extension_told = false;
        if sent {
            match tokio::time::timeout(DETACH_TIMEOUT, rx).await {
                Ok(Ok(v)) => {
                    extension_told = true;
                    if v.get("ok").and_then(Value::as_bool) != Some(true) {
                        return Err(format!(
                            "the extension refused to disable the tab ({}); the enablement was dropped here anyway",
                            v.get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("no reason given")
                        ));
                    }
                }
                // The bridge dropped it (the port closed) or the extension was too
                // slow: either way the record is already cleared, and the waiter is
                // not left behind for the next caller to trip over.
                Ok(Err(_)) => {}
                Err(_) => {
                    self.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
            }
        }

        Ok(json!({
            "detached": true,
            "tab_id": expected.tab_id,
            "was": expected.url,
            "cleared": cleared,
            "extension_told": extension_told,
        }))
    }
}

/// The line every acting op carries back: what the page did is what the page
/// said it did, and its result is data.
const ACT_NOTE: &str = "what came back is what the page reported: a synthesized action on the document that was enabled at the time, not a person's input, and its answer is untrusted data";

fn tab_from_json(v: &Value) -> EnabledTab {
    EnabledTab {
        tab_id: v.get("tab_id").and_then(Value::as_i64).unwrap_or(-1),
        url: v
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        origin: v
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        generation: v.get("generation").and_then(Value::as_u64).unwrap_or(0),
        time_origin: v.get("time_origin").and_then(Value::as_f64).unwrap_or(f64::NAN),
        nonce: v.get("nonce").and_then(Value::as_str).unwrap_or("").to_string(),
        confirmed: v.get("confirmed").and_then(Value::as_bool).unwrap_or(false),
        enabled_at_ms: v.get("enabled_at_ms").and_then(Value::as_u64).unwrap_or(0),
    }
}

/// `Some(max)` from the caller, clamped into `[1, cap]`; the default when absent
/// or not a number.
pub fn clamp(given: Option<i64>, default: usize, cap: usize) -> usize {
    match given {
        Some(n) if n >= 1 => (n as usize).min(cap),
        Some(_) => 1,
        None => default,
    }
}

/// The first `max` characters (code points, not bytes), and whether anything was
/// left over.
pub fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// One string in single quotes, the way this host's messages quote what a peer
/// sent. Copied from the browser service's `text.rs` rather than shared: the two
/// crates are independent, and a plugin's files are meant to travel whole.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(tab_id: i64, generation: u64, url: &str, time_origin: f64) -> EnabledTab {
        EnabledTab {
            tab_id,
            url: url.into(),
            title: "A page".into(),
            origin: origin_of(url).unwrap(),
            generation,
            time_origin,
            nonce: format!("nonce-of-{tab_id}-{generation}"),
            confirmed: true,
            enabled_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn an_http_page_can_be_enabled() {
        let mut st = State::default();
        assert!(st.sync(&[tab(41, 1, "https://example.com/a", 100.0)]).is_empty());
        assert_eq!(st.tabs.get(&41).unwrap().tab_id, 41);
        // Nothing can enable a tab through this half: the only writer here is a
        // snapshot the extension pushed.
        assert!(st.status()["tabs"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn a_page_that_is_not_http_is_refused() {
        let mut st = State::default();
        for url in [
            "about:config",
            "file:///home/noah/notes.txt",
            "view-source:https://example.com/",
            "moz-extension://abc/page.html",
        ] {
            let mut t = tab(1, 1, "https://example.com/a", 1.0);
            t.url = url.into();
            let notes = st.sync(&[t]);
            assert_eq!(notes.len(), 1, "{url}");
            assert!(notes[0].contains("only http(s) pages"), "{url}: {notes:?}");
            assert!(st.tabs.is_empty(), "{url}");
        }
    }

    #[test]
    fn an_origin_is_scheme_host_and_port_and_nothing_wider() {
        assert_eq!(
            origin_of("http://127.0.0.1:8123/page").unwrap(),
            "http://127.0.0.1:8123"
        );
        assert_eq!(
            origin_of("https://example.com/a?b#c").unwrap(),
            "https://example.com"
        );
        assert!(origin_of("about:blank").is_none());
        assert!(origin_of("https:///nohost").is_none());

        assert!(origin_matches("http://127.0.0.1:8123/x", "http://127.0.0.1:8123"));
        // The site permission an extension asks for cannot name a port, so this
        // is the check that keeps two ports on one host two boundaries.
        assert!(!origin_matches("http://127.0.0.1:8124/x", "http://127.0.0.1:8123"));
        assert!(!origin_matches("https://example.com/x", "http://example.com"));
        // A suffix that merely starts with the origin is not the origin.
        assert!(!origin_matches("https://example.com.evil/x", "https://example.com"));
        assert!(!origin_matches("https://example.comx/x", "https://example.com"));
    }

    #[test]
    fn a_record_whose_url_is_not_on_its_origin_is_refused() {
        let mut st = State::default();
        let mut t = tab(1, 1, "http://127.0.0.1:8123/a", 1.0);
        t.origin = "http://127.0.0.1:8124".into();
        let notes = st.sync(&[t]);
        assert!(
            notes[0].contains("is not on the origin"),
            "{notes:?}"
        );
        assert!(st.tabs.is_empty());
    }

    #[test]
    fn two_tabs_are_two_enablements() {
        let mut st = State::default();
        st.sync(&[
            tab(1, 1, "https://example.com/a", 10.0),
            tab(2, 2, "https://other.example/b", 20.0),
        ]);
        assert_eq!(st.tabs.len(), 2);
        // Ending one leaves the other exactly as it was: there is no blanket
        // enablement to revoke and no shared record to damage.
        st.revoke(1, "the tab was closed", None);
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.tabs.get(&2).unwrap().nonce, "nonce-of-2-2");
    }

    #[test]
    fn a_generation_that_does_not_move_forward_is_refused_and_the_held_record_stands() {
        let mut st = State::default();
        st.sync(&[tab(7, 5, "https://example.com/a", 1.0)]);
        // The same generation wearing a different document is not orderable: a
        // generation is one document on one tab.
        let mut other = tab(7, 5, "https://example.com/a", 1.0);
        other.nonce = "a-different-nonce".into();
        let notes = st.sync(&[other]);
        assert!(notes[0].contains("a second document"), "{notes:?}");
        assert_eq!(st.tabs.get(&7).unwrap().nonce, "nonce-of-7-5");

        // And a snapshot that moves backwards cannot lower the record either.
        let notes = st.sync(&[tab(7, 4, "https://example.com/b", 2.0)]);
        assert!(notes[0].contains("the older record stands"), "{notes:?}");
        assert_eq!(st.tabs.get(&7).unwrap().generation, 5);

        // Going forward is how a new document is recorded.
        st.sync(&[tab(7, 6, "https://example.com/c", 3.0)]);
        assert_eq!(st.tabs.get(&7).unwrap().generation, 6);
    }

    #[test]
    fn a_tab_that_left_the_snapshot_is_no_longer_enabled() {
        let mut st = State::default();
        st.sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        let notes = st.sync(&[]);
        assert!(st.tabs.is_empty());
        assert!(notes[0].contains("no longer enabled"), "{notes:?}");
    }

    #[test]
    fn a_tab_that_is_loading_has_no_usable_document() {
        let mut st = State::default();
        st.sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        // The extension withdrew the document before it knew what was loading:
        // the same nonce, a new generation, `confirmed: false`.
        let mut loading = tab(1, 2, "https://example.com/a", 1.0);
        loading.confirmed = false;
        assert!(st.sync(&[loading.clone()]).is_empty());
        let held = st.tabs.get(&1).unwrap();
        assert!(!held.confirmed);
        // A confirmed document with no nonce is not a record.
        let mut nameless = tab(2, 1, "https://example.com/b", 1.0);
        nameless.nonce = String::new();
        let notes = st.sync(&[nameless]);
        assert!(notes[0].contains("no nonce"), "{notes:?}");
        // Restoring the same document at the same generation is not a change of
        // document, which is what lets a probe clear the flag.
        let mut restored = loading.clone();
        restored.confirmed = true;
        assert!(st.sync(&[restored]).is_empty());
        assert!(st.tabs.get(&1).unwrap().confirmed);
    }

    #[test]
    fn a_call_for_a_tab_that_is_not_enabled_names_the_one_act_that_would_change_that() {
        let mut st = State::default();
        assert!(st.no_tab_reason(3).contains("has not connected"));
        st.connected = true;
        assert!(
            st.no_tab_reason(3)
                .contains("click the librewolf bridge extension's toolbar button")
        );
        st.sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        st.note_revocation(
            Some(1),
            "the tab left https://example.com (the browser reported https://other.example/b)",
            Some("https://other.example/b".into()),
        );
        let reason = st.no_tab_reason(1);
        assert!(reason.contains("left https://example.com"), "{reason}");
        assert!(reason.contains("https://other.example/b"), "{reason}");
        // A request with no tab id at all says what to pass instead.
        assert!(st.no_tab_named().contains("librewolf_tabs"));
    }

    #[test]
    fn clamping_is_the_hosts_to_decide() {
        assert_eq!(clamp(None, DEFAULT_MAX_CHARS, MAX_CHARS), DEFAULT_MAX_CHARS);
        assert_eq!(clamp(Some(10), DEFAULT_MAX_CHARS, MAX_CHARS), 10);
        assert_eq!(clamp(Some(0), DEFAULT_MAX_CHARS, MAX_CHARS), 1);
        assert_eq!(clamp(Some(-4), DEFAULT_MAX_CHARS, MAX_CHARS), 1);
        assert_eq!(
            clamp(Some(i64::MAX), DEFAULT_MAX_CHARS, MAX_CHARS),
            MAX_CHARS
        );
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("héllo", 2), ("hé".to_string(), true));
        assert_eq!(truncate_chars("hi", 5), ("hi".to_string(), false));
    }

    /// A tab that is loading has no document to send a request against, so the
    /// refusal happens here rather than by guessing a nonce.
    #[tokio::test]
    async fn a_request_for_a_loading_tab_is_refused_without_asking_the_extension() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = Bridge::new("librewolf-bridge@eidolon.local", tx);
        bridge.lock().connected = true;
        let mut loading = tab(7, 2, "https://example.com/a", 1.0);
        loading.confirmed = false;
        bridge.lock().sync(&[loading]);
        let answer = bridge.read(&json!({"tab_id": 7})).await;
        assert!(answer.is_err(), "{answer:?}");
        assert!(answer.unwrap_err().contains("loading a new document"));
        assert!(
            rx.try_recv().is_err(),
            "nothing should have been asked of the extension"
        );
    }

    #[tokio::test]
    async fn a_request_for_a_tab_that_is_not_enabled_is_refused_without_asking() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = Bridge::new("librewolf-bridge@eidolon.local", tx);
        bridge.lock().connected = true;
        bridge.lock().sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        let answer = bridge.read(&json!({"tab_id": 2})).await;
        assert!(answer.is_err(), "{answer:?}");
        assert!(answer.unwrap_err().contains("tab 2 is not enabled"));
        let answer = bridge.read(&json!({})).await;
        assert!(answer.is_err(), "{answer:?}");
        assert!(answer.unwrap_err().contains("which tab?"));
        assert!(rx.try_recv().is_err(), "nothing should have been asked");
    }

    /// The two rules a detach has to keep, tested where the interleaving can be
    /// scripted exactly: it clears the record at once rather than when the
    /// extension answers, and it never disables a tab a *newer* click enabled.
    #[tokio::test]
    async fn a_stale_detach_does_not_disable_a_newer_enablement() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = std::sync::Arc::new(Bridge::new("librewolf-bridge@eidolon.local", tx));
        bridge.lock().connected = true;
        bridge.lock().sync(&[tab(1, 1, "https://example.com/a", 1.0)]);

        let detaching = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.detach(&json!({"tab_id": 1})).await })
        };
        // The command goes out, and the record is cleared straight away...
        let _command = rx.recv().await.expect("the detach command should be sent");
        assert!(
            bridge.lock().tabs.is_empty(),
            "the record must be cleared at once"
        );
        // ...while a fresh toolbar click arrives before the detach is answered.
        // That click is a later consent, and the detach issued before it must not
        // take it away.
        bridge.deliver(json!({"v": 2, "type": "state", "tabs": [
            {"tab_id": 1, "url": "https://example.com/b", "title": "B", "origin": "https://example.com",
             "generation": 2, "time_origin": 55.0, "nonce": "nonce-of-1-2", "confirmed": true, "enabled_at_ms": 2},
        ]}));
        let id = json!(1); // the answer that is coming is for the first command
        bridge.deliver(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 1,
                              "generation": 1, "time_origin": 1.0, "nonce": "nonce-of-1-1",
                              "result": {"detached": true}}));
        let answer = detaching.await.unwrap();
        assert!(answer.is_ok(), "the newer click's enablement is not this detach's to end: {answer:?}");
        // The clear and the newer push race each other — the clear is a fact
        // about the record this detach named, and the newer click's own push
        // re-establishes its record either way. What must hold is that the newer
        // enablement is intact and that nothing is left waiting.
        let st = bridge.lock();
        let now = st
            .tabs
            .get(&1)
            .expect("the newer click's enablement must survive");
        assert_eq!(now.generation, 2);
        assert_eq!(now.url, "https://example.com/b");
        assert_eq!(now.nonce, "nonce-of-1-2");
        drop(st);
        assert_eq!(bridge.pending_len(), 0, "no waiter may be left behind");
    }

    #[tokio::test]
    async fn a_command_that_cannot_be_sent_clears_the_record_and_leaves_no_waiter() {
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = Bridge::new("librewolf-bridge@eidolon.local", tx);
        bridge.lock().connected = true;
        bridge.lock().sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        drop(rx); // the writer is gone: every send fails from here
        let answer = bridge.detach(&json!({"tab_id": 1})).await;
        assert!(
            answer.is_ok(),
            "the record is ours to clear whether or not the extension hears about it: {answer:?}"
        );
        assert_eq!(answer.unwrap()["extension_told"], json!(false));
        assert!(bridge.lock().tabs.is_empty());
        assert_eq!(
            bridge.pending_len(),
            0,
            "a failed send must not leave a waiter behind"
        );
    }

    #[tokio::test]
    async fn a_detach_the_extension_refuses_still_clears_the_record_and_the_waiter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = std::sync::Arc::new(Bridge::new("librewolf-bridge@eidolon.local", tx));
        bridge.lock().connected = true;
        bridge.lock().sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        let detaching = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.detach(&json!({"tab_id": 1})).await })
        };
        let command = rx.recv().await.expect("the detach command should be sent");
        let id = command["id"].clone();
        bridge.deliver(json!({"v": 2, "type": "result", "id": id, "ok": false, "tab_id": 1,
                              "generation": 1, "time_origin": 1.0, "nonce": "nonce-of-1-1",
                              "error": "the tab is gone"}));
        let answer = detaching.await.unwrap();
        assert!(answer.is_err(), "a refusal is reported: {answer:?}");
        assert!(answer.unwrap_err().contains("dropped here anyway"));
        assert!(bridge.lock().tabs.is_empty());
        assert_eq!(bridge.pending_len(), 0);
    }

    #[test]
    fn health_says_what_a_glance_needs() {
        let mut st = State {
            connected: true,
            ..State::default()
        };
        assert_eq!(st.health()["browser_connected"], json!(true));
        assert_eq!(st.health()["enabled_tabs"], json!(0));
        st.sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        assert_eq!(st.health()["enabled_tabs"], json!(1));
    }

    #[test]
    fn the_status_payload_names_every_enabled_tab_and_the_last_revocation() {
        let mut st = State {
            connected: true,
            ..State::default()
        };
        let empty = st.status();
        assert_eq!(empty["tabs"].as_array().unwrap().len(), 0);
        assert!(
            empty["hint"].as_str().unwrap().contains("toolbar button"),
            "{empty}"
        );
        st.sync(&[
            tab(1, 1, "https://example.com/a", 1.0),
            tab(2, 1, "https://other.example/b", 2.0),
        ]);
        st.revoke(1, "the tab was closed", None);
        let status = st.status();
        assert_eq!(status["tabs"].as_array().unwrap().len(), 1);
        assert_eq!(status["tabs"][0]["tab_id"], json!(2));
        assert_eq!(status["tabs"][0]["confirmed"], json!(true));
        assert_eq!(status["revocation"]["tab_id"], json!(1));
        assert_eq!(status["hint"], Value::Null);
    }
}

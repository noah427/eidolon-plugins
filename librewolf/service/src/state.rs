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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_MAX_CHARS: usize = 8000;
pub const MAX_CHARS: usize = 200_000;
/// How much of a field's value `librewolf_field_value` will return. The one door to a
/// value; the page-facing half caps it too, and the host caps it again rather than
/// trusting the other side of the port.
pub const FIELD_VALUE_MAX_CHARS: usize = 8000;
/// How many of a multiple select's selected values `librewolf_field_value` will return.
pub const FIELD_VALUE_MAX_SELECTED: usize = 64;
/// The largest decoded picture `librewolf_screenshot` will accept from the page-facing half.
pub const SCREENSHOT_MAX_BYTES: usize = 32 * 1024 * 1024;
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

    /// What has to match for a record to be the same *document* rather than the same
    /// tab: the nonce and the time origin. `confirmed` is deliberately **not** here.
    /// It is readiness, not identity, and folding it in makes the same document look
    /// like a second one every time a probe re-confirmed it — which is how a tab the
    /// extension had already re-identified stayed unconfirmed on this host.
    fn identity(&self) -> (&str, f64) {
        (self.nonce.as_str(), self.time_origin)
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

/// One `document_ready` record: a document that became identified and usable, as the host
/// can see it from the state stream alone. It carries no page text — a sequence, the tab, the
/// document's generation and nonce, why it became ready, and when the host saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub seq: u64,
    pub tab_id: i64,
    pub generation: u64,
    pub nonce: String,
    pub cause: String,
    pub at_ms: u64,
}

impl EventRecord {
    fn record(&self) -> Value {
        json!({
            "seq": self.seq,
            "tab_id": self.tab_id,
            "generation": self.generation,
            "nonce": self.nonce,
            "cause": self.cause,
            "at_ms": self.at_ms,
        })
    }
}

/// One tab's event log, and the incarnation of the source that produced it. The epoch is
/// minted when the log is made and dies with it: a tab disabled and enabled again gets a new
/// log and a new epoch, so a cursor from the old one can never be read as a continuation. A
/// DOCUMENT change does not touch it — the document is a filter, not a source.
#[derive(Clone, Debug, PartialEq)]
pub struct EventLog {
    /// The incarnation of the source. Minted from the OS's own randomness, not a clock: two
    /// hosts starting in the same millisecond must not share one, and a counter that dies with
    /// the process cannot tell them apart either.
    pub epoch: String,
    pub head: u64,
    /// Records dropped off the front to keep this log bounded. A cursor older than what is
    /// left is answered `cursor_expired`, never quietly.
    pub dropped: u64,
    pub records: Vec<EventRecord>,
}

/// How many records one tab's log keeps. The head is the cursor, so a bounded log bounds the
/// memory a parked wait can cost.
pub const EVENT_LOG_CAP: usize = 256;

/// An opaque, unrepeatable value: the OS's own randomness, with a pid-and-clock fallback for
/// the impossible case where /dev/urandom cannot be read.
fn random_hex() -> String {
    use std::io::Read as _;
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    format!("{}-{}", std::process::id(), clock_ms())
}

#[derive(Clone)]
pub struct ControlWatch {
    epoch: String,
    nonce: String,
    time_origin: f64,
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
    /// tab id -> its event log. A log exists while the tab is enabled; it is dropped with
    /// the enablement, so an epoch never outlives the lineage that minted it.
    pub events: BTreeMap<i64, EventLog>,
    /// How many logs this host has ever made. Part of an epoch, so two logs made in the same
    /// millisecond cannot be mistaken for one another.
    pub logs_made: u64,
    pub watches: BTreeMap<i64, ControlWatch>,
}

/// Wall-clock milliseconds. The event log's `at_ms` is when the HOST saw the edge, which is
/// the only clock it honestly has.
fn clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
                // A generation names one document. A record that withdraws and then
                // re-confirms the *same* document at the same generation — the
                // extension's unconfirm/refresh round trip, where the probe found the
                // document it had — is not a second document, and taking it is what
                // lets the tab be usable again. What must still be refused is one
                // generation wearing a *different* document: a different nonce or a
                // different time origin.
                if tab.generation == held.generation && tab.identity() != held.identity() {
                    // Keep the trusted tab record, but a document-bound observer can no
                    // longer be vouched for after conflicting document identity evidence.
                    self.watches.remove(&tab.tab_id);
                    notes.push(format!(
                        "tab {} offered a second document at generation {}; the record already held stands",
                        tab.tab_id, tab.generation
                    ));
                    next.insert(tab.tab_id, held.clone());
                    continue;
                }
            }
            // The readiness LEVEL edge, seen from the state stream alone: `confirmed`
            // going false -> true for this tab's current document. A push that arrives
            // already-confirmed emits nothing, which is the dedupe that keeps repeated
            // same-URL re-reports from making a storm.
            if tab.confirmed {
                let cause = match self.tabs.get(&tab.tab_id) {
                    None => Some("enabled"),
                    Some(held) if !held.confirmed && held.generation == tab.generation => {
                        Some("restored")
                    }
                    Some(held) if !held.confirmed => Some("settled"),
                    Some(_) => None,
                };
                if let Some(cause) = cause {
                    self.logs_made += 1;
                    let log = self.events.entry(tab.tab_id).or_insert_with(|| EventLog {
                        epoch: format!("h-{}", random_hex()),
                        head: 0,
                        dropped: 0,
                        records: Vec::new(),
                    });
                    log.head += 1;
                    log.records.push(EventRecord {
                        seq: log.head,
                        tab_id: tab.tab_id,
                        generation: tab.generation,
                        nonce: tab.nonce.clone(),
                        cause: cause.to_string(),
                        at_ms: clock_ms(),
                    });
                    // Bounded: the head stays the cursor, and what falls off the front is
                    // counted so a stale cursor can be told from a quiet source.
                    if log.records.len() > EVENT_LOG_CAP {
                        let overflow = log.records.len() - EVENT_LOG_CAP;
                        log.records.drain(0..overflow);
                        log.dropped += overflow as u64;
                    }
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
        // A log dies with the enablement that made it.
        let live: std::collections::BTreeSet<i64> = self.tabs.keys().copied().collect();
        self.events.retain(|id, _| live.contains(id));
        self.watches.retain(|id, w| self.tabs.get(id).is_some_and(|t| t.nonce == w.nonce && t.time_origin == w.time_origin));
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
        self.watches.remove(&tab_id);
        self.events.remove(&tab_id);
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
        st.watches.clear();
        st.events.clear();
        st.note_revocation(None, "the browser closed the native messaging port", None);
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
                reply.get("generation").and_then(Value::as_u64).unwrap_or(0),
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
        let out = self
            .bounded_command(
                "read",
                json!({ "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null), "max": max }),
            )
            .await?;
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

    /// The value of one form control, or the reason it is redacted. The **only** verb
    /// that returns a value: the outline and every receipt carry none, and a redacted
    /// control answers a reason with no value, no length and no digest. It carries its
    /// own name on purpose — the gate is by name, so an operator can allow the outline
    /// (`librewolf_structure`) and deny this one.
    pub async fn field_value(&self, args: &Value) -> Result<Value, String> {
        let out = self
            .bounded_command(
                "field_value",
                json!({
                    "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null),
                    "ref": args.get("ref").cloned().unwrap_or(Value::Null),
                }),
            )
            .await?;
        // The page-facing half caps this too; the host caps it again rather than
        // trusting a bug on the other side of the port, exactly as `read` does.
        let redacted = out.get("redacted").and_then(Value::as_str);
        let mut result = json!({
            "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null),
            "ref": args.get("ref").cloned().unwrap_or(Value::Null),
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "untrusted": true,
        });
        match redacted {
            Some(why) => {
                // No value, no length, no digest: the redaction is the whole answer.
                result["redacted"] = json!(why);
            }
            None => {
                if let Some(values) = out.get("values").and_then(Value::as_array) {
                    // A malformed entry makes the whole answer malformed. A values array
                    // that is not all strings is REFUSED rather than silently returning a
                    // partial selection the model could read as the whole of it.
                    let mut all: Vec<&str> = Vec::with_capacity(values.len());
                    for v in values {
                        match v.as_str() {
                            Some(s) => all.push(s),
                            None => {
                                return Err(
                                    "the extension answered a field read with a values array that is not all strings; nothing is returned rather than a partial selection"
                                        .into(),
                                );
                            }
                        }
                    }
                    let mut kept: Vec<Value> = Vec::new();
                    let mut total = 0usize;
                    let mut cut = all.len() > FIELD_VALUE_MAX_SELECTED;
                    for s in all.iter().take(FIELD_VALUE_MAX_SELECTED) {
                        let (s, c) = truncate_chars(s, FIELD_VALUE_MAX_CHARS);
                        let n = s.chars().count();
                        if total + n > FIELD_VALUE_MAX_CHARS {
                            cut = true;
                            break;
                        }
                        total += n;
                        kept.push(json!(s));
                        if c {
                            cut = true;
                            break;
                        }
                    }
                    result["values"] = json!(kept);
                    result["multiple"] = json!(true);
                    result["truncated"] = json!(
                        cut || out
                            .get("truncated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    );
                } else {
                    let Some(value) = out.get("value").and_then(Value::as_str) else {
                        // Missing data is not an empty field.
                        return Err(
                            "the extension answered a field read with neither a value nor a redaction; nothing is returned rather than an empty field"
                                .into(),
                        );
                    };
                    let (value, cut) = truncate_chars(value, FIELD_VALUE_MAX_CHARS);
                    result["value"] = json!(value);
                    result["truncated"] = json!(
                        cut || out
                            .get("truncated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    );
                }
            }
        }
        Ok(result)
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
            map.insert(
                "tab_id".into(),
                args.get("tab_id").cloned().unwrap_or(Value::Null),
            );
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
        self.act(
            "type",
            args,
            json!({ "ref": args.get("ref").cloned().unwrap_or(Value::Null), "text": text }),
        )
        .await
    }

    /// Selection is its own verb: the caller names which option it means, one way only,
    /// and the answer carries labels and indexes, never a value.
    pub async fn select(&self, args: &Value) -> Result<Value, String> {
        self.act(
            "select",
            args,
            json!({
                "ref": args.get("ref").cloned().unwrap_or(Value::Null),
                "option": args.get("option").cloned().unwrap_or(Value::Null),
                "options": args.get("options").cloned().unwrap_or(Value::Null),
            }),
        )
        .await
    }

    /// Desired state, not a toggle.
    pub async fn set_checked(&self, args: &Value) -> Result<Value, String> {
        self.act(
            "set_checked",
            args,
            json!({
                "ref": args.get("ref").cloned().unwrap_or(Value::Null),
                "checked": args.get("checked").cloned().unwrap_or(Value::Null),
            }),
        )
        .await
    }

    /// The form's own requestSubmit, not a click that hopes to find its submit control.
    pub async fn submit(&self, args: &Value) -> Result<Value, String> {
        self.act(
            "submit",
            args,
            json!({ "ref": args.get("ref").cloned().unwrap_or(Value::Null) }),
        )
        .await
    }

    /// A picture of one enabled tab, captured the narrow way and written where the caller
    /// named. `mutating` because of the disk, exactly like hyprland's screenshot; what may
    /// be captured is still only ever read, and never a tab the window is not showing.
    pub async fn screenshot(&self, args: &Value) -> Result<Value, String> {
        let tab_id = args.get("tab_id").cloned().unwrap_or(Value::Null);
        let out = self
            .bounded_command("screenshot", json!({ "tab_id": tab_id }))
            .await?;
        let data = out
            .get("image")
            .and_then(Value::as_str)
            .ok_or("the extension returned no picture; nothing was written")?;
        let png = decode_png_data_url(data)?;
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_screenshot_path(out.get("tab_id").and_then(Value::as_i64).unwrap_or(0)),
        };
        if !path.is_absolute() {
            return Err(format!(
                "the picture's path must be absolute, and {} is not; nothing was written",
                quote(&path.to_string_lossy())
            ));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = ensure_private_dir(parent)
        {
            return Err(format!(
                "the picture's directory {} could not be used: {e}; nothing was written",
                quote(&parent.to_string_lossy())
            ));
        }
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        write_png(&path, &png, overwrite)?;
        Ok(json!({
            "tab_id": out.get("tab_id").cloned().unwrap_or(tab_id),
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "path": path.to_string_lossy(),
            "bytes": png.len(),
            // Said in the answer, not only in a comment: the operator has to know that a
            // capture is not atomically bound to the enabled tab.
            "limitation": "the picture is of the window's visible tab, checked before and after the capture. It is NOT atomically bound to the enabled tab: a tab that switches away and back within the same window between those checks is indistinguishable from one that never moved. A tab that MOVED to another window is refused.",
            "untrusted": true,
        }))
    }

    /// An explicit download of one artifact of the enabled tab's own origin. The
    /// page-facing half checks the operator's `downloads` grant and refuses without it —
    /// nothing here, and nothing there, ever asks for it.
    /// The declared-event source, and the model's delta read. The harness dispatches this
    /// with the event, the filter and the arm's cursor and reads `{epoch, seq, holds}`; a
    /// model asks for `delta: true` to read the records after a cursor. The cursor is the head
    /// of the FILTER-SCOPED stream — one tab's log — never a global last-write sequence, so a
    /// wait on one tab cannot fire on another's record.
    pub async fn events(&self, args: &Value) -> Result<Value, String> {
        if args.get("event").and_then(Value::as_str) == Some("control_appeared") {
            return self.control_events(args).await;
        }
        let event = args.get("event").and_then(Value::as_str).unwrap_or("");
        if event != "document_ready" {
            return Err(format!(
                "this plugin declares no event named {}; the events it declares are document_ready",
                quote(event)
            ));
        }
        let tab_id = args
            .get("filter")
            .and_then(|f| f.get("tab_id"))
            .and_then(Value::as_i64)
            .ok_or("a document_ready filter names a tab_id: the event is per tab")?;
        let st = self.lock();
        let Some(log) = st.events.get(&tab_id) else {
            // A missing source is a TERMINAL answer with a declared code, not an error the
            // harness would read as a transport failure and park on to its deadline.
            return Ok(json!({
                "event": "document_ready",
                "filter": { "tab_id": tab_id },
                "code": "source_missing",
                "holds": false,
                "note": "that tab has no event source: it is not enabled, so nothing can be said about its document",
            }));
        };
        let holds = st.tabs.get(&tab_id).map(|t| t.confirmed).unwrap_or(false);
        let mut out = json!({
            "event": "document_ready",
            "filter": { "tab_id": tab_id },
            "epoch": log.epoch,
            "seq": log.head,
            "holds": holds,
        });
        if args.get("since").is_some() || args.get("delta").and_then(Value::as_bool) == Some(true) {
            let since_epoch = args
                .get("since")
                .and_then(|s| s.get("epoch"))
                .and_then(Value::as_str);
            let since_seq = args
                .get("since")
                .and_then(|s| s.get("seq"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if since_epoch != Some(log.epoch.as_str()) {
                // The contract's own code, and NO `lines` key at all: an empty list would
                // read as a quiet source, which is a different thing.
                out["code"] = json!("source_restarted");
                out["note"] = json!(
                    "the cursor's epoch is not this source's, so these records are not a continuation of it and none are returned"
                );
            } else {
                let oldest = log.records.first().map(|r| r.seq).unwrap_or(log.head + 1);
                if since_seq < oldest.saturating_sub(1) {
                    out["code"] = json!("cursor_expired");
                    out["note"] = json!(
                        "records after that cursor were dropped to keep this log bounded; only the retained ones are here"
                    );
                }
                if args.get("delta").and_then(Value::as_bool) == Some(true) {
                    out["lines"] = json!(
                        log.records.iter().filter(|r| r.seq > since_seq)
                            .map(EventRecord::record).collect::<Vec<Value>>()
                    );
                }
                if log.dropped > 0 {
                    out["dropped"] = json!(log.dropped);
                }
            }
        }
        Ok(out)
    }

    /// Install one document-bound subscription, replacing the tab's previous observer.
    pub async fn watch(&self, args: &Value) -> Result<Value, String> {
        let tab_id = args.get("tab_id").and_then(Value::as_i64).ok_or("watch needs tab_id")?;
        let selector = args.get("selector").and_then(Value::as_str).ok_or("watch needs selector")?;
        if selector.trim().is_empty() || selector.encode_utf16().count() > 2048 { return Err("selector must be nonempty and at most 2048 UTF-16 code units".into()); }
        let tab = self.lock().tabs.get(&tab_id).cloned().ok_or("tab is not enabled")?;
        let epoch = format!("w-{}-{}", random_hex(), random_hex());
        let out = self.bounded_command("watch", json!({"tab_id": tab_id, "selector": selector, "epoch": epoch})).await?;
        if out.get("epoch").and_then(Value::as_str) != Some(epoch.as_str()) || out.get("seq").and_then(Value::as_u64) != Some(0) || !out.get("holds").is_some_and(Value::is_boolean) {
            return Err("malformed observer installation receipt".into());
        }
        let mut st = self.lock();
        if !st.tabs.get(&tab_id).is_some_and(|t| t.confirmed && t.generation == tab.generation && t.enabled_at_ms == tab.enabled_at_ms && t.nonce == tab.nonce && t.time_origin == tab.time_origin) {
            return Err("document changed during observer installation; no subscription returned".into());
        }
        st.watches.insert(tab_id, ControlWatch { epoch: epoch.clone(), nonce: tab.nonce, time_origin: tab.time_origin });
        Ok(json!({"event": "control_appeared", "filter": {"tab_id": tab_id, "watch_id": epoch}, "epoch": epoch, "seq": 0, "holds": out["holds"]}))
    }

    async fn control_events(&self, args: &Value) -> Result<Value, String> {
        let filter = args.get("filter").and_then(Value::as_object).ok_or("control_appeared needs a filter")?;
        if filter.keys().any(|k| k != "tab_id" && k != "watch_id") { return Err("unknown control_appeared filter key".into()); }
        let tab_id = filter.get("tab_id").and_then(Value::as_i64).ok_or("filter needs tab_id")?;
        let epoch = filter.get("watch_id").and_then(Value::as_str).ok_or("filter needs watch_id from watch")?;
        let watch = self.lock().watches.get(&tab_id).cloned();
        let Some(watch) = watch.filter(|w| w.epoch == epoch) else { return Ok(json!({"code": "observer_dropped"})); };
        let since = args.get("since").cloned().unwrap_or(Value::Null);
        if !since.is_null() && (since.get("epoch").and_then(Value::as_str).is_none() || since.get("seq").and_then(Value::as_u64).is_none()) { return Err("since needs epoch and unsigned seq".into()); }
        let delta = args.get("delta").and_then(Value::as_bool) == Some(true);
        let sampled = self.bounded_command("watch_sample", json!({"tab_id": tab_id, "epoch": epoch, "since": since, "delta": delta})).await;
        // A conflicting/new document may have invalidated the subscription while the
        // request was in flight. Prefer its terminal status over a transient wire error.
        if !self.lock().watches.get(&tab_id).is_some_and(|w| w.epoch == watch.epoch) {
            return Ok(json!({"code": "observer_dropped"}));
        }
        let out = sampled?;
        if !self.lock().watches.get(&tab_id).is_some_and(|w| w.epoch == watch.epoch) { return Ok(json!({"code": "observer_dropped"})); }
        if let Some(code) = out.get("code").and_then(Value::as_str) {
            if ["observer_dropped", "source_restarted", "cursor_expired", "cursor_invalid"].contains(&code) { return Ok(json!({"code": code})); }
            return Err("unknown observer status".into());
        }
        let seq = out.get("seq").and_then(Value::as_u64).ok_or("observer yielded no sequence")?;
        let holds = out.get("holds").and_then(Value::as_bool).ok_or("observer yielded no level")?;
        if out.get("epoch").and_then(Value::as_str) != Some(epoch) { return Err("observer yielded a different epoch".into()); }
        let mut result = json!({"epoch": epoch, "seq": seq, "holds": holds});
        if delta {
            let lines = out.get("lines").and_then(Value::as_array).ok_or("observer yielded no delta")?;
            if lines.len() > EVENT_LOG_CAP { return Err("observer delta exceeds bound".into()); }
            let mut clean = Vec::new();
            for line in lines {
                let n = line.get("seq").and_then(Value::as_u64).ok_or("malformed observer record")?;
                let at = line.get("at_ms").and_then(Value::as_u64).ok_or("malformed observer time")?;
                clean.push(json!({"seq": n, "at_ms": at, "tab_id": tab_id}));
            }
            result["lines"] = json!(clean);
        }
        Ok(result)
    }

    pub async fn download(&self, args: &Value) -> Result<Value, String> {
        self.act(
            "download",
            args,
            json!({
                "url": args.get("url").cloned().unwrap_or(Value::Null),
                "filename": args.get("filename").cloned().unwrap_or(Value::Null),
                "save_as": args.get("save_as").cloned().unwrap_or(Value::Null),
                "conflict_action": args.get("conflict_action").cloned().unwrap_or(Value::Null),
            }),
        )
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

/// Where a picture lands when the caller names no path: a directory of our own, unique to
/// this user and this host process, so one run's picture cannot land on another's path.
fn default_screenshot_path(tab_id: i64) -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "eidolon-librewolf-{}-{}",
            unsafe { libc::geteuid() },
            std::process::id()
        ))
        .join(format!("tab-{tab_id}.png"))
}

/// Make `path` be a directory only this user can read, or check that it already is. A
/// pre-existing directory is *checked*, not trusted: `DirBuilder::mode` only applies to a
/// directory it creates, so an attacker-planted 0777 directory would otherwise be used.
fn ensure_private_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let where_ = quote(&path.to_string_lossy());
    if std::fs::symlink_metadata(path).is_ok() {
        let md = std::fs::symlink_metadata(path)
            .map_err(|e| format!("{where_} cannot be examined: {e}"))?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err(format!(
                "{where_} exists and is not a directory this host may write into"
            ));
        }
        let mode = md.permissions().mode() & 0o777;
        if md.uid() != unsafe { libc::geteuid() } {
            return Err(format!("{where_} belongs to another user"));
        }
        if mode & 0o077 != 0 {
            return Err(format!(
                "{where_} is mode {mode:o}; it must not be readable, writable or traversable by anybody else"
            ));
        }
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("{where_} could not be made: {e}"))
}

/// Write a picture. Never through a symlink, never mode-wider than 0600, and never over an
/// existing file unless the caller said `overwrite: true` — a capture is not a reason to
/// clobber a path somebody else chose.
fn write_png(path: &std::path::Path, bytes: &[u8], overwrite: bool) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let where_ = quote(&path.to_string_lossy());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    if overwrite {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    let mut file = opts.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => format!(
            "{where_} already exists; pass overwrite: true to replace it, or name another path"
        ),
        _ => format!(
            "{where_} could not be opened ({e}); a symlink here is refused, and nothing was written"
        ),
    })?;
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .map_err(|e| format!("{where_} could not be written: {e}"))
}

/// A PNG data URL to bytes. The prefix is checked, then the size, then the PNG signature.
/// The signature is a **signature check, not validation**: these bytes are a PNG's first
/// eight, and nothing here parses chunks or decodes the image.
fn decode_png_data_url(data: &str) -> Result<Vec<u8>, String> {
    const PREFIX: &str = "data:image/png;base64,";
    let Some(b64) = data.strip_prefix(PREFIX) else {
        return Err("the picture is not a PNG data URL; nothing was written".into());
    };
    if b64.len() > SCREENSHOT_MAX_BYTES / 3 * 4 + 16 {
        return Err(format!(
            "the picture is too large to accept (over {SCREENSHOT_MAX_BYTES} bytes decoded); nothing was written"
        ));
    }
    let Some(bytes) = base64_decode(b64) else {
        return Err("the picture's base64 is malformed; nothing was written".into());
    };
    if bytes.len() > SCREENSHOT_MAX_BYTES {
        return Err(format!(
            "the picture is {} bytes and the cap is {SCREENSHOT_MAX_BYTES}; nothing was written",
            bytes.len()
        ));
    }
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(
            "the decoded bytes do not begin with the PNG signature; nothing was written".into(),
        );
    }
    Ok(bytes)
}

/// base64, standard alphabet with padding, strictly. Small on purpose: the one place this
/// host needs it is not worth a dependency. Padding is only ever the tail: `=` ends the
/// input, and anything after it, a bad length, or a stray character is a malformed payload,
/// not something to decode leniently.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let trimmed: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if !trimmed.len().is_multiple_of(4) {
        return None;
    }
    let body = trimmed.trim_end_matches('=');
    let pad = trimmed.len() - body.len();
    if pad > 2 || body.contains('=') {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for b in body.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
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
    fn observers_are_document_and_tab_scoped_and_die_with_enablement() {
        let mut st = State::default();
        let first = tab(1, 1, "https://example.com/a", 1.0);
        let second = tab(2, 1, "https://example.com/b", 2.0);
        st.sync(&[first.clone(), second.clone()]);
        st.watches.insert(1, ControlWatch { epoch: "watch-one".into(), nonce: first.nonce.clone(), time_origin: first.time_origin });
        st.watches.insert(2, ControlWatch { epoch: "watch-two".into(), nonce: second.nonce.clone(), time_origin: second.time_origin });
        let mut loading = first.clone();
        loading.generation += 1;
        loading.confirmed = false;
        st.sync(&[loading.clone(), second.clone()]);
        assert!(st.watches.contains_key(&1), "same document withdrawal keeps observer");
        loading.confirmed = true;
        st.sync(&[loading.clone(), second.clone()]);
        assert!(st.watches.contains_key(&1), "restoration keeps observer");
        loading.generation += 1;
        loading.nonce = "replacement".into();
        st.sync(&[loading, second]);
        assert!(!st.watches.contains_key(&1), "new document invalidates observer");
        assert!(st.watches.contains_key(&2), "other tab unaffected");
        let mut conflict = st.tabs.get(&2).unwrap().clone();
        conflict.nonce = "conflicting-at-same-generation".into();
        let held = st.tabs.get(&2).unwrap().nonce.clone();
        st.sync(&[conflict]);
        assert_eq!(st.tabs.get(&2).unwrap().nonce, held, "identity guard keeps trusted tab");
        assert!(!st.watches.contains_key(&2), "but invalidates uncertain observer");
        st.revoke(2, "detached", None);
        assert!(!st.watches.contains_key(&2));
        assert!(!st.events.contains_key(&2));
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
        // A confirmed document with no nonce is not a record. It is pushed beside
        // tab 1 on purpose: the snapshot always lists every enabled tab, so tab 1
        // must stay held across this push rather than being dropped by it.
        let mut nameless = tab(2, 1, "https://example.com/b", 1.0);
        nameless.nonce = String::new();
        let notes = st.sync(&[loading.clone(), nameless]);
        assert!(notes.iter().any(|n| n.contains("no nonce")), "{notes:?}");
        assert!(!st.tabs.get(&1).unwrap().confirmed, "{notes:?}");
        // Restoring the same document at the same generation is not a change of
        // document: the probe's re-confirmation has to land, or a tab the extension
        // has already re-identified stays unconfirmed here for good.
        let mut restored = loading.clone();
        restored.confirmed = true;
        let notes = st.sync(&[restored.clone()]);
        assert!(notes.is_empty(), "{notes:?}");
        let held = st.tabs.get(&1).unwrap();
        assert!(held.confirmed);
        assert_eq!(held.nonce, restored.nonce);
        assert_eq!(held.generation, restored.generation);
    }

    #[test]
    fn a_second_document_at_one_generation_is_still_refused() {
        let mut st = State::default();
        st.sync(&[tab(1, 1, "https://example.com/a", 1.0)]);
        // Withdrawn, then a *different* document offered at the generation the
        // withdrawal took: one generation is one document, and the held (loading)
        // record stands rather than the record being replaced by a document nothing
        // identified.
        let mut loading = tab(1, 2, "https://example.com/a", 1.0);
        loading.confirmed = false;
        st.sync(&[loading]);
        let mut impostor = tab(1, 2, "https://example.com/a", 2.0);
        impostor.nonce = "a-nonce-from-somewhere-else".into();
        let notes = st.sync(&[impostor]);
        assert!(notes[0].contains("a second document"), "{notes:?}");
        let held = st.tabs.get(&1).unwrap();
        assert!(!held.confirmed);
        assert_eq!(held.nonce, "nonce-of-1-2");
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

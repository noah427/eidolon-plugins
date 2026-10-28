//! The coordination monitor's decisions, apart from its input and output.
//! Everything that decides anything lives here, and every input it decides on is
//! an argument, so the occurrence dedup, the notice lifecycle and the release
//! rules are testable without a harness, a roster or a send. `main.rs` is the
//! part that has to talk to the outside: the roster, the transport, the clock,
//! the files and the loopback endpoint.
//!
//! Four things this file deliberately does NOT do:
//!
//! - It never guesses an occurrence. A halt is announced once per *occurrence*,
//!   and the occurrence is the harness's `outcome_at` — the journal record that
//!   ended that turn. A count of records would be the wrong evidence (a peer's
//!   mail advances it while the same ending stands, turning one halt into a
//!   notice per poll), and two identical consecutive endings get different ids.
//!   `outcome_at` is null today: null means *not established*, so nothing is
//!   announced and the reason is said.
//! - An `errored` ending may have no journal record at all — only the driver that
//!   ran the turn saw it. For that case the harness publishes the registration's
//!   *count* of unjournaled endings (`outcome_observed`, one per ending, so two
//!   identical ones are 1 and then 2) together with the registration instance
//!   (`started_ms`), because a registration's *name* is reused across a restart.
//!   The occurrence key is that triple; a count of zero, or no instance token, is
//!   *not established* and nothing is announced.
//! - It never claims more than the transport said. The harness types the outcome
//!   (`delivered`/`queued`/`partial`/`failed`/`refused`), and the monitor stores
//!   the word it was given and the transport's own bytes beside it.
//! - It never resumes anything. The monitor's only effect on the world is one
//!   `send` per notice, to the coordinator, about a worker.
//!
//! The durable store — atomic and flushed writes, a refusal instead of guessing
//! when a file exists but cannot be read, and one writer per store — is in
//! [`store`]. It is passed in rather than reached for, so a failure to persist can
//! be injected exactly where it matters: before the send, and before the ack.

pub mod store;
pub mod managed_start;
pub mod process_host;
pub mod rank;

pub use rank::{RankAssignment, RankRequest};

use crate::store::SaveError;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// One row of the harness's structured session roster.
///
/// Unknown fields are ignored rather than refused: the roster is the harness's
/// to grow, and a field this monitor does not read is not an error. A field it
/// needs and does not get is a stated gap, never a default that looks like data.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Row {
    /// The registration locator. Ephemeral by the harness's own documentation —
    /// recorded for display with an adoption, and never used to key anything:
    /// one registration can halt twice without a journal settle, so it cannot
    /// tell two occurrences apart.
    #[serde(default)]
    pub id: String,
    /// The durable address. Opaque, and **null today**: the harness has not
    /// minted an id into a log, and null means *not established* — never
    /// "no such session". This monitor never canonicalises it, never derives
    /// anything from it, and never matches a name to find it.
    #[serde(default)]
    pub session: Option<String>,
    /// busy | idle | parked | stuck. Derived by the harness, not stored by it.
    #[serde(default)]
    pub state: String,
    /// The raw ending word: completed, iteration limit, errored, cancelled,
    /// unknown. Kept beside the state, never folded into it.
    #[serde(default)]
    pub last: String,
    /// The journal record that ended that turn — the occurrence token. Null
    /// until the harness populates it, and null is not "the same occurrence".
    #[serde(default)]
    pub outcome_at: Option<u64>,
    /// How many endings this registration *instance* has produced that the
    /// journal never saw, counted one by one — two identical `errored` endings
    /// are 1 and then 2, so each is its own occurrence.
    ///
    /// Three states, and they are not the same: absent/null is **not
    /// established** (a row written before this field existed, or a live
    /// registration that has not published an ending yet); `0` is **established
    /// and empty** (this instance's last ending came from a journaled settle);
    /// `>= 1` is an unjournaled ending. The count belongs to the registration
    /// instance rather than to the log, so a session adopted into the same
    /// process keeps counting — the pair with `started_ms` is what makes that
    /// unambiguous.
    ///
    /// It counts **any** ending the journal never saw — an errored turn, and also
    /// a cancellation that journaled no settle — so the count alone is not an
    /// error signal: `last` is what names the kind. An occurrence ordinal is the
    /// only thing this plugin reads it as.
    #[serde(default)]
    pub outcome_observed: Option<u64>,
    /// When this *registration instance* started, epoch ms. A registration's name
    /// is reused across a restart, so the count above means nothing without it.
    #[serde(default)]
    pub started_ms: Option<u64>,
    /// Whether the scan's doorbell probe answered. Liveness is not a state a
    /// session published, so it is reported beside the state and never as one.
    #[serde(default)]
    pub reached: bool,
    /// What the session calls itself — the raw label, exactly as the session
    /// wrote it. Never an address, and never the string this plugin edits.
    #[serde(default)]
    pub label: Option<String>,
    /// Presentation text a controller attached beside that label: who owns it and
    /// what it says. The harness holds it; this plugin is one of the controllers
    /// that may, which is why it is read here rather than assumed from the record.
    #[serde(default)]
    pub decoration: Option<Decoration>,
    /// The composed name the harness renders — decoration and label together. A
    /// fact about the screen, printed in answers so the record is never reported as
    /// if it were the display.
    #[serde(default)]
    pub display: Option<String>,
}

/// A decoration as the harness publishes it on a roster row.
///
/// `controller` is the owner string the harness stores — a claim, not an
/// authentication — and `text` is the presentation text. Nothing here says what the
/// text means: this plugin's vocabulary lives in [`rank`], and core has no opinion
/// about any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoration {
    #[serde(default)]
    pub controller: String,
    #[serde(default)]
    pub text: String,
}

/// One coordinator's explicit act about one worker.
///
/// The worker is the durable address; `worker_id_seen` is what the registration
/// was called at the time — evidence for a human, never a matching key. An
/// address that stops resolving is reported as an address that stopped
/// resolving, and is never silently rebound to whatever now looks similar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adoption {
    /// Which adoption this is. Re-adopting a released worker starts a new epoch;
    /// the old one is history and stays history.
    pub epoch: u64,
    #[serde(default)]
    pub coordinator: String,
    #[serde(default)]
    pub coordinator_id: String,
    pub worker: String,
    #[serde(default)]
    pub worker_id_seen: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: String,
    pub adopted_at_ms: u64,
    #[serde(default)]
    pub released_at_ms: Option<u64>,
    #[serde(default)]
    pub release_reason: String,
}

/// Where a notice has got to.
///
/// Four of these are the harness's own words, stored as given; `Pending` is a
/// notice written down and not yet handed over; `Cancelled` is a release taking
/// back what its epoch still owed. Nothing here means "the coordinator read it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pending,
    Delivered,
    Queued,
    Partial,
    Failed,
    Refused,
    Cancelled,
}

impl Outcome {
    pub fn word(self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Delivered => "delivered",
            Outcome::Queued => "queued",
            Outcome::Partial => "partial",
            Outcome::Failed => "failed",
            Outcome::Refused => "refused",
            Outcome::Cancelled => "cancelled",
        }
    }
}

/// What one attempt at the transport produced: the harness's typed outcome and
/// the bytes it printed, kept verbatim.
///
/// None of these words means the coordinator has *read* the notice. `queued` is
/// not consumption — the recipient's inbox goes with its registration, and a
/// registration can exit — and `delivered` says a door took it, not that a
/// person saw it. `partial` is anomalous here: this monitor sends to one direct
/// recipient at a time, so a partly-delivered send is reported as the anomaly it
/// is, never smoothed into success.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub outcome: Outcome,
    pub words: String,
}

/// How many times a *failed* notice is tried again. `failed` is the one outcome
/// that delivered nothing, so a retry cannot duplicate; `queued` is in the
/// recipient's inbox already and `delivered` has arrived, so neither is ever
/// sent again. The bound is the point: no endless waking.
pub const MAX_ATTEMPTS: u32 = 3;

/// One halt worth telling a coordinator about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    /// Stable for the occurrence it is about, and scoped to the recipient: the
    /// same halt announced to two coordinators is two notices with two ids. The
    /// same halt re-attempted after a crash keeps its id, which is what makes a
    /// duplicate recognisable.
    pub notice_id: String,
    pub epoch: u64,
    pub coordinator: String,
    pub worker: String,
    #[serde(default)]
    pub name: String,
    pub at_ms: u64,
    /// What kind of notice this is. One kind today.
    pub kind: String,
    pub state: String,
    pub last: String,
    /// The occurrence token this was keyed on, as published.
    pub occurrence: String,
    /// `outcome` (the journal record that ended the turn) or `observed` (the
    /// registration instance's own count of an ending the journal never saw).
    pub key_kind: String,
    pub attempts: u32,
    pub outcome: Outcome,
    /// The transport's own bytes, verbatim. Never a summary of them.
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub answered_at_ms: Option<u64>,
}

/// The whole durable state. One file, one writer: the service.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub records: Vec<Adoption>,
    #[serde(default)]
    pub notices: Vec<Notice>,
    #[serde(default)]
    pub starts: Vec<managed_start::StartRecord>,
    /// Explicit rank assignments, one per session, durable on purpose: a resume
    /// keeps its word, a fork does not silently change it, and a session that was
    /// never assigned one gets none. The harness knows nothing about this list.
    #[serde(default)]
    pub ranks: Vec<RankAssignment>,
}

/// The store and its poison, behind the one order that is safe.
///
/// The store's lock is taken **first** and the poison is read **inside** it: a caller
/// that waited for the lock must not act on a store another caller has just condemned.
/// Checking the poison first and locking afterwards leaves exactly that window — pass
/// the check, block on the lock, and then mutate or send against a store that was
/// declared unvouchable in between — which is why this is one type rather than two
/// fields and a convention.
pub struct Gate {
    store: Mutex<Store>,
    poisoned: Mutex<Option<String>>,
}

impl Gate {
    pub fn new(store: Store) -> Self {
        Gate {
            store: Mutex::new(store),
            poisoned: Mutex::new(None),
        }
    }

    /// Take the store's critical section, or refuse because this monitor can no
    /// longer be trusted. `Err` is the whole answer then: nothing may be read or
    /// written, and the caller is told to stop and start the service again.
    pub fn enter(&self) -> Result<std::sync::MutexGuard<'_, Store>, String> {
        let guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = self
            .poisoned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(format!(
                "this monitor cannot vouch for the records it holds ({p}). Stop it and start it again \
                 (`eidolon plugins service stop coordinator`, then start it): a restart reads the store \
                 and finds out what really landed"
            ));
        }
        Ok(guard)
    }

    /// Condemn the state. Called from **inside** [`Gate::enter`]'s critical section,
    /// so a caller waiting on that section sees the poison and refuses rather than a
    /// picture that has stopped being true.
    pub fn poison(&self, why: String) {
        *self.poisoned.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
    }
}

/// A stable id for an occurrence: epoch, worker and the token it was keyed on.
/// A stable id for one notice: the **recipient**, the epoch, the worker and the
/// token.
///
/// The coordinator is part of the identity, not decoration. Two coordinators may
/// adopt the same worker, and their epochs are each their own, so without the
/// recipient in the key both adoptions produce the *same* id for the same
/// occurrence — and the second coordinator's notice is then suppressed as a
/// duplicate of the first's, or cancelled by the first's release. Nothing about a
/// notice may cross recipient scope.
pub fn notice_id(coordinator: &str, epoch: u64, worker: &str, occurrence: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for part in [coordinator, &epoch.to_string(), worker, occurrence] {
        eat(0x1f);
        for b in part.bytes() {
            eat(b);
        }
    }
    format!("{h:016x}")
}

/// The words a notice carries. The plugin's own name is in them so the
/// coordinator can tell a monitor's notice from a peer's message, and nothing in
/// them claims the worker was touched.
pub fn notice_text(n: &Notice) -> String {
    let who = if n.name.is_empty() {
        n.worker.clone()
    } else {
        format!("{} ({})", n.name, n.worker)
    };
    let keyed = if n.key_kind == "observed" {
        format!(
            "an ending the journal never saw, occurrence `{}` (that registration instance's own \
             count, so a second one is a second notice)",
            n.occurrence
        )
    } else {
        format!("ending record `{}`", n.occurrence)
    };
    format!(
        "coordinator (monitor): {} halted — state `{}`, last outcome `{}`, {keyed}. \
         One notice per observed halt, adopted in epoch {}. Nothing was resumed or steered; \
         this is a report, and how to continue is yours.",
        who, n.state, n.last, n.epoch
    )
}

/// The occurrence token for a row, or why there is none.
///
/// `outcome_at` is the harness's evidence for a settled turn and the only one
/// accepted there. For an unjournaled ending the key is the **registration id,
/// the instance token and that instance's count** — three parts, because any one
/// alone collides. Null is *not established* and is not occurrence evidence.
///
/// The token is scoped by the caller: `observe` keys a notice on
/// `(coordinator, epoch, worker, token)` — the recipient first, because two
/// coordinators may adopt one worker and their epochs are each their own, so the
/// same token under two adoptions is two notices and neither can be suppressed or
/// cancelled by the other. `Err` carries the reason, in the scanner's own words.
fn occurrence_of(row: &Row) -> Result<(String, &'static str), String> {
    if let Some(n) = row.outcome_at {
        return Ok((format!("outcome:{n}"), "outcome"));
    }
    if row.last != "errored" {
        return Err(
            "stuck observed, but the roster publishes no `outcome_at` for it, so there is nothing \
             that says which halt this is — no notice, because a notice keyed on the ending word \
             alone would repeat on every scan"
                .to_string(),
        );
    }
    match (row.id.is_empty(), row.outcome_observed, row.started_ms) {
        // The key is the registration, the instance it belongs to and that
        // instance's own count — not the instance token alone, which is the same
        // for every registration that started in the same millisecond, and not
        // the count alone, which starts again with the next instance.
        (false, Some(n), Some(started)) if n > 0 => {
            Ok((format!("observed:{}:{started}:{n}", row.id), "observed"))
        }
        (true, _, _) => Err(
            "stuck with an `errored` ending and no registration id: the id is half of what names \
             the instance's count, so which halt this is cannot be said. No notice".to_string(),
        ),
        // The word and the count are written together, out of one projection, by
        // every driver — so a zero beside `errored` cannot come from this build.
        // That is an older writer's row (before the field existed, it arrives as
        // null rather than 0) or a hand-written one, and it is reported as
        // exactly that rather than guessed at.
        (_, Some(0), _) => Err(
            "stuck with an `errored` ending and `outcome_observed: 0`: the word and the count are \
             published together, so a zero beside `errored` cannot come from this build — the row \
             was written by an older one, or by hand. No notice, because which halt this is cannot \
             be said"
                .to_string(),
        ),
        (_, None, _) => Err(
            "stuck with an `errored` ending and no `outcome_observed` at all: the count is not \
             established — an older writer's row, or a registration that has not published an ending \
             yet — so which halt this is cannot be said. No notice"
                .to_string(),
        ),
        _ => Err(
            "stuck with an `errored` ending, a count of unjournaled endings, and no registration \
             instance token (`started_ms`): a registration's name is reused across a restart, so the \
             count alone does not name an occurrence. No notice"
                .to_string(),
        ),
    }
}

/// Watch the adopted workers on one scan and write the notices that halt owes.
///
/// Returns the notes explaining what it did and what it could not do. A note is
/// the honest half: a worker that is stuck with no occurrence evidence produces a
/// note and no notice, and a worker whose address no live row carries produces a
/// note that says so without calling it stuck.
pub fn observe(
    records: &[Adoption],
    notices: &mut Vec<Notice>,
    rows: &[Row],
    now_ms: u64,
) -> Vec<String> {
    let mut notes = Vec::new();
    let live = records.iter().filter(|r| r.released_at_ms.is_none()).count();
    if live > 0 && !rows.is_empty() && rows.iter().all(|r| r.session.is_none()) {
        // The harness's own documented state today. Said once, plainly, rather
        // than as one "no live session carries this address" per adoption, which
        // would read as if each worker had died.
        notes.push(
            "no row in the roster publishes a `session`: a session with no log has no id to name, \
             so an adoption cannot be resolved to one and nothing is announced. Null means not \
             established, not no-such-session"
                .to_string(),
        );
    }
    for rec in records.iter().filter(|r| r.released_at_ms.is_none()) {
        let claimants: Vec<&Row> = rows
            .iter()
            .filter(|r| r.session.as_deref() == Some(rec.worker.as_str()))
            .collect();
        let row = match claimants.len() {
            0 => {
                notes.push(format!(
                    "{}: no live session carries this address (reached is a liveness fact, not a state: nothing was announced)",
                    rec.worker
                ));
                continue;
            }
            1 => claimants[0],
            n => {
                notes.push(format!(
                    "{}: {} live sessions claim this one address; refusing to guess which, so nothing was announced",
                    rec.worker, n
                ));
                continue;
            }
        };
        if !row.reached {
            notes.push(format!(
                "{}: listed but its doorbell did not answer; liveness is reported, not announced",
                rec.worker
            ));
        }
        if row.state != "stuck" {
            continue;
        }
        let (token, key_kind) = match occurrence_of(row) {
            Ok(t) => t,
            Err(why) => {
                notes.push(format!("{}: {why}", rec.worker));
                continue;
            }
        };
        if rec.coordinator == rec.worker {
            // Cannot happen through the plugin's door (adopt refuses self), and
            // is not announced if a hand-written record ever says it: a session
            // is not woken to be told about itself.
            notes.push(format!(
                "{}: its own coordinator; not announcing a session's halt to itself",
                rec.worker
            ));
            continue;
        }
        let id = notice_id(&rec.coordinator, rec.epoch, &rec.worker, &token);
        if notices.iter().any(|n| n.notice_id == id) {
            continue;
        }
        notices.push(Notice {
            notice_id: id,
            epoch: rec.epoch,
            coordinator: rec.coordinator.clone(),
            worker: rec.worker.clone(),
            name: rec.name.clone(),
            at_ms: now_ms,
            kind: "stuck".to_string(),
            state: row.state.clone(),
            last: row.last.clone(),
            occurrence: token,
            key_kind: key_kind.to_string(),
            attempts: 0,
            outcome: Outcome::Pending,
            transport: String::new(),
            answered_at_ms: None,
        });
        notes.push(if key_kind == "observed" {
            format!(
                "{}: halted with an ending the journal never saw; one notice, keyed on its registration instance and count",
                rec.worker
            )
        } else {
            format!("{}: halted; one notice written", rec.worker)
        });
    }
    notes
}

/// The notices the transport still has to be given: written down and never
/// handed over, plus a *failed* notice inside its attempt bound. Nothing else is
/// ever attempted — a notice that arrived, was queued, or was partly delivered is
/// not sent again, which is what keeps a coordinator from being woken repeatedly.
pub fn to_attempt(notices: &[Notice]) -> Vec<String> {
    notices
        .iter()
        .filter(|n| match n.outcome {
            Outcome::Pending => true,
            Outcome::Failed => n.attempts < MAX_ATTEMPTS,
            _ => false,
        })
        .map(|n| n.notice_id.clone())
        .collect()
}

/// Record what the transport did with one notice.
///
/// The outcome is the harness's, stored as given; `Err` means the transport
/// could not be run at all, which is `Refused` with the bytes that say so.
pub fn record_attempt(
    notices: &mut [Notice],
    id: &str,
    answer: Result<Delivery, String>,
    now_ms: u64,
) {
    let Some(n) = notices.iter_mut().find(|n| n.notice_id == id) else {
        return;
    };
    match answer {
        Ok(d) => {
            n.outcome = d.outcome;
            n.transport = d.words;
        }
        Err(e) => {
            n.outcome = Outcome::Refused;
            n.transport = e;
        }
    }
    n.attempts += 1;
    n.answered_at_ms = Some(now_ms);
}

/// End one adoption, and cancel what it still owes.
///
/// The epoch is closed rather than deleted: the record stays, with the reason,
/// and re-adopting the same worker later is a new epoch. Notices already
/// answered keep their answer — a release cannot unwrite what a coordinator has
/// read — and notices still pending for this epoch become `Cancelled` rather
/// than being sent.
pub fn release(
    records: &mut [Adoption],
    notices: &mut [Notice],
    coordinator: &str,
    worker: &str,
    reason: &str,
    now_ms: u64,
) -> Result<String, String> {
    let Some(rec) = records
        .iter_mut()
        .find(|r| r.coordinator == coordinator && r.worker == worker && r.released_at_ms.is_none())
    else {
        return Err(format!(
            "{worker} is not adopted by this session (or was already released); `coordinator_adoptions` has the records this session holds"
        ));
    };
    rec.released_at_ms = Some(now_ms);
    rec.release_reason = reason.to_string();
    let epoch = rec.epoch;
    let mut cancelled = 0;
    for n in notices
        .iter_mut()
        .filter(|n| {
            n.coordinator == coordinator
                && n.worker == worker
                && n.epoch == epoch
                && n.outcome == Outcome::Pending
        })
    {
        n.outcome = Outcome::Cancelled;
        cancelled += 1;
    }
    Ok(format!(
        "released {worker} (epoch {epoch}); monitoring ends now, {cancelled} unsent notice(s) cancelled. \
         A notice already answered is not retracted, and the record stays as history."
    ))
}

/// The epoch a new adoption of this worker by this coordinator would take. An
/// epoch is one adoption; re-adopting after a release is the next one.
pub fn next_epoch(records: &[Adoption], coordinator: &str, worker: &str) -> u64 {
    records
        .iter()
        .filter(|r| r.coordinator == coordinator && r.worker == worker)
        .map(|r| r.epoch)
        .max()
        .unwrap_or(0)
        + 1
}

/// Whether the roster can resolve an adoption at all: does any row carry a
/// durable address? Used by the doors so that an inventory says the harness's
/// state rather than reporting every worker as gone.
pub fn addresses_established(rows: &[Row]) -> bool {
    rows.iter().any(|r| r.session.as_deref().is_some_and(|s| !s.is_empty()))
}

/// Where the roster of live sessions comes from.
pub trait Roster: Send + Sync {
    fn rows(&self) -> Result<Vec<Row>, String>;
}

/// Where a notice goes: the one effect this plugin has on the world.
pub trait Transport: Send + Sync {
    fn send(&self, to: &str, text: &str) -> Result<Delivery, String>;
}

/// What one scan did, and whether it was cut short.
#[derive(Debug, Default)]
pub struct Scan {
    pub notes: Vec<String>,
    /// Set when the store could not be written. **That is fatal**, not a note: the
    /// in-memory state may no longer match the disk, so the caller must stop the
    /// process rather than keep serving a picture it cannot vouch for.
    pub error: Option<String>,
}

/// One scan, in the order that matters.
///
/// The order is the whole point, and it is written down before it is sent: what
/// this scan writes into `notices` is persisted **before** the transport is given
/// anything, and the answer is persisted **after** the attempt. So
///
/// - a store that cannot be written **stops the process** — a halt that cannot be
///   recorded must not be delivered, and a monitor whose state may contradict the
///   disk must not keep serving;
/// - a crash between the send and the acknowledgement leaves the notice pending on
///   disk under the **same** `notice_id` (the answer was never written), and a
///   restarted process attempts that id again rather than inventing a second
///   notice: at-least-once, and the duplicate is recognisable rather than denied.
///
/// `save` is a parameter rather than a path so the crash window can be injected in
/// a test through the real ordering, not mocked around it.
pub fn cycle(
    store: &mut Store,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    roster: &dyn Roster,
    transport: &dyn Transport,
    now: u64,
) -> Scan {
    cycle_with_rows(store, save, roster.rows(), transport, now)
}

/// The same scan with the roster **already read**, for a caller that needs the same
/// rows for something else.
///
/// The monitor reads the roster once per tick and uses it twice — for halts and for
/// decoration — and two reads would be two moments, so a row could be watched for a
/// halt under one registration and decorated under the next. Passing the rows in
/// keeps one observation of the swarm per tick, which is the whole of what this
/// signature buys.
pub fn cycle_with_rows(
    store: &mut Store,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    rows: Result<Vec<Row>, String>,
    transport: &dyn Transport,
    now: u64,
) -> Scan {
    let mut scan = Scan::default();
    match rows {
        Ok(rows) => scan
            .notes
            .extend(observe(&store.records, &mut store.notices, &rows, now)),
        Err(e) => scan.notes.push(format!(
            "the roster could not be read, so nothing was observed and no notice was written: {e}"
        )),
    }

    // Write-ahead: nothing is sent until what it is about is on the disk.
    if let Err(e) = save(store) {
        scan.error = Some(if e.uncertain() {
            format!(
                "the notices this scan wrote may or may not be on the disk ({e}). Nothing was handed \
                 to the transport, and this process's copy is no longer evidence either way: the file \
                 may already hold them"
            )
        } else {
            format!(
                "the notices this scan wrote could not be persisted, so nothing was handed to the \
                 transport ({e}), and this process's copy of the store is no longer evidence"
            )
        });
        return scan;
    }

    for id in to_attempt(&store.notices) {
        let (to, text) = match store.notices.iter().find(|n| n.notice_id == id) {
            Some(n) => (n.coordinator.clone(), notice_text(n)),
            None => continue,
        };
        let answer = transport.send(&to, &text);
        let words = match &answer {
            Ok(d) => format!("the harness reported `{}`: {}", d.outcome.word(), d.words),
            Err(e) => format!("the transport would not take it: {e}"),
        };
        let partial = matches!(&answer, Ok(d) if d.outcome == Outcome::Partial);
        record_attempt(&mut store.notices, &id, answer, now);
        scan.notes.push(format!("notice {id} to {to}: {words}"));
        if partial {
            scan.notes.push(format!(
                "notice {id}: `partial` for one direct recipient is an anomaly, not success — the \
                 monitor sends to one coordinator at a time, so a partly-delivered send is reported \
                 rather than smoothed over"
            ));
        }
        // The acknowledgement, durably. If this write fails the process stops: the
        // in-memory notice already carries the answer, so a "next scan" would
        // persist that and never retry — which is why the claim this used to make
        // ("it stays pending on disk") was false in the live process. A restart
        // reads the file, which holds whatever really landed.
        if let Err(e) = save(store) {
            scan.error = Some(format!(
                "the answer to notice {id} could not be persisted ({e}). The file may hold that notice \
                 as answered or as pending, and this process's copy of it is not evidence either way — \
                 so this monitor stops rather than keep serving it"
            ));
            return scan;
        }
    }
    scan
}

/// Adopt a worker, or refuse — and only after the record is durable.
///
/// The store is cloned, the clone is written, and the live store moves only once
/// the write succeeded. So an adoption that could not be persisted is not an
/// adoption: the caller is told it failed and the in-memory state still matches
/// the disk.
pub struct AdoptRequest<'a> {
    pub caller: &'a str,
    pub worker: &'a str,
    pub name: &'a str,
    pub note: &'a str,
    /// What the roster said about the worker when this was called, if anything.
    pub seen: Option<&'a Row>,
    pub now: u64,
}

/// What a caller must do when a write may or may not have landed.
///
/// An uncertain write is not a rollback: the file probably already holds the new
/// content. So the live state is taken **from the disk** and the caller is told the
/// outcome is unknown. If the disk cannot be read either, the process is told to
/// stop — there is no state left that can be vouched for.
fn reconcile(
    store: &mut Store,
    load: &mut dyn FnMut() -> Result<Store, String>,
    e: SaveError,
) -> Result<String, String> {
    match load() {
        Ok(reloaded) => {
            *store = reloaded;
            Err(format!(
                "{e}. The store has been re-read, so the live state now comes from the file rather \
                 than from the write that failed — this call's outcome is unknown, and \
                 `coordinator_adoptions` is what to read before assuming either way"
            ))
        }
        Err(le) => Err(format!(
            "{e}, and the store could not be re-read either ({le}) — there is no state this monitor \
             can vouch for. It has to be stopped and started again (`eidolon plugins service stop \
             coordinator`, then start it) before it serves anything"
        )),
    }
}

pub fn adopt(
    store: &mut Store,
    req: AdoptRequest<'_>,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
) -> Result<String, String> {
    let AdoptRequest {
        caller,
        worker,
        name,
        note,
        seen,
        now,
    } = req;
    if worker.is_empty() {
        return Err("`adopt` needs `session`: the worker's durable address, echoed verbatim".into());
    }
    if worker == caller {
        return Err(format!("a session cannot adopt itself ({worker})"));
    }
    if let Some(r) = store
        .records
        .iter()
        .find(|r| r.coordinator == caller && r.worker == worker && r.released_at_ms.is_none())
    {
        return Err(format!(
            "{worker} is already adopted here, in epoch {} and still monitored; `coordinator_release` \
             ends it first (a re-adoption is a new epoch, not a second live one)",
            r.epoch
        ));
    }
    let epoch = next_epoch(&store.records, caller, worker);
    let mut next = store.clone();
    next.records.push(Adoption {
        epoch,
        coordinator: caller.to_string(),
        coordinator_id: String::new(),
        worker: worker.to_string(),
        worker_id_seen: seen.map(|r| r.id.clone()).unwrap_or_default(),
        name: name.to_string(),
        note: note.to_string(),
        adopted_at_ms: now,
        released_at_ms: None,
        release_reason: String::new(),
    });
    match save(&next) {
        Ok(()) => *store = next,
        Err(e) if e.uncertain() => return reconcile(store, load, e),
        Err(e) => {
            return Err(format!(
                "the adoption was not written down, so it did not happen: {e}"
            ))
        }
    }
    Ok(match seen {
        Some(r) => format!(
            "adopted {worker} in epoch {epoch} ({}), state `{}`, last `{}`. It is monitored from now on: one notice per observed halt, to this session.",
            if r.id.is_empty() {
                "no registration id published".to_string()
            } else {
                format!("id `{}`", r.id)
            },
            r.state,
            r.last
        ),
        None => format!(
            "adopted {worker} in epoch {epoch}. No live session carries that address right now — the \
             record is kept, and the address is never rebound to whatever looks similar: if the \
             harness's spelling has changed, the adoption has to be released and made again."
        ),
    })
}

/// Release an adoption, or refuse — and only after the release is durable.
pub fn release_adoption(
    store: &mut Store,
    caller: &str,
    worker: &str,
    reason: &str,
    now: u64,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
) -> Result<String, String> {
    if store.starts.iter().any(|start| {
        start.request.caller == caller && start.session.as_deref() == Some(worker)
            && start.cancellation.as_ref().is_some_and(|c| c.outcome == managed_start::CancelOutcome::Attempted)
            && store.records.iter().any(|r| r.coordinator == caller && r.worker == worker
                && Some(r.epoch) == start.adoption_epoch && r.released_at_ms.is_none())
    }) {
        return Err("cancellation exit is unconfirmed; supervision must remain until exit is durably observed".into());
    }
    let mut next = store.clone();
    let out = release(&mut next.records, &mut next.notices, caller, worker, reason, now)?;
    match save(&next) {
        Ok(()) => *store = next,
        Err(e) if e.uncertain() => return reconcile(store, load, e),
        Err(e) => {
            return Err(format!(
                "the release was not written down, so it did not happen: {e}"
            ))
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: Option<&str>, state: &str, last: &str, outcome_at: Option<u64>) -> Row {
        Row {
            id: "eidolon-0001".to_string(),
            session: session.map(str::to_string),
            state: state.to_string(),
            last: last.to_string(),
            outcome_at,
            outcome_observed: None,
            started_ms: None,
            reached: true,
            label: None,
            decoration: None,
            display: None,
        }
    }

    /// A row for an ending the journal never saw: the count the harness keeps for
    /// that registration instance, and the instance token it belongs to.
    fn unjournaled(worker: &str, count: u64) -> Row {
        Row {
            id: "eidolon-0001".to_string(),
            session: Some(worker.to_string()),
            state: "stuck".to_string(),
            last: "errored".to_string(),
            outcome_at: None,
            outcome_observed: Some(count),
            started_ms: Some(1_764_000_000_000),
            reached: true,
            label: None,
            decoration: None,
            display: None,
        }
    }

    fn adopted(worker: &str) -> Adoption {
        Adoption {
            epoch: 1,
            coordinator: "/home/x/sessions/coord.eid".to_string(),
            coordinator_id: "eidolon-aaaa".to_string(),
            worker: worker.to_string(),
            worker_id_seen: "eidolon-bbbb".to_string(),
            name: "grinder".to_string(),
            note: String::new(),
            adopted_at_ms: 1,
            released_at_ms: None,
            release_reason: String::new(),
        }
    }

    fn stuck(worker: &str, token: Option<u64>) -> Row {
        row(Some(worker), "stuck", "iteration limit", token)
    }

    fn delivery(word: &str) -> Delivery {
        Delivery {
            outcome: match word {
                "delivered" => Outcome::Delivered,
                "queued" => Outcome::Queued,
                "partial" => Outcome::Partial,
                "failed" => Outcome::Failed,
                other => panic!("not an outcome word: {other}"),
            },
            words: format!("{{\"outcome\":\"{word}\"}}"),
        }
    }

    #[test]
    fn the_gate_serves_until_it_is_poisoned() {
        let gate = Gate::new(Store::default());
        assert!(gate.enter().is_ok());
        gate.poison("simulated".to_string());
        let e = gate.enter().unwrap_err();
        assert!(e.contains("cannot vouch"), "{e}");
    }

    #[test]
    fn a_caller_queued_behind_the_store_is_refused_rather_than_acting() {
        // The poison is set *inside* the store's critical section, so a caller that
        // waited for that section must see it and refuse. This pins the invariant the
        // order exists for; the structural half — one entry point, so there is no
        // pre-check left to race — is that `Gate` is the only way to the store.
        use std::sync::{Arc, Barrier};
        let gate = Arc::new(Gate::new(Store::default()));
        let barrier = Arc::new(Barrier::new(2));
        let holder = {
            let gate = gate.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let _held = gate.enter().expect("the first caller enters");
                barrier.wait();
                // Condemned while still holding the critical section.
                gate.poison("simulated: an uncertain write could not be reconciled".to_string());
                barrier.wait();
            })
        };
        barrier.wait();
        barrier.wait();
        let second = gate.enter();
        assert!(second.is_err(), "a caller that waited must be told, not served");
        assert!(second.unwrap_err().contains("cannot vouch"));
        holder.join().unwrap();
    }

    #[test]
    fn one_notice_per_occurrence_and_none_for_a_repeat() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let rows = vec![stuck("/w.eid", Some(7))];
        let notes = observe(&recs, &mut notices, &rows, 100);
        assert_eq!(notices.len(), 1, "a halt is announced once: {notes:?}");
        assert_eq!(notices[0].occurrence, "outcome:7");
        assert_eq!(notices[0].outcome, Outcome::Pending);
        observe(&recs, &mut notices, &rows, 200);
        assert_eq!(notices.len(), 1, "the same occurrence is not announced twice");
    }

    #[test]
    fn a_count_of_records_would_have_been_the_wrong_evidence() {
        // Two halts that end with the same word get different ending records.
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        observe(&recs, &mut notices, &[stuck("/w.eid", Some(7))], 100);
        observe(&recs, &mut notices, &[stuck("/w.eid", Some(9))], 200);
        assert_eq!(notices.len(), 2, "two halts are two halts");
        assert_ne!(notices[0].notice_id, notices[1].notice_id);
    }

    #[test]
    fn no_outcome_record_means_no_notice_and_a_reason() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let rows = vec![row(Some("/w.eid"), "stuck", "iteration limit", None)];
        let notes = observe(&recs, &mut notices, &rows, 100);
        assert!(notices.is_empty(), "nothing is guessed");
        assert!(
            notes.iter().any(|n| n.contains("no `outcome_at`")),
            "and the reason is said: {notes:?}"
        );
    }

    #[test]
    fn an_unjournaled_ending_is_its_own_occurrence_by_its_count() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        observe(&recs, &mut notices, &[unjournaled("/w.eid", 1)], 100);
        assert_eq!(notices.len(), 1, "the first unjournaled ending is announced");
        assert_eq!(notices[0].key_kind, "observed");
        assert_eq!(notices[0].occurrence, "observed:eidolon-0001:1764000000000:1");
        // The same count again is the same occurrence; the next ending is not.
        observe(&recs, &mut notices, &[unjournaled("/w.eid", 1)], 200);
        assert_eq!(notices.len(), 1, "the same count is not announced twice");
        observe(&recs, &mut notices, &[unjournaled("/w.eid", 2)], 300);
        assert_eq!(notices.len(), 2, "a second identical errored ending is a second halt");
        let text = notice_text(&notices[1]);
        assert!(text.contains("a second one is a second notice"), "{text}");
    }

    #[test]
    fn a_zero_count_beside_an_errored_ending_is_an_older_writers_row() {
        // This build counts every errored ending, so a zero beside one cannot
        // have come from it. Reported as that, not guessed at.
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let notes = observe(&recs, &mut notices, &[unjournaled("/w.eid", 0)], 100);
        assert!(notices.is_empty());
        assert!(
            notes.iter().any(|n| n.contains("cannot come from this build")),
            "the reason is said: {notes:?}"
        );
    }

    #[test]
    fn a_missing_count_is_not_established() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let mut r = unjournaled("/w.eid", 1);
        r.outcome_observed = None;
        let notes = observe(&recs, &mut notices, &[r], 100);
        assert!(notices.is_empty());
        assert!(
            notes.iter().any(|n| n.contains("not established")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_unjournaled_ending_with_no_instance_token_is_not_announced() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let mut r = unjournaled("/w.eid", 1);
        r.started_ms = None;
        let notes = observe(&recs, &mut notices, &[r], 100);
        assert!(notices.is_empty(), "a count without an instance token is not evidence");
        assert!(
            notes.iter().any(|n| n.contains("no registration instance token")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_missing_worker_is_a_liveness_fact_not_a_halt() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let notes = observe(&recs, &mut notices, &[], 100);
        assert!(notices.is_empty());
        assert!(notes.iter().any(|n| n.contains("no live session")), "{notes:?}");
    }

    #[test]
    fn a_roster_with_no_addresses_says_that_once_not_once_per_adoption() {
        let recs = vec![adopted("/a.eid"), adopted("/b.eid")];
        let mut notices = Vec::new();
        let rows = vec![
            row(None, "idle", "completed", None),
            row(None, "busy", "completed", None),
        ];
        let notes = observe(&recs, &mut notices, &rows, 100);
        assert!(notices.is_empty());
        assert_eq!(
            notes.iter().filter(|n| n.contains("no row in the roster publishes")).count(),
            1,
            "said once, plainly: {notes:?}"
        );
        assert!(!addresses_established(&rows));
    }

    #[test]
    fn two_claimants_are_refused_rather_than_picked_between() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let rows = vec![stuck("/w.eid", Some(1)), row(Some("/w.eid"), "idle", "completed", Some(2))];
        let notes = observe(&recs, &mut notices, &rows, 100);
        assert!(notices.is_empty(), "no coin is flipped");
        assert!(notes.iter().any(|n| n.contains("refusing to guess")), "{notes:?}");
    }

    #[test]
    fn a_busy_worker_is_not_a_halt() {
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        observe(&recs, &mut notices, &[row(Some("/w.eid"), "busy", "completed", Some(3))], 100);
        assert!(notices.is_empty());
    }

    #[test]
    fn the_harnesss_own_outcome_word_is_stored_and_never_upgraded() {
        let mut notices = vec![];
        observe(&[adopted("/w.eid")], &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let id = notices[0].notice_id.clone();
        record_attempt(&mut notices, &id, Ok(delivery("queued")), 200);
        assert_eq!(notices[0].outcome, Outcome::Queued);
        assert_eq!(notices[0].attempts, 1);
        assert_eq!(notices[0].transport, "{\"outcome\":\"queued\"}");
        assert!(to_attempt(&notices).is_empty(), "a queued notice is not sent again");

        let mut notices = vec![];
        observe(&[adopted("/w.eid")], &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let id = notices[0].notice_id.clone();
        record_attempt(&mut notices, &id, Ok(delivery("delivered")), 200);
        assert_eq!(notices[0].outcome, Outcome::Delivered);
        assert!(to_attempt(&notices).is_empty());
    }

    #[test]
    fn a_failure_is_retried_to_a_bound_and_then_left_visible() {
        let mut notices = vec![];
        observe(&[adopted("/w.eid")], &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let id = notices[0].notice_id.clone();
        for _ in 0..MAX_ATTEMPTS {
            assert!(to_attempt(&notices).contains(&id), "within the bound it is tried");
            record_attempt(&mut notices, &id, Ok(delivery("failed")), 200);
        }
        assert_eq!(notices[0].outcome, Outcome::Failed);
        assert_eq!(notices[0].attempts, MAX_ATTEMPTS);
        assert!(
            to_attempt(&notices).is_empty(),
            "the bound is the point: no endless waking"
        );
    }

    #[test]
    fn a_transport_that_would_not_run_is_refused_with_its_own_bytes() {
        let mut notices = vec![];
        observe(&[adopted("/w.eid")], &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let id = notices[0].notice_id.clone();
        record_attempt(&mut notices, &id, Err("no session `x`".to_string()), 200);
        assert_eq!(notices[0].outcome, Outcome::Refused);
        assert_eq!(notices[0].transport, "no session `x`");
        assert!(to_attempt(&notices).is_empty(), "a refusal is not retried by itself");
    }

    #[test]
    fn a_crash_between_the_write_and_the_send_repeats_the_same_id() {
        let recs = vec![adopted("/w.eid")];
        let rows = vec![stuck("/w.eid", Some(1))];
        let mut notices = vec![];
        observe(&recs, &mut notices, &rows, 100);
        let id = notices[0].notice_id.clone();
        observe(&recs, &mut notices, &rows, 200);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].notice_id, id);
        assert_eq!(to_attempt(&notices), vec![id], "it is attempted, at least once");
    }

    #[test]
    fn release_ends_the_epoch_and_cancels_what_it_still_owes() {
        let mut records = vec![adopted("/w.eid")];
        let mut notices = vec![];
        observe(&records, &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let out = release(
            &mut records,
            &mut notices,
            "/home/x/sessions/coord.eid",
            "/w.eid",
            "task over",
            300,
        )
        .unwrap();
        assert!(out.contains("monitoring ends now"), "{out}");
        assert_eq!(records[0].released_at_ms, Some(300));
        assert_eq!(records[0].release_reason, "task over");
        assert_eq!(notices[0].outcome, Outcome::Cancelled);
        assert!(to_attempt(&notices).is_empty(), "nothing is sent after release");
        let notes = observe(&records, &mut notices, &[stuck("/w.eid", Some(2))], 400);
        assert_eq!(notices.len(), 1, "a released worker is not watched");
        assert!(notes.is_empty());
    }

    #[test]
    fn a_release_cannot_unwrite_an_answer_already_given() {
        let mut records = vec![adopted("/w.eid")];
        let mut notices = vec![];
        observe(&records, &mut notices, &[stuck("/w.eid", Some(1))], 100);
        let id = notices[0].notice_id.clone();
        record_attempt(&mut notices, &id, Ok(delivery("queued")), 150);
        release(
            &mut records,
            &mut notices,
            "/home/x/sessions/coord.eid",
            "/w.eid",
            "",
            300,
        )
        .unwrap();
        assert_eq!(notices[0].outcome, Outcome::Queued);
        assert_eq!(notices[0].answered_at_ms, Some(150));
    }

    #[test]
    fn re_adoption_is_a_new_epoch_not_a_revival() {
        let mut records = vec![adopted("/w.eid")];
        let mut notices = vec![];
        release(
            &mut records,
            &mut notices,
            "/home/x/sessions/coord.eid",
            "/w.eid",
            "",
            300,
        )
        .unwrap();
        assert_eq!(next_epoch(&records, "/home/x/sessions/coord.eid", "/w.eid"), 2);
        assert_eq!(
            next_epoch(&records, "/home/x/sessions/coord.eid", "/elsewhere.eid"),
            1
        );
        assert_eq!(
            next_epoch(&records, "/home/x/sessions/other.eid", "/w.eid"),
            1,
            "another coordinator's epochs are its own"
        );
    }

    #[test]
    fn releasing_what_was_never_adopted_is_refused() {
        let mut records = vec![adopted("/w.eid")];
        let mut notices = vec![];
        let e = release(
            &mut records,
            &mut notices,
            "/home/x/sessions/coord.eid",
            "/other.eid",
            "",
            300,
        )
        .unwrap_err();
        assert!(e.contains("not adopted by this session"), "{e}");
    }

    fn adopted_by(coordinator: &str, worker: &str) -> Adoption {
        Adoption {
            epoch: 1,
            coordinator: coordinator.to_string(),
            coordinator_id: String::new(),
            worker: worker.to_string(),
            worker_id_seen: String::new(),
            name: String::new(),
            note: String::new(),
            adopted_at_ms: 1,
            released_at_ms: None,
            release_reason: String::new(),
        }
    }

    #[test]
    fn two_coordinators_adopting_one_worker_each_get_their_own_notice() {
        // The defect this guards: with the recipient left out of the notice id, the
        // second coordinator's notice was suppressed as a duplicate of the first's —
        // one halt, two adoptions, and only one of them ever told.
        let recs = vec![
            adopted_by("/c-one.eid", "/w.eid"),
            adopted_by("/c-two.eid", "/w.eid"),
        ];
        let mut notices = Vec::new();
        let rows = vec![stuck("/w.eid", Some(7))];
        observe(&recs, &mut notices, &rows, 100);
        assert_eq!(notices.len(), 2, "one halt, two recipients, two notices");
        assert_ne!(notices[0].notice_id, notices[1].notice_id, "distinct ids");
        let recipients: Vec<&str> = notices.iter().map(|n| n.coordinator.as_str()).collect();
        assert!(
            recipients.contains(&"/c-one.eid") && recipients.contains(&"/c-two.eid"),
            "{recipients:?}"
        );
        assert!(notices.iter().all(|n| n.occurrence == "outcome:7"));
        observe(&recs, &mut notices, &rows, 200);
        assert_eq!(notices.len(), 2, "and a repeat scan adds nothing");
    }

    #[test]
    fn bookkeeping_cannot_cross_recipient_scope() {
        let recs = vec![
            adopted_by("/c-one.eid", "/w.eid"),
            adopted_by("/c-two.eid", "/w.eid"),
        ];
        let mut notices = Vec::new();
        observe(&recs, &mut notices, &[stuck("/w.eid", Some(7))], 100);
        let ids = to_attempt(&notices);
        assert_eq!(ids.len(), 2);
        for id in &ids {
            let n = notices.iter().find(|n| &n.notice_id == id).expect("by id");
            assert!(
                n.coordinator == "/c-one.eid" || n.coordinator == "/c-two.eid",
                "an id reaches exactly the notice it names"
            );
        }
        // One answered, the other still owed: the answer cannot land on the wrong one.
        record_attempt(&mut notices, &ids[0], Ok(delivery("delivered")), 150);
        assert_eq!(
            notices.iter().filter(|n| n.outcome == Outcome::Pending).count(),
            1
        );
        assert_eq!(to_attempt(&notices), vec![ids[1].clone()]);
    }

    #[test]
    fn releasing_one_coordinator_leaves_the_others_notice_alone() {
        let mut recs = vec![
            adopted_by("/c-one.eid", "/w.eid"),
            adopted_by("/c-two.eid", "/w.eid"),
        ];
        let mut notices = Vec::new();
        observe(&recs, &mut notices, &[stuck("/w.eid", Some(7))], 100);
        assert_eq!(notices.len(), 2);
        release(&mut recs, &mut notices, "/c-one.eid", "/w.eid", "done", 300).unwrap();
        let by = |c: &str| {
            notices
                .iter()
                .find(|n| n.coordinator == c)
                .expect("a notice for that coordinator")
        };
        assert_eq!(by("/c-one.eid").outcome, Outcome::Cancelled, "A's is cancelled");
        assert_eq!(by("/c-two.eid").outcome, Outcome::Pending, "B's is untouched");
        let owed = to_attempt(&notices);
        assert_eq!(owed.len(), 1, "B is still owed its notice");
        assert_eq!(owed[0], by("/c-two.eid").notice_id);
        // And B's record is still live, so A's release did not end B's adoption.
        assert!(recs
            .iter()
            .any(|r| r.coordinator == "/c-two.eid" && r.released_at_ms.is_none()));
    }

    #[test]
    fn a_session_is_not_woken_to_be_told_about_itself() {
        let mut rec = adopted("/w.eid");
        rec.coordinator = "/w.eid".to_string();
        let mut notices = vec![];
        let notes = observe(&[rec], &mut notices, &[stuck("/w.eid", Some(1))], 100);
        assert!(notices.is_empty());
        assert!(
            notes.iter().any(|n| n.contains("not announcing a session's halt to itself")),
            "{notes:?}"
        );
    }

    #[test]
    fn the_notice_says_what_it_is_and_what_it_did_not_do() {
        let n = Notice {
            notice_id: notice_id("/c.eid", 1, "/w.eid", "outcome:7"),
            epoch: 1,
            coordinator: "/c.eid".to_string(),
            worker: "/w.eid".to_string(),
            name: "grinder".to_string(),
            at_ms: 0,
            kind: "stuck".to_string(),
            state: "stuck".to_string(),
            last: "iteration limit".to_string(),
            occurrence: "outcome:7".to_string(),
            key_kind: "outcome".to_string(),
            attempts: 0,
            outcome: Outcome::Pending,
            transport: String::new(),
            answered_at_ms: None,
        };
        let t = notice_text(&n);
        assert!(t.contains("grinder (/w.eid)"), "{t}");
        assert!(t.contains("iteration limit"), "{t}");
        assert!(t.contains("outcome:7"), "{t}");
        assert!(t.contains("Nothing was resumed"), "{t}");
    }

    #[test]
    fn a_row_that_is_not_this_monitors_business_is_ignored() {
        // The roster carries every live session; only adopted ones are watched.
        let recs = vec![adopted("/w.eid")];
        let mut notices = Vec::new();
        let rows = vec![stuck("/somebody-else.eid", Some(1))];
        let notes = observe(&recs, &mut notices, &rows, 100);
        assert!(notices.is_empty());
        assert!(notes.iter().any(|n| n.contains("no live session")), "{notes:?}");
    }
}

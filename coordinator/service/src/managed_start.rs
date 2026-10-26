//! Managed-start ordering, independent of a process launcher or public tool.
//! The caller must hold the store's Gate and have admitted the exact request
//! through the harness dispatcher. This module grants no permission. No service
//! endpoint may bypass these contracts: the process Host enforces the pre-turn hold.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::{Adoption, Outcome, Store, next_epoch, reconcile, store::SaveError};

/// Immutable admitted launch input. `caller` is harness-derived, never model input.
/// An operation id identifies an attempt, not an address a peer resolver accepts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub operation: String,
    pub caller: String,
    pub workspace: String,
    pub model: String,
    pub persona: Option<String>,
    pub brief: String,
    /// The rank whose block is appended beside the persona at spawn. Only the
    /// **name**: the addition's content is bound by `rank_digest` below, which the
    /// monitor computes from the block's bytes and a caller may not supply.
    ///
    /// Defaulted on read so a store written before ranks existed still loads, and
    /// `None` for an unranked launch, which is every launch that names no rank.
    #[serde(default)]
    pub rank: Option<String>,
    /// SHA-256 of the exact bytes of that rank's block, or `None` when the rank has
    /// no written block (so nothing is appended). Part of the immutable input on
    /// purpose: a retry that reads a changed file is a *different* request and is
    /// refused rather than spawning a child with text nobody reviewed.
    #[serde(default)]
    pub rank_digest: Option<String>,
    /// How the child reaches the harness, which decides how its pid is
    /// verified. Defaulted for stores written before this existed: every
    /// launch up to then was headless.
    #[serde(default)]
    pub shape: LaunchShape,
    /// Where a **visible** child's window belongs, resolved by the caller
    /// immediately before the call — the launch performs the placement, it
    /// does not decide the policy of where workers go. Absent for a headless
    /// launch, which has no window; required for a terminal one, which refuses
    /// rather than guessing a workspace.
    #[serde(default)]
    pub destination: Option<i64>,
}

/// **Which honest identity check applies** — the shape is a fact of the
/// launch, not a guess made from what answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchShape {
    /// The service spawns the harness itself: the child's pid *is* the
    /// harness's, and equality is the whole check. Left strict — a headless
    /// launch that suddenly has a grandchild is a fault, not a terminal.
    #[default]
    Headless,
    /// The service spawns a terminal, which spawns the harness: the harness
    /// is a **descendant**, never the child, so equality is false for a
    /// launch that is entirely ours.
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Intent,
    Supervised,
    Running,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BriefState {
    #[default]
    NotAttempted,
    /// Write-ahead intent: after a crash, whether send happened is ambiguous.
    /// Under a correlated delivery the delivery key stored beside it is what
    /// makes a repeat safe; an `Attempted` with no key is an older receipt.
    Attempted,
    /// Transport acceptance only, never consumption or execution proof. The
    /// legacy inbox path wrote this and nothing writes it now; its meaning is
    /// unchanged and it is never reported as a correlated receipt.
    Accepted { outcome: Outcome, words: String },
    /// This call appended the record to the recipient's log. The receipt is
    /// what the log holds, not what the transport returned.
    Journaled { receipt: Receipt },
    /// An earlier call had appended it, under the same key and the same
    /// binding. Nothing was sent again.
    Existing { receipt: Receipt },
    /// The recipient's owner answered and declined. `kind` is the refusal's
    /// own word — invalid, conflict, log, unconfirmed.
    Refused { kind: String, why: String },
    /// No durable acceptance and no evidence of absence. The delivery key
    /// stays durable, so a repeat is safe; nothing here may be read as a
    /// delivery that happened.
    Unknown { why: String },
    /// The recipient's build does not know the correlated op. Fail closed:
    /// no legacy message was sent in its place.
    Unsupported,
}

/// The receipt one correlated delivery produced — the recipient's log's own
/// evidence.
///
/// A mirror of `eidolon_core::correlated::Receipt`, carried here because this
/// service is a plugin and does not link the harness. The field names and
/// meanings are the contract's; the store keeps the receipt rather than a
/// summary of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The session address the record is in — the log's stem.
    pub session: String,
    /// The correlation key, as the record holds it.
    pub delivery_key: String,
    /// The binding digest, as the record holds it.
    pub binding_digest: String,
    /// The record's own id in that log.
    pub record: u64,
    /// The recipient's file identity at the moment it answered. Evidence
    /// about that host then, never a durable name.
    pub log_binding: LogBinding,
    /// Whether the record is on the branch ending at the head. **Not** whether
    /// the recipient's next prompt will contain it.
    pub on_current_branch: bool,
}

/// The on-host identity of the recipient's log, as the receipt reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogBinding {
    pub dev: u64,
    pub ino: u64,
}

/// What became of one correlated brief delivery: the five answers the harness
/// contract distinguishes.
///
/// Two of them are success and they mean different things — `Journaled` is this
/// call putting the record in the recipient's log, `Existing` is an earlier call
/// having done so — and none of them is consumption, a started turn or proof the
/// work happened. The other three are not success, and none is permission to
/// resend as an ordinary message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Correlated {
    Journaled(Receipt),
    Existing(Receipt),
    Refused {
        /// `invalid`, `conflict`, `log` or `unconfirmed` — the recipient's own
        /// classification, passed through rather than reinvented.
        kind: String,
        /// The refusal's sentence, verbatim.
        why: String,
    },
    Unknown {
        /// What the transport observed, in its own words.
        why: String,
    },
    /// The recipient's build does not know the correlated op. Fail closed.
    Unsupported,
}

impl Correlated {
    /// Whether a durable record is known to exist — the only case in which
    /// this monitor may say the brief reached the recipient's log. Still not
    /// evidence that the model read it.
    pub fn is_journaled(&self) -> bool {
        matches!(self, Correlated::Journaled(_) | Correlated::Existing(_))
    }
}

/// Where a brief's correlated delivery goes: one record in the recipient's log,
/// under the key this monitor minted. Distinct from [`crate::Transport`], which
/// carries notices as inbox mail: a brief that went down that path would be a
/// second path to the same session, and this seam exists to have exactly one.
pub trait BriefTransport: Send + Sync {
    fn send_correlated(
        &self,
        to: &str,
        delivery_key: &str,
        binding_digest: &str,
        text: &str,
    ) -> Result<Correlated, String>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartRecord {
    pub request: Request,
    pub phase: Phase,
    pub session: Option<String>,
    pub adoption_epoch: Option<u64>,
    #[serde(default)]
    pub brief: BriefState,
    /// The opaque correlation key for this operation's brief, minted once and
    /// persisted **before** the first transport call. A repeat sends under this
    /// same key and the recipient deduplicates on it. It is correlation, not
    /// authority: it grants nothing on the far side.
    #[serde(default)]
    pub brief_delivery_key: Option<String>,
    /// The digest of the immutable input the key is bound to, persisted beside
    /// it. The recipient does not verify it; it enforces that a repeat under
    /// one key carries the same digest, so a key is never silently reused for
    /// different bytes.
    #[serde(default)]
    pub brief_binding_digest: Option<String>,
    #[serde(default)]
    pub cancellation: Option<Cancellation>,
}

/// Write-ahead cancellation intent. The owning request carries the actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancellation {
    pub requested_at_ms: u64,
    pub reason: String,
    pub outcome: CancelOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelOutcome {
    /// Pipes may or may not have closed; no exit has been durably established.
    Attempted,
    /// Observed through this instance's owned Child, not inferred from a roster.
    Exited(ExitEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitEvidence {
    pub observed_at_ms: u64,
    pub status: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub roster_before_close: CancelRosterEvidence,
}

/// Snapshot taken before the first pipe closure, never an atomic exit/roster claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRosterEvidence {
    pub bound_session: Option<String>,
    pub queried: bool,
    pub matching_rows: Option<usize>,
    pub reached: Option<bool>,
}

/// Separate from start authority: caller must explicitly admit cancellation.
pub trait CancelHost {
    fn check_cancel(&mut self, request: &Request) -> Result<(), String>;
    fn cancel(&mut self, request: &Request) -> Result<ExitEvidence, String>;
}

/// Evidence from the harness, not a model's account or a missing roster row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchState {
    /// The host can establish this operation has never created a child.
    NotCreated,
    /// The child cannot run a model turn, consume its brief or act on peer mail.
    Held { session: String },
    /// The matching child has been released and is live, not merely requested.
    Running { session: String },
    /// Absence, an ambiguous address, lost host state or an unreachable probe.
    Unknown(String),
}

/// A host keys durable receipts on (caller, operation) and the immutable request.
/// `prepare` is idempotent even after a lost reply and must never release a turn.
/// `release` only releases that held child; it never creates/replaces one. Both
/// methods require admitted authority; this trait is not an alternate gate.
pub trait Host {
    fn inspect(&mut self, request: &Request) -> Result<LaunchState, String>;
    fn prepare(&mut self, request: &Request) -> Result<(), String>;
    fn release(&mut self, request: &Request, session: &str) -> Result<(), String>;
    /// Send the brief as one correlated delivery under the key and digest the
    /// state machine minted and persisted. There is no legacy inbox alternative
    /// here: a caller that cannot compose this must answer `Unsupported` or
    /// `Unknown`, never fall back.
    fn deliver_brief(
        &mut self,
        request: &Request,
        session: &str,
        delivery_key: &str,
        binding_digest: &str,
    ) -> Result<Correlated, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub session: String,
    pub adoption_epoch: u64,
    /// What the recipient's log established about the brief. Only `Journaled`
    /// or `Existing` can be reported here, and neither means it was read.
    pub brief: Correlated,
}

fn persist(
    store: &mut Store,
    next: Store,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
) -> Result<(), String> {
    match save(&next) {
        Ok(()) => { *store = next; Ok(()) }
        Err(e) if e.uncertain() => {
            // Reconcile the live copy, but never cross the barrier on an
            // uncertain durability receipt. A later attempt must save again.
            Err(reconcile(store, load, e).unwrap_err())
        }
        Err(e) => Err(format!("managed start did not advance: {e}")),
    }
}

fn session_of(state: &LaunchState) -> Option<&str> {
    match state {
        LaunchState::Held { session } | LaunchState::Running { session } => Some(session),
        _ => None,
    }
}

/// Why a retry under the same operation is not the same request. When the only
/// thing that differs is the rank binding, say that the file changed, because that
/// is the thing an operator can check — a launch that re-read a rewritten block
/// would tell the child text nobody reviewed for this operation.
fn changed_input(recorded: &Request, request: &Request) -> String {
    if recorded.rank == request.rank && recorded.rank_digest != request.rank_digest {
        return format!(
            "the `{}` rank block changed since this operation was recorded: the operation is bound to {}, \
             and the block's bytes now hash to {}. Refusing to reuse the operation — this launch would \
             inject different rank text, and an operation id is bound to one immutable input",
            request.rank.as_deref().unwrap_or_default(),
            recorded.rank_digest.as_deref().unwrap_or("no block (nothing was appended)"),
            request.rank_digest.as_deref().unwrap_or("no block (nothing would be appended)"),
        );
    }
    "operation already exists with different immutable launch input".into()
}

/// A delivery key: opaque, minted once per operation, and **not** a capability.
///
/// 128 random bits, hex, well inside the contract's bound. Minted here and
/// persisted with the operation before the transport is given anything, so a
/// repeat carries the same key and the recipient deduplicates on it instead of
/// receiving a second brief.
fn mint_delivery_key() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .map_err(|e| format!("minting a brief delivery key: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The digest of the immutable input the delivery key is bound to: who asked,
/// which operation, which recipient and the brief's exact bytes.
///
/// Length-prefixed and domain-separated, so two different inputs cannot share
/// one encoding. It is this sender's assertion — the recipient only enforces
/// that a repeat under one key carries the same digest.
fn binding_digest(request: &Request, session: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"eidolon.coordinator.brief.v1");
    for part in [request.caller.as_str(), request.operation.as_str(), session, request.brief.as_str()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Complete or resume one admitted start. Errors describe partial/unknown state,
/// never managed success. Intent, supervision and completion are separate durable
/// checkpoints; retrying the same request reconciles them rather than spawning anew.
/// This does not authorize automatic retries or manufacture a lifecycle owner.
pub fn start(
    store: &mut Store,
    request: &Request,
    host: &mut dyn Host,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
    now: u64,
) -> Result<Started, String> {
    if request.operation.trim().is_empty() || request.caller.trim().is_empty()
        || request.workspace.trim().is_empty() || request.model.trim().is_empty()
    {
        return Err("managed start needs an operation, caller, workspace and model".into());
    }
    // The rank is a name from the operator's vocabulary, and a digest only exists
    // because a block's bytes were read for it. Neither is a model's to invent.
    if let Some(rank) = &request.rank {
        if !crate::rank::RANKS.contains(&rank.as_str()) {
            return Err(crate::rank::unknown_rank(rank));
        }
    } else if request.rank_digest.is_some() {
        return Err("managed start binds a rank digest with no rank; a digest names the bytes of a block \
                    nobody named, and there is no session to append it to"
            .into());
    }
    let matching: Vec<usize> = store.starts.iter().enumerate()
        .filter(|(_, r)| r.request.caller == request.caller && r.request.operation == request.operation)
        .map(|(i, _)| i).collect();
    let index = match matching.as_slice() {
        [] => {
            let mut next = store.clone();
            next.starts.push(StartRecord {
                request: request.clone(), phase: Phase::Intent, session: None, adoption_epoch: None, brief: BriefState::NotAttempted,
                brief_delivery_key: None, brief_binding_digest: None, cancellation: None,
            });
            persist(store, next, save, load)?;
            store.starts.len() - 1
        }
        [index] if store.starts[*index].request == *request => *index,
        [index] => return Err(changed_input(&store.starts[*index].request, request)),
        _ => return Err("multiple records claim this operation; refusing to guess".into()),
    };
    let record = store.starts[index].clone();
    if record.cancellation.is_some() {
        return Err("cancellation intent exists; this operation can never start again or release its startup barrier; adoption release is separate after durable exit".into());
    }
    if (record.phase == Phase::Intent && (record.session.is_some() || record.adoption_epoch.is_some()))
        || (record.phase != Phase::Intent && (record.session.is_none() || record.adoption_epoch.is_none()))
    {
        return Err("managed start has inconsistent durable phase/binding; repair required".into());
    }

    let mut observed = host.inspect(request)?;
    if observed == LaunchState::NotCreated && record.phase == Phase::Intent {
        host.prepare(request).map_err(|e| format!("held launch unconfirmed; reconcile before retry: {e}"))?;
        observed = host.inspect(request)?;
    }
    if record.phase == Phase::Intent {
        let session = match &observed {
            LaunchState::Held { session } if !session.is_empty() => session.clone(),
            _ => return Err(format!("launch not established as held; no adoption or release: {observed:?}")),
        };
        if session == request.caller {
            return Err("a managed worker cannot be its own coordinator".into());
        }
        if store.records.iter().any(|r| r.worker == session && r.released_at_ms.is_none()) {
            return Err("held session already has an active adoption; refusing to take ownership implicitly".into());
        }
        let epoch = next_epoch(&store.records, &request.caller, &session);
        let mut next = store.clone();
        // Empty locators mean not established. They are not addresses and
        // must never be filled from a model claim or a guessed registration.
        next.records.push(Adoption {
            epoch, coordinator: request.caller.clone(), coordinator_id: String::new(),
            worker: session.clone(), worker_id_seen: String::new(), name: String::new(),
            note: request.brief.clone(), adopted_at_ms: now, released_at_ms: None, release_reason: String::new(),
        });
        next.starts[index].phase = Phase::Supervised;
        next.starts[index].session = Some(session);
        next.starts[index].adoption_epoch = Some(epoch);
        // Adoption and the session binding become durable in the SAME save.
        persist(store, next, save, load)?;
    }

    let record = &store.starts[index];
    let session = record.session.clone().ok_or("managed start has no session binding")?;
    let epoch = record.adoption_epoch.ok_or("managed start has no adoption epoch")?;
    if observed == LaunchState::NotCreated {
        return Err("store binds a created child but host says never created; state disagreement needs repair, not respawn".into());
    }
    if session_of(&observed) != Some(session.as_str()) {
        return Err(format!("bound child is not established; never spawning a replacement: {observed:?}"));
    }
    let active = store.records.iter().filter(|r| {
        r.coordinator == request.caller && r.worker == session && r.epoch == epoch && r.released_at_ms.is_none()
    }).count();
    if active != 1 {
        return Err("supervision is no longer uniquely active; held worker will not be released".into());
    }
    if matches!(observed, LaunchState::Held { .. }) {
        if record.phase == Phase::Running {
            return Err("a previously running child now claims held; refusing a contradictory receipt".into());
        }
        // Also on recovery after an uncertain save: read-back alone is not a
        // durability receipt. Reflush the exact adoption before releasing.
        persist(store, store.clone(), save, load)?;
        host.release(request, &session).map_err(|e| format!("release unconfirmed; supervision retained: {e}"))?;
        observed = host.inspect(request).map_err(|e| {
            format!("release acknowledged, running state unobserved; child may be running; supervision retained: {e}")
        })?;
        if observed != (LaunchState::Running { session: session.clone() }) {
            return Err(format!("release acknowledged, running state unconfirmed; child may be running; supervision retained: {observed:?}"));
        }
    }
    if observed != (LaunchState::Running { session: session.clone() }) {
        return Err(format!("running state not established; supervision retained: {observed:?}"));
    }
    // ---- The brief: one correlated delivery, one journaled receipt. ----
    //
    // What a repeat means is decided here, and only from durable facts.
    let previous = store.starts[index].brief.clone();
    let resend = match &previous {
        BriefState::NotAttempted => {
            if store.starts[index].phase == Phase::Running {
                return Err("older running receipt has no brief evidence; explicit reconciliation required".into());
            }
            true
        }
        // The key below was persisted before the first attempt, so a repeat is
        // not blind: it carries the same key and the recipient answers
        // `Existing` if the first one landed rather than receiving a second
        // brief. An `Attempted` with no stored key is an older receipt from the
        // legacy inbox path, where the ambiguity is real and a resend could
        // arrive beside a delivery that already happened.
        BriefState::Attempted | BriefState::Unknown { .. } => {
            if store.starts[index].brief_delivery_key.is_none()
                || store.starts[index].brief_binding_digest.is_none()
            {
                return Err("brief delivery is ambiguous; the write-ahead attempt carries no delivery key, so a resend could arrive beside a delivery that already happened; never resending blindly".into());
            }
            true
        }
        // Settled under the correlated contract: the record is in the log, and
        // nothing is sent again. This is the repeat that does not re-send.
        BriefState::Journaled { .. } | BriefState::Existing { .. } => false,
        BriefState::Refused { kind, why } => return Err(format!(
            "brief delivery was refused ({kind}): {why}; a refusal is not a licence to resend, and no \
             ordinary message was sent in its place"
        )),
        BriefState::Unsupported => return Err(
            "the recipient's build does not support correlated delivery; nothing was sent, and no legacy \
             message was sent in its place".into(),
        ),
        // A receipt from the legacy inbox path. Its meaning is unchanged —
        // transport acceptance, never a record — so it is not reported as one.
        BriefState::Accepted { .. } => return Err(
            "this operation's brief was accepted by the legacy inbox transport, which left no correlated \
             receipt; an explicit reconciliation is required, not a claim of delivery".into(),
        ),
    };
    if resend {
        // The plan is durable BEFORE the transport is given anything, and it
        // never changes for this operation: one key, one digest, every repeat.
        let mut next = store.clone();
        if next.starts[index].brief_delivery_key.is_none() {
            next.starts[index].brief_delivery_key = Some(mint_delivery_key()?);
            next.starts[index].brief_binding_digest = Some(binding_digest(request, &session));
        }
        next.starts[index].brief = BriefState::Attempted;
        persist(store, next, save, load)?;
        let delivery_key = store.starts[index].brief_delivery_key.clone()
            .ok_or("brief delivery key was not durably stored")?;
        let digest = store.starts[index].brief_binding_digest.clone()
            .ok_or("brief binding digest was not durably stored")?;
        // A correlated delivery does **not** wake the recipient: the record is
        // in its log and it reads that at its next turn. This monitor does not
        // invent a wake to make a journaled record look like a started turn.
        let answer = host.deliver_brief(request, &session, &delivery_key, &digest)
            .map_err(|e| format!("brief send attempted; acceptance ambiguous, never retry blindly: {e}"))?;
        let mut next = store.clone();
        next.starts[index].brief = match &answer {
            Correlated::Journaled(receipt) => BriefState::Journaled { receipt: receipt.clone() },
            Correlated::Existing(receipt) => BriefState::Existing { receipt: receipt.clone() },
            Correlated::Refused { kind, why } => BriefState::Refused { kind: kind.clone(), why: why.clone() },
            Correlated::Unknown { why } => BriefState::Unknown { why: why.clone() },
            Correlated::Unsupported => BriefState::Unsupported,
        };
        persist(store, next, save, load)?;
        // Only `Journaled` and `Existing` are a record in the log. The other
        // three are said exactly: no delivery, no downgrade, no resend here.
        match answer {
            Correlated::Journaled(_) | Correlated::Existing(_) => {}
            Correlated::Refused { kind, why } => return Err(format!(
                "brief delivery was refused ({kind}): {why}; a refusal is not a licence to resend, and no \
                 ordinary message was sent in its place"
            )),
            Correlated::Unknown { why } => return Err(format!(
                "brief delivery is unconfirmed: no durable acceptance and no evidence of absence ({why}); the \
                 delivery key is durable, so repeating this operation re-sends under the same key and answers \
                 `existing` if the first attempt landed — never as an ordinary message"
            )),
            Correlated::Unsupported => return Err(
                "the recipient's build does not support correlated delivery; nothing was sent, and no legacy \
                 message was sent in its place".into(),
            ),
        }
    }
    let brief = match store.starts[index].brief.clone() {
        BriefState::Journaled { receipt } => Correlated::Journaled(receipt),
        BriefState::Existing { receipt } => Correlated::Existing(receipt),
        _ => return Err("brief acceptance not established".into()),
    };
    if store.starts[index].phase != Phase::Running {
        let mut next = store.clone();
        next.starts[index].phase = Phase::Running;
        persist(store, next, save, load)?;
    }
    Ok(Started { session, adoption_epoch: epoch, brief })
}

/// Explicitly admitted cancellation, under the same Gate then Host lock order.
/// No adoption is released: coordinator_release remains a separate admitted act.
#[allow(clippy::too_many_arguments)]
pub fn cancel(
    store: &mut Store,
    caller: &str,
    operation: &str,
    reason: &str,
    host: &mut dyn CancelHost,
    save: &mut dyn FnMut(&Store) -> Result<(), SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
    now: u64,
) -> Result<ExitEvidence, String> {
    if caller.trim().is_empty() || operation.trim().is_empty() || reason.trim().is_empty() || reason.len() > 4096 {
        return Err("cancel needs caller, operation and a reason of 1..4096 bytes".into());
    }
    let matches: Vec<_> = store.starts.iter().enumerate()
        .filter(|(_, r)| r.request.caller == caller && r.request.operation == operation).map(|(i, _)| i).collect();
    let [index] = matches.as_slice() else { return Err("cancel needs exactly one existing owned start operation".into()) };
    let index = *index;
    let record = store.starts[index].clone();
    if record.phase == Phase::Running || record.brief != BriefState::NotAttempted {
        return Err("cancel refuses a running operation or any brief attempt".into());
    }
    match (&record.session, record.adoption_epoch, &record.phase) {
        (None, None, Phase::Intent) => {}
        (Some(session), Some(epoch), Phase::Supervised) => {
            let matching: Vec<_> = store.records.iter().filter(|r| r.worker == *session && r.coordinator == caller).collect();
            if matching.iter().filter(|r| r.epoch == epoch && r.released_at_ms.is_none()).count() != 1
                || matching.iter().any(|r| r.epoch != epoch && r.released_at_ms.is_none())
            {
                return Err("cancel refuses changed or inactive adoption epoch".into());
            }
        }
        _ => return Err("cancel refuses inconsistent durable phase/binding".into()),
    }
    // Even an old successful receipt cannot authorize a restarted host to act.
    host.check_cancel(&record.request)?;
    if let Some(previous) = &record.cancellation {
        if previous.reason != reason { return Err("cancel reason differs from immutable cancellation intent".into()) }
        if let CancelOutcome::Exited(evidence) = &previous.outcome { return Ok(evidence.clone()) }
    }
    let mut next = store.clone();
    next.starts[index].cancellation.get_or_insert(Cancellation {
        requested_at_ms: now, reason: reason.into(), outcome: CancelOutcome::Attempted,
    });
    // Reflush even on an explicit retry after an uncertain persistence receipt.
    persist(store, next, save, load)?;
    let evidence = host.cancel(&record.request)
        .map_err(|e| format!("cancellation intent recorded; exit unconfirmed; supervision retained: {e}"))?;
    let mut next = store.clone();
    next.starts[index].cancellation.as_mut().unwrap().outcome = CancelOutcome::Exited(evidence.clone());
    persist(store, next, save, load)
        .map_err(|e| format!("exit observed but cancellation receipt durability unconfirmed; supervision retained: {e}"))?;
    Ok(evidence)
}

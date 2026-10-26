//! Managed-start ordering, independent of a process launcher or public tool.
//! The caller must hold the store's Gate and have admitted the exact request
//! through the harness dispatcher. This module grants no permission. No service
//! endpoint may bypass these contracts: the process Host enforces the pre-turn hold.

use serde::{Deserialize, Serialize};
use crate::{Adoption, Delivery, Outcome, Store, next_epoch, reconcile, store::SaveError};

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
    Attempted,
    /// Transport acceptance only, never consumption or execution proof.
    Accepted { outcome: Outcome, words: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartRecord {
    pub request: Request,
    pub phase: Phase,
    pub session: Option<String>,
    pub adoption_epoch: Option<u64>,
    #[serde(default)]
    pub brief: BriefState,
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
    fn deliver_brief(&mut self, request: &Request, session: &str) -> Result<Delivery, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub session: String,
    pub adoption_epoch: u64,
    pub brief_outcome: Outcome,
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
    let matching: Vec<usize> = store.starts.iter().enumerate()
        .filter(|(_, r)| r.request.caller == request.caller && r.request.operation == request.operation)
        .map(|(i, _)| i).collect();
    let index = match matching.as_slice() {
        [] => {
            let mut next = store.clone();
            next.starts.push(StartRecord {
                request: request.clone(), phase: Phase::Intent, session: None, adoption_epoch: None, brief: BriefState::NotAttempted,
                cancellation: None,
            });
            persist(store, next, save, load)?;
            store.starts.len() - 1
        }
        [index] if store.starts[*index].request == *request => *index,
        [_] => return Err("operation already exists with different immutable launch input".into()),
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

    if store.starts[index].brief == BriefState::Attempted {
        return Err("brief delivery is ambiguous; write-ahead attempt exists without durable acceptance; never resending blindly".into());
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
    if store.starts[index].brief == BriefState::NotAttempted {
        if store.starts[index].phase == Phase::Running {
            return Err("older running receipt has no brief evidence; explicit reconciliation required".into());
        }
        let mut next = store.clone();
        next.starts[index].brief = BriefState::Attempted;
        persist(store, next, save, load)?;
        let delivery = host.deliver_brief(request, &session)
            .map_err(|e| format!("brief send attempted; acceptance ambiguous, never retry blindly: {e}"))?;
        if !matches!(delivery.outcome, Outcome::Delivered | Outcome::Queued) {
            return Err(format!("brief transport reported {}; no success or blind retry: {}", delivery.outcome.word(), delivery.words));
        }
        let mut next = store.clone();
        next.starts[index].brief = BriefState::Accepted { outcome: delivery.outcome, words: delivery.words };
        persist(store, next, save, load)?;
    }
    let BriefState::Accepted { outcome: brief_outcome, .. } = store.starts[index].brief else {
        return Err("brief acceptance not established".into());
    };
    if !matches!(brief_outcome, Outcome::Delivered | Outcome::Queued) {
        return Err("stored brief receipt is not transport acceptance".into());
    }
    if store.starts[index].phase != Phase::Running {
        let mut next = store.clone();
        next.starts[index].phase = Phase::Running;
        persist(store, next, save, load)?;
    }
    Ok(Started { session, adoption_epoch: epoch, brief_outcome })
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

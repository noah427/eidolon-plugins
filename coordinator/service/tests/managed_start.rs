//! Failure injection at every durable checkpoint of the held-start seam, and
//! the brief's correlated receipt.
//! The fake host counts model turns only at release and independently reads the
//! persisted store there. No real process, provider, roster or service is used.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Mutex;
use eidolon_coordinator::{Store, managed_start::{self, BriefState, BriefTransport, Correlated, Host, LaunchState, LogBinding, Phase, Receipt, Request}, store::SaveError};

const SESSION: &str = "1792962214694";

fn request() -> Request {
    Request {
        operation: "start-1".into(), caller: "1792959651919".into(),
        workspace: "/isolated/work".into(), model: "mock".into(), persona: None, brief: "bounded task".into(),
        rank: None, rank_digest: None,
        shape: eidolon_coordinator::managed_start::LaunchShape::Headless,
        destination: None,
    }
}

/// One scripted answer from the fake transport. `Journaled`/`Existing` build
/// their receipt from the key and digest the send actually carried, so the
/// test reads back what went over the wire rather than what it guessed.
#[derive(Clone, Debug)]
enum Answer {
    Journaled,
    Existing,
    Refused(String, String),
    Unknown(String),
    Unsupported,
    /// No answer at all — the host could not even compose the send.
    Lost(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    to: String,
    delivery_key: String,
    binding_digest: String,
    text: String,
}

struct FakeBriefs {
    answers: Mutex<VecDeque<Answer>>,
    calls: Mutex<Vec<Call>>,
}

impl FakeBriefs {
    fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self { answers: Mutex::new(answers.into_iter().collect()), calls: Mutex::new(Vec::new()) }
    }

    fn calls(&self) -> Vec<Call> { self.calls.lock().unwrap().clone() }
}

impl BriefTransport for FakeBriefs {
    fn send_correlated(&self, to: &str, delivery_key: &str, binding_digest: &str, text: &str) -> Result<Correlated, String> {
        let call = Call {
            to: to.into(), delivery_key: delivery_key.into(),
            binding_digest: binding_digest.into(), text: text.into(),
        };
        self.calls.lock().unwrap().push(call.clone());
        let receipt = || Receipt {
            session: call.to.clone(), delivery_key: call.delivery_key.clone(),
            binding_digest: call.binding_digest.clone(), record: 7,
            log_binding: LogBinding { dev: 64768, ino: 4242 }, on_current_branch: true,
        };
        match self.answers.lock().unwrap().pop_front() {
            Some(Answer::Journaled) => Ok(Correlated::Journaled(receipt())),
            Some(Answer::Existing) => Ok(Correlated::Existing(receipt())),
            Some(Answer::Refused(kind, why)) => Ok(Correlated::Refused { kind, why }),
            Some(Answer::Unknown(why)) => Ok(Correlated::Unknown { why }),
            Some(Answer::Unsupported) => Ok(Correlated::Unsupported),
            Some(Answer::Lost(why)) => Err(why),
            // An unscripted send is a test bug, and a loud one.
            None => panic!("the brief was sent more times than the test scripted"),
        }
    }
}

struct FakeHost {
    disk: Rc<RefCell<Store>>,
    briefs: Rc<FakeBriefs>,
    state: LaunchState,
    prepares: usize,
    releases: usize,
    turns: usize,
    brief_sends: usize,
    lose_prepare_reply: bool,
    lose_release_reply: bool,
    ignore_release: bool,
    lose_liveness_after_release: bool,
}

impl Host for FakeHost {
    fn inspect(&mut self, _: &Request) -> Result<LaunchState, String> { Ok(self.state.clone()) }

    fn prepare(&mut self, request: &Request) -> Result<(), String> {
        assert!(self.disk.borrow().starts.iter().any(|r| r.request == *request), "intent must already be durable");
        assert_eq!(self.state, LaunchState::NotCreated);
        self.prepares += 1;
        self.state = LaunchState::Held { session: SESSION.into() };
        assert_eq!(self.turns, 0, "preparation cannot run a model turn");
        if std::mem::take(&mut self.lose_prepare_reply) { Err("lost prepare reply".into()) } else { Ok(()) }
    }

    fn release(&mut self, request: &Request, session: &str) -> Result<(), String> {
        assert_eq!(self.state, LaunchState::Held { session: session.into() });
        let disk = self.disk.borrow();
        let record = disk.starts.iter().find(|r| r.request == *request).unwrap();
        assert_eq!(record.phase, Phase::Supervised, "release needs a DURABLE supervised checkpoint");
        assert!(disk.records.iter().any(|r| {
            r.worker == session && r.coordinator == request.caller
                && Some(r.epoch) == record.adoption_epoch && r.released_at_ms.is_none()
        }), "release needs the matching durable active adoption");
        drop(disk);
        self.releases += 1;
        if !self.ignore_release {
            self.state = LaunchState::Running { session: session.into() };
            self.turns += 1;
        }
        if self.lose_liveness_after_release { self.state = LaunchState::Unknown("probe unavailable".into()); }
        if std::mem::take(&mut self.lose_release_reply) { Err("lost release reply".into()) } else { Ok(()) }
    }

    fn deliver_brief(&mut self, request: &Request, session: &str, delivery_key: &str, binding_digest: &str) -> Result<Correlated, String> {
        let disk = self.disk.borrow();
        let record = disk.starts.iter().find(|r| r.request == *request).unwrap();
        // The write-ahead attempt, AND the key it is bound to, are durable
        // before any transport is given anything.
        assert_eq!(record.brief, BriefState::Attempted, "the attempt must be durable before the send");
        assert_eq!(record.brief_delivery_key.as_deref(), Some(delivery_key), "the send must carry the stored key");
        assert_eq!(record.brief_binding_digest.as_deref(), Some(binding_digest), "the send must carry the stored digest");
        drop(disk);
        self.brief_sends += 1;
        self.briefs.send_correlated(session, delivery_key, binding_digest, &request.brief)
    }
}

struct Fixture {
    store: Store,
    host: FakeHost,
    briefs: Rc<FakeBriefs>,
    saves: usize,
    fail_at: Option<usize>,
    uncertain: bool,
    uncertain_keeps_old: bool,
}

impl Fixture {
    fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        let briefs = Rc::new(FakeBriefs::new(answers));
        Self {
            store: Store::default(), saves: 0, fail_at: None, uncertain: false, uncertain_keeps_old: false,
            briefs: briefs.clone(),
            host: FakeHost {
                disk: Rc::new(RefCell::new(Store::default())), briefs, state: LaunchState::NotCreated,
                prepares: 0, releases: 0, turns: 0, brief_sends: 0, lose_prepare_reply: false,
                lose_release_reply: false, ignore_release: false, lose_liveness_after_release: false,
            },
        }
    }

    fn with_answers() -> Self {
        Self::new([Answer::Journaled])
    }

    fn run(&mut self, request: &Request) -> Result<managed_start::Started, String> {
        let disk = self.host.disk.clone();
        let load_disk = disk.clone();
        let saves = &mut self.saves;
        let fail_at = self.fail_at;
        let uncertain = self.uncertain;
        let uncertain_keeps_old = self.uncertain_keeps_old;
        managed_start::start(&mut self.store, request, &mut self.host, &mut |next| {
            *saves += 1;
            if Some(*saves) == fail_at {
                if uncertain {
                    if !uncertain_keeps_old { *disk.borrow_mut() = next.clone(); }
                    return Err(SaveError::Uncertain("injected after rename".into()));
                }
                return Err(SaveError::Unchanged("injected before rename".into()));
            }
            *disk.borrow_mut() = next.clone();
            Ok(())
        }, &mut || Ok(load_disk.borrow().clone()), 100)
    }

    fn restart_monitor(&mut self) {
        let bytes = serde_json::to_vec(&*self.host.disk.borrow()).unwrap();
        self.store = serde_json::from_slice(&bytes).unwrap();
    }
}

#[test]
fn complete_start_is_supervised_before_the_first_turn_and_retries_are_idempotent() {
    let mut f = Fixture::with_answers();
    let first = f.run(&request()).unwrap();
    assert_eq!(f.host.turns, 1);
    assert_eq!(f.store.records.len(), 1);
    assert!(matches!(first.brief, Correlated::Journaled(_)), "the first send journals the record");
    f.restart_monitor();
    assert_eq!(f.run(&request()).unwrap(), first);
    assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
    assert_eq!(f.store.records.len(), 1);
}

#[test]
fn each_persistence_failure_is_partial_not_success_and_restart_recovers_without_an_extra_child() {
    for uncertain in [false, true] {
        for checkpoint in 1..=6 {
            // A keyed repeat is answered `Existing`: the recipient recognises
            // the key and appends nothing.
            let mut f = Fixture::new([Answer::Journaled, Answer::Existing]);
            f.fail_at = Some(checkpoint);
            f.uncertain = uncertain;
            assert!(f.run(&request()).is_err(), "checkpoint {checkpoint}, uncertain {uncertain}");
            assert_eq!(f.host.turns, usize::from(checkpoint >= 4), "no turn before durable adoption/reflush");
            if checkpoint == 1 { assert_eq!(f.host.prepares, 0); }
            // A failure at the receipt checkpoint leaves `Attempted` with the
            // key already durable, and the send did happen. A repeat under
            // that same key is what the correlated contract calls safe, so it
            // completes rather than staying blocked.
            let keyed_resume = checkpoint == 5 && !uncertain;
            f.restart_monitor();
            f.fail_at = None;
            f.run(&request()).unwrap();
            assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
            assert_eq!(f.store.records.len(), 1);
            let calls = f.briefs.calls();
            assert_eq!(calls.len(), if keyed_resume { 2 } else { 1 }, "one send, plus at most one keyed repeat");
            assert!(calls.windows(2).all(|w| w[0].delivery_key == w[1].delivery_key), "a repeat never changes the key");
            assert!(matches!(f.store.starts[0].brief, BriefState::Journaled { .. } | BriefState::Existing { .. }));
        }
    }
}

#[test]
fn a_lost_prepare_reply_leaves_a_held_child_and_recovery_does_not_prepare_again() {
    let mut f = Fixture::with_answers();
    f.host.lose_prepare_reply = true;
    assert!(f.run(&request()).unwrap_err().contains("held launch unconfirmed"));
    assert_eq!(f.host.turns, 0);
    assert!(f.store.records.is_empty());
    f.restart_monitor();
    f.run(&request()).unwrap();
    assert_eq!((f.host.prepares, f.host.turns), (1, 1));
}

#[test]
fn a_lost_release_reply_never_claims_success_and_recovery_observes_instead_of_releasing_again() {
    let mut f = Fixture::with_answers();
    f.host.lose_release_reply = true;
    assert!(f.run(&request()).unwrap_err().contains("release unconfirmed"));
    assert_eq!(f.host.turns, 1);
    assert_eq!(f.store.starts[0].phase, Phase::Supervised);
    f.restart_monitor();
    f.run(&request()).unwrap();
    assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
}

#[test]
fn a_release_ack_without_a_running_observation_is_not_managed_success() {
    let mut f = Fixture::with_answers();
    f.host.ignore_release = true;
    assert!(f.run(&request()).unwrap_err().contains("release acknowledged, running state unconfirmed"));
    assert_eq!(f.host.turns, 0);
    assert_eq!(f.store.starts[0].phase, Phase::Supervised);
}

#[test]
fn missing_host_evidence_never_becomes_permission_to_spawn_a_replacement() {
    let mut f = Fixture::with_answers();
    f.host.lose_prepare_reply = true;
    assert!(f.run(&request()).is_err());
    f.host.state = LaunchState::Unknown("not in the roster".into());
    assert!(f.run(&request()).is_err());
    assert_eq!((f.host.prepares, f.host.turns), (1, 0));
}

#[test]
fn changed_input_or_released_adoption_cannot_release_a_held_child() {
    let mut f = Fixture::with_answers();
    f.fail_at = Some(3); // adoption saved, release not attempted
    assert!(f.run(&request()).is_err());
    f.fail_at = None;
    let mut changed = request();
    changed.model = "another:model".into();
    assert!(f.run(&changed).unwrap_err().contains("different immutable"));
    f.store.records[0].released_at_ms = Some(101);
    *f.host.disk.borrow_mut() = f.store.clone();
    assert!(f.run(&request()).unwrap_err().contains("no longer uniquely active"));
    assert_eq!(f.host.turns, 0);
}

#[test]
fn old_store_shape_loads_without_start_records() {
    let store: Store = serde_json::from_str(r#"{"records":[],"notices":[]}"#).unwrap();
    assert!(store.starts.is_empty());
    // A record written before correlated delivery has neither the key nor the
    // digest, and still reads: the ambiguity of its `Attempted` is real.
    let record: managed_start::StartRecord = serde_json::from_str(
        r#"{"request":{"operation":"op","caller":"c","workspace":"/w","model":"m","persona":null,
            "brief":"b"},"phase":"Supervised","session":"s","adoption_epoch":1,"brief":"Attempted"}"#,
    ).unwrap();
    assert_eq!(record.brief, BriefState::Attempted);
    assert!(record.brief_delivery_key.is_none());
    assert!(record.brief_binding_digest.is_none());
}

#[test]
fn uncertain_adoption_with_old_store_on_reload_rebinds_the_same_held_child_once() {
    let mut f = Fixture::with_answers();
    f.fail_at = Some(2);
    f.uncertain = true;
    f.uncertain_keeps_old = true;
    assert!(f.run(&request()).is_err());
    assert_eq!(f.store.starts[0].phase, Phase::Intent);
    assert!(f.store.records.is_empty());
    assert_eq!(f.host.turns, 0);
    f.restart_monitor();
    f.fail_at = None;
    f.run(&request()).unwrap();
    assert_eq!(f.store.records.len(), 1);
    assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
}

#[test]
fn acknowledged_release_with_unknown_liveness_reports_that_the_child_may_be_running() {
    let mut f = Fixture::with_answers();
    f.host.lose_liveness_after_release = true;
    let error = f.run(&request()).unwrap_err();
    assert!(error.contains("release acknowledged"));
    assert!(error.contains("child may be running"));
    assert_eq!(f.host.turns, 1);
    assert_eq!(f.store.starts[0].phase, Phase::Supervised);
}

#[test]
fn bound_child_reported_never_created_is_a_repair_case_not_respawn() {
    let mut f = Fixture::with_answers();
    f.fail_at = Some(3);
    assert!(f.run(&request()).is_err());
    f.fail_at = None;
    f.host.state = LaunchState::NotCreated;
    assert!(f.run(&request()).unwrap_err().contains("state disagreement needs repair"));
    assert_eq!((f.host.prepares, f.host.turns), (1, 0));
}

/// Each of the five answers the correlated contract distinguishes: what the
/// store records, and what the caller (and so the tool) is told.
#[test]
fn each_correlated_outcome_is_recorded_distinctly_and_reported_as_itself() {
    // Journaled: this call put the record in the log.
    let mut f = Fixture::new([Answer::Journaled]);
    let started = f.run(&request()).unwrap();
    let call = f.briefs.calls().pop().unwrap();
    assert_eq!(call.to, SESSION);
    assert_eq!(call.text, request().brief);
    let BriefState::Journaled { receipt } = f.store.starts[0].brief.clone() else { panic!("not journaled") };
    assert_eq!(receipt.delivery_key, call.delivery_key);
    assert_eq!(receipt.binding_digest, call.binding_digest);
    assert_eq!(receipt.session, SESSION);
    assert_eq!(started.brief, Correlated::Journaled(receipt.clone()));
    // The tool's answer is this value under `brief`; it says what the log has,
    // and the separate `brief_consumption` word stays "not established".
    assert_eq!(serde_json::to_value(&started.brief).unwrap(), serde_json::json!({ "Journaled": receipt }));
    assert_eq!(f.store.starts[0].phase, Phase::Running);
    assert!(f.store.starts[0].brief_delivery_key.is_some(), "the key stays with the operation");

    // Existing: an earlier call did; nothing is sent again.
    let mut f = Fixture::new([Answer::Existing]);
    let started = f.run(&request()).unwrap();
    let BriefState::Existing { receipt } = f.store.starts[0].brief.clone() else { panic!("not existing") };
    assert_eq!(started.brief, Correlated::Existing(receipt));
    assert_eq!(f.store.starts[0].phase, Phase::Running);

    // Refused: the recipient's own kind is kept, word for word.
    let mut f = Fixture::new([Answer::Refused("conflict".into(), "key already names other bytes".into())]);
    let error = f.run(&request()).unwrap_err();
    assert_eq!(f.store.starts[0].brief, BriefState::Refused {
        kind: "conflict".into(), why: "key already names other bytes".into(),
    });
    assert!(error.contains("refused (conflict)"), "{error}");
    assert!(error.contains("no ordinary message was sent in its place"), "{error}");
    assert_ne!(f.store.starts[0].phase, Phase::Running, "a refusal is not managed success");

    // Unknown: no durable acceptance, no evidence of absence, no delivery claim.
    let mut f = Fixture::new([Answer::Unknown("nobody answered the doorbell".into())]);
    let error = f.run(&request()).unwrap_err();
    assert_eq!(f.store.starts[0].brief, BriefState::Unknown { why: "nobody answered the doorbell".into() });
    assert!(error.contains("unconfirmed"), "{error}");
    assert!(error.contains("same key"), "{error}");
    assert!(!error.contains("journaled"), "{error}");
    assert_ne!(f.store.starts[0].phase, Phase::Running);

    // Unsupported: fail closed, and never a legacy message in its place.
    let mut f = Fixture::new([Answer::Unsupported]);
    let error = f.run(&request()).unwrap_err();
    assert_eq!(f.store.starts[0].brief, BriefState::Unsupported);
    assert!(error.contains("does not support correlated delivery"), "{error}");
    assert!(error.contains("no legacy message was sent in its place"), "{error}");
    assert_ne!(f.store.starts[0].phase, Phase::Running);
}

/// A repeat of the same operation after a journaled receipt sends nothing, and
/// the key it would have used is the one the operation already holds.
#[test]
fn a_repeat_uses_the_stored_delivery_key_and_does_not_send_again() {
    let mut f = Fixture::with_answers();
    f.run(&request()).unwrap();
    let key = f.store.starts[0].brief_delivery_key.clone().unwrap();
    let digest = f.store.starts[0].brief_binding_digest.clone().unwrap();
    assert_eq!(key.len(), 32);
    assert_eq!(digest.len(), 64);
    assert_eq!(f.briefs.calls().len(), 1);
    f.restart_monitor();
    f.run(&request()).unwrap();
    assert_eq!(f.briefs.calls().len(), 1, "a settled receipt is never sent again");
    assert_eq!(f.store.starts[0].brief_delivery_key.as_deref(), Some(key.as_str()));
    assert_eq!(f.store.starts[0].brief_binding_digest.as_deref(), Some(digest.as_str()));
}

/// A repeat after `Unknown` is the contract's recovery: the same key and
/// digest, so the recipient answers `Existing` instead of taking a second
/// brief.
#[test]
fn an_unconfirmed_send_is_repeated_under_the_same_key_and_the_recipient_deduplicates() {
    let mut f = Fixture::new([Answer::Unknown("transport silence".into()), Answer::Existing]);
    assert!(f.run(&request()).unwrap_err().contains("unconfirmed"));
    let key = f.store.starts[0].brief_delivery_key.clone().unwrap();
    let first = f.briefs.calls().pop().unwrap();
    let started = f.run(&request()).unwrap();
    let second = f.briefs.calls().pop().unwrap();
    assert_eq!(first.delivery_key, key);
    assert_eq!(second.delivery_key, key, "the repeat carries the SAME key");
    assert_eq!(first.binding_digest, second.binding_digest, "and the same binding");
    assert!(matches!(started.brief, Correlated::Existing(_)));
    assert!(matches!(f.store.starts[0].brief, BriefState::Existing { .. }));
    assert_eq!(f.store.starts[0].phase, Phase::Running);
}

/// A transport that never answered leaves the keyed attempt durable: nothing
/// is reported as delivered, and the repeat carries the same key.
#[test]
fn a_lost_send_reply_is_not_success_and_the_keyed_repeat_resumes_it() {
    let mut f = Fixture::new([Answer::Lost("lost send acknowledgement".into()), Answer::Existing]);
    assert!(f.run(&request()).unwrap_err().contains("acceptance ambiguous"));
    assert_eq!(f.store.starts[0].brief, BriefState::Attempted);
    assert_ne!(f.store.starts[0].phase, Phase::Running);
    let key = f.store.starts[0].brief_delivery_key.clone().unwrap();
    f.restart_monitor();
    f.run(&request()).unwrap();
    assert_eq!(f.briefs.calls().len(), 2);
    assert_eq!(f.briefs.calls()[1].delivery_key, key, "the repeat carries the SAME key");
    assert!(matches!(f.store.starts[0].brief, BriefState::Existing { .. }));
    assert_eq!(f.store.starts[0].phase, Phase::Running);
}

/// An older `Attempted` receipt has no delivery key, so there a resend really
/// is blind. It stays blocked rather than being repaired by guessing.
#[test]
fn a_write_ahead_attempt_without_a_delivery_key_is_never_resent() {
    let mut f = Fixture::new([]);
    f.fail_at = Some(4); // brief attempt durable, key never minted (old shape)
    f.run(&request()).unwrap_err();
    f.fail_at = None;
    f.store.starts[0].brief = BriefState::Attempted;
    f.store.starts[0].brief_delivery_key = None;
    f.store.starts[0].brief_binding_digest = None;
    *f.host.disk.borrow_mut() = f.store.clone();
    let error = f.run(&request()).unwrap_err();
    assert!(error.contains("brief delivery is ambiguous"), "{error}");
    assert!(f.briefs.calls().is_empty(), "an old ambiguous attempt is never blindly resent");
}

/// The shapes `eidolon send --json --delivery-key` prints — the harness's own
/// `Correlated` — are what this service parses, and what it reports back.
#[test]
fn the_harness_correlated_shape_is_what_this_service_reads_and_reports() {
    let receipt = serde_json::json!({
        "session": "1792962214694", "delivery_key": "k1", "binding_digest": "d1",
        "record": 7, "log_binding": { "dev": 64768, "ino": 4242 }, "on_current_branch": true,
    });
    for (answer, expected) in [
        (serde_json::json!({ "Journaled": receipt }), "journaled"),
        (serde_json::json!({ "Existing": receipt }), "existing"),
        (serde_json::json!({ "Refused": { "kind": "conflict", "why": "other bytes" } }), "refused"),
        (serde_json::json!({ "Unknown": { "why": "silence" } }), "unknown"),
        (serde_json::json!("Unsupported"), "unsupported"),
    ] {
        let parsed: Correlated = serde_json::from_value(answer).unwrap();
        let round_trip: Correlated = serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(round_trip, parsed);
        assert_eq!(parsed.is_journaled(), matches!(expected, "journaled" | "existing"));
    }
}

#[test]
fn a_legacy_inbox_acceptance_is_not_reported_as_a_correlated_receipt() {
    let mut f = Fixture::with_answers();
    f.run(&request()).unwrap();
    f.store.starts[0].brief = BriefState::Accepted {
        outcome: eidolon_coordinator::Outcome::Delivered, words: "accepted, not consumed".into(),
    };
    *f.host.disk.borrow_mut() = f.store.clone();
    let error = f.run(&request()).unwrap_err();
    assert!(error.contains("legacy inbox transport"), "{error}");
    assert_eq!(f.briefs.calls().len(), 1, "an accepted legacy brief is not sent again");
}

// ---- the rank binding ------------------------------------------------------
//
// The digest is not a caller's to choose: the monitor reads the block's bytes and
// puts the digest into the immutable request before the intent is saved. These
// tests exercise the decision layer with a digest already bound, so they are about
// what a *retry* does with one — refuse, never re-tell.

fn ranked(digest: Option<&str>) -> Request {
    let mut r = request();
    r.rank = Some("miles".into());
    r.rank_digest = digest.map(str::to_string);
    r
}

#[test]
fn a_retry_whose_rank_block_bytes_changed_is_refused_and_names_the_file() {
    let mut f = Fixture::with_answers();
    // Save 1 is the durable intent; save 2 fails, so the operation is recorded and
    // bound, and no child was ever created for it.
    f.fail_at = Some(2);
    assert!(f.run(&ranked(Some("aaaa"))).is_err());
    assert_eq!(f.store.starts.len(), 1);
    assert_eq!(f.store.starts[0].phase, Phase::Intent);
    assert_eq!(f.store.starts[0].request.rank_digest.as_deref(), Some("aaaa"));

    f.fail_at = None;
    let error = f.run(&ranked(Some("bbbb"))).unwrap_err();
    assert!(error.contains("`miles` rank block changed since this operation was recorded"), "{error}");
    assert!(error.contains("aaaa") && error.contains("bbbb"), "both digests are named: {error}");
    assert_eq!(f.host.prepares, 1, "a changed block never reaches prepare");
    assert_eq!(f.store.starts[0].request.rank_digest.as_deref(), Some("aaaa"), "the record keeps what it was bound to");

    // The block appearing where the operation was bound to none is the same kind
    // of change, and is refused the same way.
    let mut f = Fixture::with_answers();
    f.fail_at = Some(2);
    assert!(f.run(&ranked(None)).is_err());
    f.fail_at = None;
    assert!(f.run(&ranked(Some("cccc"))).unwrap_err().contains("bound to no block"));
}

#[test]
fn the_same_rank_and_digest_remain_the_same_immutable_operation() {
    let mut f = Fixture::with_answers();
    let first = f.run(&ranked(Some("aaaa"))).unwrap();
    f.restart_monitor();
    assert_eq!(f.run(&ranked(Some("aaaa"))).unwrap(), first);
    assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
    assert_eq!(f.store.starts[0].request.rank.as_deref(), Some("miles"));
    assert_eq!(f.store.starts[0].request.rank_digest.as_deref(), Some("aaaa"));
}

#[test]
fn a_digest_without_a_rank_or_a_rank_outside_the_vocabulary_is_refused_before_anything_is_written() {
    let mut f = Fixture::with_answers();
    let mut unranked = request();
    unranked.rank_digest = Some("aaaa".into());
    assert!(f.run(&unranked).unwrap_err().contains("digest with no rank"));
    assert!(f.store.starts.is_empty());

    let mut unknown = request();
    unknown.rank = Some("duke".into());
    let error = f.run(&unknown).unwrap_err();
    assert!(error.contains("`duke` is not a rank"), "{error}");
    assert!(error.contains("imperator") && error.contains("miles"), "the vocabulary is named: {error}");
    assert!(f.store.starts.is_empty());
    assert_eq!(f.host.prepares, 0);
}

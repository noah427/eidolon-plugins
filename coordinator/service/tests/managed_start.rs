//! Failure injection at every durable checkpoint of the held-start seam.
//! The fake host counts model turns only at release and independently reads the
//! persisted store there. No real process, provider, roster or service is used.

use std::cell::RefCell;
use std::rc::Rc;
use eidolon_coordinator::{Delivery, Outcome, Store, managed_start::{self, BriefState, Host, LaunchState, Phase, Request}, store::SaveError};

fn request() -> Request {
    Request {
        operation: "start-1".into(), caller: "1792959651919".into(),
        workspace: "/isolated/work".into(), model: "mock".into(), persona: None, brief: "bounded task".into(),
    }
}

struct FakeHost {
    disk: Rc<RefCell<Store>>,
    state: LaunchState,
    prepares: usize,
    releases: usize,
    turns: usize,
    brief_sends: usize,
    lose_brief_reply: bool,
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
        self.state = LaunchState::Held { session: "1792962214694".into() };
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

    fn deliver_brief(&mut self, _: &Request, _: &str) -> Result<Delivery, String> {
        assert_eq!(self.disk.borrow().starts[0].brief, BriefState::Attempted);
        self.brief_sends += 1;
        if self.lose_brief_reply { return Err("lost send acknowledgement".into()) }
        Ok(Delivery { outcome: Outcome::Delivered, words: "accepted, not consumed".into() })
    }
}

struct Fixture {
    store: Store,
    host: FakeHost,
    saves: usize,
    fail_at: Option<usize>,
    uncertain: bool,
    uncertain_keeps_old: bool,
}

impl Fixture {
    fn new() -> Self {
        Self {
            store: Store::default(), saves: 0, fail_at: None, uncertain: false, uncertain_keeps_old: false,
            host: FakeHost {
                disk: Rc::new(RefCell::new(Store::default())), state: LaunchState::NotCreated,
                prepares: 0, releases: 0, turns: 0, brief_sends: 0, lose_brief_reply: false, lose_prepare_reply: false,
                lose_release_reply: false, ignore_release: false, lose_liveness_after_release: false,
            },
        }
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
    let mut f = Fixture::new();
    let first = f.run(&request()).unwrap();
    assert_eq!(f.host.turns, 1);
    assert_eq!(f.store.records.len(), 1);
    f.restart_monitor();
    assert_eq!(f.run(&request()).unwrap(), first);
    assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
    assert_eq!(f.store.records.len(), 1);
}

#[test]
fn each_persistence_failure_is_partial_not_success_and_restart_recovers_without_an_extra_child() {
    for uncertain in [false, true] {
        for checkpoint in 1..=6 {
            let mut f = Fixture::new();
            f.fail_at = Some(checkpoint);
            f.uncertain = uncertain;
            assert!(f.run(&request()).is_err(), "checkpoint {checkpoint}, uncertain {uncertain}");
            assert_eq!(f.host.turns, usize::from(checkpoint >= 4), "no turn before durable adoption/reflush");
            if checkpoint == 1 { assert_eq!(f.host.prepares, 0); }
            f.restart_monitor();
            f.fail_at = None;
            if (checkpoint == 4 && uncertain) || (checkpoint == 5 && !uncertain) {
                assert!(f.run(&request()).unwrap_err().contains("brief delivery is ambiguous"));
            } else {
                f.run(&request()).unwrap();
            }
            assert!(f.host.brief_sends <= 1, "ambiguous delivery must never be blindly repeated");
            assert_eq!((f.host.prepares, f.host.releases, f.host.turns), (1, 1, 1));
            assert_eq!(f.store.records.len(), 1);
        }
    }
}

#[test]
fn a_lost_prepare_reply_leaves_a_held_child_and_recovery_does_not_prepare_again() {
    let mut f = Fixture::new();
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
    let mut f = Fixture::new();
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
    let mut f = Fixture::new();
    f.host.ignore_release = true;
    assert!(f.run(&request()).unwrap_err().contains("release acknowledged, running state unconfirmed"));
    assert_eq!(f.host.turns, 0);
    assert_eq!(f.store.starts[0].phase, Phase::Supervised);
}

#[test]
fn missing_host_evidence_never_becomes_permission_to_spawn_a_replacement() {
    let mut f = Fixture::new();
    f.host.lose_prepare_reply = true;
    assert!(f.run(&request()).is_err());
    f.host.state = LaunchState::Unknown("not in the roster".into());
    assert!(f.run(&request()).is_err());
    assert_eq!((f.host.prepares, f.host.turns), (1, 0));
}

#[test]
fn changed_input_or_released_adoption_cannot_release_a_held_child() {
    let mut f = Fixture::new();
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
}

#[test]
fn uncertain_adoption_with_old_store_on_reload_rebinds_the_same_held_child_once() {
    let mut f = Fixture::new();
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
    let mut f = Fixture::new();
    f.host.lose_liveness_after_release = true;
    let error = f.run(&request()).unwrap_err();
    assert!(error.contains("release acknowledged"));
    assert!(error.contains("child may be running"));
    assert_eq!(f.host.turns, 1);
    assert_eq!(f.store.starts[0].phase, Phase::Supervised);
}

#[test]
fn bound_child_reported_never_created_is_a_repair_case_not_respawn() {
    let mut f = Fixture::new();
    f.fail_at = Some(3);
    assert!(f.run(&request()).is_err());
    f.fail_at = None;
    f.host.state = LaunchState::NotCreated;
    assert!(f.run(&request()).unwrap_err().contains("state disagreement needs repair"));
    assert_eq!((f.host.prepares, f.host.turns), (1, 0));
}

#[test]
fn lost_brief_acceptance_stays_ambiguous_and_is_never_blindly_resent() {
    let mut f = Fixture::new();
    f.host.lose_brief_reply = true;
    assert!(f.run(&request()).unwrap_err().contains("acceptance ambiguous"));
    assert_eq!(f.host.brief_sends, 1);
    f.restart_monitor();
    f.host.state = LaunchState::Unknown("host restarted and has no process receipt".into());
    assert!(f.run(&request()).unwrap_err().contains("brief delivery is ambiguous"));
    assert_eq!(f.host.brief_sends, 1);
    assert_eq!(f.store.starts[0].brief, BriefState::Attempted);
}

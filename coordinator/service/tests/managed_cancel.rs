//! Cancellation decision seam; fake host plus durability failure injection.
use std::{cell::RefCell, rc::Rc};
use eidolon_coordinator::{Adoption, Store, managed_start::{self, BriefState, CancelHost, CancelOutcome, CancelRosterEvidence, ExitEvidence, Phase, Request, StartRecord}, store::SaveError};

struct Host {
    disk: Rc<RefCell<Store>>,
    refusal: Option<&'static str>,
    exit: bool,
    closes: usize,
}
impl CancelHost for Host {
    fn check_cancel(&mut self, _: &Request) -> Result<(), String> {
        self.refusal.map_or(Ok(()), |e| Err(e.into()))
    }
    fn cancel(&mut self, _: &Request) -> Result<ExitEvidence, String> {
        assert_eq!(self.disk.borrow().starts[0].cancellation.as_ref().unwrap().outcome, CancelOutcome::Attempted);
        self.closes += 1;
        if !self.exit { return Err("exit observation unconfirmed".into()) }
        Ok(ExitEvidence { observed_at_ms: 101, status: "exit status: 0".into(), code: Some(0), signal: None,
            roster_before_close: CancelRosterEvidence { bound_session: Some("child".into()), queried: true, matching_rows: Some(0), reached: None } })
    }
}
fn fixture() -> (Store, Host) {
    let mut store = Store::default();
    store.starts.push(StartRecord {
        request: Request { caller: "caller".into(), operation: "op".into(), workspace: "/fixture".into(), model: "mock".into(),
            persona: None, brief: "task".into(),
            rank: None, rank_digest: None,
            shape: eidolon_coordinator::managed_start::LaunchShape::Headless, destination: None },
        phase: Phase::Supervised, session: Some("child".into()), adoption_epoch: Some(1), brief: BriefState::NotAttempted,
        brief_delivery_key: None, brief_binding_digest: None,
        wake: eidolon_coordinator::managed_start::WakeState::NotAttempted, cancellation: None,
    });
    store.records.push(Adoption { epoch: 1, coordinator: "caller".into(), coordinator_id: String::new(), worker: "child".into(),
        worker_id_seen: String::new(), name: String::new(), note: String::new(), adopted_at_ms: 1,
        released_at_ms: None, release_reason: String::new() });
    let host = Host { disk: Rc::new(RefCell::new(store.clone())), refusal: None, exit: true, closes: 0 };
    (store, host)
}
fn cancel(store: &mut Store, host: &mut Host, fail: Option<(usize, bool)>) -> Result<ExitEvidence, String> {
    let disk = host.disk.clone();
    let load = disk.clone();
    let mut saves = 0;
    managed_start::cancel(store, "caller", "op", "stop held launch", host, &mut |next| {
        saves += 1;
        if fail == Some((saves, false)) { return Err(SaveError::Unchanged("before rename".into())) }
        *disk.borrow_mut() = next.clone();
        if fail == Some((saves, true)) { return Err(SaveError::Uncertain("after rename".into())) }
        Ok(())
    }, &mut || Ok(load.borrow().clone()), 100)
}
#[test]
fn exit_receipt_does_not_release_adoption_and_explicit_adoption_release_then_works() {
    let (mut store, mut host) = fixture();
    let evidence = cancel(&mut store, &mut host, None).unwrap();
    assert_eq!(evidence.code, Some(0));
    assert!(matches!(store.starts[0].cancellation.as_ref().unwrap().outcome, CancelOutcome::Exited(_)));
    assert!(store.records[0].released_at_ms.is_none());
    assert_eq!(host.closes, 1);
    assert_eq!(cancel(&mut store, &mut host, None).unwrap(), evidence);
    assert_eq!(host.closes, 1, "completed call only reads the stored receipt");
    eidolon_coordinator::release_adoption(&mut store, "caller", "child", "exit confirmed", 102,
        &mut |_| Ok(()), &mut || unreachable!()).unwrap();
    assert_eq!(store.records[0].released_at_ms, Some(102));
}
#[test]
fn timeout_retains_supervision_and_blocks_adoption_release() {
    let (mut store, mut host) = fixture();
    host.exit = false;
    assert!(cancel(&mut store, &mut host, None).unwrap_err().contains("exit unconfirmed"));
    assert!(store.records[0].released_at_ms.is_none());
    assert!(host.disk.borrow().records[0].released_at_ms.is_none());
    assert_eq!(store.starts[0].cancellation.as_ref().unwrap().outcome, CancelOutcome::Attempted);
    assert!(eidolon_coordinator::release_adoption(&mut store, "caller", "child", "stop", 102,
        &mut |_| panic!("must not save a release"), &mut || unreachable!()).unwrap_err().contains("exit is unconfirmed"));
}
#[test]
fn persistence_failures_never_forge_exit_or_drop_supervision() {
    for checkpoint in 1..=2 {
        for uncertain in [false, true] {
            let (mut store, mut host) = fixture();
            assert!(cancel(&mut store, &mut host, Some((checkpoint, uncertain))).is_err());
            assert_eq!(host.closes, usize::from(checkpoint == 2));
            assert!(store.records[0].released_at_ms.is_none());
            assert!(host.disk.borrow().records[0].released_at_ms.is_none());
            if checkpoint == 2 && !uncertain {
                assert_eq!(store.starts[0].cancellation.as_ref().unwrap().outcome, CancelOutcome::Attempted);
            }
        }
    }
}
#[test]
fn restart_missing_ownership_refuses_even_with_durable_exit() {
    let (mut store, mut host) = fixture();
    cancel(&mut store, &mut host, None).unwrap();
    store = serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
    host.refusal = Some("ownership lost after restart");
    assert!(cancel(&mut store, &mut host, None).unwrap_err().contains("ownership lost"));
    assert_eq!(host.closes, 1);
}
#[test]
fn durable_boundary_refusals_do_not_touch_child() {
    for case in 0..6 {
        let (mut store, mut host) = fixture();
        match case {
            0 => store.starts[0].phase = Phase::Running,
            1 => store.starts[0].brief = BriefState::Attempted,
            2 => store.records[0].epoch = 2,
            3 => store.records[0].released_at_ms = Some(2),
            4 => store.starts[0].request.caller = "another".into(),
            _ => store.starts.push(store.starts[0].clone()),
        }
        assert!(cancel(&mut store, &mut host, None).is_err(), "case {case}");
        assert_eq!(host.closes, 0);
        assert!(store.starts[0].cancellation.is_none());
    }
}
#[test]
fn unbound_owned_launch_can_be_cancelled_without_inventing_an_adoption() {
    let (mut store, mut host) = fixture();
    store.starts[0].phase = Phase::Intent;
    store.starts[0].session = None;
    store.starts[0].adoption_epoch = None;
    store.records.clear();
    cancel(&mut store, &mut host, None).unwrap();
    assert!(store.records.is_empty());
}

impl managed_start::Host for Host {
    fn inspect(&mut self, _: &Request) -> Result<managed_start::LaunchState, String> { panic!("cancelled start must not inspect or release") }
    fn prepare(&mut self, _: &Request) -> Result<(), String> { panic!("cancelled start must not prepare") }
    fn release(&mut self, _: &Request, _: &str) -> Result<(), String> { panic!("cancelled start must not release") }
    fn deliver_brief(&mut self, _: &Request, _: &str, _: &str, _: &str) -> Result<managed_start::Correlated, String> { panic!("no brief") }
    fn wake(&mut self, _: &Request, _: &str, _: &str) -> Result<eidolon_coordinator::Delivery, String> { panic!("cancelled start must not wake") }
}
#[test]
fn pending_cancel_survives_reload_blocks_start_and_refuses_changed_reason() {
    let (mut store, mut host) = fixture();
    host.exit = false;
    assert!(cancel(&mut store, &mut host, None).is_err());
    store = serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
    let request = store.starts[0].request.clone();
    assert!(managed_start::start(&mut store, &request, &mut host,
        &mut |_| panic!("no new start save"), &mut || unreachable!(), 102).unwrap_err().contains("cancellation intent"));
    assert!(managed_start::cancel(&mut store, "caller", "op", "different", &mut host,
        &mut |_| panic!("no changed intent save"), &mut || unreachable!(), 102).unwrap_err().contains("immutable"));
    assert_eq!(host.closes, 1);
}

#[test]
fn explicit_retries_keep_pending_supervision_until_late_exit_is_observed() {
    let (mut store, mut host) = fixture();
    host.exit = false;
    for _ in 0..2 {
        assert!(cancel(&mut store, &mut host, None).unwrap_err().contains("exit unconfirmed"));
        assert_eq!(store.starts[0].cancellation.as_ref().unwrap().outcome, CancelOutcome::Attempted);
        assert!(store.records[0].released_at_ms.is_none());
        assert!(eidolon_coordinator::release_adoption(&mut store, "caller", "child", "stop", 102,
            &mut |_| panic!("no release before exit"), &mut || unreachable!()).is_err());
    }
    host.exit = true;
    let evidence = cancel(&mut store, &mut host, None).unwrap();
    assert_eq!(evidence.code, Some(0));
    assert!(store.records[0].released_at_ms.is_none());
    assert_eq!(store.starts[0].cancellation.as_ref().unwrap().requested_at_ms, 100);
}

//! Identity regressions for the lifecycle consumer: a registration is not an
//! address, and a restart of the same log does not create a new adoption.
//! These exercise the existing monitor only; no process or service is started.

use eidolon_coordinator::{Adoption, Row, Store, observe};
use serde_json::json;

const WORKER: &str = "1792962214694";

fn adopted() -> Adoption {
    Adoption {
        epoch: 3,
        coordinator: "1792959651919".into(),
        coordinator_id: "old-coordinator-locator".into(),
        worker: WORKER.into(),
        worker_id_seen: "old-worker-locator".into(),
        name: "lifecycle".into(),
        note: "a brief, not an identity claim".into(),
        adopted_at_ms: 1,
        released_at_ms: None,
        release_reason: String::new(),
    }
}

fn halted(session: &str, locator: &str, outcome: u64) -> Row {
    Row {
        id: locator.into(),
        session: Some(session.into()),
        state: "stuck".into(),
        last: "iteration limit".into(),
        outcome_at: Some(outcome),
        outcome_observed: Some(0),
        started_ms: Some(10),
        reached: true,
    }
}

#[test]
fn same_log_resume_preserves_epoch_and_dedup_across_store_reload() {
    let mut store = Store { records: vec![adopted()], ..Store::default() };
    observe(&store.records, &mut store.notices, &[halted(WORKER, "old-worker-locator", 7)], 20);
    assert_eq!(store.notices.len(), 1);
    let first_id = store.notices[0].notice_id.clone();

    // A monitor restart reloads its own state; the worker's same-log resume
    // may publish a different registration instance but the same old ending.
    let bytes = serde_json::to_vec(&store).unwrap();
    let mut store: Store = serde_json::from_slice(&bytes).unwrap();
    let mut resumed = halted(WORKER, "new-worker-locator", 7);
    resumed.started_ms = Some(30);
    observe(&store.records, &mut store.notices, &[resumed.clone()], 40);
    assert_eq!(store.notices.len(), 1);
    assert_eq!(store.notices[0].notice_id, first_id);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].epoch, 3);
    assert!(store.records[0].released_at_ms.is_none());

    resumed.outcome_at = Some(9);
    observe(&store.records, &mut store.notices, &[resumed], 50);
    assert_eq!(store.notices.len(), 2);
    assert_ne!(store.notices[1].notice_id, first_id);
    assert_eq!(store.notices[1].epoch, 3);
    assert_eq!(store.notices[1].worker, WORKER);
}

#[test]
fn a_reused_registration_does_not_rebind_an_adoption() {
    let records = vec![adopted()];
    let mut notices = Vec::new();
    let replacement = halted("1792962214695", "old-worker-locator", 7);
    let notes = observe(&records, &mut notices, &[replacement], 20);
    assert!(notices.is_empty());
    assert!(notes.iter().any(|n| n.contains("no live session carries this address")));
    assert_eq!(records[0].worker, WORKER);
    assert!(records[0].released_at_ms.is_none());

    // A roster miss is not a retirement: the original address can return.
    observe(&records, &mut notices, &[halted(WORKER, "new-worker-locator", 8)], 30);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].worker, WORKER);
    assert_eq!(notices[0].epoch, 3);
}

#[test]
fn metadata_and_partial_spellings_cannot_substitute_for_the_exact_address() {
    let records = vec![adopted()];
    for other in ["179296221469", "1792962214694-1", "/sessions/1792962214694.eid", WORKER] {
        // Deliberately keep the locator equal to WORKER while varying the
        // address: matching `id` instead of `session` must fail this test.
        let row: Row = serde_json::from_value(json!({
            "id": WORKER,
            "session": other,
            "state": "stuck",
            "last": "iteration limit",
            "outcome_at": 7,
            "reached": true,
            "label": WORKER,
            "tags": [WORKER, "persona:μέλι", "provider:mock", "model:mock"],
            "log": "/sessions/1792962214694.eid"
        })).unwrap();
        let mut notices = Vec::new();
        observe(&records, &mut notices, &[row], 20);
        if other == WORKER {
            assert_eq!(notices.len(), 1, "positive control: the same JSON shape is a usable roster row");
        } else {
            assert!(notices.is_empty(), "metadata must not alias {other} to {WORKER}");
        }
    }
}

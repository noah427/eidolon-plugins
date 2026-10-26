//! **The destination is resolved once and pinned** — the sequence that made the
//! first version of this ruling wrong, and the boundary the capability chain was
//! broken at.
//!
//! No compositor is contacted, no process is started and no window is moved: the
//! resolver is a fake whose answer can be *changed between two attempts*, which
//! is the only way to test that a retry does not resolve a second time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use eidolon_coordinator::ancestry::{Client, PlacementRefusal};
use eidolon_coordinator::managed_start::{BriefState, LaunchShape, Phase, Request, StartRecord, WakeState};
use eidolon_coordinator::{
    destination_for_operation, chain_root, request_from_args, Delivery, Destination, Resolver, Row, Store,
};

/// A resolver whose answer can change, and which counts how often it was asked.
struct Shifting {
    workspace: Mutex<i64>,
    calls: AtomicUsize,
}

impl Shifting {
    fn new(workspace: i64) -> Self {
        Self { workspace: Mutex::new(workspace), calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn moved_to(&self, workspace: i64) {
        *self.workspace.lock().unwrap() = workspace;
    }
}

impl Resolver for Shifting {
    fn destination(&self, caller: &str, _store: &Store, _rows: &[Row]) -> Result<Destination, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Destination {
            workspace: *self.workspace.lock().unwrap(),
            root: format!("root-of-{caller}"),
            window: "0xaaaa".into(),
        })
    }
}

fn visible_args(operation: &str) -> serde_json::Value {
    serde_json::json!({
        "caller": "1793000000001",
        "operation": operation,
        "workspace": "/tmp/work",
        "model": "mock",
        "brief": "bounded task",
        "visible": true,
    })
}

fn record(request: &Request) -> StartRecord {
    StartRecord {
        request: request.clone(),
        phase: Phase::Intent,
        session: None,
        adoption_epoch: None,
        brief: BriefState::NotAttempted,
        brief_delivery_key: None,
        brief_binding_digest: None,
        wake: WakeState::NotAttempted,
        cancellation: None,
    }
}

/// **The sequence the ruling names.** Attempt, resolve, persist, then move the
/// Imperator's workspace and retry the same operation: the recorded destination
/// is unchanged, the resolver did not run a second time, and the reconciled
/// request is *equal to the recorded one* — which is what stops `start` refusing
/// a legitimate retry as changed input.
#[test]
fn a_retry_reuses_the_pinned_destination_and_never_resolves_twice() {
    let resolver = Shifting::new(11);
    let mut store = Store::default();
    let mut rows: Vec<Row> = Vec::new();

    // Attempt 1: a terminal operation with no destination. Resolved once.
    let mut first = request_from_args(&visible_args("op-1")).expect("valid args");
    assert_eq!(first.shape, LaunchShape::Terminal, "`visible: true` is what makes it terminal");
    assert_eq!(first.destination, None, "a caller cannot supply one, and the tool sends none");
    let resolved = destination_for_operation(&store, &rows, &resolver, "1793000000001", &mut first)
        .expect("resolves");
    assert_eq!(resolved.as_ref().map(|d| d.workspace), Some(11));
    assert_eq!(first.destination, Some(11), "the resolved value goes into the request itself");
    assert_eq!(resolver.calls(), 1);
    store.starts.push(record(&first));

    // The Imperator moves. A retry of the SAME operation must not notice.
    resolver.moved_to(12);
    let mut retry = request_from_args(&visible_args("op-1")).expect("valid args");
    let second = destination_for_operation(&store, &rows, &resolver, "1793000000001", &mut retry)
        .expect("reconciles");
    assert!(second.is_none(), "a retry resolves nothing: it reconciles against the record");
    assert_eq!(resolver.calls(), 1, "the resolver ran exactly once for this operation");
    assert_eq!(retry.destination, Some(11), "the admitted destination, not the new workspace");
    assert_eq!(
        retry, store.starts[0].request,
        "the reconciled request must equal the recorded one, or `start` refuses it as changed input"
    );

    // A *different* operation is a different question and is resolved afresh.
    let mut other = request_from_args(&visible_args("op-2")).expect("valid args");
    let third = destination_for_operation(&store, &rows, &resolver, "1793000000001", &mut other)
        .expect("resolves");
    assert_eq!(third.as_ref().map(|d| d.workspace), Some(12), "the new workspace, for a new operation");
    assert_eq!(resolver.calls(), 2);
    let _ = rows.drain(..);
}

/// A headless launch resolves nothing, calls nothing and carries nothing — this
/// is the path every launch in the fleet takes today.
#[test]
fn a_headless_request_never_resolves() {
    let resolver = Shifting::new(11);
    let store = Store::default();
    let mut args = visible_args("op-headless");
    args.as_object_mut().unwrap().remove("visible");
    let mut request = request_from_args(&args).expect("valid args");
    assert_eq!(request.shape, LaunchShape::Headless);
    let resolved = destination_for_operation(&store, &[], &resolver, "1793000000001", &mut request)
        .expect("no resolution needed");
    assert!(resolved.is_none());
    assert_eq!(resolver.calls(), 0, "a headless launch has no window and asks no compositor");
    assert_eq!(request.destination, None);
}

/// **The boundary that was broken.** The caller may ask for a visible launch and
/// may not name a shape or a workspace; a non-boolean `visible` is refused rather
/// than read as "false".
#[test]
fn a_caller_asks_for_visible_and_never_names_a_shape_or_a_destination() {
    let base = |patch: serde_json::Value| {
        let mut args = visible_args("op-boundary");
        let object = args.as_object_mut().unwrap();
        for (k, v) in patch.as_object().unwrap() {
            object.insert(k.clone(), v.clone());
        }
        args
    };
    let refused = request_from_args(&base(serde_json::json!({"shape": "Terminal"}))).unwrap_err();
    assert!(refused.contains("`shape` is not a caller's input"), "{refused}");
    let refused = request_from_args(&base(serde_json::json!({"destination": 11}))).unwrap_err();
    assert!(refused.contains("`destination` is not a caller's input"), "{refused}");
    let refused = request_from_args(&base(serde_json::json!({"visible": "yes"}))).unwrap_err();
    assert!(refused.contains("must be true or false"), "{refused}");
    // `false` and absent are the same launch: headless, no destination.
    for said in [serde_json::json!(false), serde_json::Value::Null] {
        let request = request_from_args(&base(serde_json::json!({"visible": said}))).expect("valid args");
        assert_eq!(request.shape, LaunchShape::Headless);
        assert_eq!(request.destination, None);
    }
}

fn row(session: &str) -> Row {
    Row {
        id: format!("{session}-reg"),
        session: Some(session.to_string()),
        state: "busy".into(),
        last: "completed".into(),
        outcome_at: None,
        outcome_observed: None,
        started_ms: None,
        reached: true,
    }
}

fn start_record(caller: &str, child: &str, rank: &str) -> StartRecord {
    let mut request = Request {
        operation: format!("op-{child}"),
        caller: caller.into(),
        workspace: "/w".into(),
        model: "mock".into(),
        persona: None,
        brief: "b".into(),
        rank: Some(rank.into()),
        rank_digest: None,
        shape: LaunchShape::Headless,
        destination: None,
    };
    request.destination = None;
    let mut record = record(&request);
    record.phase = Phase::Running;
    record.session = Some(child.into());
    record
}

/// **The chain, walked from a Miles to its root.** A Legatus's worker lands on the
/// *Imperator's* workspace plus one, which is not the Legatus's plus one — and the
/// root is the first session with no start record of its own, which is what an
/// Imperator is. No tag, nothing self-asserted.
#[test]
fn the_chain_walks_past_the_legatus_to_the_recordless_root() {
    let rows = vec![
        row("1793000000001"), // the Miles asking
        row("1793000000002"), // its Legatus
        row("1793000000003"), // the root: started by hand, so no record names it
    ];
    let starts = vec![
        start_record("1793000000002", "1793000000001", "miles"),
        start_record("1793000000003", "1793000000002", "legatus"),
    ];
    assert_eq!(chain_root(&starts, &rows, "1793000000001").unwrap(), "1793000000003");
    // The Legatus's own answer is the same root, not itself.
    assert_eq!(chain_root(&starts, &rows, "1793000000002").unwrap(), "1793000000003");
    // The root answers with itself: by this definition the operator-launched
    // session is the Imperator, so its own spawns are relative to it.
    assert_eq!(chain_root(&starts, &rows, "1793000000003").unwrap(), "1793000000003");
}

/// **The three shapes, and the first one is the primary path.** A direct start by
/// the root — the operator's own Imperator starting a worker, with no record above
/// it — must **place**, not refuse: the walk terminates on the caller itself, and
/// that is the normal case, not a fault.
#[test]
fn a_direct_start_by_the_root_places_beside_the_root() {
    let rows = vec![
        row("1793000000003"),
        row("1793000000002"),
        row("1793000000001"),
    ];
    // The root has no record of its own; it starts the worker directly.
    let starts: Vec<eidolon_coordinator::managed_start::StartRecord> = Vec::new();
    assert_eq!(
        chain_root(&starts, &rows, "1793000000003").expect("a direct start is placeable"),
        "1793000000003"
    );
    // Two hops: root → legatus → worker.
    let starts = vec![
        start_record("1793000000003", "1793000000002", "legatus"),
        start_record("1793000000002", "1793000000001", "miles"),
    ];
    assert_eq!(chain_root(&starts, &rows, "1793000000001").unwrap(), "1793000000003");
    // And one hop, which is the same answer for the legatus itself.
    assert_eq!(chain_root(&starts, &rows, "1793000000002").unwrap(), "1793000000003");
}

/// A link with no roster row is a refusal naming where the walk stopped — a root
/// named only by a record is not a session on this machine — and **never** a fall
/// back to the caller's own workspace.
#[test]
fn a_chain_with_a_link_that_is_not_live_refuses_and_never_falls_back() {
    // The Legatus is recorded as the Miles's parent but has no row: no process.
    let rows = vec![row("1793000000001")];
    let starts = vec![start_record("1793000000002", "1793000000001", "miles")];
    let refused = chain_root(&starts, &rows, "1793000000001").unwrap_err();
    assert!(refused.contains("no row for `1793000000002`"), "{refused}");
    assert!(refused.contains("live process"), "{refused}");
    // A caller with no row at all is a different refusal, and still not a guess.
    let refused = chain_root(&starts, &rows, "1793000000099").unwrap_err();
    assert!(refused.contains("carries no row"), "{refused}");
    // A cycle is bounded rather than followed for ever.
    let rows = vec![row("a"), row("b")];
    let starts = vec![start_record("b", "a", "miles"), start_record("a", "b", "miles")];
    assert!(chain_root(&starts, &rows, "a").unwrap_err().contains("loops"));
    // And a chain longer than the bound is refused, not truncated.
    let mut rows = Vec::new();
    let mut starts = Vec::new();
    let names: Vec<String> = (0..10).map(|i| format!("s{i}")).collect();
    for name in &names {
        rows.push(row(name));
    }
    for pair in names.windows(2) {
        starts.push(start_record(&pair[1], &pair[0], "miles"));
    }
    assert!(chain_root(&starts, &rows, "s0").unwrap_err().contains("8 links"));
}

/// **The fleet as it is today**, and the point of the derivation: the walk from
/// this lane reaches `1793036140983` — a session no record names as a child — and
/// that is the root, so resolution answers instead of refusing.
#[test]
fn the_current_fleet_resolves_to_its_recordless_root() {
    let rows = vec![
        row("1793036858962"),
        row("1793036728114"),
        row("1793036140983"),
    ];
    // Exactly the records the live store carries for this chain.
    let starts = vec![
        start_record("1793036728114", "1793036858962", "miles"),
        start_record("1793036140983", "1793036728114", "legatus"),
    ];
    assert_eq!(chain_root(&starts, &rows, "1793036858962").unwrap(), "1793036140983");
}

/// The chain's own refusal shape, so a caller reads a sentence rather than a
/// variant name.
#[test]
fn a_broken_chain_reads_as_a_sentence() {
    let said = PlacementRefusal::ChainBroken {
        at: "1793000000002".into(),
        why: "no start record names who started it".into(),
    }
    .to_string();
    assert!(said.contains("does not reach an Imperator"), "{said}");
    assert!(said.contains("1793000000002"), "{said}");
}

/// **Guard four still refuses, from the tool's own payload.** A visible request
/// built out of the arguments the tool now sends reaches `prepare` with a shape,
/// a destination and no capability — and is refused before any process exists.
#[test]
fn a_visible_request_from_the_tools_payload_still_refuses_at_the_guard() {
    use eidolon_coordinator::managed_start::Host;
    use eidolon_coordinator::process_host::ProcessHost;
    use eidolon_coordinator::managed_start::{BriefTransport, Correlated};
    use eidolon_coordinator::{Row as _Row, Roster};

    struct NoRows;
    impl Roster for NoRows {
        fn rows(&self) -> Result<Vec<_Row>, String> {
            Ok(Vec::new())
        }
    }
    struct NoBriefs;
    impl BriefTransport for NoBriefs {
        fn send_correlated(&self, _: &str, _: &str, _: &str, _: &str) -> Result<Correlated, String> {
            panic!("a refused launch must not send a brief")
        }
        fn wake(&self, _: &str, _: &str) -> Result<Delivery, String> {
            panic!("a refused launch must not wake a worker")
        }
    }
    struct NeverCalled;
    impl eidolon_coordinator::ancestry::Placement for NeverCalled {
        fn place(
            &self,
            _: u32,
            _: i64,
        ) -> Result<eidolon_coordinator::ancestry::Placed, String> {
            panic!("placement must not run for a launch refused in preparation")
        }
    }

    let logs = std::env::temp_dir().join(format!("destination-pinning-{}", std::process::id()));
    let mut host = ProcessHost::new(
        PathBuf::from("/bin/true"),
        logs.clone(),
        Box::new(NoRows),
        Box::new(NoBriefs),
    )
    .with_terminal("/bin/true")
    .with_placement(std::sync::Arc::new(NeverCalled));

    let mut args = visible_args("op-guard");
    args.as_object_mut().unwrap().insert("caller".into(), "1793000000001".into());
    let mut request = request_from_args(&args).expect("valid args");
    // The resolution the service would do, supplied here directly.
    request.destination = Some(11);
    host.register_fresh(&request).expect("registered");
    let refused = host.prepare(&request).unwrap_err();
    assert!(refused.contains("no-focus map"), "{refused}");

    // And the mutation this lane cannot spy on directly: a caller that asked for
    // `visible` while nothing resolved a destination does **not** get a launch
    // that merely forgot to place its window — the guard refuses it, naming the
    // missing destination. That is the loud failure in place of a test that could
    // watch the Rune tool and cannot exist in this repo.
    let mut unresolved = request_from_args(&args).expect("valid args");
    assert_eq!(unresolved.destination, None, "the tool's payload carries no destination");
    unresolved.operation = "op-guard-unresolved".into();
    host.register_fresh(&unresolved).expect("registered");
    let refused = host.prepare(&unresolved).unwrap_err();
    assert!(refused.contains("needs a destination workspace"), "{refused}");
    let _ = std::fs::remove_dir_all(&logs);
}

/// The client seam's shape, asserted because the resolver reads exactly this and
/// nothing else from the compositor.
#[test]
fn a_client_carries_the_workspace_the_destination_is_derived_from() {
    let client = Client {
        address: "0x57ea".into(),
        pid: 280282,
        workspace_id: 10,
        workspace_name: "10".into(),
        class: "foot".into(),
        title: "Meli".into(),
    };
    assert_eq!(client.workspace_id + 1, 11);
}

//! Exact-argv process adapter for the admitted managed-start operation.
//! The inherited channel is the harness's enforced pre-runtime hold. No shell,
//! `--yolo`, operator context verb or caller-selected executable is involved.
//! After this service restarts an operation without a live local receipt stays
//! Unknown: a missing handle is not permission to create another worker.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{fs::{DirBuilderExt, OpenOptionsExt}, net::UnixStream, process::{CommandExt, ExitStatusExt}};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde_json::{Value, json};
use crate::{Roster, Transport, managed_start::{CancelHost, CancelRosterEvidence, ExitEvidence, Host, LaunchState, Request}};

struct Launch {
    request: Request,
    child: Option<Child>,
    input: Option<ChildStdin>,
    channel: Option<UnixStream>,
    instance: String,
    session: Option<String>,
    attempted: bool,
    release_attempted: bool,
    release_acknowledged: bool,
    cancel_attempted: bool,
    cancel_roster: Option<CancelRosterEvidence>,
}

pub struct ProcessHost {
    executable: PathBuf,
    logs: PathBuf,
    roster: Box<dyn Roster>,
    transport: Box<dyn Transport>,
    launches: HashMap<(String, String), Launch>,
    timeout: Duration,
}

impl ProcessHost {
    pub fn new(executable: PathBuf, logs: PathBuf, roster: Box<dyn Roster>, transport: Box<dyn Transport>) -> Self {
        Self { executable, logs, roster, transport, launches: HashMap::new(), timeout: Duration::from_secs(15) }
    }

    /// Called ONLY while holding the service Gate, for an operation not yet in
    /// its durable store. Never call this to "recover" an existing operation.
    pub fn register_fresh(&mut self, request: &Request) -> Result<(), String> {
        let key = (request.caller.clone(), request.operation.clone());
        if let Some(launch) = self.launches.get(&key) {
            return if launch.request == *request { Ok(()) } else { Err("fresh operation input changed".into()) };
        }
        self.launches.insert(key, Launch {
            request: request.clone(), child: None, input: None, channel: None, instance: String::new(),
            session: None, attempted: false, release_attempted: false, release_acknowledged: false, cancel_attempted: false, cancel_roster: None,
        });
        Ok(())
    }

    fn launch(&mut self, request: &Request) -> Result<&mut Launch, String> {
        let launch = self.launches.get_mut(&(request.caller.clone(), request.operation.clone()))
            .ok_or("host restarted or has no receipt for this operation; retain supervision and reconcile, never respawn")?;
        if launch.request != *request { return Err("host receipt belongs to different immutable input".into()) }
        Ok(launch)
    }
}

fn receipt(channel: &mut UnixStream) -> Result<Value, String> {
    let mut bytes = Vec::new();
    for _ in 0..4096 {
        let mut byte = [0];
        channel.read_exact(&mut byte).map_err(|e| format!("reading child receipt: {e}"))?;
        if byte[0] == b'\n' { return serde_json::from_slice(&bytes).map_err(|e| format!("decoding child receipt: {e}")) }
        bytes.push(byte[0]);
    }
    Err("child receipt exceeds 4096 bytes".into())
}

fn matches(value: &Value, state: &str, request: &Request, instance: &str, session: Option<&str>) -> bool {
    value["state"] == state && value["operation"] == request.operation && value["instance"] == instance
        && value["session"].as_str().is_some_and(|s| !s.is_empty() && session.is_none_or(|wanted| wanted == s))
}

fn unique_reached(roster: &dyn Roster, session: &str) -> Result<bool, String> {
    let rows = roster.rows()?;
    let found: Vec<_> = rows.iter().filter(|r| r.session.as_deref() == Some(session)).collect();
    match found.as_slice() {
        [] => Ok(false),
        [row] => Ok(row.reached),
        _ => Err("multiple live rows claim the managed session; refusing readiness".into()),
    }
}

impl Host for ProcessHost {
    fn inspect(&mut self, request: &Request) -> Result<LaunchState, String> {
        let launch = match self.launch(request) {
            Ok(launch) => launch,
            Err(e) => return Ok(LaunchState::Unknown(e)),
        };
        if !launch.attempted { return Ok(LaunchState::NotCreated) }
        let Some(child) = launch.child.as_mut() else {
            return Ok(LaunchState::Unknown("launch attempted without an owned child receipt; no automatic retry".into()));
        };
        if let Some(status) = child.try_wait().map_err(|e| format!("checking owned child: {e}"))? {
            return Ok(LaunchState::Unknown(format!("owned child exited ({status}); no automatic replacement")));
        }
        let Some(session) = launch.session.clone() else {
            return Ok(LaunchState::Unknown("child exists but Held receipt is not verified".into()));
        };
        if !launch.release_attempted { return Ok(LaunchState::Held { session }) }
        if !launch.release_acknowledged {
            return Ok(LaunchState::Unknown("release acknowledgement is unconfirmed; do not repeat it".into()));
        }
        if unique_reached(self.roster.as_ref(), &session)? {
            Ok(LaunchState::Running { session })
        } else {
            Ok(LaunchState::Unknown("owned released child is not currently reached in the roster".into()))
        }
    }

    fn prepare(&mut self, request: &Request) -> Result<(), String> {
        let executable = self.executable.clone();
        let logs = self.logs.clone();
        let timeout = self.timeout;
        let launch = self.launch(request)?;
        if launch.attempted { return Err("launch already attempted; reconcile its receipt, never spawn twice".into()) }
        // Even a failed syscall is an attempted launch, not inferred NotCreated.
        launch.attempted = true;
        let workspace = PathBuf::from(&request.workspace).canonicalize().map_err(|e| format!("workspace: {e}"))?;
        if !workspace.is_dir() { return Err("workspace is not a directory".into()) }
        let mut random = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| format!("creating launch instance: {e}"))?;
        launch.instance = random.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&logs).map_err(|e| format!("launch logs: {e}"))?;
        let output = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(logs.join(format!("{}.log", launch.instance))).map_err(|e| format!("launch output: {e}"))?;
        let error = output.try_clone().map_err(|e| format!("launch stderr: {e}"))?;
        let (parent, child_end) = UnixStream::pair().map_err(|e| format!("launch channel: {e}"))?;
        parent.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        parent.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let fd = child_end.as_raw_fd();
        let mut command = Command::new(executable);
        command.current_dir(workspace).args([
            "--managed-start-fd", &fd.to_string(), "--managed-operation", &request.operation,
            "--managed-instance", &launch.instance, "chat", "--model", &request.model,
        ]).stdin(Stdio::piped()).stdout(Stdio::from(output)).stderr(Stdio::from(error));
        if let Some(persona) = &request.persona { command.args(["--persona", persona]); }
        // Child gets only its channel endpoint; the opposite endpoint retains
        // CLOEXEC. The harness immediately restores CLOEXEC on its own endpoint.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| format!("starting held harness: {e}"))?;
        launch.input = child.stdin.take(); // keep the persistent chat alive, never feed approval answers
        launch.child = Some(child);
        drop(child_end);
        launch.channel = Some(parent);
        let held = receipt(launch.channel.as_mut().unwrap())?;
        if !matches(&held, "held", request, &launch.instance, None)
            || held["pid"].as_u64() != launch.child.as_ref().map(|c| u64::from(c.id()))
        {
            return Err("Held receipt does not identify this owned child and operation instance".into());
        }
        launch.session = held["session"].as_str().map(str::to_string);
        Ok(())
    }

    fn release(&mut self, request: &Request, session: &str) -> Result<(), String> {
        let timeout = self.timeout;
        let launch = self.launch(request)?;
        if launch.session.as_deref() != Some(session) || launch.release_attempted || launch.cancel_attempted {
            return Err("release is not for a uniquely held matching child".into());
        }
        launch.release_attempted = true;
        let channel = launch.channel.as_mut().ok_or("held channel was lost")?;
        let mut bytes = serde_json::to_vec(&json!({
            "operation": request.operation, "session": session, "instance": launch.instance,
        })).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        channel.write_all(&bytes).map_err(|e| format!("release write unconfirmed: {e}"))?;
        let released = receipt(channel)?;
        if !matches(&released, "released", request, &launch.instance, Some(session)) {
            return Err("release acknowledgement is not for this child instance".into());
        }
        // A verified release ACK is a fact even when readiness times out.
        // Later inspection may observe readiness without sending release again.
        self.launch(request)?.release_acknowledged = true;
        let deadline = Instant::now() + timeout;
        while !unique_reached(self.roster.as_ref(), session)? {
            if Instant::now() >= deadline { return Err("release acknowledged but runtime readiness is unconfirmed".into()) }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    fn deliver_brief(&mut self, request: &Request, session: &str) -> Result<crate::Delivery, String> {
        if !matches!(self.inspect(request)?, LaunchState::Running { session: s } if s == session) {
            return Err("brief target is not the verified running child".into());
        }
        // The state machine persists Attempted BEFORE calling this. A typed
        // acceptance is not consumption; an ambiguous reply is never resent.
        self.transport.send(session, &request.brief)
    }
}

impl ProcessHost {
    fn cancel_evidence(&mut self, request: &Request) -> Result<CancelRosterEvidence, String> {
        let launch = self.launch(request)?;
        if launch.release_attempted || launch.release_acknowledged {
            return Err("cancel refuses any release attempt, including an uncertain acknowledgement".into());
        }
        if !launch.attempted || launch.child.is_none() {
            return Err("cancel needs this service instance's still-owned child handle".into());
        }
        let session = launch.session.clone();
        if let Some(session) = session {
            let rows = self.roster.rows()?;
            let matching: Vec<_> = rows.iter().filter(|r| r.session.as_deref() == Some(&session)).collect();
            if matching.len() > 1 { return Err("cancel refuses ambiguous roster ownership".into()) }
            let reached = matching.first().map(|r| r.reached);
            if reached == Some(true) { return Err("cancel refuses a reached session, even with a held receipt".into()) }
            Ok(CancelRosterEvidence { bound_session: Some(session), queried: true, matching_rows: Some(matching.len()), reached })
        } else {
            Ok(CancelRosterEvidence { bound_session: None, queried: false, matching_rows: None, reached: None })
        }
    }
}

impl CancelHost for ProcessHost {
    fn check_cancel(&mut self, request: &Request) -> Result<(), String> {
        self.cancel_evidence(request).map(|_| ())
    }

    fn cancel(&mut self, request: &Request) -> Result<ExitEvidence, String> {
        let roster = self.cancel_evidence(request)?;
        let launch = self.launch(request)?;
        // The service Gate and Host lock serialize start/cancel; after restart
        // no owned handle exists. These flags are valid ONLY with that ownership.
        // Durable intent was saved before reaching this effect. Explicit retries
        // only observe the same Child; they neither close again nor retry release.
        if !launch.cancel_attempted {
            launch.cancel_roster = Some(roster);
            launch.cancel_attempted = true;
            drop(launch.channel.take());
            drop(launch.input.take());
        }
        let roster_before_close = launch.cancel_roster.clone().ok_or("cancel attempt lacks pre-close evidence")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let child = launch.child.as_mut().ok_or("owned child receipt lost")?;
            if let Some(status) = child.try_wait().map_err(|e| format!("observing owned child exit: {e}"))? {
                return Ok(ExitEvidence {
                    observed_at_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as u64,
                    status: status.to_string(), code: status.code(), signal: status.signal(), roster_before_close,
                });
            }
            if Instant::now() >= deadline {
                return Err("owned child has not exited within the observation window; no force-kill or automatic retry".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

// A concurrent fork can briefly inherit a flock descriptor until exec. Keep
// subprocess fixtures out of the store test's drop-then-immediate-relock window.
#[cfg(test)]
pub(crate) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use crate::{Delivery, Row};

    struct Rows(Arc<Mutex<Vec<Row>>>);
    impl Roster for Rows { fn rows(&self) -> Result<Vec<Row>, String> { Ok(self.0.lock().unwrap().clone()) } }
    struct NoTransport;
    impl Transport for NoTransport {
        fn send(&self, _: &str, _: &str) -> Result<Delivery, String> { panic!("cancel must not send a brief") }
    }
    fn fixture(slow: bool) -> (ProcessHost, Request, Arc<Mutex<Vec<Row>>>) {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let mut host = ProcessHost::new(PathBuf::new(), PathBuf::new(), Box::new(Rows(rows.clone())), Box::new(NoTransport));
        let request = Request { operation: "cancel-fixture".into(), caller: "fixture".into(), workspace: ".".into(),
            model: "mock".into(), persona: None, brief: "no turns".into() };
        host.register_fresh(&request).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "process_host::tests::child_entry"])
            .env("COORDINATOR_CANCEL_TEST_CHILD", if slow { "slow" } else { "normal" })
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let launch = host.launch(&request).unwrap();
        launch.attempted = true;
        launch.session = Some("fixture-child".into());
        launch.input = child.stdin.take();
        launch.child = Some(child);
        (host, request, rows)
    }
    fn finish(host: &mut ProcessHost, request: &Request) {
        let launch = host.launch(request).unwrap();
        drop(launch.input.take());
        launch.child.as_mut().unwrap().wait().unwrap();
    }
    #[test]
    #[ignore = "subprocess fixture entry; invoked only by these tests"]
    fn child_entry() {
        let Ok(mode) = std::env::var("COORDINATOR_CANCEL_TEST_CHILD") else { return };
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        if mode == "slow" { std::thread::sleep(Duration::from_secs(3)); }
    }
    #[test]
    fn owned_child_exit_is_reaped_and_release_is_blocked() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let (mut host, request, _) = fixture(false);
        let evidence = host.cancel(&request).unwrap();
        assert_eq!(evidence.code, Some(0));
        assert_eq!(evidence.signal, None);
        assert!(host.launch(&request).unwrap().child.as_mut().unwrap().try_wait().unwrap().is_some());
        assert!(host.release(&request, "fixture-child").is_err());
    }
    #[test]
    fn unobserved_exit_is_an_error_not_a_success_receipt() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let (mut host, request, _) = fixture(true);
        assert!(host.cancel(&request).unwrap_err().contains("has not exited"));
        assert!(host.launch(&request).unwrap().child.as_mut().unwrap().try_wait().unwrap().is_none());
        assert!(host.release(&request, "fixture-child").is_err());
        finish(&mut host, &request);
        let first_roster = host.launch(&request).unwrap().cancel_roster.clone();
        let evidence = host.cancel(&request).unwrap();
        assert_eq!(Some(evidence.roster_before_close), first_roster);
        assert_eq!(evidence.code, Some(0));
        assert!(host.launch(&request).unwrap().input.is_none());
    }
    #[test]
    fn release_attempt_ack_or_reached_roster_refuse_before_closing_pipes() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        for case in 0..4 {
            let (mut host, request, rows) = fixture(false);
            match case {
                // Lost ACK: release bytes may already have opened the barrier.
                0 => host.launch(&request).unwrap().release_attempted = true,
                1 => {
                    let launch = host.launch(&request).unwrap();
                    launch.release_attempted = true;
                    launch.release_acknowledged = true;
                }
                // Defensive refusal even for an inconsistent ACK-only receipt.
                2 => host.launch(&request).unwrap().release_acknowledged = true,
                _ => rows.lock().unwrap().push(Row { id: "locator".into(), session: Some("fixture-child".into()),
                    state: "idle".into(), last: "unknown".into(), outcome_at: None, outcome_observed: None,
                    started_ms: None, reached: true, label: None, decoration: None, display: None }),
            }
            assert!(host.cancel(&request).is_err());
            let launch = host.launch(&request).unwrap();
            assert!(launch.input.is_some());
            assert!(!launch.cancel_attempted);
            finish(&mut host, &request);
        }
    }
    #[test]
    fn absent_receipt_and_changed_input_never_reacquire_a_child() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let (mut host, request, _) = fixture(false);
        let mut other = request.clone();
        other.model = "other".into();
        assert!(host.cancel(&other).unwrap_err().contains("different immutable"));
        let mut restarted = ProcessHost::new(PathBuf::new(), PathBuf::new(), Box::new(Rows(Arc::new(Mutex::new(Vec::new())))), Box::new(NoTransport));
        assert!(restarted.cancel(&request).unwrap_err().contains("host restarted"));
        assert!(host.launch(&request).unwrap().input.is_some());
        finish(&mut host, &request);
    }
}

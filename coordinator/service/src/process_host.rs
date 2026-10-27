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
use crate::rank::Blocks;
use crate::{Delivery, Roster, managed_start::{BriefTransport, CancelHost, CancelRosterEvidence, Correlated, ExitEvidence, Host, LaunchState, Request}};

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

/// How long an aborted launch waits to observe the child exit, before the
/// launch timeout itself is even considered. Derived, never silently equal to
/// a number somebody typed twice: see the `.min(timeout)` at the call site,
/// which keeps this from drifting away from the one configured bound.
const ABORT_EXIT_WAIT: Duration = Duration::from_secs(5);

/// **Abort a child this launcher started and nobody else knows about.**
///
/// Closing the channel is the mechanism the cancel path already uses: a closed
/// managed channel is a failed start, and the child is built to exit before it
/// opens the runtime. No signal is sent and none may be.
///
/// The sentence it returns says only what was observed, and names what a person
/// will actually meet if the exit was not: a window that may be visible on the
/// wrong workspace, and a session that may exist in the roster.
fn abort_held(launch: &mut Launch, why: String, budget: Duration) -> String {
    launch.channel = None;
    launch.input = None;
    // **Three states, not two.** A child that is still running and a child whose
    // state could not be read are different facts, and neither is "there was no
    // child". A failed observation is not an absence — the same rule the receipt
    // path follows, and the reason this returns a sentence rather than a verdict.
    let had_child = launch.child.is_some();
    let mut exited = None;
    let mut running = false;
    let mut unobservable: Option<String> = None;
    if let Some(child) = launch.child.as_mut() {
        let deadline = Instant::now() + budget;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited = Some(status);
                    break;
                }
                Ok(None) => running = true,
                Err(e) => {
                    unobservable = Some(e.to_string());
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    let consequences = "a window may be visible on the wrong workspace, and a session may exist \
                        in the roster";
    match exited {
        Some(status) => format!(
            "{why}; the held child was aborted by closing its channel and exited ({status})"
        ),
        None if unobservable.is_some() => format!(
            "{why}; the held child was aborted by closing its channel and its state could not be \
             observed ({}) — it may still be running, {consequences}",
            unobservable.as_deref().unwrap_or("try_wait failed")
        ),
        None if running => format!(
            "{why}; the held child was aborted by closing its channel and its exit was NOT \
             observed within {budget:?} — it may still be running, {consequences}"
        ),
        None if had_child => format!(
            "{why}; the held child was aborted by closing its channel and its exit was not \
             observed — it may still be running, {consequences}"
        ),
        None => format!("{why}; there was no child left to abort"),
    }
}

/// **The refusal a visible launch gets when no no-focus capability is in force.**
///
/// One sentence, in one place: the guard below returns it, and the service returns
/// it **before the resolver runs**, so a refusal never depends on what a
/// compositor answered — and never requires contacting one.
pub const NO_FOCUS_REFUSAL: &str = "this host cannot establish a no-focus map, so it will not start a visible child: a launch that takes the operator's keyboard is not permitted, and no observation of the no-focus rule has been recorded for this compositor instance";

pub struct ProcessHost {
    executable: PathBuf,
    logs: PathBuf,
    /// The terminal a visible launch runs through. Absent means this host can
    /// only start headless children, and a terminal launch is refused rather
    /// than downgraded.
    terminal: Option<String>,
    /// The compositor, for placing a visible child's window. Absent means the
    /// launch cannot place one, which is a refusal and not a silent skip.
    placement: Option<std::sync::Arc<dyn crate::ancestry::Placement>>,
    /// The no-focus capability. Absent means this host cannot promise the
    /// operator keeps the keyboard, and a visible launch is **refused**.
    no_focus: Option<std::sync::Arc<dyn crate::ancestry::NoFocus>>,
    roster: Box<dyn Roster>,
    /// The brief's own seam: one correlated record in the recipient's log.
    /// The notices go through the caller's transport; a brief never does — it
    /// would be a second path to the same session.
    briefs: Box<dyn BriefTransport>,
    /// Where the rank blocks are read from. The host re-reads a block here, right
    /// before it spawns, so the bytes the child is handed are the bytes the
    /// operation recorded — and a host with none configured **refuses** a ranked
    /// start rather than appending nothing and calling it ranked.
    blocks: Option<Blocks>,
    launches: HashMap<(String, String), Launch>,
    timeout: Duration,
}

impl ProcessHost {
    pub fn new(executable: PathBuf, logs: PathBuf, roster: Box<dyn Roster>, briefs: Box<dyn BriefTransport>) -> Self {
        Self {
            executable,
            logs,
            terminal: None,
            placement: None,
            no_focus: None,
            roster,
            briefs,
            blocks: None,
            launches: HashMap::new(),
            timeout: Duration::from_secs(15),
        }
    }

    /// Read this host's rank blocks from `blocks`. Without it, a **ranked**
    /// launch is refused: the digest the operation is bound to names bytes this
    /// host would not read, and a child told no rank text while the record said
    /// it was ranked is worse than no launch.
    pub fn with_ranks(mut self, blocks: Blocks) -> Self {
        self.blocks = Some(blocks);
        self
    }

    /// Run visible children through this terminal. Without it, a terminal
    /// launch is refused: a child nobody can see is not a visible launch.
    pub fn with_terminal(mut self, terminal: impl Into<String>) -> Self {
        self.terminal = Some(terminal.into());
        self
    }

    /// Place visible children through this compositor. Without it, a terminal
    /// launch refuses rather than starting a window it cannot place.
    pub fn with_placement(
        mut self,
        placement: std::sync::Arc<dyn crate::ancestry::Placement>,
    ) -> Self {
        self.placement = Some(placement);
        self
    }

    /// Establish no-focus maps through this capability. There is exactly one
    /// production implementation (`ancestry::AttestedNoFocus`) and it establishes
    /// an *attestation*, not a mechanism; the host's own construction attaches it
    /// only when a dated observation of the running compositor instance was
    /// supplied at start. Without it a visible launch refuses.
    pub fn with_no_focus(mut self, no_focus: std::sync::Arc<dyn crate::ancestry::NoFocus>) -> Self {
        self.no_focus = Some(no_focus);
        self
    }

    /// `--system-append` must be an option of the *subcommand this host spawns*.
    /// A harness that has it elsewhere (or not at all) would reject the argv and
    /// exit before any session existed; that refusal is worth naming here, in the
    /// rank's own words, instead of surfacing as a missing held receipt.
    ///
    /// This is a preflight, not a guarantee: it asks the binary, and the binary
    /// decides what it does with the flag afterwards.
    fn accepts_system_append(
        &self,
        subcommand: &str,
        addition: &crate::rank::Addition,
    ) -> Result<(), String> {
        let asked = Command::new(&self.executable).args([subcommand, "--help"]).output().map_err(|e| {
            format!(
                "cannot ask `{}` whether `{subcommand}` accepts `--system-append`: {e}. The `{}` rank text is \
                 appended at spawn, so a harness that cannot say whether it carries the flag is not \
                 asked to launch with it",
                self.executable.display(),
                addition.rank
            )
        })?;
        let mut said = String::from_utf8_lossy(&asked.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&asked.stderr));
        if asked.status.success() && said.contains("--system-append") {
            return Ok(());
        }
        let observed = if asked.status.success() {
            "exited 0 and its help does not list the flag".to_string()
        } else {
            format!("exited {} and its help does not list the flag", asked.status)
        };
        Err(format!(
            "this harness's `{subcommand}` does not accept `--system-append`, so the `{}` rank block cannot \
             be appended to the child's resolved system prompt: `{} {subcommand} --help` {observed}. \
             Refusing to start — a child launched without it would be told no rank text while the record \
             said it was ranked. The flag is the core/CLI lane's, generic and per-subcommand, and the \
             spawn here is `{subcommand}`",
            addition.rank,
            self.executable.display(),
        ))
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
        // Cloned before the launch borrow: the terminal and the compositor are
        // configuration, read once per prepare, and the mutable borrow on the
        // launch record must not be widened to cover them.
        let terminal_program = self.terminal.clone();
        let compositor = self.placement.clone();
        let no_focus = self.no_focus.clone();
        // The rank block is read and hashed again here, immediately before
        // anything is created, and compared with the digest the operation was
        // recorded with. It runs before `attempted` is set on purpose: changed
        // text is a refusal to start, not a launch whose outcome is unknown, so
        // the host still reports NotCreated and nothing was spawned.
        let subcommand = crate::launch::subcommand(request.shape);
        let addition = match (&self.blocks, request.rank.as_deref()) {
            (Some(blocks), _) => blocks.verify(request.rank.as_deref(), request.rank_digest.as_deref())?,
            (None, Some(rank)) => {
                return Err(format!(
                    "this host was started without a rank-block directory, so the `{rank}` block cannot be \
                     read and hashed against the digest this operation is bound to. Refusing to start — a \
                     child launched here would be told no rank text while the record said it was ranked"
                ))
            }
            (None, None) => None,
        };
        if let Some(addition) = &addition {
            self.accepts_system_append(subcommand, addition)?;
        }
        let launch = self.launch(request)?;
        if launch.attempted { return Err("launch already attempted; reconcile its receipt, never spawn twice".into()) }
        // A launch that cannot run or cannot be placed is refused **before it
        // counts as an attempt**: nothing was started, so there is no receipt
        // to reconcile and a caller with a corrected request may try again.
        if request.shape == crate::managed_start::LaunchShape::Terminal {
            // Told in the order the launcher would discover them: can this
            // host show a window at all, can it place one, and does the caller
            // say where it goes.
            // **This block is the guard.** The `ok_or(…)`s further down on the
            // same three values are unreachable by construction and stay only
            // as a defensive second read; if you are changing the rule, change
            // it here.
            if terminal_program.is_none() {
                return Err(
                    "this host has no terminal configured, so it cannot start a visible child"
                        .into(),
                );
            }
            if compositor.is_none() {
                return Err("this host cannot place a window: it has no compositor".into());
            }
            if request.destination.is_none() {
                return Err("a terminal launch needs a destination workspace".into());
            }
            // **The no-focus precondition, checked before anything is started.**
            // The capability is attached by the host's construction only when a
            // dated observation of the running instance was supplied; absent, this
            // refuses — rather than spawning a window that might take the operator's
            // keyboard while the code merely disclaims the promise. The same
            // sentence is returned by the service's pre-resolution check
            // ([`NO_FOCUS_REFUSAL`]), so a caller reads one answer whether the
            // refusal came from here or from before the resolver ran.
            if no_focus.is_none() {
                return Err(NO_FOCUS_REFUSAL.into());
            }
        }

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
        // The rank text the child will read, written once from the bytes that were
        // hashed into this operation. Named for the launch instance, 0600 in the
        // 0700 launches directory, and left behind with the log as the evidence of
        // what this child was told.
        let append = match &addition {
            Some(addition) => {
                let copy = logs.join(format!("{}.system-append", launch.instance));
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                    .open(&copy).map_err(|e| format!("rank addition {}: {e}", copy.display()))?;
                file.write_all(&addition.bytes).map_err(|e| format!("rank addition {}: {e}", copy.display()))?;
                // Flushed and then closed before anything can read it, and read-only
                // afterwards: the child opens this file, so its contents are the last
                // thing that has to still be true.
                file.sync_all().map_err(|e| format!("rank addition {}: {e}", copy.display()))?;
                drop(file);
                std::fs::set_permissions(&copy, std::os::unix::fs::PermissionsExt::from_mode(0o400))
                    .map_err(|e| format!("rank addition {}: {e}", copy.display()))?;
                let written = std::fs::read(&copy).map_err(|e| format!("rank addition {}: {e}", copy.display()))?;
                if crate::rank::digest_hex(&written) != addition.digest {
                    return Err(format!(
                        "the rank addition written for this launch at {} does not hash to the digest the \
                         operation is bound to ({}); refusing to spawn a child that would read something else",
                        copy.display(),
                        addition.digest
                    ));
                }
                Some(copy)
            }
            None => None,
        };
        let error = output.try_clone().map_err(|e| format!("launch stderr: {e}"))?;
        let (parent, child_end) = UnixStream::pair().map_err(|e| format!("launch channel: {e}"))?;
        parent.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        parent.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let fd = child_end.as_raw_fd();
        // **One command builder for both shapes.** The terminal shape differs
        // in the program and the subcommand and in nothing else, so the
        // barrier, the adoption and the cancellation path stay one mechanism.
        // **A visible launch's inputs are checked before the process exists**,
        // so a launch that cannot be placed never starts a child at all.
        let terminal = match request.shape {
            crate::managed_start::LaunchShape::Headless => None,
            crate::managed_start::LaunchShape::Terminal => Some(crate::launch::Terminal {
                program: terminal_program
                    .clone()
                    .ok_or("this host has no terminal configured, so it cannot start a visible child")?,
                app_id: format!("{}{}", crate::launch::APP_ID_PREFIX, launch.instance),
                title: format!(
                    "Meli — {} · {}",
                    request.persona.as_deref().unwrap_or("worker"),
                    request.model
                ),
            }),
        };
        let built = crate::launch::launch_command(
            request.shape,
            &executable.display().to_string(),
            terminal.as_ref(),
            &workspace.display().to_string(),
            fd,
            &request.operation,
            &launch.instance,
            &request.model,
            request.persona.as_deref(),
        )?;
        let mut command = Command::new(&built.program);
        command
            .current_dir(workspace)
            .args(&built.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error));
        // The rank's addition is a generic append of text: not a persona rewrite,
        // not folded into `--persona`'s semantics, and not an enforcement of
        // anything. The child is told it before its first turn, so the harness's
        // SessionStart record captures the prompt it actually had.
        //
        // The path handed over is the launch-private copy written above — the exact
        // bytes that were hashed — never the block itself: the CLI reads the file
        // itself, and a path to the operator's file would let that read differ from
        // the digest this operation is bound to.
        if let Some(copy) = &append {
            command.arg("--system-append").arg(copy.as_os_str());
        }
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
        // Established **before** the spawn, so the window is under the map when
        // it maps; the guard releases on drop, on every path out of this
        // function, including the aborts below.
        let _no_focus = match request.shape {
            crate::managed_start::LaunchShape::Terminal => {
                let capability = no_focus
                    .as_ref()
                    .ok_or("this host cannot establish a no-focus map")?;
                let app_id = format!("{}{}", crate::launch::APP_ID_PREFIX, launch.instance);
                Some(
                    capability
                        .establish(&app_id)
                        .map_err(|e| format!("the no-focus map could not be established: {e}"))?,
                )
            }
            crate::managed_start::LaunchShape::Headless => None,
        };
        let mut child = command.spawn().map_err(|e| format!("starting held harness: {e}"))?;
        launch.input = child.stdin.take(); // keep the persistent chat alive, never feed approval answers
        launch.child = Some(child);
        drop(child_end);
        launch.channel = Some(parent);
        let read = {
            let channel = launch.channel.as_mut().expect("the channel was just stored");
            receipt(channel)
        };
        let held = match read {
            Ok(held) => held,
            // A launch whose receipt never arrived has, by definition, no
            // receipt to reconcile — and the child that would have sent one is
            // ours, so it is aborted here rather than left waiting on a channel
            // this side is about to drop anyway.
            Err(e) => {
                return Err(abort_held(
                    launch,
                    format!("the launch receipt failed: {e}"),
                    timeout.min(ABORT_EXIT_WAIT),
                ))
            }
        };
        if !matches(&held, "held", request, &launch.instance, None) {
            return Err("Held receipt does not identify this operation instance".into());
        }
        // **Whose process is this?** Headless, the harness *is* the child we
        // spawned, and pid equality is the whole answer. Visible, the service
        // spawns a terminal and the harness is its grandchild, so equality is
        // false for a launch that is entirely ours. The rule that covers both
        // without weakening to "some pid answered": the claimed pid is the
        // spawned one, or it descends from it — read from `/proc`, bounded, and
        // a refusal when the ancestry cannot be read at all.
        let spawned = launch
            .child
            .as_ref()
            .map(|c| u64::from(c.id()))
            .ok_or("held child is missing")?;
        let claimed = held["pid"].as_u64().ok_or("Held receipt carries no pid")?;
        let ours = crate::ancestry::identity_holds(
            std::path::Path::new("/proc"),
            request.shape,
            spawned as u32,
            claimed,
        )
        .map_err(|e| format!("Held receipt's pid cannot be verified: {e}"))?;
        if !ours {
            return Err("Held receipt names a process this launch did not start".into());
        }
        launch.session = held["session"].as_str().map(str::to_string);
        // **Placement, for a visible child, before the barrier opens.** A
        // window that cannot be placed where the caller said is a refused
        // launch: the child is still held, and the caller is told exactly what
        // happened rather than being handed a running worker on the wrong
        // workspace.
        if request.shape == crate::managed_start::LaunchShape::Terminal {
            let destination = request
                .destination
                .ok_or("a terminal launch needs a destination workspace")?;
            let placement = compositor
                .as_ref()
                .ok_or("this host cannot place a window: it has no compositor")?;
            let outcome = placement
                .place(claimed as u32, destination)
                .map_err(|e| format!("the window was not placed: {e}"))
                .and_then(|placed| {
                    if placed.workspace_id == destination {
                        Ok(())
                    } else {
                        Err(format!(
                            "the window is on workspace {}, not {destination}",
                            placed.workspace_id
                        ))
                    }
                });
            if let Err(why) = outcome {
                return Err(abort_held(launch, why, timeout.min(ABORT_EXIT_WAIT)));
            }
        }
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

    fn deliver_brief(
        &mut self,
        request: &Request,
        session: &str,
        delivery_key: &str,
        binding_digest: &str,
    ) -> Result<Correlated, String> {
        if !matches!(self.inspect(request)?, LaunchState::Running { session: s } if s == session) {
            return Err("brief target is not the verified running child".into());
        }
        // The state machine persisted `Attempted` — and the delivery key with
        // it — BEFORE calling this, so a repeat carries the same key and the
        // recipient deduplicates instead of receiving a second brief.
        //
        // A correlated delivery does **not** wake the recipient: the record is
        // in its log and it reads that at its next turn. Nothing here adds a
        // wake, and there is no legacy inbox send to fall back to — an
        // `Unsupported` or `Unknown` answer is reported as itself.
        self.briefs.send_correlated(session, delivery_key, binding_digest, &request.brief)
    }

    fn wake(&mut self, request: &Request, session: &str, text: &str) -> Result<Delivery, String> {
        if !matches!(self.inspect(request)?, LaunchState::Running { session: s } if s == session) {
            return Err("wake target is not the verified running child".into());
        }
        // Ordinary inbox mail on the harness's own door, with `--wake`: the one
        // thing a correlated delivery may not be, and deliberately a different
        // seam from `send_correlated` so the brief cannot travel down it.
        self.briefs.wake(session, text)
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

/// **A visible launch that cannot run or cannot be placed never starts.**
/// The three refusals happen before a process exists, which is the point: a
/// refused launch leaves nothing running to clean up.
#[test]
fn a_terminal_launch_without_its_inputs_is_refused_before_spawning() {
    use crate::managed_start::LaunchShape;
    // A real, writable log directory: the refusal must come from the missing
    // terminal or compositor, not from a path this test invented and the OS
    // refused.
    let logs = std::env::temp_dir().join(format!("eidolon-refusal-{}", std::process::id()));
    let rows = Arc::new(Mutex::new(Vec::new()));
    let mut host = ProcessHost::new(
        PathBuf::from("/bin/true"),
        logs.clone(),
        Box::new(Rows(rows)),
        Box::new(NoBriefs),
    );
    let request = Request {
        operation: "visible-1".into(),
        caller: "fixture".into(),
        workspace: ".".into(),
        model: "mock".into(),
        persona: None,
        brief: "bounded task".into(),
        rank: None, rank_digest: None,
        shape: LaunchShape::Terminal,
        destination: Some(11),
    };
    host.register_fresh(&request).unwrap();

    // No terminal configured on this host.
    let refused = host.prepare(&request).unwrap_err();
    assert!(refused.contains("no terminal configured"), "{refused}");

    // A terminal, but no compositor to place the window with.
    host = host.with_terminal("/bin/true");
    let refused = host.prepare(&request).unwrap_err();
    assert!(refused.contains("no compositor"), "{refused}");

    // A compositor, but no way to establish a no-focus map: refused, and this
    // is the check that matters most — without it a visible child would spawn
    // and merely disclaim the promise.
    host = host.with_placement(Arc::new(UnreachableCompositor));
    let refused = host.prepare(&request).unwrap_err();
    assert!(refused.contains("no-focus map"), "{refused}");

    // A no-focus capability, but nobody said where the window belongs. A
    // separate operation, because changing a registered request's input is
    // itself a refusal the launch record is right to raise.
    host = host.with_no_focus(Arc::new(DeclaredNoFocus));
    let mut blind = request.clone();
    blind.operation = "visible-2".into();
    blind.destination = None;
    host.register_fresh(&blind).unwrap();
    let refused = host.prepare(&blind).unwrap_err();
    assert!(refused.contains("needs a destination workspace"), "{refused}");
    let _ = std::fs::remove_dir_all(&logs);
}

/// **Adapter integration proof for a visible launch — NOT real focus proof, and not a
/// window appearing.**
///
/// The seam the success path needed is on the *adapter* side, not in front of
/// `spawn`: a real child process stands in for the terminal (a script with a shebang,
/// so its pid and `/proc` identity are genuine), it answers the barrier with a held
/// receipt naming itself, and a **mocked compositor adapter** stands where
/// `placement.place` calls through. Nothing contacts Hyprland and no window is created
/// or moved.
///
/// What it proves is the adapter path end to end: every visible gate passes, the spawn
/// command carries the app-id the no-focus map was asked for, placement is called with
/// the child's **real** pid and the requested destination, the workspace the adapter
/// reports is checked, and the launch is admitted with the session the receipt named.
/// Real focus suppression and real placement stay live-unverified.
#[test]
fn a_visible_launch_reaches_the_placement_adapter_with_the_childs_real_pid() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();

    /// **The conforming adapter** — reports the window where the caller asked for it.
    /// The success test's claims rest on *this* contract, and it is deliberately a
    /// different type from `ElsewhereAdapter` below, so a reader can see which contract
    /// is being assumed and which is being checked.
    struct ConformingAdapter(Arc<Mutex<Vec<(u32, i64)>>>);
    impl crate::ancestry::Placement for ConformingAdapter {
        fn place(&self, pid: u32, destination: i64) -> Result<crate::ancestry::Placed, String> {
            self.0.lock().unwrap().push((pid, destination));
            Ok(crate::ancestry::Placed {
                workspace_id: destination,
                client: crate::ancestry::Client {
                    address: "0xfeedface".into(),
                    pid,
                    workspace_id: destination,
                    workspace_name: destination.to_string(),
                    class: "eidolon-worker-fake".into(),
                    title: "stand-in terminal".into(),
                },
            })
        }
    }

    let root = std::env::temp_dir().join(format!("eidolon-adapter-{}", std::process::id()));
    let workspace = root.join("ws");
    let logs = root.join("logs");
    let argv_file = root.join("argv");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = stand_in_terminal(&root);

    let asked = Arc::new(Mutex::new(None));
    let placed = Arc::new(Mutex::new(Vec::new()));
    let mut host = ProcessHost::new(
        PathBuf::from("/bin/true"),
        logs.clone(),
        Box::new(Rows(Arc::new(Mutex::new(Vec::new())))),
        Box::new(NoBriefs),
    )
    .with_terminal(script.display().to_string())
    .with_placement(Arc::new(ConformingAdapter(placed.clone())))
    .with_no_focus(Arc::new(Recorder(asked.clone())));

    let request = Request {
        operation: "visible-adapter-1".into(),
        caller: "fixture".into(),
        workspace: workspace.display().to_string(),
        model: "mock".into(),
        persona: None,
        brief: "bounded task".into(),
        rank: None,
        rank_digest: None,
        shape: crate::managed_start::LaunchShape::Terminal,
        destination: Some(11),
    };
    host.register_fresh(&request).unwrap();

    host.prepare(&request).expect("a visible launch with every capability must proceed");

    let argv = std::fs::read_to_string(&argv_file).expect("the stand-in terminal recorded its argv");
    let launch = host.launch(&request).unwrap();
    let app_id = format!("{}{}", crate::launch::APP_ID_PREFIX, launch.instance);
    assert!(argv.contains(&format!("--app-id={app_id}")), "the window class the rule matches: {argv}");
    assert!(argv.contains("--title=Meli — worker · mock"), "the title a person reads: {argv}");
    assert!(argv.contains(&format!("--working-directory={}", workspace.display())), "{argv}");
    assert!(argv.contains("tui"), "the visible subcommand, not `chat`: {argv}");
    assert!(argv.contains("--managed-start-fd"), "the same barrier as a headless launch: {argv}");
    assert_eq!(asked.lock().unwrap().as_deref(), Some(app_id.as_str()),
        "the no-focus map is asked for exactly the app-id the spawn command carries");
    assert_eq!(launch.session.as_deref(), Some("fake-visible-session"), "the receipt named the child");
    assert!(launch.attempted, "the launch is admitted");
    let seen = placed.lock().unwrap().clone();
    let child = launch.child.as_ref().unwrap().id();
    assert_eq!(seen, vec![(child, 11)], "placement is called once, with the child's real pid and the destination");
    finish(&mut host, &request);
    let _ = std::fs::remove_dir_all(&root);
}

/// A no-focus capability that claims to work and does nothing, used only to
/// prove the refusal order: with it present the launch reaches the *next*
/// guard, which is how we know the no-focus check is in the path rather than in
/// a comment. It never establishes anything for real.
struct DeclaredNoFocus;

impl crate::ancestry::NoFocus for DeclaredNoFocus {
    fn establish(
        &self,
        _app_id: &str,
    ) -> Result<Box<dyn crate::ancestry::NoFocusGuard>, String> {
        Ok(Box::new(DeclaredGuard))
    }
}

struct DeclaredGuard;

impl crate::ancestry::NoFocusGuard for DeclaredGuard {}

/// Records the app-id it was asked to establish; `DeclaredGuard` establishes nothing.
/// Shared by the visible-launch tests, both of which assert on the app-id alone.
struct Recorder(Arc<Mutex<Option<String>>>);

impl crate::ancestry::NoFocus for Recorder {
    fn establish(&self, app_id: &str) -> Result<Box<dyn crate::ancestry::NoFocusGuard>, String> {
        *self.0.lock().unwrap() = Some(app_id.to_string());
        Ok(Box::new(DeclaredGuard))
    }
}

/// Resolve Bash through PATH: a script generated during a test cannot be fixed by
/// Nix's earlier patchShebangs phase, and /usr/bin/env is absent in its sandbox.
fn fixture_bash() -> String {
    let output = Command::new("bash")
        .args(["-c", "printf '%s' \"$BASH\""])
        .output()
        .expect("process fixtures require bash on PATH");
    assert!(output.status.success(), "resolving the fixture's Bash interpreter failed");
    let bash = String::from_utf8(output.stdout).expect("Bash path is UTF-8");
    assert!(std::path::Path::new(&bash).is_absolute(), "Bash must report an absolute interpreter path: {bash}");
    assert!(std::path::Path::new(&bash).is_file(), "fixture interpreter must exist: {bash}");
    bash
}

/// A stand-in terminal for the visible-launch tests: it records its own argv, answers the
/// barrier with a held receipt naming itself and its **real** pid, then stays alive on the
/// stdin the host holds — the shape of a terminal, with nothing of a terminal in it.
fn stand_in_terminal(root: &std::path::Path) -> PathBuf {
    let bash = fixture_bash();
    let argv = root.join("argv");
    let script = root.join("fake-terminal");
    std::fs::write(&script, format!(
        "#!{bash}\nprintf '%s\\n' \"$@\" > '{argv}'\nfd=\"\"; op=\"\"; inst=\"\"\n\
         while [ $# -gt 0 ]; do case \"$1\" in \
         --managed-start-fd) fd=\"$2\"; shift 2;; \
         --managed-operation) op=\"$2\"; shift 2;; \
         --managed-instance) inst=\"$2\"; shift 2;; \
         *) shift;; esac; done\n\
         printf '{{\"state\":\"held\",\"operation\":\"%s\",\"instance\":\"%s\",\"session\":\"fake-visible-session\",\"pid\":%d}}\\n' \
         \"$op\" \"$inst\" \"$$\" >&\"$fd\"\ncat > /dev/null\n",
        argv = argv.display(),
    )).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

/// **The arm at `process_host.rs:561-570`, asserted.** An adapter that answers `Ok` with a
/// workspace other than the requested destination is refused, the held child is aborted,
/// and no worker is admitted — because a host that trusts an adapter's contract is one
/// refactor away from reporting the intended workspace, which is the class of claim this
/// feature exists to avoid.
///
/// The shipped adapter already returns its own error for an unconfirmed window, so this
/// arm is unreachable in production. That is exactly why it needs a committed test: a
/// hand-run mutation is not coverage. `ElsewhereAdapter` is deliberately a different type
/// from the `ConformingAdapter` the success test assumes.
#[test]
fn a_launch_whose_adapter_reports_the_wrong_workspace_is_refused_and_aborted() {
    let _serial = PROCESS_TEST_LOCK.lock().unwrap();

    /// **The non-conforming adapter** — answers `Ok`, with a workspace nobody asked for.
    struct ElsewhereAdapter(Arc<Mutex<Vec<(u32, i64)>>>);
    impl crate::ancestry::Placement for ElsewhereAdapter {
        fn place(&self, pid: u32, destination: i64) -> Result<crate::ancestry::Placed, String> {
            self.0.lock().unwrap().push((pid, destination));
            Ok(crate::ancestry::Placed {
                workspace_id: destination + 1,
                client: crate::ancestry::Client {
                    address: "0xfeedface".into(),
                    pid,
                    workspace_id: destination + 1,
                    workspace_name: (destination + 1).to_string(),
                    class: "eidolon-worker-fake".into(),
                    title: "stand-in terminal".into(),
                },
            })
        }
    }

    let root = std::env::temp_dir().join(format!("eidolon-elsewhere-{}", std::process::id()));
    let workspace = root.join("ws");
    let logs = root.join("logs");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = stand_in_terminal(&root);
    let asked = Arc::new(Mutex::new(None));
    let placed = Arc::new(Mutex::new(Vec::new()));
    let mut host = ProcessHost::new(
        PathBuf::from("/bin/true"),
        logs.clone(),
        Box::new(Rows(Arc::new(Mutex::new(Vec::new())))),
        Box::new(NoBriefs),
    )
    .with_terminal(script.display().to_string())
    .with_placement(Arc::new(ElsewhereAdapter(placed.clone())))
    .with_no_focus(Arc::new(Recorder(asked.clone())));

    let request = Request {
        operation: "visible-elsewhere-1".into(),
        caller: "fixture".into(),
        workspace: workspace.display().to_string(),
        model: "mock".into(),
        persona: None,
        brief: "bounded task".into(),
        rank: None,
        rank_digest: None,
        shape: crate::managed_start::LaunchShape::Terminal,
        destination: Some(11),
    };
    host.register_fresh(&request).unwrap();

    let refused = host.prepare(&request).expect_err("a window on the wrong workspace is not a launch");
    assert!(refused.contains("the window is on workspace 12, not 11"), "{refused}");
    assert!(refused.contains("the held child was aborted"), "the child is closed, never left held: {refused}");
    assert_eq!(placed.lock().unwrap().len(), 1, "placement was called once, and its answer is what refused the launch");
    let launch = host.launch(&request).unwrap();
    assert_eq!(launch.session.as_deref(), Some("fake-visible-session"),
        "the receipt was read before placement, so the refusal is about the workspace alone");
    // Nothing is admitted: no held and no running child exists for the caller, so
    // `managed_start::start` has nothing it could adopt.
    assert!(matches!(host.inspect(&request), Ok(LaunchState::Unknown(_))),
        "an aborted launch must not report Held or Running");
    let _ = std::fs::remove_dir_all(&root);
}

/// A compositor that must never be called: these refusals are all upstream of
/// placement, and a placement that runs would mean a child had been started.
struct UnreachableCompositor;

impl crate::ancestry::Placement for UnreachableCompositor {
    fn place(&self, _pid: u32, _destination: i64) -> Result<crate::ancestry::Placed, String> {
        panic!("placement must not run for a launch refused in preparation");
    }
}

    use super::*;
    use std::sync::{Arc, Mutex};
    use crate::Row;

    struct Rows(Arc<Mutex<Vec<Row>>>);
    impl Roster for Rows { fn rows(&self) -> Result<Vec<Row>, String> { Ok(self.0.lock().unwrap().clone()) } }
    struct NoBriefs;
    impl BriefTransport for NoBriefs {
        fn send_correlated(&self, _: &str, _: &str, _: &str, _: &str) -> Result<Correlated, String> {
            panic!("cancel must not send a correlated brief")
        }
        fn wake(&self, _: &str, _: &str) -> Result<Delivery, String> {
            panic!("cancel must not wake a worker")
        }
    }
    fn fixture(slow: bool) -> (ProcessHost, Request, Arc<Mutex<Vec<Row>>>) {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let mut host = ProcessHost::new(PathBuf::new(), PathBuf::new(), Box::new(Rows(rows.clone())), Box::new(NoBriefs));
        let request = Request { operation: "cancel-fixture".into(), caller: "fixture".into(), workspace: ".".into(),
            model: "mock".into(), persona: None, brief: "no turns".into(),
            rank: None, rank_digest: None,
            shape: crate::managed_start::LaunchShape::Headless, destination: None };
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
    /// **The wake's refusal must send nothing**, and the assertion has to be that
    /// nothing was sent — not merely that an error came back. Every other test of this
    /// branch's neighbourhood goes through a fake host or the happy path, so this is
    /// the only failing case behind the one new production refusal.
    ///
    /// The last case is what stops it passing by refusing everything: the verified
    /// target is accepted, and only then is one wake counted.
    #[test]
    fn wake_refuses_anything_that_is_not_the_verified_running_child_and_sends_nothing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();

        /// A transport that counts rather than panics, so "nothing was sent" is a
        /// number and not an inference from a landmine.
        struct Counting(Arc<AtomicUsize>);
        impl BriefTransport for Counting {
            fn send_correlated(&self, _: &str, _: &str, _: &str, _: &str) -> Result<Correlated, String> {
                panic!("a wake must never send a correlated brief")
            }
            fn wake(&self, _: &str, _: &str) -> Result<Delivery, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Delivery { outcome: crate::Outcome::Delivered, words: "delivered".into() })
            }
        }

        let rows = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(AtomicUsize::new(0));
        let mut host = ProcessHost::new(
            PathBuf::new(), PathBuf::new(), Box::new(Rows(rows.clone())), Box::new(Counting(sent.clone())),
        );
        let request = Request { operation: "wake-fixture".into(), caller: "fixture".into(), workspace: ".".into(),
            model: "mock".into(), persona: None, brief: "no turns".into(),
            rank: None, rank_digest: None,
            shape: crate::managed_start::LaunchShape::Headless, destination: None };
        host.register_fresh(&request).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "process_host::tests::child_entry"])
            .env("COORDINATOR_CANCEL_TEST_CHILD", "normal")
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        {
            let launch = host.launch(&request).unwrap();
            launch.attempted = true;
            launch.session = Some("fixture-child".into());
            launch.input = child.stdin.take();
            launch.child = Some(child);
        }

        // Held: no release has been attempted, so there is no running child to wake.
        let refused = host.wake(&request, "fixture-child", "you have mail").unwrap_err();
        assert!(refused.contains("wake target is not the verified running child"), "{refused}");
        assert_eq!(sent.load(Ordering::SeqCst), 0, "a held child must not be woken");

        // Released and reached — but the wake names a different session than the one
        // this launch bound. The child is running; the *target* is not verified.
        rows.lock().unwrap().push(Row { id: "locator".into(), session: Some("fixture-child".into()),
            state: "idle".into(), last: "unknown".into(), outcome_at: None, outcome_observed: None,
            started_ms: None, reached: true });
        {
            let launch = host.launch(&request).unwrap();
            launch.release_attempted = true;
            launch.release_acknowledged = true;
        }
        let refused = host.wake(&request, "1793000000000", "you have mail").unwrap_err();
        assert!(refused.contains("wake target is not the verified running child"), "{refused}");
        assert_eq!(sent.load(Ordering::SeqCst), 0, "a mismatched target must not be woken");

        // And the verified target IS woken, so the refusals above are about the
        // target rather than about a branch that refuses everything.
        host.wake(&request, "fixture-child", "you have mail").expect("the verified running child is woken");
        assert_eq!(sent.load(Ordering::SeqCst), 1);

        finish(&mut host, &request);
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
                    started_ms: None, reached: true }),
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
        let mut restarted = ProcessHost::new(PathBuf::new(), PathBuf::new(), Box::new(Rows(Arc::new(Mutex::new(Vec::new())))), Box::new(NoBriefs));
        assert!(restarted.cancel(&request).unwrap_err().contains("host restarted"));
        assert!(host.launch(&request).unwrap().input.is_some());
        finish(&mut host, &request);
    }
}

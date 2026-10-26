//! The coordination monitor.
//!
//! It is the one process that holds the adoption records and watches the workers
//! that were adopted. The tools are thin doors onto it (`POST /call`), so the
//! record and the notice have a single implementation and no session has to be
//! awake for a halt to be noticed.
//!
//! What it does, once per interval: read the harness's structured roster; write
//! one notice per *new observed halt* of an adopted worker; hand each notice that
//! has never been handed over to the transport exactly once; save. The scan itself
//! never resumes, cancels or starts anything. Two acts are available as admitted
//! doors and are the only things that change a worker's fate: an explicitly
//! requested start through the held-launch adapter, and a bounded cancellation of
//! a child this instance still owns and has never tried to release. Neither
//! claims a delivery the transport did not report. The decisions themselves are in
//! `lib.rs`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use eidolon_coordinator::{
    addresses_established, adopt, cycle as scan_cycle, rank, release_adoption, store, AdoptRequest,
    Delivery, Gate, Notice, Outcome, Roster, Row, Store, Transport,
};
use subtle::ConstantTimeEq;

/// Where the roster comes from.
///
/// `eidolon peers --json` is the process-level face of the same row builder the
/// script-side `swarm_call("roster")` uses. The monitor reads those rows and
/// nothing else: it does not read the presence directory, and it does not parse
/// `peers`' prose.
struct CliRoster {
    eidolon: String,
}

impl Roster for CliRoster {
    fn rows(&self) -> Result<Vec<Row>, String> {
        let out = Command::new(&self.eidolon)
            .args(["peers", "--json"])
            .output()
            .map_err(|e| format!("running `{} peers --json`: {e}", self.eidolon))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            return Err(format!(
                "`eidolon peers --json` exited {:?}: {stderr} — the monitor reads the harness's structured rows \
                 and nothing else; it will not read the presence directory and will not parse `peers`' prose.",
                out.status.code()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // The roster is `{"me": <row>|null, "sessions": [<row>, …]}`. `me` is null
        // for a scan with no session of its own, which `eidolon peers --json` is —
        // the monitor only needs the rows, so a bare array is accepted as well.
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            format!("`eidolon peers --json` did not answer with JSON ({e}); it answered with: {text}")
        })?;
        let rows = match value.get("sessions").cloned() {
            Some(rows) => rows,
            None if value.is_array() => value,
            None => {
                return Err(format!(
                    "`eidolon peers --json` answered without a `sessions` list: {text}"
                ))
            }
        };
        serde_json::from_value::<Vec<Row>>(rows).map_err(|e| {
            format!("`eidolon peers --json` answered rows this monitor cannot read ({e}): {text}")
        })
    }
}

/// Where a notice goes. `send` is the harness's own door; the monitor never
/// writes an inbox itself.
struct CliTransport {
    eidolon: String,
}

impl eidolon_coordinator::managed_start::BriefTransport for CliTransport {
    /// One correlated delivery: `eidolon send` with the key and digest the
    /// state machine minted. **No `--wake`** — a correlated delivery writes the
    /// record into the recipient's log and never starts a turn, and the
    /// harness refuses the flag rather than dropping it silently. There is no
    /// legacy send behind this: an answer this monitor cannot read is
    /// `Unknown`, never a downgrade.
    fn send_correlated(
        &self,
        to: &str,
        delivery_key: &str,
        binding_digest: &str,
        text: &str,
    ) -> Result<eidolon_coordinator::managed_start::Correlated, String> {
        use eidolon_coordinator::managed_start::Correlated;
        // A transport that never ran has established no acceptance and no
        // absence, which is exactly `Unknown` in the contract's words.
        let ran = Command::new(&self.eidolon)
            .args([
                "send", "--json", "--from", "coordinator",
                "--delivery-key", delivery_key, "--binding-digest", binding_digest,
                to, "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match ran {
            Ok(child) => child,
            Err(e) => return Ok(Correlated::Unknown { why: format!("running `{} send`: {e}", self.eidolon) }),
        };
        if let Some(stdin) = child.stdin.as_mut()
            && let Err(e) = stdin.write_all(text.as_bytes())
        {
            return Ok(Correlated::Unknown { why: format!("writing the brief to `send`: {e}") });
        }
        let out = match child.wait_with_output() {
            Ok(out) => out,
            Err(e) => return Ok(Correlated::Unknown { why: format!("waiting for `send`: {e}") }),
        };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // The harness prints the outcome as JSON and exits 1 for every answer
        // that is not journaled, so the shape is read first and the status only
        // decides when there is nothing to read.
        match serde_json::from_str::<serde_json::Value>(&stdout) {
            Ok(value) => Ok(match serde_json::from_value::<Correlated>(value) {
                Ok(outcome) => outcome,
                Err(e) => Correlated::Unknown {
                    why: format!("`send` answered a correlated outcome this monitor does not know ({e}): {stdout}"),
                },
            }),
            Err(e) => Ok(Correlated::Unknown {
                why: format!(
                    "`eidolon send --json --delivery-key` exited {:?} and did not print an outcome as JSON ({e}): {}{}",
                    out.status.code(),
                    stdout,
                    if stderr.is_empty() { String::new() } else { format!(" ({stderr})") }
                ),
            }),
        }
    }
}

impl Transport for CliTransport {
    fn send(&self, to: &str, text: &str) -> Result<Delivery, String> {
        let mut child = Command::new(&self.eidolon)
            // `--from coordinator` and never `--operator`: the recipient's model is
            // told this came from a tool, not from a peer and not from the person.
            // `--json` is the harness's typed outcome — delivered, queued, partial,
            // failed, refused — so the monitor stores the word it was given instead
            // of reading a sentence.
            .args(["send", "--json", "--from", "coordinator", "--wake", to, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("running `{} send`: {e}", self.eidolon))?;
        child
            .stdin
            .as_mut()
            .ok_or("the send child had no stdin")?
            .write_all(text.as_bytes())
            .map_err(|e| format!("writing the notice to `send`: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("waiting for `send`: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // A refusal prints JSON too and exits 1, so the shape is read first and the
        // status only decides when there is nothing to read.
        let value: serde_json::Value = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "`eidolon send --json` exited {:?} and did not print the outcome as JSON ({e}): {}{}",
                    out.status.code(),
                    stdout,
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(" ({stderr})")
                    }
                ))
            }
        };
        let outcome = match value.get("outcome").and_then(|v| v.as_str()) {
            Some("delivered") => Outcome::Delivered,
            Some("queued") => Outcome::Queued,
            Some("partial") => Outcome::Partial,
            Some("failed") => Outcome::Failed,
            Some("refused") => Outcome::Refused,
            other => {
                return Err(format!(
                    "`eidolon send --json` answered an outcome this monitor does not know ({other:?}): {stdout}"
                ))
            }
        };
        Ok(Delivery {
            outcome,
            words: stdout,
        })
    }
}

struct App {
    /// The store and its poison, behind one lock order — see `Gate`.
    gate: Gate,
    file: PathBuf,
    token: String,
    /// The per-rank text the children are told. Read by this process, never named
    /// by a caller beyond the rank itself.
    blocks: rank::Blocks,
    roster: Box<dyn Roster>,
    transport: Box<dyn Transport>,
    // Lock order is always Gate, then process host. No caller-provided executable.
    launches: Mutex<eidolon_coordinator::process_host::ProcessHost>,
    /// Why the last scan could not see anything, kept so a tool answer can say it
    /// instead of reporting an empty watch.
    blind: Mutex<Vec<String>>,
    now: fn() -> u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One scan. The order — write it down, send it, write the answer down — belongs to
/// `eidolon_coordinator::cycle`; this only supplies the real store, roster and
/// transport, and reports what the scan could not do.
fn cycle(app: &App) {
    let now = (app.now)();
    // The store's lock first, the poison inside it — the same order every door uses.
    let mut store = match app.gate.enter() {
        Ok(s) => s,
        Err(p) => {
            eprintln!("[coordinator] refusing to scan: {p}");
            return;
        }
    };
    let file = app.file.clone();
    let mut save = |s: &Store| store::save(&file, s);
    let scan = scan_cycle(
        &mut store,
        &mut save,
        app.roster.as_ref(),
        app.transport.as_ref(),
        now,
    );
    for note in &scan.notes {
        println!("[coordinator] scan at {now}: {note}");
    }
    *app.blind.lock().expect("the blind lock") = scan.notes.clone();
    if let Some(e) = scan.error {
        // Fail-stop. The store could not be written, so this process's picture of it
        // is no longer evidence: it stops rather than serve that picture, and a
        // restart reads the file and finds out what really landed.
        eprintln!("[coordinator] {e}");
        eprintln!(
            "[coordinator] stopping: a monitor that cannot write its store must not keep serving \
             state it cannot vouch for. Start it again and it will read the file."
        );
        app.gate.poison(e);
        std::process::exit(1);
    }
}

/// The state root, and the one file in it.
fn root() -> PathBuf {
    if let Ok(r) = std::env::var("EIDOLON_COORDINATOR_ROOT") {
        return PathBuf::from(r);
    }
    let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.local/state")
    });
    PathBuf::from(base).join("eidolon/coordinator")
}

/// Where the plugin's rank blocks live: one file per rank, `<rank>.md`.
///
/// The plugin folder *is* the plugin directory, so the blocks ship inside it and an
/// operator edits them where the plugin was installed. `EIDOLON_COORDINATOR_RANKS`
/// overrides the directory for a fixture or a hand-run monitor.
fn ranks_dir() -> PathBuf {
    if let Ok(p) = std::env::var("EIDOLON_COORDINATOR_RANKS") {
        return PathBuf::from(p);
    }
    let cfg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.config")
    });
    PathBuf::from(cfg).join("eidolon/plugins/coordinator/ranks")
}

fn token_path() -> PathBuf {
    match std::env::var("EIDOLON_COORDINATOR_TOKEN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let cfg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.config")
            });
            PathBuf::from(cfg).join("eidolon/coordinator.token")
        }
    }
}

/// The token, made on first run the way the other plugins' services make theirs.
fn token(create: bool) -> Result<String, String> {
    let path = token_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if !create {
        return Err(format!(
            "{} does not hold a token; start the service (or `--print-token`) to make one",
            path.display()
        ));
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let mut bytes = [0u8; 32];
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")
            .map_err(|e| format!("reading /dev/urandom for a token: {e}"))?;
        f.read_exact(&mut bytes)
            .map_err(|e| format!("reading /dev/urandom for a token: {e}"))?;
    }
    let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(text)
}

/// One method of the plugin's door, answered as `{ok, result}` or `{ok, error}`.
fn method(app: &App, method: &str, args: &serde_json::Value, now: u64) -> Result<String, String> {
    let text = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let caller = text("caller");
    if caller.is_empty() {
        // Not a role check — the token is transport auth and this is why the tool
        // is the only thing that can say it: `caller` is this session's own
        // durable address, read from the harness by the tool. A record about a
        // caller nobody named is not an adoption.
        return Err("this call carries no `caller`: the adopting session's own durable address, as the harness publishes it. The plugin's tools read it from the roster; nothing else may name it".to_string());
    }
    // The store's critical section, and the poison read inside it: one order, one
    // entry point, so there is no pre-check left for a caller to race past.
    let mut store = app.gate.enter()?;
    match method {
        "start" => {
            // The rank is a name a caller may give. The digest is not: it is this
            // monitor's reading of the block's bytes, bound into the immutable input
            // below. A caller that names one is asking to be trusted about content
            // nobody hashed, which is the thing the binding exists to prevent.
            if args.get("rank_digest").is_some_and(|v| !v.is_null()) {
                return Err(
                    "`rank_digest` is not a caller's input: the monitor reads the rank block as it prepares \
                     the launch and binds the digest of those exact bytes. Name the `rank`; do not name its \
                     hash"
                        .into(),
                );
            }
            let mut request: eidolon_coordinator::managed_start::Request = serde_json::from_value(args.clone())
                .map_err(|e| format!("invalid managed-start request: {e}"))?;
            if request.brief.trim().is_empty() || request.brief.len() > 65536 {
                return Err("managed start needs a nonempty brief of at most 65536 bytes".into());
            }
            // Read the block now, before the intent is made durable: the digest that
            // goes into the record is the digest of the bytes this launch will hand
            // the child. A rank with no written block binds nothing and adds nothing.
            request.rank_digest = app.blocks.resolve(request.rank.as_deref())?.map(|block| block.digest);
            let mut host = app.launches.lock().map_err(|_| "managed host lock poisoned")?;
            if !store.starts.iter().any(|r| r.request.caller == caller && r.request.operation == request.operation) {
                host.register_fresh(&request)?;
            }
            let file = app.file.clone();
            let mut save = |s: &Store| store::save(&file, s);
            let mut load = || match store::load(&file) {
                Ok(s) => Ok(s),
                Err(e) => { app.gate.poison(e.clone()); Err(e) }
            };
            let result = eidolon_coordinator::managed_start::start(
                &mut store, &request, &mut *host, &mut save, &mut load, now,
            );
            match result {
                Ok(started) => Ok(serde_json::json!({
                    "status": "managed", "operation": request.operation,
                    "session": started.session, "adoption_epoch": started.adoption_epoch,
                    // The receipt as the log gave it, never a transport sentence:
                    // `Journaled`/`Existing` say the record is in that log, and
                    // neither says the model read it.
                    "rank": request.rank, "rank_block_sha256": request.rank_digest,
                    "brief": started.brief, "brief_consumption": "not established",
                }).to_string()),
                Err(error) => {
                    use eidolon_coordinator::managed_start::{Host, LaunchState, Phase};
                    let observed = host.inspect(&request);
                    // A refusal before anything was created is not a partial start.
                    // The host says NotCreated and the record is still a bare intent;
                    // calling that "partial/unconfirmed" would claim an attempt that
                    // never happened — which is what a refused rank block is.
                    let untouched = matches!(observed, Ok(LaunchState::NotCreated))
                        && store.starts.iter().any(|r| {
                            r.request.caller == caller
                                && r.request.operation == request.operation
                                && r.phase == Phase::Intent
                                && r.session.is_none()
                        });
                    if untouched {
                        Err(format!(
                            "managed start {} was refused before anything was created: {error}; host evidence: {observed:?}",
                            request.operation
                        ))
                    } else {
                        Err(format!("managed start {} is partial/unconfirmed: {error}; host evidence: {observed:?}", request.operation))
                    }
                }
            }
        }
        "cancel" => {
            let operation = text("operation");
            let reason = text("reason");
            let mut host = app.launches.lock().map_err(|_| "managed host lock poisoned")?;
            let file = app.file.clone();
            let mut save = |s: &Store| store::save(&file, s);
            let mut load = || match store::load(&file) {
                Ok(s) => Ok(s),
                Err(e) => { app.gate.poison(e.clone()); Err(e) }
            };
            let evidence = eidolon_coordinator::managed_start::cancel(
                &mut store, &caller, &operation, &reason, &mut *host, &mut save, &mut load, now,
            )?;
            Ok(serde_json::json!({
                "status": "exit_observed", "operation": operation, "exit": evidence,
                "adoption_closed": false,
                "supervision": "any existing adoption is retained; coordinator_release is a separate admitted act",
            }).to_string())
        }
        "adopt" => {
            let worker = text("session");
            // A roster that cannot be read is not an absent worker. The lookup's
            // failure refuses the call **before** anything is written, and says
            // which of the two happened, so a reader never has to guess whether
            // the address was missing or the harness was.
            let seen = match app.roster.rows() {
                Ok(rows) => rows
                    .into_iter()
                    .find(|r| r.session.as_deref() == Some(worker.as_str())),
                Err(e) => {
                    return Err(format!(
                        "the roster could not be read, so this adoption was not recorded and nothing \
                         was written: {e}. An unread roster is not an absent worker — try again when \
                         the harness answers"
                    ))
                }
            };
            let file = app.file.clone();
            let mut save = |s: &Store| store::save(&file, s);
            let mut load = || -> Result<Store, String> {
                match store::load(&file) {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        // Condemned from inside the store's critical section, so a
                        // caller already queued on it refuses instead of acting.
                        app.gate.poison(e.clone());
                        Err(e)
                    }
                }
            };
            let name = text("name");
            let note = text("note");
            adopt(
                &mut store,
                AdoptRequest {
                    caller: &caller,
                    worker: &worker,
                    name: &name,
                    note: &note,
                    seen: seen.as_ref(),
                    now,
                },
                &mut save,
                &mut load,
            )
        }
        "release" => {
            let worker = text("session");
            let file = app.file.clone();
            let mut save = |s: &Store| store::save(&file, s);
            let mut load = || -> Result<Store, String> {
                match store::load(&file) {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        app.gate.poison(e.clone());
                        Err(e)
                    }
                }
            };
            release_adoption(
                &mut store,
                &caller,
                &worker,
                &text("reason"),
                now,
                &mut save,
                &mut load,
            )
        }
        "adoptions" => {
            let rows = app.roster.rows();
            Ok(render_adoptions(&store, &caller, rows, &app.blind.lock().expect("the blind lock")))
        }
        "notices" => Ok(render_notices(&store, &caller)),
        other => Err(format!("unknown coordinator method `{other}`")),
    }
}

fn render_adoptions(
    store: &Store,
    caller: &str,
    rows: Result<Vec<Row>, String>,
    blind: &[String],
) -> String {
    let mine = store.records.iter().filter(|r| r.coordinator == caller);
    let mut out = String::new();
    for start in store.starts.iter().filter(|r| r.request.caller == caller) {
        out.push_str(&format!("managed start {}: {}; durable phase {:?}, session {:?}, adoption epoch {:?} (not current liveness)\n",
            start.request.operation,
            match (&start.request.rank, &start.request.rank_digest) {
                (None, _) => "no rank".to_string(),
                (Some(rank), None) => format!("rank `{rank}`, no rank block (nothing appended)"),
                (Some(rank), Some(digest)) => format!("rank `{rank}`, block sha256 {digest}"),
            },
            start.phase, start.session, start.adoption_epoch));
        out.push_str(&format!("  cancellation: {:?} (historical exit receipt, not current liveness)\n", start.cancellation));
    }
    match &rows {
        Ok(rs) if addresses_established(rs) => {}
        Ok(rs) => {
            if !rs.is_empty() {
                out.push_str(
                    "the roster publishes no `session` for any live row: a session with no log has no \
                     id to name, so no adoption can be resolved to one. Null means not established, \
                     not no-such-session.\n",
                );
            }
        }
        Err(e) => out.push_str(&format!(
            "the monitor cannot see the workers: {e}\nNothing below is a claim about any worker's state.\n"
        )),
    }
    let mut any = false;
    for r in mine {
        any = true;
        let live = match &rows {
            Err(_) => "unobservable".to_string(),
            Ok(rs) => {
                let claimants: Vec<&Row> = rs
                    .iter()
                    .filter(|x| x.session.as_deref() == Some(r.worker.as_str()))
                    .collect();
                match claimants.len() {
                    0 => "no live session carries this address".to_string(),
                    1 => {
                        let c = claimants[0];
                        format!(
                            "live as `{}`{}: state `{}`, last `{}`{}",
                            if c.id.is_empty() { "?" } else { &c.id },
                            if c.reached { "" } else { " (doorbell silent)" },
                            c.state,
                            c.last,
                            match c.outcome_at {
                                Some(n) => format!(", ending record `{n}`"),
                                None if c.last == "errored" => match c.outcome_observed {
                                    Some(n) if n > 0 => format!(
                                        ", no journal record for that ending; {n} unjournaled ending(s) counted on this registration instance"
                                    ),
                                    _ => ", no journal record for that ending and no count published — an older writer's row, since this build counts every errored ending".to_string(),
                                },
                                None => ", no ending record published".to_string(),
                            }
                        )
                    }
                    n => format!("{n} live sessions claim this address; refusing to guess"),
                }
            }
        };
        out.push_str(&format!(
            "epoch {} — {}{}\n  adopted {} (this session); {}\n  {}\n",
            r.epoch,
            r.worker,
            if r.name.is_empty() {
                String::new()
            } else {
                format!(" (called `{}`)", r.name)
            },
            r.adopted_at_ms,
            match r.released_at_ms {
                Some(t) => format!("RELEASED at {t}; not monitored"),
                None => "monitored".to_string(),
            },
            live,
        ));
        if !r.note.is_empty() {
            out.push_str(&format!("  note: {}\n", r.note));
        }
    }
    if !any {
        out.push_str("no adoptions recorded by this session.\n");
    }
    if !blind.is_empty() {
        out.push_str("\nthe last scan said:\n");
        for line in blind {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    out
}

fn render_notices(store: &Store, caller: &str) -> String {
    let mine: Vec<&Notice> = store
        .notices
        .iter()
        .filter(|n| n.coordinator == caller)
        .collect();
    if mine.is_empty() {
        return "no notices are owed to this session.\n".to_string();
    }
    let mut out = String::new();
    for n in mine {
        out.push_str(&format!(
            "{} — {} (epoch {}, at {}): state `{}`, last `{}`, occurrence `{}`\n  transport: {} attempt(s), outcome `{}`{}{}\n",
            n.notice_id,
            if n.name.is_empty() { n.worker.clone() } else { format!("{} ({})", n.name, n.worker) },
            n.epoch,
            n.at_ms,
            n.state,
            n.last,
            n.occurrence,
            n.attempts,
            n.outcome.word(),
            if n.key_kind == "observed" {
                " (an ending the journal never saw: keyed on that registration instance's own count)"
            } else {
                ""
            },
            if n.transport.is_empty() {
                String::new()
            } else {
                format!(" — the transport's own words: {}", n.transport)
            }
        ));
    }
    out.push_str(
        "\nThe outcome is the harness's own: `delivered`, `queued`, `partial`, `failed` or `refused`, \
         with the transport's bytes beside it. NONE of these means the coordinator has read the notice: \
         `delivered` says a door took it, and `queued` is not consumption — the recipient's inbox goes \
         with its registration, so a queued notice can disappear with it. `partial` for one direct \
         recipient is an anomaly, not success. A `failed` notice is tried again to a bound; a \
         `delivered`, `queued` or `partial` one is never sent again.\n",
    );
    out
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn call(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let offered = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !bool::from(offered.as_bytes().ct_eq(app.token.as_bytes())) {
        return axum::Json(serde_json::json!({"ok": false, "error": "the coordinator monitor needs its token: the plugin's tools read ~/.config/eidolon/coordinator.token and the grant rows the README's install writes"}));
    }
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return axum::Json(serde_json::json!({"ok": false, "error": format!("not JSON: {e}")})),
    };
    let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or_default();
    let args = parsed.get("args").cloned().unwrap_or(serde_json::json!({}));
    match method_async(&app, method, &args).await {
        Ok(text) => axum::Json(serde_json::json!({"ok": true, "result": text})),
        Err(e) => axum::Json(serde_json::json!({"ok": false, "error": e})),
    }
}

async fn method_async(app: &Arc<App>, name: &str, args: &serde_json::Value) -> Result<String, String> {
    let app = app.clone();
    let name = name.to_string();
    let args = args.clone();
    tokio::task::spawn_blocking(move || method(&app, &name, &args, now_ms())).await
        .map_err(|e| format!("coordinator method task failed: {e}"))?
}

fn usage() -> ! {
    eprintln!(
        "eidolon-coordinator — the coordination monitor\n\
         \n\
           --once               one scan, then exit (the fixture runs this)\n\
           --print-token        make the token if it is missing and print it\n\
           --root <dir>         the state root (default: $EIDOLON_COORDINATOR_ROOT, else\n\
         \t\t\t$XDG_STATE_HOME/eidolon/coordinator)\n\
           --interval-s <n>     seconds between scans (default 30, or $EIDOLON_COORDINATOR_INTERVAL_S)\n\
         \n\
         It watches the workers the adoption records name and sends one notice per\n\
         observed halt. The scan never resumes, steers or cancels anything; starts and\n\
         bounded cancellations are separate admitted calls."
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    let mut once = false;
    let mut print_token = false;
    let mut interval_s: u64 = std::env::var("EIDOLON_COORDINATOR_INTERVAL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => once = true,
            "--print-token" => print_token = true,
            "--root" => match args.next() {
                Some(p) => unsafe_root(p),
                None => usage(),
            },
            "--interval-s" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => interval_s = n,
                None => usage(),
            },
            _ => usage(),
        }
    }

    let token = match token(true) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[coordinator] {e}");
            std::process::exit(1);
        }
    };
    if print_token {
        println!("{token}");
        println!("{}", token_path().display());
        return;
    }

    let dir = root();
    let file = dir.join("state.json");
    // One writer per store, and a store that exists but cannot be read is a
    // refusal rather than a fresh start: starting empty would replace a
    // coordinator's history on the next write. The lock is held for the life of
    // this process — a second `--once` or a second service refuses to start.
    let _owner = match store::own(&dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[coordinator] {e}");
            std::process::exit(1);
        }
    };
    let opened = match store::load(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[coordinator] {e}");
            std::process::exit(1);
        }
    };
    let eidolon = std::env::var("EIDOLON_COORDINATOR_BIN").unwrap_or_else(|_| "eidolon".to_string());
    let app = Arc::new(App {
        gate: Gate::new(opened),
        file,
        token,
        // The same directory the request's digest is read from, handed to the host
        // so it can re-read the block itself right before the spawn.
        blocks: rank::Blocks::new(ranks_dir()),
        roster: Box::new(CliRoster { eidolon: eidolon.clone() }),
        transport: Box::new(CliTransport { eidolon: eidolon.clone() }),
        // **The visible path is wired, and it refuses at its own guard.** A
        // terminal and a compositor are offered here so that a visible request
        // reaching this host is answered with the reason that actually applies —
        // "this host cannot establish a no-focus map" — rather than with "this
        // host has no terminal configured", which would be true but not the
        // point. Nothing establishes a no-focus map, so guard four still refuses
        // every visible launch before a process exists; wiring the other two
        // changes the sentence, not the behaviour.
        launches: Mutex::new(
            eidolon_coordinator::process_host::ProcessHost::new(
                PathBuf::from(&eidolon), dir.join("launches"),
                Box::new(CliRoster { eidolon: eidolon.clone() }), Box::new(CliTransport { eidolon }),
            )
            .with_terminal(
                std::env::var("EIDOLON_COORDINATOR_TERMINAL").unwrap_or_else(|_| "foot".to_string()),
            )
            .with_placement(std::sync::Arc::new(eidolon_coordinator::ancestry::Hyprland))
            // The host re-reads a block here, right before it spawns, and compares
            // it with the digest the operation was recorded with — the same
            // directory this process read the digest from, never a caller's path.
            .with_ranks(rank::Blocks::new(ranks_dir())),
        ),
        blind: Mutex::new(Vec::new()),
        now: now_ms,
    });

    if once {
        cycle(&app);
        return;
    }

    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_s.max(1)));
            loop {
                tick.tick().await;
                cycle(&app);
            }
        });
    }

    let port: u16 = std::env::var("EIDOLON_COORDINATOR_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8095);
    let router = Router::new()
        .route("/health", get(health))
        .route("/call", post(call))
        .with_state(app.clone());
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[coordinator] cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "[coordinator] watching from {} on 127.0.0.1:{port}, every {interval_s}s",
        app.file.display()
    );
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("[coordinator] the listener stopped: {e}");
        std::process::exit(1);
    }
}

fn unsafe_root(p: String) {
    // SAFETY: single-threaded here — this runs during argument parsing, before any
    // thread is started, and nothing else reads the environment concurrently.
    unsafe { std::env::set_var("EIDOLON_COORDINATOR_ROOT", p) }
}

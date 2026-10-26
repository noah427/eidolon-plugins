//! The driver host — the half that runs beside a session, as a plugin's
//! service, and owns the external CLI.
//!
//! eidolon dials this host on a private unix socket and the two sides speak
//! one framed JSON message at a time. The host owns the CLI — its argv, its
//! credential, its stream, and the hook and MCP servers it needs — and reports
//! what it observed; eidolon owns the record, the gate and the execution. The
//! frozen contract is
//! `~/.local/share/eidolon/reports/claude-cli-extraction-final-2026-10-09.md`;
//! this file is its host side.
//!
//! ## What this host deliberately does not do
//!
//! It carries **no session id and no `origin`**. The connection is *about* one
//! session because the dialling side chose the socket, so nothing on the wire
//! needs to say which, and a frame that tries to is a protocol error. And it
//! journals nothing: a record is eidolon's, written from what the host reports,
//! which is what keeps a driver turn indistinguishable from a provider turn.
//!
//! ## What ships today
//!
//! The **first slice**: the transport, the handshake, the refusal rules, and a
//! *fake* backend that scripts one turn so the session side can be tested
//! against a host that never spawns a CLI, spends nothing and reads no
//! credential. The fake host is the real host with a different backend, so
//! nothing here moves when the adapter lands.
//!
//! ## The contract, in one place
//!
//! ```text
//! host → session   hello      {protocol, host, version, backend, modes}
//! session → host   run_turn   {req_id, turn_id, mode, model, guidance,
//!                              tools?, prompt, images?, resume, cwd, scratch}
//! session → host   cancel     {turn_id, req_id, deadline_ms}
//! host → session   event      {turn_id, seq, kind, payload}
//! host → session   session    {turn_id, opaque}      the vendor's id, not ours
//! host → session   usage      {turn_id, priced, vendor}
//! host → session   settle     {turn_id, outcome, stop_reason}
//! host → session   cancel_ack {turn_id, req_id — the session's, echoed, killed}
//! host → session   call       {req_id, turn_id, call_id, tool, input}   registry
//! host → session   adjudicate {req_id, turn_id, call_id, tool, input}   own tools
//! ```
//!
//! In registry mode the session executes the call and the host's `tool_result`
//! event is a **marker** naming the `call_id` — the session journals *its own*
//! held result at that position, so the host's text never becomes the record.
//! In own-tools mode the same event carries the host's reported content, because
//! the CLI ran the tool and the host is the only witness there is.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, oneshot};

pub mod real;

use real::Adapter;

/// The one protocol version this host speaks. A session that names another is
/// refused at the handshake rather than half-understood.
pub const PROTOCOL: &str = "eidolon-driver/1";

/// The largest frame either side will read. The same number the tree already
/// treats as implausible (`ipc::MAX_REQUEST`), checked on the length word
/// before a byte of the body is read.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// The backend this host drives, as the session's `hello` check compares it.
pub const DEFAULT_BACKEND: &str = "claude-cli";

/// What the host calls itself in the handshake and in a refusal message.
pub const DEFAULT_HOST: &str = "claude-bridge";

// ------------------------------------------------------------------ framing

/// Read one length-prefixed frame.
///
/// `u32` little-endian length, then exactly that many bytes of one JSON
/// object — the shape the session log uses on disk (`session/log.rs`), minus
/// its checksum: a log a crash can tear needs one, a socket that cannot reorder
/// or lose a byte does not. A short read, a body that never arrives, or a length
/// past the cap all end the connection, so a frame is never half-applied.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len);
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a frame of {n} bytes is past the {MAX_FRAME}-byte cap"),
        ));
    }
    let mut body = vec![0u8; n as usize];
    r.read_exact(&mut body).await?;
    Ok(body)
}

/// Write one length-prefixed frame, whole or not at all.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if body.len() as u64 > MAX_FRAME as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame over cap",
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

// -------------------------------------------------------------------- binding

/// Bind `path` privately: replace a stale socket file, and refuse to serve
/// from it unless `chmod 0600` succeeds — the rule `eidolon_core::ipc` applies
/// to the hook, MCP and doorbell sockets, and the whole of this host's access
/// story. Not isolation: a same-uid process can reach it, exactly as it can
/// reach those three, and as it can read the session log directly.
pub fn bind_private(path: &Path) -> std::io::Result<UnixListener> {
    // A socket that still answers belongs to a running host. Taking the name
    // away from it would leave every session dialling nothing, so a live socket
    // is never replaced — only a socket nobody is listening on, which is the
    // ordinary case for a host that died without cleaning up after itself.
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            use std::os::unix::fs::FileTypeExt;
            // `symlink_metadata`, never `metadata`: a symlink planted at this
            // path must be refused, not followed and not removed.
            if !meta.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a socket; refusing to touch it",
                        path.display()
                    ),
                ));
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("{} is already served by a running host", path.display()),
                    ));
                }
                // Nobody is listening: stale, and this host may take the name.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                // Anything else — a directory this uid cannot traverse, a socket
                // owned by someone else — is a refusal, not a guess.
                Err(e) => return Err(e),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    // Bind with `std`, narrow the mode at once, then hand it to tokio
    // (`from_std` needs the caller to be inside a runtime).
    //
    // **This sequence is not atomic.** `bind(2)` creates the socket with
    // `0777 & !umask`, and the `chmod` below is a second syscall — between them
    // the socket carries the umask's mode, and a peer that can reach the path can
    // connect in that interval. Two things bound it, and neither is the path's
    // spelling:
    //   * the shipped binary sets `umask(0o077)` before it starts a runtime, so
    //     `bind` creates the socket `0700` itself — owner-only, exactly as
    //     connectable by the owner as `0600` and no more by anyone else — and
    //     there is no interval in which a stranger could connect. It is `0o077`
    //     and not `0o177` because the mask also falls on the *directories* this
    //     process creates, and a directory without its search bit is one nobody,
    //     including its owner, can put a file in;
    //   * a caller that sets no umask (a test, an embedder) is bounded by the
    //     containing directory, which is the only thing deciding who may
    //     traverse to the path at all.
    // The socket's own mode is the protection against *connecting*; the
    // directory's permissions are the protection against *replacing the name*.
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// The default socket: the operator's runtime directory, named for the plugin.
///
/// The directory, not the session tree, because a unix socket path is capped at
/// about 108 bytes.
pub fn default_socket(plugin: &str) -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("eidolon-{plugin}.sock"))
}

/// The default readiness port. Distinct from the browser's 8090 and the
/// LibreWolf host's 8091 so that a `status` for one plugin can never be answered
/// by another.
pub const DEFAULT_HEALTH_PORT: u16 = 8093;

/// Serve the readiness probe `plugins service status` asks: a TCP connect to
/// `127.0.0.1:<port>` and a 2xx from `/health`.
///
/// This is **not** the turn protocol. The transport is the unix socket above;
/// this port exists only so `service start` and `status` have an answer, and it
/// says nothing but "a host is up and speaks which protocol" — there is no
/// secret behind it and nothing here to authenticate. Returns the bound port.
pub async fn serve_health(port: u16) -> std::io::Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let bound = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let asked = String::from_utf8_lossy(&buf[..n]);
                let body = if asked.starts_with("GET /health ") {
                    format!("{{\"ok\":true,\"protocol\":\"{PROTOCOL}\"}}")
                } else {
                    "{\"ok\":false}".to_string()
                };
                let status = if asked.starts_with("GET /health ") {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Ok(bound)
}

// --------------------------------------------------------------------- script

/// What the fake backend does with a turn. A test knob, not a contract: these
/// exist so the session side's negative paths can be exercised by spawning this
/// binary, and the real backend replaces the whole struct.
#[derive(Clone, Debug)]
pub struct Script {
    /// Answer a `cancel` with `cancel_ack`. Off, the connection closes with the
    /// cancel unanswered, which is what the session journals as
    /// *cancelled-unacknowledged*.
    pub cancel_ack: bool,
    /// Skip a sequence number, so the session sees a gap it must fail the turn on.
    pub seq_gap: bool,
    /// The one tool call the scripted turn makes.
    pub tools: Vec<(String, Value)>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            cancel_ack: true,
            seq_gap: false,
            tools: vec![(
                // In own-tools mode this is the vendor's own name for the tool;
                // in registry mode it must be one the session's registry has.
                "bash".to_string(),
                json!({ "command": "echo the fake host ran nothing" }),
            )],
        }
    }
}

impl Script {
    /// Apply the `EIDOLON_FAKE_*` knobs, so a test can spawn the binary with an
    /// environment instead of an API.
    pub fn from_env(mut self) -> Self {
        if let Ok(v) = std::env::var("EIDOLON_FAKE_CANCEL_ACK") {
            self.cancel_ack = v != "0";
        }
        if let Ok(v) = std::env::var("EIDOLON_FAKE_SEQ_GAP") {
            self.seq_gap = v == "1";
        }
        self
    }
}

// ----------------------------------------------------------------------- host

/// Who this host is, and what it will do.
#[derive(Clone)]
pub struct Host {
    pub host: String,
    pub version: String,
    pub backend: String,
    /// The modes offered in `hello`. A `run_turn` naming another is refused and
    /// the connection closes: a session that wants own-tools enforcement and is
    /// handed registry mode, or the reverse, must not be quietly served the
    /// wrong one.
    pub modes: Vec<String>,
    pub script: Script,
    /// The vendor CLI to spawn, when this build has an adapter. `None` is the
    /// fake backend: nothing is spawned and a turn is scripted.
    pub real: Option<Adapter>,
}

impl Host {
    pub fn fake() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            backend: DEFAULT_BACKEND.to_string(),
            modes: vec!["registry".to_string(), "own_tools".to_string()],
            script: Script::default(),
            real: None,
        }
    }
}

/// Serve connections until the listener fails. One connection per session; the
/// host holds many at once and knows nothing about which session any of them
/// is — that binding is the dialling side's, by construction.
pub async fn serve(listener: UnixListener, host: Arc<Host>) {
    // A listener that stops accepting is over; there is nothing to retry into.
    while let Ok((stream, _)) = listener.accept().await {
        let host = host.clone();
        tokio::spawn(async move {
            let _ = connection(stream, host).await;
        });
    }
}

/// The turn a connection has open: its id, and the token a cancel fires. One
/// turn at a time on a connection, which is the session's rule too.
type OpenTurn = Arc<Mutex<Option<(String, Arc<tokio::sync::Notify>)>>>;

/// One connection's outbound side, plus the requests it is waiting on.
pub(crate) struct Conn {
    pub(crate) wr: Arc<Mutex<OwnedWriteHalf>>,
    /// The requests this host is waiting on, by the id it minted. A **synchronous**
    /// mutex, deliberately: it is held for a map insert or a map remove and never
    /// across an `await`, so it is the plainest thing that works — and it means the
    /// guard in [`Slot`] can lock for real when an ask is dropped, instead of the
    /// `try_lock` a `tokio` mutex would force on a `Drop` that cannot await.
    pub(crate) pending: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    pub(crate) next: Arc<AtomicU64>,
}

impl Conn {
    pub(crate) async fn send(&self, v: &Value) -> std::io::Result<()> {
        let mut w = self.wr.lock().await;
        write_frame(&mut *w, v).await
    }

    /// Ask the session something and wait for the answer.
    ///
    /// The id is minted here (`h1`, `h2`, …) and the session echoes it; the
    /// reader routes any frame carrying an id it asked for back to the waiter,
    /// which is why one connection serves both directions.
    pub(crate) async fn ask(&self, mut msg: Value) -> std::io::Result<Value> {
        let rid = format!("h{}", self.next.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(rid.clone(), tx);
        // The slot is cleared when the ask *ends*, however it ends. The reader
        // clears it when a reply arrives and the send error below clears it when
        // the frame cannot go out — but a caller that stops waiting (the hook
        // door's deadline, a cancelled turn) leaves a future that is simply
        // dropped, and nothing on this side would ever remove its slot. One
        // expired hook, one slot, for the life of the connection.
        let _slot = Slot { pending: &self.pending, rid: rid.clone() };
        msg["req_id"] = json!(rid);
        // No `remove` on the error path: the slot goes when this future does,
        // whichever way it ends. See `Slot`.
        self.send(&msg).await?;
        rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the session side went away before it answered",
            )
        })
    }
}

/// Clears an [`Conn::ask`]'s slot when the ask ends, however it ends.
struct Slot<'a> {
    pending: &'a std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>,
    rid: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        // A real lock, not a `try_lock`, so this is removal and not an attempt at
        // it. It can be, because the map is a synchronous mutex: the guard is
        // never held across an `await` on either side, so there is no one who can
        // be holding it for long enough to matter. Poisoning is a panic that
        // happened while the map was locked; the map is a `HashMap` of senders and
        // a panic cannot have left it in a torn state, so taking the guard back is
        // right where `unwrap` would be a second panic inside a `Drop`.
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.rid);
    }
}

async fn connection(stream: UnixStream, host: Arc<Host>) -> std::io::Result<()> {
    let (mut rd, wr) = stream.into_split();
    let conn = Arc::new(Conn {
        wr: Arc::new(Mutex::new(wr)),
        pending: Arc::default(),
        next: Arc::new(AtomicU64::new(1)),
    });

    // The host speaks first: the handshake is what the session validates against
    // the backend it meant to dial.
    conn.send(&json!({
        "method": "hello",
        "protocol": PROTOCOL,
        "host": host.host,
        "version": host.version,
        "backend": host.backend,
        "modes": host.modes,
    }))
    .await?;

    let open: OpenTurn = Arc::default();
    // The process group the current turn spawned, if any: what a cancel signals.
    let pid: Arc<Mutex<Option<u32>>> = Arc::default();
    // Whether the turn now running was cancelled. A turn that ends *because* it
    // was cancelled is a cancelled turn, not a failed one, and saying otherwise
    // would be a different and less true claim.
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The `req_id` of the cancel in flight, for the ack the turn task sends once
    // it has *seen* the process group go.
    let cancel_req: Arc<Mutex<Option<Value>>> = Arc::default();
    let mut turn: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        let Ok(buf) = read_frame(&mut rd).await else {
            break;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            break;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // An answer to something this host asked, if it names an id we minted.
        let answering = msg
            .get("req_id")
            .and_then(Value::as_str)
            .and_then(|rid| conn.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(rid));
        if let Some(tx) = answering {
            let _ = tx.send(msg);
            continue;
        }

        match method.as_str() {
            // The session's confirmation, sent only after it has validated the
            // hello above. Nothing to answer — it names the one mode it wants,
            // and the `run_turn` that follows must use that one. A protocol it
            // does not speak is still a protocol error.
            "hello" => {
                if msg.get("protocol").and_then(Value::as_str) != Some(PROTOCOL) {
                    break;
                }
            }
            "run_turn" => {
                // No session id, no origin: the connection is about one session
                // already, and a frame that names one is a protocol error.
                if msg.get("origin").is_some() || msg.get("session").is_some() {
                    break;
                }
                let mode = msg
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !host.modes.iter().any(|m| m == &mode) {
                    break;
                }
                let Some(turn_id) = msg.get("turn_id").and_then(Value::as_str).map(str::to_string)
                else {
                    break;
                };
                // One turn at a time on a connection; the session owns the same
                // rule on its side and is the one that refuses a second dial.
                if open.lock().await.is_some() {
                    break;
                }
                *open.lock().await = Some((turn_id.clone(), Arc::new(tokio::sync::Notify::new())));
                cancelled.store(false, Ordering::SeqCst);
                let host = host.clone();
                let conn = conn.clone();
                let open = open.clone();
                let pid = pid.clone();
                let cancelled = cancelled.clone();
                let cancel_req = cancel_req.clone();
                let req = msg.clone();
                turn = Some(tokio::spawn(async move {
                    let result = match host.real.clone() {
                        Some(adapter) => {
                            // The turn task owns the process, so it owns the kill
                            // and the ack: only a side that can reap can observe
                            // that a group is gone.
                            let cancel = open
                                .lock()
                                .await
                                .as_ref()
                                .map(|(_, token)| token.clone())
                                .expect("just set");
                            real::real_turn(
                                &conn,
                                &req,
                                &adapter,
                                &host.backend,
                                pid.clone(),
                                cancel,
                                cancel_req.clone(),
                            )
                            .await
                        }
                        None => {
                            let tool = host.script.tools.first().cloned();
                            scripted_turn(&host, &conn, &turn_id, &mode, tool).await
                        }
                    };
                    if let Err(e) = result {
                        // Say how the turn ended rather than leaving the session to
                        // wait for a settle that is not coming. A settle is not an
                        // event, so this cannot disturb the sequence.
                        let (outcome, reason) = if cancelled.load(Ordering::SeqCst) {
                            ("cancelled", "cancelled".to_string())
                        } else {
                            ("failed", format!("the driver host failed the turn: {e}"))
                        };
                        let _ = conn
                            .send(&json!({
                                "method": "settle",
                                "turn_id": turn_id,
                                "outcome": outcome,
                                "stop_reason": reason,
                            }))
                            .await;
                    }
                    *open.lock().await = None;
                    *pid.lock().await = None;
                }));
            }
            "cancel" => {
                // Signal the process group this turn spawned, if it spawned one,
                // then answer. The ack is the host's own word as that process's
                // parent — so a host that has no child to signal says `false`
                // rather than claiming a kill.
                cancelled.store(true, Ordering::SeqCst);
                if host.real.is_some() {
                    // The turn task holds the child, so it signals the group,
                    // reaps it, and only then says whether it saw it go. This side
                    // cannot observe that, so it does not claim it.
                    *cancel_req.lock().await = msg.get("req_id").cloned();
                    if let Some((_, token)) = open.lock().await.as_ref() {
                        token.notify_one();
                    }
                    continue;
                }
                // No process to signal in the scripted backend: it says so
                // rather than reporting a kill it did not make.
                if let Some((_, token)) = open.lock().await.as_ref() {
                    token.notify_one();
                }
                let signalled = false;
                // A cancel the host cannot answer is a closed connection, not a
                // silence: the session then journals cancelled-unacknowledged,
                // and says so, because this host is not a lever it holds.
                if !host.script.cancel_ack {
                    break;
                }
                let turn_id = msg
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let req_id = msg.get("req_id").cloned().unwrap_or(Value::Null);
                let ack = json!({
                    "method": "cancel_ack",
                    "turn_id": turn_id,
                    // The session's own id, echoed — never a new one, so a reply
                    // and a request can never be read as each other.
                    "req_id": req_id,
                    "killed": signalled,
                });
                if conn.send(&ack).await.is_err() {
                    break;
                }
            }
            // An unknown method, a reply nobody asked for, malformed JSON, a
            // short frame: all fail closed.
            _ => break,
        }
    }

    // Wake anything still waiting, so a turn task cannot outlive its connection.
    conn.pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    if let Some(t) = turn {
        t.abort();
    }
    Ok(())
}

/// One scripted turn: the fake backend's whole behaviour.
async fn scripted_turn(
    host: &Host,
    conn: &Conn,
    turn_id: &str,
    mode: &str,
    tool: Option<(String, Value)>,
) -> std::io::Result<()> {
    fn bump(seq: &mut u32) -> u32 {
        let n = *seq;
        *seq += 1;
        n
    }

    let mut seq = 0u32;

    // The model's message. The payload shape is a stand-in for the canonical
    // assistant message the session decodes; the fake backend is the only
    // producer of it, so it moves with the decoder.
    conn.send(&json!({
        "method": "event",
        "turn_id": turn_id,
        "seq": bump(&mut seq),
        "kind": "assistant_message",
        "payload": {
            "role": "assistant",
            "content": [{ "type": "text", "text": "the fake host says hello" }],
        },
    }))
    .await?;

    // A deliberate gap, for the session's negative path: a frame was "lost".
    if host.script.seq_gap {
        seq += 1;
    }

    if let Some((name, input)) = tool {
        let call_id = "c1".to_string();
        if mode == "registry" {
            // The session resolves, gates and runs it, and journals the result
            // in stream order. This host only says where in the stream it sat.
            conn.ask(json!({
                "method": "call",
                "turn_id": turn_id,
                "call_id": call_id,
                "tool": name,
                "input": input,
            }))
            .await?;
            conn.send(&json!({
                "method": "event",
                "turn_id": turn_id,
                "seq": bump(&mut seq),
                "kind": "tool_result",
                // A marker: the call id, and nothing else. The text is the
                // session's own, and this host must not be able to overwrite it.
                "payload": { "call_id": call_id },
            }))
            .await?;
        } else {
            // The CLI ran the tool; this host is the only witness, so its report
            // is the content — and the session marks it as reported, not as
            // something it watched happen.
            let verdict = conn
                .ask(json!({
                    "method": "adjudicate",
                    "turn_id": turn_id,
                    "call_id": call_id,
                    "tool": name,
                    "input": input,
                }))
                .await?;
            // `verdict {req_id, call_id, allow, reason}`: the session's answer,
            // which on this path is advice the CLI is configured to honour and
            // not an execution this host can be made to skip.
            let allowed = verdict.get("allow").and_then(Value::as_bool).unwrap_or(false);
            conn.send(&json!({
                "method": "event",
                "turn_id": turn_id,
                "seq": bump(&mut seq),
                "kind": "tool_result",
                "payload": {
                    "call_id": call_id,
                    "content": if allowed {
                        "the fake host reports a result"
                    } else {
                        "the session denied this call"
                    },
                    "is_error": !allowed,
                },
            }))
            .await?;
        }
    }

    // The vendor's session id: opaque here, and never a name this host gives a
    // session.
    conn.send(&json!({
        "method": "session",
        "turn_id": turn_id,
        "opaque": "fake-vendor-session",
    }))
    .await?;
    conn.send(&json!({
        "method": "usage",
        "turn_id": turn_id,
        "priced": false,
        "vendor": {},
    }))
    .await?;
    conn.send(&json!({
        "method": "settle",
        "turn_id": turn_id,
        "outcome": "completed",
        "stop_reason": "end_turn",
    }))
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped `ask` leaves nothing behind.
    ///
    /// The hook door's deadline drops the future it was waiting on, and a
    /// cancelled turn drops it too. Neither is the send-failure path and neither
    /// is the reply path, so without the guard in `Slot` the slot outlives both
    /// — one per expired hook, for as long as the connection lives.
    #[tokio::test]
    async fn a_dropped_ask_clears_its_slot() {
        let (mine, _theirs) = UnixStream::pair().expect("a socket pair");
        let (_rd, wr) = mine.into_split();
        let conn = Arc::new(Conn {
            wr: Arc::new(Mutex::new(wr)),
            pending: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        });

        // Nobody reads `_theirs`, so the frame is buffered and the ask blocks —
        // exactly what a session that never answers looks like.
        let asked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            conn.ask(json!({ "method": "adjudicate" })),
        )
        .await;
        assert!(asked.is_err(), "nobody was there to answer");

        assert!(
            conn.pending.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "a dropped ask left its slot behind"
        );
    }

    /// **Contention does not save a slot from being cleared.**
    ///
    /// The guard's predecessor used `try_lock` and quietly did nothing if the map
    /// happened to be held, and the honest way to say that is "the leak is unlikely"
    /// — which is exactly the kind of claim that should not survive into a release.
    /// With a synchronous mutex the removal is not attempted but made. This test asks
    /// the question directly: many asks started at once and all dropped at once, so
    /// the drops land while other tasks are inserting.
    ///
    /// It also refuses to pass vacuously. If the map were never observed non-empty
    /// the asks might have failed before inserting anything, and an empty map would
    /// prove nothing; so the run has to show slots in flight as well as none at the
    /// end.
    #[tokio::test]
    async fn concurrent_dropped_asks_all_clear_their_slots() {
        const ASKS: usize = 32;
        let (mine, _theirs) = UnixStream::pair().expect("a socket pair");
        let (_rd, wr) = mine.into_split();
        let conn = Arc::new(Conn {
            wr: Arc::new(Mutex::new(wr)),
            pending: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        });

        let mut tasks = Vec::new();
        for _ in 0..ASKS {
            let conn = conn.clone();
            tasks.push(tokio::spawn(async move {
                // Nobody answers, so every one of these ends by being dropped.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    conn.ask(json!({ "method": "adjudicate" })),
                )
                .await;
            }));
        }

        // Slots really were in flight, or the assertion below means nothing.
        let mut seen_in_flight = false;
        for _ in 0..200 {
            if !conn.pending.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
                seen_in_flight = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(seen_in_flight, "no ask ever registered: the test proves nothing");

        for t in tasks {
            let _ = t.await;
        }
        let left = conn.pending.lock().unwrap_or_else(|p| p.into_inner()).len();
        assert_eq!(left, 0, "{left} of {ASKS} dropped asks left their slot behind");
    }
}

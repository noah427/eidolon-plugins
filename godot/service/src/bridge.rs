//! The bridge: one editor, dialled in, and the calls waiting on it.
//!
//! This is the half of the plugin that faces the editor. It owns the socket the
//! addon connects to, the handshake that decides whether the connection is the
//! editor the operator lent us, the ids that pair a request with its answer, and
//! the rules for what happens when the editor stops answering — which is the
//! normal case, not an exceptional one, because an editor is a window somebody
//! closes.
//!
//! Nothing here knows what a scene tree is. The methods are the addon's, the
//! reasons are the addon's, and this side forwards both: a bridge that second-
//! guessed GDScript would be a second place for the two halves to disagree.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

/// The methods a tool may name. `status` is the host's own (it answers with or
/// without an editor, so a session can ask what it is in front of before it asks
/// anything else); the rest are the addon's, and this host forwards them.
pub const METHODS: [&str; 7] = ["status", "tree", "node", "set", "call", "scene", "run"];

/// The protocol this host and the addon speak. A `hello` from another version is
/// refused by name rather than served with messages it would misread.
const PROTOCOL: u64 = 1;

/// The handshake has to arrive promptly: a socket that connects and then says
/// nothing is a port scanner or a half-dead process, not somebody's editor.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// A line longer than this is not a message either half sends. Without a cap, a
/// local process that never sends a newline could grow this process until the
/// machine noticed.
const MAX_LINE: usize = 1 << 20;

/// How long one write to the editor may take before its socket is called dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The one error a tool gets when there is no editor to ask, in the words the
/// person reading it needs: what is missing, and what makes it appear.
pub const NOT_ATTACHED: &str = "no Godot editor is attached to the bridge. The editor dials this host; \
     open the project you want to work on in Godot and enable the eidolon bridge addon in \
     Project > Project Settings > Plugins. `eidolon plugins service start godot` is what runs this \
     host, and the plugin's README (Prerequisites) says the rest";

/// The editor-side state, exactly as long as an editor is attached.
struct Attached {
    /// Which connection this is. Every clear of the attachment is conditional on it,
    /// because a connection can be superseded: an editor that has gone *stale* may be
    /// replaced — the whole point of the staleness rule — while its own read loop is
    /// still winding down, and that loop's `detach` must not take the editor that
    /// replaced it with it.
    id: u64,
    /// The write half of the editor's socket. A `tokio::sync::Mutex` because a
    /// write *is* an await, and two callers must not interleave their lines.
    write: Arc<AsyncMutex<OwnedWriteHalf>>,
}

struct Inner {
    attached: Option<Attached>,
    /// Ids are the host's: the addon echoes one back and is not asked to keep
    /// any state between messages.
    next_id: u64,
    /// The attachments handed out so far, so that a `detach` can say whether the
    /// attachment it is clearing is still its own.
    attachments: u64,
    pending: BTreeMap<u64, oneshot::Sender<Result<Value, String>>>,
    /// What the editor said about itself in its `hello`. Cleared on detach: this
    /// host must never answer about an editor that is gone.
    project: String,
    engine: String,
    since: Option<Instant>,
    /// When anything last arrived from the editor. The addon sends a tick every
    /// couple of seconds, so silence means the process is gone — killed, crashed,
    /// or the machine went to sleep — and not merely busy.
    last_heard: Option<Instant>,
}

impl Inner {
    /// What this host knows on its own, with no editor asked.
    fn status(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("status".into(), json!("ok"));
        out.insert("attached".into(), json!(self.attached.is_some()));
        out.insert("pending".into(), json!(self.pending.len()));
        out.insert("protocol".into(), json!(PROTOCOL));
        out.insert("since_s".into(), json!(self.since.map(|t| t.elapsed().as_secs())));
        out.insert(
            "last_heard_s".into(),
            json!(self.last_heard.map(|t| t.elapsed().as_secs())),
        );
        out.insert("project".into(), json!(self.project));
        out.insert("engine".into(), json!(self.engine));
        out
    }

    /// True while the attachment is recent enough to believe.
    fn live(&self) -> bool {
        self.attached.is_some()
    }
}

/// The bridge's whole state: the one editor (at most), and the calls waiting on
/// it.
pub struct Bridge {
    /// The token the addon must present, as bytes so the compare can be
    /// constant-time. Never logged, never in an answer: it is the one thing
    /// between any local process and somebody's editor.
    token: Vec<u8>,
    /// How long the editor may be silent before this host calls it gone.
    stale: Duration,
    /// How long one call may wait on the editor.
    call_timeout: Duration,
    inner: Mutex<Inner>,
}

impl Bridge {
    pub fn new(token: String, stale: Duration, call_timeout: Duration) -> Bridge {
        Bridge {
            token: token.into_bytes(),
            stale,
            call_timeout,
            inner: Mutex::new(Inner {
                attached: None,
                next_id: 1,
                attachments: 0,
                pending: BTreeMap::new(),
                project: String::new(),
                engine: String::new(),
                since: None,
                last_heard: None,
            }),
        }
    }

    /// A poisoned lock is a panicking thread, not a reason to stop answering
    /// somebody's editor: the state behind it is a map and two counters, and the
    /// worst a thread can have left it in is one id never reused.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Accept editors until this process ends. One at a time, deliberately: a
    /// second editor is not a second attachment, it is a question about which
    /// project the tools would be talking to.
    pub async fn accept(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    note!("godot bridge: accepting a connection failed: {e}");
                    continue;
                }
            };
            if !peer.ip().is_loopback() {
                note!("godot bridge: refused a connection from {peer}: this bridge is loopback only");
                continue;
            }
            let bridge = self.clone();
            tokio::spawn(async move { bridge.connection(stream).await });
        }
    }

    /// One editor, from hello to the socket closing.
    async fn connection(&self, stream: TcpStream) {
        if let Err(e) = stream.set_nodelay(true) {
            note!("godot bridge: could not turn off Nagle on the editor's socket: {e}");
        }
        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let write = Arc::new(AsyncMutex::new(write));
        let mut buf: Vec<u8> = Vec::new();

        let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_line(&mut reader, &mut buf)).await
        {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                note!("godot bridge: reading an editor's hello failed: {e}");
                return;
            }
            Err(_) => {
                note!(
                    "godot bridge: a socket said nothing for {}s without a hello; closing it",
                    HELLO_TIMEOUT.as_secs()
                );
                return;
            }
        };
        let hello: Value = match serde_json::from_slice(&hello) {
            Ok(v) => v,
            Err(e) => {
                note!("godot bridge: an editor's first line was not JSON: {e}");
                return refuse(
                    &write,
                    "the first line on this socket must be a JSON `hello` — this bridge is \
                     newline-delimited JSON, not a byte stream",
                )
                .await;
            }
        };
        let Some(hello) = hello.get("hello").filter(|h| h.is_object()) else {
            note!("godot bridge: an editor's first message was not a `hello`");
            return refuse(
                &write,
                "the first message must be `{\"hello\": {\"protocol\": 1, \"token\": \"…\"}}` — \
                 see the plugin's addon/eidolon_bridge/bridge.gd, which is the only thing that \
                 should be dialling this port",
            )
            .await;
        };

        let protocol = hello.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != PROTOCOL {
            note!("godot bridge: an editor spoke protocol {protocol}, not {PROTOCOL}");
            return refuse(
                &write,
                &format!(
                    "this host speaks protocol {PROTOCOL} and the addon said {protocol}: update \
                     the addon in the project from the plugin directory, and enable it again"
                ),
            )
            .await;
        }

        let supplied = hello.get("token").and_then(Value::as_str).unwrap_or("");
        if !bool::from(supplied.as_bytes().ct_eq(&self.token)) {
            note!("godot bridge: an editor dialled in with the wrong token; refusing it");
            return refuse(
                &write,
                "the token in the addon's hello does not match this host's token file. The addon \
                 re-reads that file every time it connects, so if it was replaced, disable and \
                 re-enable the plugin in Project > Project Settings",
            )
            .await;
        }

        // One editor at a time. A live attachment wins; a stale one is not
        // detached here — the conversation loop that owns it is about to notice,
        // and two threads detaching the same attachment is a race this can do
        // without.
        let busy = {
            let inner = self.lock();
            inner.live() && inner.last_heard.is_some_and(|t| t.elapsed() < self.stale)
        };
        if busy {
            note!("godot bridge: refused a second editor while one is attached");
            return refuse(
                &write,
                "another Godot editor is already attached to this bridge. Disable the addon \
                 there, or close that editor: this host serves one editor, because a call that \
                 could land in either project would be a call that lands in the wrong one",
            )
            .await;
        }

        let now = Instant::now();
        let (id, engine, project) = {
            let mut inner = self.lock();
            inner.attachments += 1;
            let id = inner.attachments;
            inner.attached = Some(Attached { id, write: write.clone() });
            inner.engine = hello.get("engine").and_then(Value::as_str).unwrap_or("").to_string();
            inner.project = hello.get("project").and_then(Value::as_str).unwrap_or("").to_string();
            inner.since = Some(now);
            inner.last_heard = Some(now);
            (id, inner.engine.clone(), inner.project.clone())
        };
        note!("godot bridge: editor attached: {engine} at {project}");
        if let Err(e) = say(&write, json!({"welcome": {"host": "eidolon-godot", "protocol": PROTOCOL}})).await
        {
            note!("godot bridge: welcoming the editor failed: {e}");
            self.detach(id);
            return;
        }

        let mut last_heard = now;
        loop {
            // Recomputed every turn: any line at all — a result, a tick — pushes
            // the deadline out, and the addon ticks every couple of seconds.
            let left = self.stale.saturating_sub(last_heard.elapsed());
            tokio::select! {
                read = read_line(&mut reader, &mut buf) => match read {
                    Ok(Some(line)) => {
                        match serde_json::from_slice::<Value>(&line) {
                            Ok(msg) => {
                                if self.deliver(msg) {
                                    last_heard = Instant::now();
                                    self.lock().last_heard = Some(last_heard);
                                }
                            }
                            Err(e) => note!("godot bridge: a line from the editor was not JSON ({e}); ignoring it"),
                        }
                    }
                    Ok(None) => {
                        note!("godot bridge: the editor closed the bridge socket");
                        break;
                    }
                    Err(e) => {
                        note!("godot bridge: reading from the editor failed: {e}");
                        break;
                    }
                },
                _ = tokio::time::sleep(left) => {
                    note!(
                        "godot bridge: nothing from the editor for {}s; calling it gone",
                        self.stale.as_secs()
                    );
                    // Whatever was half-read is abandoned with the socket: a
                    // partial line glued to the next connection's first line would
                    // be two messages in one, and neither of them true.
                    buf.clear();
                    break;
                }
            }
        }
        self.detach(id);
    }

    /// One message from the editor. Returns whether it is evidence the editor is
    /// still there — a result for a call, or a tick. Anything else is a peer that
    /// is not the addon this host expects, and is said out loud rather than
    /// silently tolerated.
    fn deliver(&self, msg: Value) -> bool {
        let Value::Object(msg) = msg else {
            note!("godot bridge: an editor sent a JSON value that is not an object");
            return false;
        };
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let waiter = self.lock().pending.remove(&id);
            match waiter {
                Some(tx) => {
                    let reply = if msg.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(msg
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("the editor refused the call without saying why")
                            .to_string())
                    };
                    let _ = tx.send(reply);
                }
                None => note!("godot bridge: a result arrived for id {id}, which nothing is waiting on"),
            }
            return true;
        }
        if msg.contains_key("tick") {
            return true;
        }
        note!(
            "godot bridge: ignoring a message from the editor that is neither a result nor a tick \
             (keys: {:?})",
            msg.keys().collect::<Vec<_>>()
        );
        false
    }

    /// The editor is gone, or never was. Every call in flight fails *now* and by
    /// name: a caller must not be left waiting out its whole timeout on an
    /// attachment that no longer exists.
    ///
    /// Only the connection that holds the attachment may clear it: a connection
    /// whose editor has already been replaced is a fact about a socket, not about
    /// this bridge, and clearing on its way out would drop the editor that took its
    /// place — leaving a live editor attached to nothing.
    fn detach(&self, id: u64) {
        let waiters = {
            let mut inner = self.lock();
            if !inner.attached.as_ref().is_some_and(|a| a.id == id) {
                return;
            }
            inner.attached = None;
            inner.since = None;
            inner.last_heard = None;
            inner.project.clear();
            inner.engine.clear();
            std::mem::take(&mut inner.pending)
        };
        if !waiters.is_empty() {
            note!("godot bridge: the editor went away with {} call(s) in flight", waiters.len());
        }
        for (_, tx) in waiters {
            let _ = tx.send(Err(
                "Godot's bridge connection closed before it answered this call. The editor may \
                 have been closed, or the addon disabled; the answer would have come from a \
                 scene tree that is gone, so there is none"
                    .to_string(),
            ));
        }
    }

    /// Ask the editor one method; the answer is the addon's own.
    pub async fn request(&self, method: &str, args: Value) -> Result<Value, String> {
        let (id, rx, write) = {
            let mut inner = self.lock();
            let Some(attached) = inner.attached.as_ref() else {
                return Err(NOT_ATTACHED.to_string());
            };
            let write = attached.write.clone();
            let id = inner.next_id;
            inner.next_id += 1;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(id, tx);
            (id, rx, write)
        };

        // No lock is held across the write or the wait: the attachment was read
        // once, above, and a `status` call stays answerable while a tree read is
        // outstanding.
        let request = json!({ "id": id, "method": method, "args": args });
        let written = {
            let mut w = write.lock().await;
            tokio::time::timeout(WRITE_TIMEOUT, write_json(&mut w, &request)).await
        };
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.lock().pending.remove(&id);
                return Err(format!("writing `{method}` to the editor failed: {e}"));
            }
            Err(_) => {
                self.lock().pending.remove(&id);
                return Err(format!(
                    "writing `{method}` to the editor took longer than {}s; the socket is not \
                     draining, so this call was given up on",
                    WRITE_TIMEOUT.as_secs()
                ));
            }
        }

        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(format!(
                "Godot's bridge connection closed before it answered `{method}`"
            )),
            Err(_) => {
                self.lock().pending.remove(&id);
                Err(format!(
                    "Godot did not answer `{method}` within {}s. The editor is attached but not \
                     answering: it may be busy, blocked on a modal dialog, or importing; the \
                     call is not retried on its own — look at the editor, and ask again",
                    self.call_timeout.as_secs()
                ))
            }
        }
    }

    /// What the bridge knows, whether or not an editor is attached — a session
    /// can ask before it has anything to ask about. When one *is* attached the
    /// editor's own status is folded in, so the answer says both what this host
    /// sees and what the project says, and an editor that has stopped answering
    /// says that here rather than in a refusal.
    pub async fn status(&self) -> Value {
        let mut out = self.lock().status();
        if !out["attached"].as_bool().unwrap_or(false) {
            out.insert("reason".into(), json!(NOT_ATTACHED));
            return Value::Object(out);
        }
        match self.request("status", json!({})).await {
            Ok(editor) => {
                out.insert("editor_state".into(), editor);
            }
            Err(e) => {
                out.insert("error".into(), json!(e));
            }
        }
        Value::Object(out)
    }

    /// The shallow answer `/health` gives: no editor is asked, so readiness can
    /// never be blocked by one.
    pub fn health(&self) -> Value {
        let inner = self.lock();
        json!({
            "status": "ok",
            "attached": inner.live(),
            "pending": inner.pending.len(),
            "project": inner.project,
            "engine": inner.engine,
        })
    }
}

/// Tell the editor why it is being let go, then close. Nothing waits for it to
/// be read: the write half is dropped when this returns.
async fn refuse(write: &Arc<AsyncMutex<OwnedWriteHalf>>, why: &str) {
    let _ = say(write, json!({ "bye": why })).await;
}

async fn say(write: &Arc<AsyncMutex<OwnedWriteHalf>>, v: Value) -> std::io::Result<()> {
    let mut w = write.lock().await;
    match tokio::time::timeout(WRITE_TIMEOUT, write_json(&mut w, &v)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the editor's socket did not drain",
        )),
    }
}

async fn write_json(w: &mut OwnedWriteHalf, v: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(v).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

/// One line off the editor's socket, without its newline. `buf` is the
/// accumulator, and nothing else may be: bytes read but not yet terminated by a
/// newline stay here across calls, so a `select!` arm that loses a race loses no
/// data. The length is capped, because a peer that never sends a newline is not
/// sending a message.
async fn read_line(
    reader: &mut BufReader<OwnedReadHalf>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>> {
    loop {
        if let Some(at) = buf.iter().position(|b| *b == b'\n') {
            let rest = buf.split_off(at + 1);
            let mut line = std::mem::replace(buf, rest);
            line.truncate(at);
            return Ok(Some(line));
        }
        if buf.len() > MAX_LINE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the editor sent more than {MAX_LINE} bytes with no newline"),
            ));
        }
        if reader.read_buf(buf).await? == 0 {
            // End of stream. A line left unterminated here was cut off by the
            // close, so it is not a message and is dropped with the connection.
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write half of a connected socket pair, which is all an attachment needs to
    /// hold. The other end is dropped: nothing here writes, and a closed peer is not
    /// what these tests are about.
    async fn a_write_half() -> OwnedWriteHalf {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted, connected) = tokio::join!(listener.accept(), TcpStream::connect(addr));
        accepted.unwrap();
        connected.unwrap().into_split().1
    }

    /// Attach `id` to `bridge`. The socket is made *before* the lock is taken: the
    /// guard never crosses an await, the same rule the real attach follows.
    async fn attach(bridge: &Bridge, id: u64) {
        let write = Arc::new(AsyncMutex::new(a_write_half().await));
        let mut inner = bridge.lock();
        inner.attached = Some(Attached { id, write });
        inner.last_heard = Some(Instant::now());
    }

    /// The rule this exists for: an editor that goes stale may be replaced while its
    /// own read loop is still winding down, and that loop's `detach` must not take the
    /// editor that replaced it with it. Written directly against the state, because
    /// the window it guards is between two statements in another task and cannot be
    /// opened on purpose from outside.
    #[tokio::test]
    async fn a_superseded_connection_cannot_detach_the_editor_that_replaced_it() {
        let bridge = Bridge::new("token".into(), Duration::from_secs(30), Duration::from_secs(1));
        attach(&bridge, 1).await;
        attach(&bridge, 2).await;

        bridge.detach(1);
        let inner = bridge.lock();
        assert!(
            inner.attached.is_some(),
            "the older connection's detach took the newer attachment with it"
        );
        assert_eq!(inner.attached.as_ref().unwrap().id, 2);
        drop(inner);

        bridge.detach(2);
        assert!(bridge.lock().attached.is_none(), "the holder may still clear its own");
    }

    #[tokio::test]
    async fn detaching_with_nothing_attached_is_not_an_error() {
        let bridge = Bridge::new("token".into(), Duration::from_secs(30), Duration::from_secs(1));
        bridge.detach(1);
        assert!(bridge.lock().attached.is_none());
    }
}

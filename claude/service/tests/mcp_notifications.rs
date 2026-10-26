//! What the shim does with an id-less request — driven through the shipped
//! binary, over a real door socket, with no CLI and no credential.
//!
//! The audited claim was that `notifications/cancelled` is skipped before its
//! method is even read, so a call the client had cancelled was answered anyway.
//! The door here is this test's own, which is what makes two claims separately
//! observable: **the work was forwarded** (the door is asked) and **the answer
//! was not written** (stdout carries the next call's reply instead).
//!
//! Timing, stated rather than hidden: a notification is answered never, so
//! nothing on the wire can confirm the reader thread has consumed it. Each test
//! that injects one waits [`SETTLE`] before letting the door answer — a wait on
//! a pipe that is already written, not a race the door can lose.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eidolon_claude::{bind_private, read_frame, write_frame};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

/// Long enough for a thread already awake to read one line from a pipe.
const SETTLE: Duration = Duration::from_millis(200);
/// A test that waits forever is a test that has found a hang.
const DEADLINE: Duration = Duration::from_secs(10);

fn me() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eidolon-claude"))
}

fn sock(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "eidolon-claude-mcp-{tag}-{}-{n}.sock",
        std::process::id()
    ))
}

// ----------------------------------------------------------------- the door

/// One forwarded frame, and the answer this test will give it.
struct Door(mpsc::UnboundedReceiver<(Value, oneshot::Sender<Value>)>);

impl Door {
    /// The next **call**, skipping (and answering) a per-call cancellation:
    /// a cancellation is now a door op of its own, and only the test that is
    /// about the cancellation should have to look at it.
    async fn next_call(&mut self) -> (Value, oneshot::Sender<Value>) {
        loop {
            let (op, answer) = self.next_ask().await;
            if op["op"] == "cancel" {
                let _ = answer.send(json!({ "forwarded": true }));
                continue;
            }
            return (op, answer);
        }
    }

    /// The next thing the shim forwarded, held unanswered until the sender is
    /// used — which is what keeps a call "in flight" as long as a test needs.
    async fn next_ask(&mut self) -> (Value, oneshot::Sender<Value>) {
        tokio::time::timeout(DEADLINE, self.0.recv())
            .await
            .expect("the shim forwards within the deadline")
            .expect("the door is still serving")
    }
}

/// Serve the door the way the host does: one frame in, one frame out, and one
/// connection per forward, because the shim connects afresh each time.
async fn door(tag: &str) -> (PathBuf, Door, tokio::task::JoinHandle<()>) {
    let path = sock(tag);
    let listener = bind_private(&path).expect("bind");
    let (tx, asked) = mpsc::unbounded_channel();
    let serving = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let Ok(buf) = read_frame(&mut stream).await else {
                    return;
                };
                let op: Value = match serde_json::from_slice(&buf) {
                    Ok(op) => op,
                    Err(_) => return,
                };
                let (answer_tx, answer_rx) = oneshot::channel();
                if tx.send((op, answer_tx)).is_err() {
                    return;
                }
                let Ok(reply) = answer_rx.await else {
                    return;
                };
                let _ = write_frame(&mut stream, &reply).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (path, Door(asked), serving)
}

// ----------------------------------------------------------------- the shim

struct Shim {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
}

impl Shim {
    async fn start(socket: &Path) -> Self {
        let mut child = tokio::process::Command::new(me())
            .arg("mcp")
            .env("EIDOLON_CLAUDE_MCP", socket)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the shim starts");
        let stdin = child.stdin.take().expect("piped stdin");
        let out = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self { child, stdin, out }
    }

    /// One client line, exactly as the CLI writes them: one JSON object, a
    /// newline, nothing else.
    async fn say(&mut self, v: Value) {
        let mut line = serde_json::to_string(&v).expect("a line");
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.expect("the shim's stdin");
        self.stdin.flush().await.expect("flushed");
    }

    /// The next answer on stdout.
    async fn answer(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(DEADLINE, self.out.read_line(&mut line))
            .await
            .expect("an answer within the deadline")
            .expect("stdout readable");
        assert!(read > 0, "the shim closed its stdout instead of answering");
        serde_json::from_str(&line).expect("one JSON object")
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

// -------------------------------------------------------------------- pieces

fn call(id: u32, command: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "bash", "arguments": { "command": command } },
    })
}

fn cancelled(request: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": request },
    })
}

fn answered(v: &Value) -> (Value, String) {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let text = v
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (id, text)
}

// --------------------------------------------------------------------- tests

/// **The reproduction.** At the pin this test fails on its last two assertions,
/// and its first two pass: the call was forwarded (a cancelled call keeps
/// running — that half of the audit's consequence is still true, and is a
/// boundary rather than a defect), and the shim then answered the very call it
/// had been told had been cancelled, because it skipped every id-less line
/// before reading the method.
#[tokio::test]
async fn a_cancelled_call_still_reaches_the_door_and_its_answer_is_dropped() {
    let (path, mut door, _serving) = door("cancelled").await;
    let mut shim = Shim::start(&path).await;

    shim.say(call(7, "echo the call that will be cancelled")).await;
    let (asked, answer) = door.next_call().await;
    assert_eq!(asked["op"], "call", "a call is forwarded as a door op: {asked}");
    assert_eq!(asked["tool"], "bash");

    // The client gives up on it while it is in flight.
    shim.say(cancelled(json!(7))).await;
    tokio::time::sleep(SETTLE).await;
    answer
        .send(json!({ "content": "the call ran to its answer", "is_error": false, "images": [] }))
        .expect("the door answers");

    // A second call, so the claim is "no answer for 7" rather than "no answer
    // at all": the next thing on stdout must be 9's.
    shim.say(call(9, "echo the call after it")).await;
    let (asked, answer) = door.next_call().await;
    assert_eq!(asked["op"], "call");
    answer
        .send(json!({ "content": "the next call ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(
        id,
        json!(9),
        "the shim answered a call it had been told it was cancelled (id {id}, not 9)"
    );
    assert_eq!(text, "the next call ran");
}

/// A cancellation reaches **the call it names** — including one still waiting to
/// be forwarded, which is the only place this side can genuinely stop work —
/// and does not touch the call that is actually in flight.
#[tokio::test]
async fn a_cancellation_stops_the_call_it_names_and_not_the_one_in_flight() {
    let (path, mut door, _serving) = door("named").await;
    let mut shim = Shim::start(&path).await;

    shim.say(call(7, "echo seven, in flight")).await;
    let (asked, seven) = door.next_call().await;
    assert_eq!(asked["op"], "call");

    // Queued behind it, then cancelled before it is ever forwarded.
    shim.say(call(9, "echo nine, never forwarded")).await;
    shim.say(cancelled(json!(9))).await;
    tokio::time::sleep(SETTLE).await;
    seven
        .send(json!({ "content": "seven ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    // Seven is answered; nine must never reach the door at all.
    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(7), "the call in flight is the one the cancellation did not name");
    assert_eq!(text, "seven ran");

    shim.say(call(11, "echo eleven")).await;
    let (asked, eleven) = door.next_call().await;
    assert_eq!(
        asked["input"]["command"], "echo eleven",
        "nine was forwarded anyway: {asked}"
    );
    eleven
        .send(json!({ "content": "eleven ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(11));
    assert_eq!(text, "eleven ran");
}

/// A cancellation that names nothing in flight — a request never issued, a bare
/// notification with no request id, one naming a call already answered — cancels
/// nothing, and cannot reach anyone else's work.
#[tokio::test]
async fn a_cancellation_that_names_nothing_in_flight_cancels_nothing() {
    let (path, mut door, _serving) = door("unowned").await;
    let mut shim = Shim::start(&path).await;

    shim.say(call(7, "echo seven")).await;
    let (asked, seven) = door.next_call().await;
    assert_eq!(asked["op"], "call");

    // Eight was never issued by this client; a bare notification names nothing
    // at all; the string "7" is a different id from the number 7.
    shim.say(cancelled(json!(8))).await;
    shim.say(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" })).await;
    shim.say(cancelled(json!("7"))).await;
    tokio::time::sleep(SETTLE).await;
    seven
        .send(json!({ "content": "seven ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(7), "an unowned cancellation cancelled someone else's work");
    assert_eq!(text, "seven ran");

    // And one naming a call already answered is stale, not retroactive.
    shim.say(cancelled(json!(7))).await;
    shim.say(call(9, "echo nine")).await;
    let (asked, nine) = door.next_call().await;
    assert_eq!(asked["op"], "call");
    nine.send(json!({ "content": "nine ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(9));
    assert_eq!(text, "nine ran");
}

/// **A live id is never overwritten, and that is what makes the `initialize`
/// refusal mean something.**
///
/// The hole a reviewer discriminated: `heard` used to replace whatever was
/// registered under an id. A duplicate `tools/call` naming the id of a *queued*
/// `initialize` took its slot, a later `notifications/cancelled` then marked the
/// slot as a cancelled call, and the `initialize` was dropped unanswered — by a
/// shim whose own doc says a client must not cancel it. So: the first request
/// keeps the slot, the duplicate is refused with a JSON-RPC error, and the
/// `initialize` is answered.
#[tokio::test]
async fn a_duplicate_live_id_is_refused_and_displaces_nothing() {
    let (path, mut door, _serving) = door("duplicate").await;
    let mut shim = Shim::start(&path).await;

    // One call in flight, so the next requests queue behind it.
    shim.say(call(5, "echo five, in flight")).await;
    let (asked, five) = door.next_call().await;
    assert_eq!(asked["op"], "call");

    // A queued `initialize`, then a duplicate id, then a cancellation naming it.
    shim.say(json!({
        "jsonrpc": "2.0", "id": 7, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" },
    }))
    .await;
    shim.say(call(7, "echo seven, a duplicate")).await;
    shim.say(cancelled(json!(7))).await;
    tokio::time::sleep(SETTLE).await;
    five.send(json!({ "content": "five ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    // Five is answered, then the initialize — not cancelled, not overwritten.
    let first = shim.answer().await;
    assert_eq!(first["id"], json!(5), "the in-flight call: {first}");
    let second = shim.answer().await;
    assert_eq!(second["id"], json!(7), "the initialize: {second}");
    assert_eq!(
        second.pointer("/result/serverInfo/name").and_then(Value::as_str),
        Some("eidolon"),
        "the initialize did not answer: {second}"
    );

    // And the duplicate is refused rather than served.
    let third = shim.answer().await;
    assert_eq!(third["id"], json!(7), "the duplicate: {third}");
    assert_eq!(
        third.pointer("/error/code").and_then(Value::as_i64),
        Some(-32600),
        "the duplicate was not refused: {third}"
    );
}

/// **A cancellation of a call in flight is told to the door, by name.**
///
/// Suppressing the answer is what this shim can do alone; stopping the work needs
/// the session, and the session can only be reached through the door. So the
/// reader — the thread that is free while the forward is blocked — sends
/// `{op:"cancel", call_id, gen}` on a connection of its own. The answer is still
/// never written, whatever the door says about forwarding: the two are separate
/// obligations and neither one is conditional on the other.
#[tokio::test]
async fn a_cancellation_is_told_to_the_door_by_name() {
    let (path, mut door, _serving) = door("told").await;
    let mut shim = Shim::start(&path).await;

    shim.say(call(7, "echo the call that will be cancelled")).await;
    let (asked, seven) = door.next_ask().await;
    assert_eq!(asked["op"], "call");
    let named = asked["call_id"].as_str().expect("the call carries an id").to_string();
    let generation = asked["gen"].as_u64().expect("the call carries a generation");

    // The client gives up while the call is in flight.
    shim.say(cancelled(json!(7))).await;
    let (told, answer) = door.next_ask().await;
    assert_eq!(told["op"], "cancel", "the door was not told: {told}");
    assert_eq!(told["call_id"], json!(named), "the cancellation names the wrong call: {told}");
    assert_eq!(told["gen"], json!(generation), "the generation must match the call: {told}");
    answer.send(json!({ "forwarded": true })).expect("the door answers the op");

    // And the answer to the call itself is still withheld.
    seven
        .send(json!({ "content": "the call ran", "is_error": false, "images": [] }))
        .expect("the door answers the call");
    shim.say(call(9, "echo the call after it")).await;
    let (_, nine) = door.next_ask().await;
    nine.send(json!({ "content": "nine ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(9), "the cancelled call was answered: {id}");
    assert_eq!(text, "nine ran");
}

/// **A cancellation of something never issued tells the door nothing.**
///
/// The bound the session relies on: the only call ids this shim can name are the
/// ones it minted for its own live requests. An id it never registered produces no
/// door op at all, so there is nothing for a stale or forged name to reach.
#[tokio::test]
async fn a_cancellation_of_an_unissued_id_tells_the_door_nothing() {
    let (path, mut door, _serving) = door("untold").await;
    let mut shim = Shim::start(&path).await;

    shim.say(cancelled(json!(8))).await;
    shim.say(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" })).await;
    tokio::time::sleep(SETTLE).await;

    // The next thing the door sees is a real call, not a cancel op.
    shim.say(call(9, "echo nine")).await;
    let (asked, nine) = door.next_ask().await;
    assert_eq!(
        asked["op"], "call",
        "a cancellation of an unissued id was passed to the door: {asked}"
    );
    nine.send(json!({ "content": "nine ran", "is_error": false, "images": [] }))
        .expect("the door answers");

    let (id, text) = answered(&shim.answer().await);
    assert_eq!(id, json!(9));
    assert_eq!(text, "nine ran");
}

//! The host's obligations, one test each. Every test terminates: a host is
//! spawned, driven, and the task is dropped. Nothing here reads a credential,
//! spawns a CLI, or touches the network.
//!
//! The list is the acceptance list of the first slice, host side: the
//! handshake, framing and its refusals, the seq rule, both modes' event order,
//! the marker that carries no text, the cancel's ack and its absence, and the
//! one thing this host must *not* do — limit itself to a single session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eidolon_claude::{
    Host, MAX_FRAME, PROTOCOL, Script, bind_private, read_frame, serve, serve_health, write_frame,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn sock(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "eidolon-claude-{tag}-{}-{n}.sock",
        std::process::id()
    ))
}

async fn host(script: Script, tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let path = sock(tag);
    let listener = bind_private(&path).expect("bind");
    let host = Arc::new(Host {
        script,
        ..Host::fake()
    });
    (path, tokio::spawn(serve(listener, host)))
}

/// One frame, or a panic: a test that hangs is a test that has found a hang.
async fn read(s: &mut UnixStream) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(s))
        .await
        .expect("a frame within the deadline")
        .expect("a frame");
    serde_json::from_slice(&frame).expect("one JSON object")
}

/// Connect and read the host's `hello`, which the host speaks first.
async fn dial(path: &Path) -> UnixStream {
    let mut s = UnixStream::connect(path).await.expect("connect");
    let hello = read(&mut s).await;
    assert_eq!(hello["method"], "hello");
    assert_eq!(hello["protocol"], PROTOCOL);
    assert_eq!(hello["backend"], "claude-cli");
    s
}

/// The session's confirmation, naming the one mode it wants.
async fn confirm(s: &mut UnixStream, mode: &str) {
    write_frame(
        s,
        &json!({
            "method": "hello",
            "protocol": PROTOCOL,
            "host": "test-client",
            "version": "0",
            "backend": "claude-cli",
            "modes": [mode],
        }),
    )
    .await
    .expect("the confirmation");
}

async fn run_turn(s: &mut UnixStream, mode: &str) {
    write_frame(
        s,
        &json!({
            "method": "run_turn",
            "req_id": "c1",
            "turn_id": "t1",
            "mode": mode,
            "model": "sonnet",
            "guidance": { "vault": null, "registry": null },
            "tools": [],
            "prompt": "hello",
            "images": [],
            "resume": null,
            "cwd": "/tmp",
            "scratch": "/tmp",
        }),
    )
    .await
    .expect("a turn");
}

#[tokio::test]
async fn the_host_announces_the_contract_before_it_is_asked_anything() {
    let (path, h) = host(Script::default(), "hello").await;
    // No confirmation, no question: the hello is the first frame on the wire.
    let mut s = UnixStream::connect(&path).await.expect("connect");
    let hello = read(&mut s).await;
    assert_eq!(hello["method"], "hello");
    assert_eq!(hello["host"], "claude-bridge");
    assert_eq!(hello["modes"], json!(["registry", "own_tools"]));
    h.abort();
}

#[tokio::test]
async fn a_registry_turn_marks_the_result_it_may_not_author() {
    let (path, h) = host(Script::default(), "reg").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "registry").await;
    run_turn(&mut s, "registry").await;

    // seq 0: the model's message, in the shape the session decodes.
    let first = read(&mut s).await;
    assert_eq!(first["method"], "event");
    assert_eq!(first["kind"], "assistant_message");
    assert_eq!(first["seq"], 0);
    assert_eq!(first["payload"]["role"], "assistant");
    assert_eq!(first["payload"]["content"][0]["type"], "text");

    // Then the host asks the session to run the tool, and waits.
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "call");
    assert_eq!(ask["tool"], "bash");
    let req_id = ask["req_id"].clone();
    let call_id = ask["call_id"].clone();

    // The session answers with the result it executed and holds.
    write_frame(
        &mut s,
        &json!({
            "method": "tool_result",
            "req_id": req_id,
            "call_id": call_id,
            "content": "the session's own result",
            "is_error": false,
            "images": [],
        }),
    )
    .await
    .expect("the session's answer");

    // The marker: the call id and nothing else. The text is the session's, and
    // a host that could put its own words here could overwrite evidence.
    let marker = read(&mut s).await;
    assert_eq!(marker["method"], "event");
    assert_eq!(marker["kind"], "tool_result");
    assert_eq!(marker["seq"], 1, "sequence numbers are contiguous from zero");
    assert_eq!(marker["payload"]["call_id"], "c1");
    assert!(
        marker["payload"].get("content").is_none(),
        "a registry marker carries no text"
    );

    // Then the mark, the accounting, and the end.
    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    assert_eq!(mark["opaque"], "fake-vendor-session");
    let usage = read(&mut s).await;
    assert_eq!(usage["method"], "usage");
    assert_eq!(usage["priced"], false);
    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["outcome"], "completed");
    assert_eq!(settle["turn_id"], "t1");
    h.abort();
}

#[tokio::test]
async fn an_own_tools_turn_carries_the_result_the_host_is_the_only_witness_of() {
    let (path, h) = host(Script::default(), "own").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "own_tools").await;
    run_turn(&mut s, "own_tools").await;

    let first = read(&mut s).await;
    assert_eq!(first["kind"], "assistant_message");

    // Own tools: the host asks for a verdict and never executes anything.
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "adjudicate");
    assert_eq!(ask["tool"], "bash");
    write_frame(
        &mut s,
        &json!({
            "method": "verdict",
            "req_id": ask["req_id"].clone(),
            "call_id": ask["call_id"].clone(),
            "allow": true,
            "reason": "the operator's table says yes",
        }),
    )
    .await
    .expect("a verdict");

    // The reported result: here the host *is* the witness, so the text travels.
    let reported = read(&mut s).await;
    assert_eq!(reported["method"], "event");
    assert_eq!(reported["kind"], "tool_result");
    assert_eq!(reported["seq"], 1);
    assert_eq!(reported["payload"]["call_id"], "c1");
    assert!(reported["payload"]["content"].is_string());
    assert_eq!(reported["payload"]["is_error"], false);
    h.abort();
}

#[tokio::test]
async fn one_host_serves_two_sessions_at_once() {
    let (path, h) = host(Script::default(), "two").await;

    // Both connections are open together: the host holds no session identity and
    // must never be read as limiting itself to one. Exclusivity is the dialling
    // side's rule, per session.
    let mut a = dial(&path).await;
    let mut b = dial(&path).await;
    confirm(&mut a, "registry").await;
    confirm(&mut b, "own_tools").await;
    run_turn(&mut a, "registry").await;
    run_turn(&mut b, "own_tools").await;

    let first_a = read(&mut a).await;
    let first_b = read(&mut b).await;
    assert_eq!(first_a["turn_id"], "t1");
    assert_eq!(first_b["turn_id"], "t1");

    let ask_a = read(&mut a).await;
    let ask_b = read(&mut b).await;
    assert_eq!(ask_a["method"], "call", "the registry connection is executed for");
    assert_eq!(ask_b["method"], "adjudicate", "the own-tools connection is asked");
    h.abort();
}

#[tokio::test]
async fn a_mode_the_host_did_not_offer_closes_the_connection() {
    let (path, h) = host(Script::default(), "mode").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "registry").await;
    run_turn(&mut s, "wire").await;
    // Never quietly served the wrong mode.
    assert!(read_frame(&mut s).await.is_err());
    h.abort();
}

#[tokio::test]
async fn a_frame_naming_a_session_closes_the_connection() {
    let (path, h) = host(Script::default(), "session").await;
    let mut s = dial(&path).await;
    write_frame(
        &mut s,
        &json!({ "method": "run_turn", "session": "someone-else", "turn_id": "t1" }),
    )
    .await
    .expect("the offence");
    assert!(read_frame(&mut s).await.is_err());
    h.abort();
}

#[tokio::test]
async fn a_truncated_frame_closes_the_connection() {
    let (path, h) = host(Script::default(), "trunc").await;
    let mut s = dial(&path).await;
    // A length word that promises 100 bytes, three of them, then a closed write
    // side: a half frame is never applied.
    s.write_all(&100u32.to_le_bytes()).await.expect("a length");
    s.write_all(b"abc").await.expect("a fragment");
    s.shutdown().await.expect("a closed write side");
    assert!(read_frame(&mut s).await.is_err());
    h.abort();
}

#[tokio::test]
async fn a_length_past_the_cap_closes_the_connection_before_the_body() {
    let (path, h) = host(Script::default(), "cap").await;
    let mut s = dial(&path).await;
    s.write_all(&(MAX_FRAME + 1).to_le_bytes())
        .await
        .expect("an implausible length");
    assert!(read_frame(&mut s).await.is_err());
    h.abort();
}

#[tokio::test]
async fn a_cancel_is_acknowledged_with_the_sessions_own_request_id() {
    let (path, h) = host(Script::default(), "cancel").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "registry").await;
    write_frame(
        &mut s,
        &json!({ "method": "cancel", "turn_id": "t1", "req_id": "c8", "deadline_ms": 5000 }),
    )
    .await
    .expect("a cancel");
    let ack = read(&mut s).await;
    assert_eq!(ack["method"], "cancel_ack");
    assert_eq!(ack["req_id"], "c8", "the session's id, echoed, never a new one");
    assert_eq!(ack["turn_id"], "t1");
    // `killed` is the host's own word as a process's parent, and this host is
    // not one: the fake backend spawns nothing. It says so rather than claiming
    // a kill it did not make; the real adapter signals the group it spawned and
    // reports `true` there.
    assert_eq!(ack["killed"], false);
    h.abort();
}

#[tokio::test]
async fn without_an_ack_the_host_closes_with_the_cancel_unanswered() {
    let script = Script {
        cancel_ack: false,
        ..Script::default()
    };
    let (path, h) = host(script, "noack").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "registry").await;
    write_frame(
        &mut s,
        &json!({ "method": "cancel", "turn_id": "t1", "req_id": "c8", "deadline_ms": 5000 }),
    )
    .await
    .expect("a cancel");
    // Nothing is claimed on this path: the session journals
    // cancelled-unacknowledged, because this host is not a lever it holds.
    assert!(read_frame(&mut s).await.is_err());
    h.abort();
}

#[tokio::test]
async fn the_gap_knob_skips_a_sequence_number() {
    let script = Script {
        seq_gap: true,
        ..Script::default()
    };
    let (path, h) = host(script, "gap").await;
    let mut s = dial(&path).await;
    confirm(&mut s, "registry").await;
    run_turn(&mut s, "registry").await;

    let first = read(&mut s).await;
    assert_eq!(first["seq"], 0);
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "call");
    write_frame(
        &mut s,
        &json!({
            "method": "tool_result",
            "req_id": ask["req_id"].clone(),
            "call_id": ask["call_id"].clone(),
            "content": "x",
            "is_error": false,
            "images": [],
        }),
    )
    .await
    .expect("the answer");
    let marker = read(&mut s).await;
    assert_eq!(marker["seq"], 2, "1 was skipped: a gap the session must fail on");
    h.abort();
}

#[tokio::test]
async fn framing_round_trips_and_the_cap_is_read_before_the_body() {
    let mut buf = Vec::new();
    write_frame(&mut buf, &json!({ "a": 1 })).await.expect("a frame");
    assert_eq!(&buf[..4], &(7u32).to_le_bytes(), "u32 little-endian length");
    let mut reader = &buf[..];
    let got: Value = serde_json::from_slice(&read_frame(&mut reader).await.expect("a frame"))
        .expect("one JSON object");
    assert_eq!(got["a"], 1);

    // An implausible length word is refused without a body ever being read, so
    // no peer can make this host allocate.
    let mut bad = (MAX_FRAME + 1).to_le_bytes().to_vec();
    bad.extend_from_slice(b"x");
    let mut reader = &bad[..];
    assert!(read_frame(&mut reader).await.is_err());
}

/// The binary, not the library: binding happens inside a runtime, and the mode
/// is set before the socket can be listened on.
///
/// This is the test for a bug the library tests could not see — `bind_private`
/// called outside a runtime panicked after creating the file, leaving a socket
/// at the umask's mode, world-readable, in a directory anyone can list. Driving
/// the real process is the only way to catch an ordering mistake in `main`.
#[tokio::test]
async fn the_binary_binds_a_private_socket_and_answers_on_it() {
    let path = sock("bin");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_eidolon-claude"))
        .arg("--socket")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the host");

    // Wait until it accepts. Accepting implies the chmod ran, so the mode check
    // below is not racing the bind — and if the chmod never ran, this never
    // becomes 0600.
    let mut connected = None;
    for _ in 0..200 {
        if let Ok(s) = UnixStream::connect(&path).await {
            connected = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut stream = connected.expect("the host never accepted a connection");

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path)
        .expect("the socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the socket is private before it is listened on");

    let hello = read(&mut stream).await;
    assert_eq!(hello["method"], "hello");
    assert_eq!(hello["protocol"], PROTOCOL);

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
}

// ------------------------------------------------------- the socket's own rules

#[tokio::test]
async fn a_live_socket_is_never_taken_from_a_running_host() {
    let path = sock("live");
    let first = bind_private(&path).expect("the first host binds");
    // A second host starting must not unlink the name out from under the first:
    // every session dialling it would find nothing, and the failure would look
    // like the running host's.
    let err = bind_private(&path).expect_err("a live socket is not replaceable");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    // And the first host is still the one answering there.
    let _still_there = UnixStream::connect(&path)
        .await
        .expect("the running host still answers");
    drop(first);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_stale_socket_is_replaced() {
    let path = sock("stale");
    {
        let listener = bind_private(&path).expect("bind");
        drop(listener); // a host that died without cleaning up after itself
    }
    assert!(path.exists(), "the dead host left its socket behind");
    let _fresh = bind_private(&path).expect("a socket nobody answers on is stale, not live");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_file_that_is_not_a_socket_is_refused_and_left_alone() {
    let path = sock("file");
    std::fs::write(&path, b"not a socket").expect("a planted file");
    let err = bind_private(&path).expect_err("a plain file is not this host's to remove");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&path).expect("still there"),
        b"not a socket",
        "the refusal deleted nothing"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_symlink_at_the_path_is_refused_and_not_followed() {
    let target = sock("symlink-target");
    let path = sock("symlink");
    std::fs::write(&target, b"keep me").expect("a target");
    std::os::unix::fs::symlink(&target, &path).expect("a symlink");
    assert!(
        bind_private(&path).is_err(),
        "a symlink is neither followed nor removed"
    );
    assert_eq!(std::fs::read(&target).expect("target intact"), b"keep me");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn the_bound_socket_is_private() {
    let path = sock("mode");
    let _listener = bind_private(&path).expect("bind");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path)
        .expect("the socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_readiness_probe_answers_health_and_nothing_else() {
    let port = serve_health(0).await.expect("bind the readiness probe");

    let answer = |request: &'static str| async move {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        s.write_all(request.as_bytes()).await.expect("ask");
        let mut body = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut body)).await;
        String::from_utf8_lossy(&body).to_string()
    };

    // What `plugins service status` asks, and the only thing answered.
    let health = answer("GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.contains(PROTOCOL), "the probe says which protocol is up");

    // A port that answers anything is a probe that cannot fail.
    let other = answer("GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(other.starts_with("HTTP/1.1 404 Not Found"), "{other}");
}

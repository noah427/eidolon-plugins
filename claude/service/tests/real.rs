//! The adapter, driven against a fake CLI.
//!
//! The fake CLI is not a stub of the stream: it emits the stream-json shapes
//! `crates/claude/src/stream.rs` parses, and it really runs the `PreToolUse`
//! command named in the settings file the adapter wrote. So these tests exercise
//! the hook door, the gate's answer, the message assembly and the settle — over
//! a real socket, with a real child process — and none of them touches a
//! credential, a network or a real CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eidolon_claude::real::{Adapter, HOOK_DEADLINE};
use eidolon_claude::{Host, PROTOCOL, bind_private, read_frame, serve, write_frame};
use serde_json::{Value, json};
use tokio::net::UnixStream;

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cli.py")
}

fn me() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eidolon-claude"))
}

fn sock(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "eidolon-claude-real-{tag}-{}-{n}.sock",
        std::process::id()
    ))
}

async fn host(real: bool, tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let path = sock(tag);
    let listener = bind_private(&path).expect("bind");
    let adapter = real.then(|| Adapter {
        cli: fake_cli(),
        me: me(),
        hook_deadline: HOOK_DEADLINE,
    });
    let host = Arc::new(Host {
        real: adapter,
        ..Host::fake()
    });
    (path, tokio::spawn(serve(listener, host)))
}

async fn read(s: &mut UnixStream) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(20), read_frame(s))
        .await
        .expect("a frame within the deadline")
        .expect("a frame");
    serde_json::from_slice(&frame).expect("one JSON object")
}

async fn dial(path: &std::path::Path) -> UnixStream {
    let mut s = UnixStream::connect(path).await.expect("connect");
    let hello = read(&mut s).await;
    assert_eq!(hello["method"], "hello");
    assert_eq!(hello["protocol"], PROTOCOL);
    s
}

async fn run_turn(s: &mut UnixStream, mode: &str) {
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
    write_frame(
        s,
        &json!({
            "method": "run_turn",
            "req_id": "c1",
            "turn_id": "t1",
            "mode": mode,
            "model": "sonnet",
            "guidance": { "vault": null, "registry": null },
            "tools": null,
            "prompt": "run the check",
            "images": [],
            "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "scratch": std::env::temp_dir().to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");
}

#[tokio::test]
async fn a_real_backend_offers_both_modes_because_it_serves_both() {
    let (path, h) = host(true, "modes").await;
    let mut s = UnixStream::connect(&path).await.expect("connect");
    let hello = read(&mut s).await;
    // The hook door serves own-tools and the MCP door serves registry. A host
    // that offered a mode it could not serve would be advertising a turn that
    // runs ungated, which is worse than not offering it.
    assert_eq!(hello["modes"], json!(["registry", "own_tools"]));
    h.abort();
}

#[tokio::test]
async fn an_own_tools_turn_spawns_the_cli_gates_its_call_and_settles() {
    let (path, h) = host(true, "turn").await;
    let mut s = dial(&path).await;
    run_turn(&mut s, "own_tools").await;

    // The resume mark, straight off the CLI's `system/init`.
    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    assert_eq!(mark["opaque"], "fake-session-1");

    // The gate is asked *before* the CLI reports anything, because the CLI is
    // blocked on the hook while it waits.
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "adjudicate");
    assert_eq!(ask["tool"], "Bash");
    assert_eq!(ask["input"]["command"], "echo hi");
    // The tier travels with the ask: the vendor tool names are this side's to
    // know, and a session that does not read it falls back to the conservative
    // one. Bash is not a read.
    assert_eq!(ask["approval"], "mutating");
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

    // The message: two `assistant` lines under one id, concatenated — not two
    // messages, which is what the CLI's framing would suggest.
    let message = read(&mut s).await;
    assert_eq!(message["method"], "event");
    assert_eq!(message["kind"], "assistant_message");
    assert_eq!(message["seq"], 0);
    let content = message["payload"]["content"]
        .as_array()
        .expect("the message's blocks");
    assert_eq!(content.len(), 2, "one message, two blocks: {content:?}");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["id"], "toolu_1");

    // One usage frame for the one model call the message was.
    let usage = read(&mut s).await;
    assert_eq!(usage["method"], "usage");
    assert_eq!(usage["priced"]["input_tokens"], 11);

    // The reported result — own-tools mode, so this host is the only witness and
    // its text travels.
    let reported = read(&mut s).await;
    assert_eq!(reported["method"], "event");
    assert_eq!(reported["kind"], "tool_result");
    assert_eq!(reported["seq"], 1);
    assert_eq!(reported["payload"]["call_id"], "toolu_1");
    assert_eq!(reported["payload"]["content"], "faked tool output");
    assert_eq!(reported["payload"]["is_error"], false);

    // What the transcript had grown to, before the settle it belongs to. The
    // in-process driver journals this too, so a driver turn shows a size where a
    // provider turn does; without it the client prints `[context —]`.
    let context = read(&mut s).await;
    assert_eq!(context["method"], "event");
    assert_eq!(context["kind"], "context_size");
    assert_eq!(context["seq"], 2, "the next event number, with no gap");
    assert_eq!(context["payload"]["tokens"], 11);

    // And the end.
    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["turn_id"], "t1");
    assert_eq!(settle["outcome"], "completed");
    h.abort();
}

#[tokio::test]
async fn a_denied_call_reaches_the_cli_as_a_deny_and_is_reported_as_a_failure() {
    let (path, h) = host(true, "deny").await;
    let mut s = dial(&path).await;
    run_turn(&mut s, "own_tools").await;

    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "adjudicate");
    // The gate says no. The hook door turns that into the CLI's deny, and the CLI
    // reports the refusal as its result.
    write_frame(
        &mut s,
        &json!({
            "method": "verdict",
            "req_id": ask["req_id"].clone(),
            "call_id": ask["call_id"].clone(),
            "allow": false,
            "reason": "not on this machine",
        }),
    )
    .await
    .expect("a verdict");

    let message = read(&mut s).await;
    assert_eq!(message["kind"], "assistant_message");
    let usage = read(&mut s).await;
    assert_eq!(usage["method"], "usage");
    let reported = read(&mut s).await;
    assert_eq!(reported["kind"], "tool_result");
    assert_eq!(reported["payload"]["content"], "the gate said no");
    assert_eq!(reported["payload"]["is_error"], true);
    h.abort();
}


#[tokio::test]
async fn a_registry_turn_offers_the_sessions_tools_and_marks_the_result_it_did_not_author() {
    let (path, h) = host(true, "registry-run").await;
    let mut s = dial(&path).await;
    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "c", "version": "0",
            "backend": "claude-cli", "modes": ["registry"],
        }),
    )
    .await
    .expect("the confirmation");
    // A registry turn carries the tools the session is offering, in the shape
    // the MCP door can pass through unchanged.
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "registry",
            "model": "sonnet", "guidance": { "vault": null, "registry": null },
            "tools": [{ "name": "echo", "description": "say it back", "inputSchema": { "type": "object" } }],
            "prompt": "run it", "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": std::env::temp_dir().to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    // The vendor session mark, straight off `system/init`.
    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    assert_eq!(mark["opaque"], "fake-session-1");

    // The CLI's MCP client listed the tools and called one; the door asks the
    // session, which runs it and holds the result.
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "call");
    assert_eq!(ask["tool"], "echo");
    assert_eq!(ask["call_id"], "toolu_1");
    write_frame(
        &mut s,
        &json!({
            "method": "tool_result", "req_id": ask["req_id"].clone(),
            "call_id": ask["call_id"].clone(), "content": "echo: hi",
            "is_error": false, "images": [],
        }),
    )
    .await
    .expect("the session's answer");

    let message = read(&mut s).await;
    assert_eq!(message["kind"], "assistant_message");
    let usage = read(&mut s).await;
    assert_eq!(usage["method"], "usage");

    // The marker: the call id and nothing else.
    let marker = read(&mut s).await;
    assert_eq!(marker["kind"], "tool_result");
    assert_eq!(marker["payload"]["call_id"], "toolu_1");
    assert!(
        marker["payload"].get("content").is_none(),
        "a registry marker carries no text: the session journals its own result"
    );

    let context = read(&mut s).await;
    assert_eq!(context["kind"], "context_size");
    assert_eq!(context["payload"]["tokens"], 11);

    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["outcome"], "completed");
    h.abort();
}

#[tokio::test]
async fn a_cancel_signals_the_process_group_and_the_host_says_so() {
    // Named for the test: both cancel tests run in one process and would
    // otherwise share a pidfile and race.
    let tag = "cancel";
    let (path, h) = host(true, tag).await;
    let mut s = dial(&path).await;
    let pidfile = std::env::temp_dir().join(format!("eidolon-fake-cli-{tag}.pid"));
    let _ = std::fs::remove_file(&pidfile);

    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "c", "version": "0",
            "backend": "claude-cli", "modes": ["own_tools"],
        }),
    )
    .await
    .expect("the confirmation");
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "own_tools",
            "model": "sonnet", "guidance": { "vault": null, "registry": null }, "tools": null,
            "prompt": format!("SLEEP {}", pidfile.display()), "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": std::env::temp_dir().to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    // The CLI is up — the mark proves it ran — and has written us its pid.
    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    let mut cli_pid = None;
    let mut descendant = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Some(pid) = text.split_whitespace().next().and_then(|p| p.parse().ok())
        {
            cli_pid = Some(pid);
            descendant = text.split_whitespace().nth(1).and_then(|p| p.parse().ok());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cli_pid = cli_pid.expect("the fake CLI reported its pid");
    // It ignores SIGTERM, so stopping the leader is not enough: the group is what
    // has to go.
    assert_eq!(unsafe { libc::kill(cli_pid, 0) }, 0, "it is running to begin with");

    write_frame(
        &mut s,
        &json!({ "method": "cancel", "turn_id": "t1", "req_id": "c9", "deadline_ms": 5000 }),
    )
    .await
    .expect("a cancel");

    let ack = read(&mut s).await;
    assert_eq!(ack["method"], "cancel_ack");
    assert_eq!(ack["req_id"], "c9", "the session's id, echoed");
    assert_eq!(ack["killed"], true, "a real host signals the group it spawned");

    // The turn ends as a cancellation, not a failure: it ended *because* it was
    // cancelled, and saying otherwise would be a different claim.
    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["outcome"], "cancelled");

    // And the whole group really is gone: the leader *and* the descendant it
    // left behind. The descendant is the point — a leader-only kill would leave
    // it running with the CLI's authority.
    let descendant = descendant.expect("the fake CLI reported its descendant");
    for (who, pid) in [("the CLI", cli_pid), ("its descendant", descendant)] {
        let mut gone = false;
        for _ in 0..250 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "{who} was left running: the group was signalled, not just the leader");
    }
    let _ = std::fs::remove_file(&pidfile);
    h.abort();
}

#[tokio::test]
async fn only_the_descendant_ignoring_sigterm_still_leaves_the_group_gone() {
    let tag = "cancelterm";
    let (path, h) = host(true, tag).await;
    let mut s = dial(&path).await;
    let pidfile = std::env::temp_dir().join(format!("eidolon-fake-cli-{tag}.pid"));
    let _ = std::fs::remove_file(&pidfile);

    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "c", "version": "0",
            "backend": "claude-cli", "modes": ["own_tools"],
        }),
    )
    .await
    .expect("the confirmation");
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "own_tools",
            "model": "sonnet", "guidance": { "vault": null, "registry": null }, "tools": null,
            "prompt": format!("SLEEPTERM {}", pidfile.display()), "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": std::env::temp_dir().to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    // The CLI is up — the mark proves it ran — and has written us its pid.
    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    let mut cli_pid = None;
    let mut descendant = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Some(pid) = text.split_whitespace().next().and_then(|p| p.parse().ok())
        {
            cli_pid = Some(pid);
            descendant = text.split_whitespace().nth(1).and_then(|p| p.parse().ok());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cli_pid = cli_pid.expect("the fake CLI reported its pid");
    // It ignores SIGTERM, so stopping the leader is not enough: the group is what
    // has to go.
    assert_eq!(unsafe { libc::kill(cli_pid, 0) }, 0, "it is running to begin with");

    write_frame(
        &mut s,
        &json!({ "method": "cancel", "turn_id": "t1", "req_id": "c9", "deadline_ms": 5000 }),
    )
    .await
    .expect("a cancel");

    let ack = read(&mut s).await;
    assert_eq!(ack["method"], "cancel_ack");
    assert_eq!(ack["req_id"], "c9", "the session's id, echoed");
    assert_eq!(ack["killed"], true, "a real host signals the group it spawned");

    // The turn ends as a cancellation, not a failure: it ended *because* it was
    // cancelled, and saying otherwise would be a different claim.
    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["outcome"], "cancelled");

    // And the whole group really is gone: the leader *and* the descendant it
    // left behind. The descendant is the point — a leader-only kill would leave
    // it running with the CLI's authority.
    let descendant = descendant.expect("the fake CLI reported its descendant");
    for (who, pid) in [("the CLI", cli_pid), ("its descendant", descendant)] {
        let mut gone = false;
        for _ in 0..250 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "{who} was left running: the group was signalled, not just the leader");
    }
    let _ = std::fs::remove_file(&pidfile);
    h.abort();
}

#[tokio::test]
async fn a_cli_that_ends_without_a_result_fails_the_turn_rather_than_going_quiet() {
    let (path, h) = host(true, "noresult").await;
    let mut s = dial(&path).await;
    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "c", "version": "0",
            "backend": "claude-cli", "modes": ["own_tools"],
        }),
    )
    .await
    .expect("the confirmation");
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "own_tools",
            "model": "sonnet", "guidance": { "vault": null, "registry": null }, "tools": null,
            "prompt": "NORESULT", "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": std::env::temp_dir().to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");
    // The CLI exits through its own doors; there is no `result`, so the host says
    // the turn failed instead of leaving the session waiting for a settle.
    let settle = read(&mut s).await;
    assert_eq!(settle["method"], "settle");
    assert_eq!(settle["outcome"], "failed");
    assert!(
        settle["stop_reason"].as_str().unwrap_or("").contains("result"),
        "and says why: {}",
        settle["stop_reason"]
    );
    h.abort();
}

/// The shipped binary, not the library — the one thing every test above
/// structurally cannot see.
///
/// Each test above starts the host **in-process**, so the process's umask is
/// whatever `cargo test` had. The binary is the only place that calls
/// `umask(0o177)`, so that the listener socket is born 0600 instead of being
/// `bind`ed and then `chmod`ed. A umask masks a *directory's* mode the same
/// way, and `0700 & !0o177` is `0600` — read and write, no search bit. That is
/// not "stricter than 0700", it is unusable: nothing, not even the process that
/// made it, can create or stat a name inside, so the first `bind` of the turn's
/// hook socket failed `EACCES` and the whole turn came back as
/// `settle {outcome: "failed", stop_reason: "…Permission denied (os error 13)"}`.
///
/// It was found by pointing the real client at this host over a socket, not
/// here — which is why this test drives the shipped binary end to end, walks to
/// the settle, and asserts the scratch it created is one it can work in.
#[tokio::test]
async fn the_binary_can_work_in_the_scratch_it_creates() {
    let path = sock("bin-scratch");
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_nanos();
    let scratch = std::env::temp_dir().join(format!(
        "eidolon-claude-scratch-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);

    let mut child = std::process::Command::new(me())
        .arg("--socket")
        .arg(&path)
        .arg("--health-port")
        .arg("0")
        .arg("--backend")
        .arg("claude-cli")
        .arg("--real")
        .arg("--cli")
        .arg(fake_cli())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the host");

    let mut connected = None;
    for _ in 0..200 {
        match UnixStream::connect(&path).await {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut s = connected.expect("the host never accepted a connection");
    let hello = read(&mut s).await;
    assert_eq!(hello["method"], "hello");
    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "test-client",
            "version": "0", "backend": "claude-cli", "modes": ["own_tools"],
        }),
    )
    .await
    .expect("the confirmation");
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "own_tools",
            "model": "", "guidance": { "vault": null, "registry": null }, "tools": null,
            "prompt": "run the check", "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": scratch.to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    drive(&mut s, &scratch).await;

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Walk a turn to its settle, answering the gate the way a session does, and
/// assert the scratch is a directory its owner can actually work in.
async fn drive(s: &mut UnixStream, scratch: &std::path::Path) {
    // The turn must end of its own accord — a host that cannot reach its own
    // scratch never gets here.
    let mut settle = None;
    for _ in 0..64 {
        let f = read(s).await;
        match f["method"].as_str().unwrap_or("") {
            "adjudicate" => {
                write_frame(
                    s,
                    &json!({
                        "method": "verdict", "req_id": f["req_id"].clone(),
                        "call_id": f["call_id"].clone(), "allow": true,
                        "reason": "the operator's table says yes",
                    }),
                )
                .await
                .expect("a verdict");
            }
            "settle" => {
                settle = Some(f);
                break;
            }
            _ => {}
        }
    }
    let settle = settle.expect("the turn settled");
    assert_eq!(settle["outcome"], "completed", "settle: {settle}");
    assert_eq!(settle["stop_reason"], "end_turn");

    // And the scratch is a directory its owner can enter and create in: the
    // search bit is the whole of it.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(scratch)
        .expect("the scratch exists")
        .permissions()
        .mode()
        & 0o777;
    assert_ne!(
        mode & 0o100,
        0,
        "the scratch has no search bit: {mode:o} — nothing can be created inside it"
    );
    std::fs::write(scratch.join("probe"), b"x").expect("the scratch is writable");
}

/// **Every missing component, not just the last one.**
///
/// `create_dir_all` makes as many directories as it needs, and the umask falls
/// on every one of them. Under `umask(0o177)` an *intermediate* came out with no
/// search bit and the next component failed with the same `EACCES` — so a fix
/// that restored the leaf's bit after the fact would have been no fix at all
/// here. The scratch this hands over has two components that do not exist, which
/// is the case a repair cannot reach.
#[tokio::test]
async fn the_binary_can_make_a_scratch_several_components_deep() {
    let path = sock("bin-nested");
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("eidolon-claude-nested-{}-{n}", std::process::id()));
    let scratch = root.join("one").join("two");
    let _ = std::fs::remove_dir_all(&root);

    let mut child = std::process::Command::new(me())
        .arg("--socket")
        .arg(&path)
        .arg("--health-port")
        .arg("0")
        .arg("--backend")
        .arg("claude-cli")
        .arg("--real")
        .arg("--cli")
        .arg(fake_cli())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the host");

    let mut connected = None;
    for _ in 0..200 {
        match UnixStream::connect(&path).await {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut s = connected.expect("the host never accepted a connection");
    assert_eq!(read(&mut s).await["method"], "hello");
    write_frame(
        &mut s,
        &json!({
            "method": "hello", "protocol": PROTOCOL, "host": "test-client",
            "version": "0", "backend": "claude-cli", "modes": ["own_tools"],
        }),
    )
    .await
    .expect("the confirmation");
    write_frame(
        &mut s,
        &json!({
            "method": "run_turn", "req_id": "c1", "turn_id": "t1", "mode": "own_tools",
            "model": "", "guidance": { "vault": null, "registry": null }, "tools": null,
            "prompt": "run the check", "images": [], "resume": null,
            "cwd": env!("CARGO_MANIFEST_DIR"), "scratch": scratch.to_string_lossy(),
        }),
    )
    .await
    .expect("a turn");

    // Both components this host had to make are usable, not just the leaf.
    // Checked after the settle: the directories are made by the host, not by
    // this test, so before the turn runs they do not exist yet.
    drive(&mut s, &scratch).await;

    use std::os::unix::fs::PermissionsExt;
    for dir in [&root, &root.join("one"), &scratch] {
        let mode = std::fs::metadata(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .permissions()
            .mode()
            & 0o777;
        assert_ne!(
            mode & 0o100,
            0,
            "{} has no search bit: {mode:o}",
            dir.display()
        );
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}

/// **A hung gate is not a failure path, so it needs one.**
///
/// Every other way the hook door can go wrong is already a deny — a payload that
/// names no tool, a connection that dies, a session that hangs up. But a session
/// that simply never answers is not a failure at all: the door waits, the CLI's
/// `PreToolUse` command is blocked on it, and the turn stalls until the session's
/// own idle bound takes the whole turn down. The plugin's promise is that this
/// door denies rather than stalls, and inherited from the client is not the same
/// as held here.
///
/// So this drives a real turn, reads the `adjudicate`, and **never answers it.**
/// The deadline here is a quarter of a second; without the bound the door waits
/// forever, the fake CLI's hook never returns, no `tool_result` is ever emitted,
/// and this test fails on `read`'s own twenty-second deadline. The short deadline
/// is also why the assertion is on the *shape* of the settle rather than on the
/// sentence inside it: the sentence carries the deadline, and the deadline is the
/// thing the test chose.
#[tokio::test]
async fn a_gate_that_is_never_answered_is_denied_rather_than_stalled() {
    let path = sock("hook-deadline");
    let listener = bind_private(&path).expect("bind");
    let host = Arc::new(Host {
        real: Some(Adapter {
            cli: fake_cli(),
            me: me(),
            hook_deadline: Duration::from_millis(250),
        }),
        ..Host::fake()
    });
    let h = tokio::spawn(serve(listener, host));

    let mut s = dial(&path).await;
    run_turn(&mut s, "own_tools").await;

    let mark = read(&mut s).await;
    assert_eq!(mark["method"], "session");

    // The ask arrives, and is deliberately left unanswered.
    let ask = read(&mut s).await;
    assert_eq!(ask["method"], "adjudicate");
    assert_eq!(ask["tool"], "Bash");

    // The CLI emits the message carrying the call, then blocks on the hook, so
    // the result is a few frames further on. Walk to it rather than assuming a
    // position — the point of the test is that it *arrives at all*.
    let mut reported = None;
    for _ in 0..8 {
        let f = read(&mut s).await;
        if f["kind"] == "tool_result" {
            reported = Some(f);
            break;
        }
    }
    let reported = reported.expect("no tool_result arrived: the door stalled the hook");
    assert_eq!(
        reported["payload"]["is_error"], true,
        "an unanswered gate must deny the call: {reported}"
    );
    assert_eq!(
        reported["payload"]["content"], "the gate said no",
        "the CLI must have been told no, not left waiting: {reported}"
    );

    // `context_size` rides between the result and the settle; walk to it.
    let mut settle = None;
    for _ in 0..4 {
        let f = read(&mut s).await;
        if f["method"] == "settle" {
            settle = Some(f);
            break;
        }
    }
    let settle = settle.expect("no settle arrived");
    assert_eq!(settle["outcome"], "completed");

    h.abort();
    let _ = std::fs::remove_file(&path);
}

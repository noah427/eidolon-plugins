//! The host against a **fake editor**, over both of its real faces: the HTTP
//! endpoint the tools dial, and the TCP socket the addon dials.
//!
//! This is the half of the bridge that can be proved without a Godot window: the
//! handshake and its token, the protocol check, one editor at a time, the ids that
//! pair a request with its answer, and every way an editor can leave — closed,
//! silent, or never there at all. Each of those has to end in an answer a person
//! can act on, and not in a call that hangs until its timeout.
//!
//! What this does **not** prove, and must not be read as proving: that the shipped
//! addon works inside a real editor. That needs Godot, and it is
//! `tests/godot/README.md`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(15);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Tests run in parallel threads, so two of them can pick the same free port
/// between binding it and the host's own bind. Starting is therefore serialized
/// and then *identified*: the host's token is unique per test, and a `/call` that
/// comes back 200 proves the endpoint on that port is ours. Anything else (a
/// refused connection because the child lost the race and exited, or a 401 from
/// another test's host) means try another port.
static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Host {
    child: Child,
    tools_port: u16,
    bridge_port: u16,
    token: String,
}

impl Host {
    fn start() -> Host {
        Host::start_with(&[])
    }

    fn start_with(env: &[(&str, &str)]) -> Host {
        let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..12 {
            let tools_port = free_port();
            let bridge_port = free_port();
            if bridge_port == tools_port {
                continue;
            }
            let host = Host::start_on(tools_port, bridge_port, env);
            if host.is_ours() {
                return host;
            }
        }
        panic!("could not start an eidolon-godot that answered on a free port");
    }

    fn start_on(tools_port: u16, bridge_port: u16, env: &[(&str, &str)]) -> Host {
        let token = format!("test-token-{tools_port}-{bridge_port}");
        let mut command = Command::new(env!("CARGO_BIN_EXE_eidolon-godot"));
        command
            .env("EIDOLON_GODOT_PORT", tools_port.to_string())
            .env("EIDOLON_GODOT_BRIDGE_PORT", bridge_port.to_string())
            .env("EIDOLON_GODOT_TOKEN", &token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            command.env(k, v);
        }
        let child = command.spawn().expect("the host binary should start");
        let host = Host {
            child,
            tools_port,
            bridge_port,
            token,
        };
        host.wait_for_listening();
        host
    }

    fn wait_for_listening(&self) {
        for _ in 0..250 {
            if TcpStream::connect(("127.0.0.1", self.tools_port)).is_ok()
                && TcpStream::connect(("127.0.0.1", self.bridge_port)).is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "the host never listened on {} and {}",
            self.tools_port, self.bridge_port
        );
    }

    /// The token is ours and unique, so a 200 from `/call` is this host answering
    /// and nobody else's. A child that lost a port race exits instead, and then
    /// there is nothing to connect to — which is a `false`, not a panic.
    fn is_ours(&self) -> bool {
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", self.tools_port)) else {
            return false;
        };
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        let body = "{}";
        let req = format!(
            "POST /call HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
             Content-Length: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\r\n{body}",
            self.tools_port,
            body.len(),
            self.token
        );
        if s.write_all(req.as_bytes()).is_err() {
            return false;
        }
        let mut raw = String::new();
        if s.read_to_string(&mut raw).is_err() {
            return false;
        }
        raw.starts_with("HTTP/1.1 200")
    }

    fn health(&self) -> Value {
        let (status, body) = self.http("GET", "/health", None, None);
        assert_eq!(status, 200, "health: {body}");
        serde_json::from_str(&body).expect("health answers JSON")
    }

    /// A tool's call, exactly as a `godot_*.rn` makes it: the token, then
    /// `{method, args}`.
    fn call(&self, method: &str, args: Value) -> Value {
        let body = json!({ "method": method, "args": args }).to_string();
        let (status, body) = self.http("POST", "/call", Some(&self.token), Some(&body));
        assert_eq!(status, 200, "{method}: {body}");
        serde_json::from_str(&body).expect("a call answers JSON")
    }

    /// A tool's call, made from another thread while this one drives the editor —
    /// the shape a real call has, since it blocks until the editor answers.
    fn call_in_background(&self, method: &str, args: Value) -> std::thread::JoinHandle<Value> {
        let tools_port = self.tools_port;
        let token = self.token.clone();
        let method = method.to_string();
        std::thread::spawn(move || {
            let body = json!({ "method": method, "args": args }).to_string();
            let (status, body) = http(tools_port, "POST", "/call", Some(&token), Some(&body));
            assert_eq!(status, 200, "{body}");
            serde_json::from_str::<Value>(&body).expect("a call answers JSON")
        })
    }

    fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        http(self.tools_port, method, path, token, body)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // A leaked host is a port and a process kept for the rest of the run, and
        // the next test's start would find the port taken. Reaped, not orphaned.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(TIMEOUT)).unwrap();
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("an HTTP status line")
        .parse()
        .expect("a numeric status");
    (status, body.to_string())
}

/// The other end of the bridge: what the addon does, in Rust. It speaks the wire
/// for real — a newline-delimited JSON line in each direction — because the wire
/// is what is being tested.
struct Editor {
    writer: TcpStream,
    inbox: Receiver<Value>,
}

impl Editor {
    /// Dial the editor's port and say hello, as `bridge.gd` does the moment its
    /// socket connects. Nothing here waits for the answer: the test decides whether
    /// it wants the `welcome` or the `bye`.
    fn dial(host: &Host, token: &str) -> Editor {
        Editor::dial_saying(
            host,
            json!({
                "protocol": 1,
                "token": token,
                "engine": "4.7.2-stable (test)",
                "project": "/home/nobody/projects/demo",
            }),
        )
    }

    fn dial_saying(host: &Host, hello: Value) -> Editor {
        let stream = TcpStream::connect(("127.0.0.1", host.bridge_port))
            .expect("the host listens for an editor");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let writer = stream.try_clone().unwrap();
        let (tx, inbox) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let msg = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut editor = Editor { writer, inbox };
        editor.send(json!({ "hello": hello }));
        editor
    }

    fn send(&mut self, v: Value) {
        writeln!(self.writer, "{v}").unwrap();
        self.writer.flush().unwrap();
    }

    fn next(&self) -> Value {
        self.inbox.recv_timeout(TIMEOUT).expect("something from the bridge")
    }

    /// Whether the bridge dropped this socket. End of stream, not a timeout: the
    /// two are different facts and a test has to be able to tell them apart.
    fn closed(&self, wait: Duration) -> bool {
        matches!(self.inbox.recv_timeout(wait), Err(RecvTimeoutError::Disconnected))
    }

    /// The next thing the bridge asks this editor to do.
    fn request(&self) -> (u64, String, Value) {
        let msg = self.next();
        let id = msg["id"]
            .as_u64()
            .unwrap_or_else(|| panic!("expected a request with an id, got {msg}"));
        (
            id,
            msg["method"].as_str().expect("a method").to_string(),
            msg["args"].clone(),
        )
    }

    fn welcome(&self) {
        let msg = self.next();
        assert!(msg.get("welcome").is_some(), "expected a welcome, got {msg}");
    }

    fn bye(&self) -> String {
        let msg = self.next();
        msg.get("bye")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("expected a bye, got {msg}"))
            .to_string()
    }

    fn answer(&mut self, id: u64, result: Value) {
        self.send(json!({ "id": id, "ok": true, "result": result }));
    }

    /// Close the connection the way a killed editor does: nothing said, the socket
    /// gone. `shutdown` and not a dropped struct, because the reader thread in this
    /// test holds a duplicate of the file descriptor — only the socket itself is
    /// shared, so only shutting the socket down reaches the far end.
    fn hang_up(&self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }

    fn tick(&mut self, n: u64) {
        self.send(json!({ "tick": n }));
    }
}

#[test]
fn health_answers_and_the_call_endpoint_wants_the_token() {
    let host = Host::start();

    let health = host.health();
    assert_eq!(health["status"], json!("ok"));
    assert_eq!(health["attached"], json!(false), "nothing has dialled in yet");

    for (why, token) in [("no token", None), ("the wrong token", Some("not-the-token"))] {
        let (status, body) = host.http("POST", "/call", token, Some("{}"));
        assert_eq!(status, 401, "{why}: {body}");
        assert!(
            body.contains("bad token"),
            "{why} must be told apart from a refusal by the far end: {body}"
        );
    }

    let (status, body) = host.http("POST", "/call", Some(&host.token), Some("not json"));
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("invalid JSON body"), "{body}");
}

#[test]
fn status_answers_with_no_editor_and_says_what_would_make_one_appear() {
    let host = Host::start();
    let reply = host.call("status", json!({}));
    assert_eq!(reply["ok"], json!(true), "{reply}");
    let status = &reply["result"];
    assert_eq!(status["attached"], json!(false));
    assert_eq!(status["pending"], json!(0));
    let reason = status["reason"].as_str().expect("a reason when nothing is attached");
    assert!(reason.contains("no Godot editor is attached"), "{reason}");
    assert!(reason.contains("Plugins"), "the reason names what to enable: {reason}");
    assert!(reason.contains("README"), "and where to read the rest: {reason}");
}

#[test]
fn a_method_nothing_forwards_is_refused_by_name() {
    let host = Host::start();
    let reply = host.call("launch_missiles", json!({}));
    assert_eq!(reply["ok"], json!(false), "{reply}");
    let error = reply["error"].as_str().unwrap();
    assert!(error.contains("unknown method"), "{error}");
    assert!(error.contains("launch_missiles"), "{error}");

    let reply = host.call("tree", json!("not an object"));
    assert_eq!(reply["ok"], json!(false), "{reply}");
    assert_eq!(
        reply["error"],
        json!("args must be an object"),
        "args are the addon's, but their shape is this host's to hold"
    );
}

#[test]
fn every_verb_before_an_editor_dials_in_answers_the_same_way() {
    // Not one error per verb: one sentence a session can act on, from whichever of
    // the six it reached for. `status` is the exception, and says the same thing as
    // data rather than as a refusal.
    let host = Host::start();
    for method in ["tree", "node", "set", "call", "scene", "run"] {
        let reply = host.call(method, json!({}));
        assert_eq!(reply["ok"], json!(false), "{method}: {reply}");
        let error = reply["error"].as_str().unwrap();
        assert!(
            error.contains("no Godot editor is attached"),
            "{method} must say why: {error}"
        );
        assert!(
            error.contains("Project > Project Settings > Plugins"),
            "{method} must say what to do: {error}"
        );
    }
}

#[test]
fn an_editor_attaches_and_its_answers_come_back_under_the_hosts_own_ids() {
    let host = Host::start();
    let mut editor = Editor::dial(&host, &host.token);
    editor.welcome();

    let health = host.health();
    assert_eq!(health["attached"], json!(true));
    assert_eq!(health["engine"], json!("4.7.2-stable (test)"));
    assert_eq!(health["project"], json!("/home/nobody/projects/demo"));

    // A call is answered by asking the editor, and the answer that comes back is
    // the editor's, unwrapped.
    let called = host.call_in_background("tree", json!({ "path": ".", "depth": 2 }));
    let (id, method, args) = editor.request();
    assert_eq!(method, "tree");
    assert_eq!(args["path"], json!("."), "the caller's args are passed through whole");
    assert_eq!(args["depth"], json!(2));
    editor.answer(id, json!({ "nodes": [{ "path": ".", "class": "Node2D" }], "count": 1 }));

    let reply = called.join().unwrap();
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["result"]["count"], json!(1));
    assert_eq!(reply["result"]["nodes"][0]["class"], json!("Node2D"));

    // And the ids are the host's: nothing a tool sends carries one, and the next
    // call gets another.
    let called = host.call_in_background("status", json!({}));
    let (next_id, method, _args) = editor.request();
    assert_eq!(method, "status");
    assert!(next_id > id, "ids are handed out, not reused: {next_id} after {id}");
    editor.answer(next_id, json!({ "playing": false }));
    assert_eq!(called.join().unwrap()["result"]["editor_state"]["playing"], json!(false));
}

#[test]
fn a_second_editor_is_refused_while_one_is_attached() {
    let host = Host::start();
    let mut first = Editor::dial(&host, &host.token);
    first.welcome();
    first.tick(1);

    let second = Editor::dial(&host, &host.token);
    let bye = second.bye();
    assert!(bye.contains("another Godot editor is already attached"), "{bye}");
    assert!(bye.contains("serves one editor"), "and why: {bye}");

    // The refusal did not disturb the attachment that won.
    assert_eq!(host.health()["attached"], json!(true));
    let called = host.call_in_background("status", json!({}));
    let (id, method, _args) = first.request();
    assert_eq!(method, "status");
    first.answer(id, json!({ "playing": false, "edited_scene": "res://main.tscn" }));
    let reply = called.join().unwrap();
    assert_eq!(reply["result"]["editor_state"]["edited_scene"], json!("res://main.tscn"));
}

#[test]
fn a_wrong_token_is_refused_and_nothing_attaches() {
    let host = Host::start();
    let impostor = Editor::dial(&host, "not-the-token");
    let bye = impostor.bye();
    assert!(bye.contains("does not match this host's token file"), "{bye}");
    assert!(impostor.closed(TIMEOUT), "and the socket is dropped, not left open");

    assert_eq!(host.health()["attached"], json!(false));
    let reply = host.call("tree", json!({}));
    assert_eq!(reply["ok"], json!(false), "the token bought nothing: {reply}");
}

#[test]
fn an_addon_from_another_protocol_is_refused_by_name() {
    let host = Host::start();
    let future = Editor::dial_saying(
        &host,
        json!({
            "protocol": 99,
            "token": host.token,
            "engine": "9.9.9",
            "project": "/tmp/x",
        }),
    );
    let bye = future.bye();
    assert!(bye.contains("speaks protocol 1"), "{bye}");
    assert!(bye.contains("update the addon"), "{bye}");
    assert_eq!(host.health()["attached"], json!(false));
}

#[test]
fn a_call_nobody_answers_times_out_naming_the_method_and_leaves_nothing_stuck() {
    let host = Host::start_with(&[("EIDOLON_GODOT_CALL_TIMEOUT_S", "1")]);
    let mut editor = Editor::dial(&host, &host.token);
    editor.welcome();

    let called = host.call_in_background("tree", json!({}));
    let (_id, method, _args) = editor.request();
    assert_eq!(method, "tree");
    // Said nothing at all, which is the case the timeout exists for.
    let reply = called.join().unwrap();
    assert_eq!(reply["ok"], json!(false), "{reply}");
    let error = reply["error"].as_str().unwrap();
    assert!(error.contains("did not answer `tree` within 1s"), "{error}");
    assert!(
        error.contains("not retried on its own"),
        "and is not silently retried: {error}"
    );

    // Nothing is left waiting: the abandoned id is gone, and the next call gets a
    // clean answer.
    assert_eq!(host.health()["pending"], json!(0));
    let called = host.call_in_background("node", json!({ "path": "Player" }));
    let (id, method, _args) = editor.request();
    assert_eq!(method, "node");
    editor.answer(id, json!({ "class": "CharacterBody2D", "fingerprint": "1|x|res://player.gd" }));
    let reply = called.join().unwrap();
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["result"]["class"], json!("CharacterBody2D"));
}

#[test]
fn the_editor_going_away_fails_the_call_in_flight_now_not_at_the_timeout() {
    let host = Host::start_with(&[("EIDOLON_GODOT_CALL_TIMEOUT_S", "60")]);
    let editor = Editor::dial(&host, &host.token);
    editor.welcome();

    let started = Instant::now();
    let called = host.call_in_background("tree", json!({}));
    let (_id, method, _args) = editor.request();
    assert_eq!(method, "tree");

    // The editor is closed the way a person closes one: the socket goes, with no
    // farewell.
    editor.hang_up();

    let reply = called.join().unwrap();
    assert_eq!(reply["ok"], json!(false), "{reply}");
    let error = reply["error"].as_str().unwrap();
    assert!(error.contains("connection closed before it answered"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a call on a dead attachment must not wait out its timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(host.health()["attached"], json!(false));
}

#[test]
fn an_editor_that_goes_silent_is_called_gone() {
    // Killed with -9, or a machine that sleeps: no close, no FIN, just nothing.
    let host = Host::start_with(&[("EIDOLON_GODOT_STALE_S", "1")]);
    let editor = Editor::dial(&host, &host.token);
    editor.welcome();
    assert_eq!(host.health()["attached"], json!(true));

    // The socket stays open and says nothing. The attachment has to expire on its
    // own, or a verb would hang on an editor that is not there.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if host.health()["attached"] == json!(false) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a silent editor was still reported as attached after 10s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let reply = host.call("tree", json!({}));
    assert_eq!(reply["ok"], json!(false), "{reply}");
    assert!(
        reply["error"].as_str().unwrap().contains("no Godot editor is attached"),
        "{reply}"
    );
    assert!(
        editor.closed(TIMEOUT),
        "and the socket is dropped, so the addon can dial in again"
    );
}

#[test]
fn a_line_the_bridge_cannot_read_is_ignored_rather_than_ending_the_attachment() {
    let host = Host::start();
    let mut editor = Editor::dial(&host, &host.token);
    editor.welcome();

    editor.writer.write_all(b"this is not json at all\n").unwrap();
    editor.writer.flush().unwrap();
    editor.tick(1);

    // The editor is still the editor: a bad line is a fact about one message, not
    // about the socket.
    assert_eq!(host.health()["attached"], json!(true));
    let called = host.call_in_background("status", json!({}));
    let (id, _method, _args) = editor.request();
    editor.answer(id, json!({ "playing": false }));
    assert_eq!(called.join().unwrap()["ok"], json!(true));
}

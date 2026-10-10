//! The host against a **fake extension**, over its real stdin/stdout and its
//! real HTTP endpoint.
//!
//! This is the half of the bridge that can be proved without a browser: the
//! native messaging handshake and its add-on check, the length-prefixed wire in
//! both directions, the loopback endpoint's token, and the rule that an answer
//! is returned only when the attachment *and the document* it was issued
//! against are still the current ones. A same-URL reload and a revocation that
//! races a read are both scripted here.
//!
//! What this does **not** prove, and must not be read as proving: that the
//! shipped extension (toolbar click, `activeTab`, `tabs.executeScript`) works
//! in LibreWolf. That needs a real browser and a real click; see
//! `tests/librewolf/README.md`.
//!
//! The frame codec below is written out again rather than imported — an
//! integration test of a binary crate cannot see its modules, and a second
//! implementation is the point when what is being tested is a wire format.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const ADDON: &str = "librewolf-bridge@eidolon.local";
const TIMEOUT: Duration = Duration::from_secs(15);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn read_frame<R: Read>(r: &mut R) -> Option<Value> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header).ok()?;
    let len = u32::from_ne_bytes(header) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_frame<W: Write>(w: &mut W, v: &Value) {
    let body = serde_json::to_vec(v).unwrap();
    w.write_all(&(body.len() as u32).to_ne_bytes()).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
}

struct Host {
    child: Child,
    port: u16,
    token: String,
    stdin: Option<ChildStdin>,
    inbox: Receiver<Value>,
}

/// Tests run in parallel threads, so two of them can pick the same free port
/// between binding it and the host's own bind. Starting is therefore serialized
/// and then *identified*: the host's token is unique per test, and a `/call`
/// that comes back 200 proves the endpoint on that port is ours. Anything else
/// (a refused connection because the child lost the race and exited, or a 401
/// from another test's host) means try another port.
static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Host {
    fn start() -> Host {
        let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..12 {
            let host = Host::start_on(free_port());
            if host.is_ours() {
                return host;
            }
        }
        panic!("could not start a host on a free port");
    }

    /// Our own token is accepted, so the endpoint on this port is our child's.
    fn is_ours(&self) -> bool {
        TcpStream::connect(("127.0.0.1", self.port)).is_ok()
            && request(self.port, "POST", "/call", Some(&self.token), Some("{}")).0 == 200
    }

    fn start_on(port: u16) -> Host {
        let token = format!("test-token-{port}");
        let mut child = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
            .env("EIDOLON_LIBREWOLF_PORT", port.to_string())
            .env("EIDOLON_LIBREWOLF_TOKEN", &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the host binary should start");
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, inbox) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Some(v) = read_frame(&mut r) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let host = Host {
            child,
            port,
            token,
            stdin: Some(stdin),
            inbox,
        };
        host.wait_until_listening();
        host
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the host never listened on 127.0.0.1:{}", self.port);
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("the native port is still open");
        write_frame(stdin, &v);
    }

    fn hello(&mut self) {
        self.send(json!({"v": 2, "type": "hello", "addon": ADDON, "ext_version": "0.2.0"}));
    }

    /// The whole enabled set, the way the extension pushes it: a snapshot, not a
    /// diff. `revoked` is the reason for the last change when there was one.
    fn state(&mut self, tabs: Value, revoked: Value) {
        self.send(json!({"v": 2, "type": "state", "tabs": tabs, "revoked": revoked}));
    }

    /// One enabled tab, by id, at a generation, with the document it is showing.
    fn enabled_tab(&mut self, tab_id: i64, generation: u64, url: &str, time_origin: f64) {
        self.state(
            json!([{
                "tab_id": tab_id, "url": url, "title": "A page",
                "origin": origin_of(url), "generation": generation,
                "time_origin": time_origin, "nonce": format!("nonce-of-generation-{generation}"),
                "confirmed": true, "enabled_at_ms": 1,
            }]),
            Value::Null,
        );
    }

    /// Tab 7 enabled, which is what most of these tests work against.
    fn enabled(&mut self, generation: u64, url: &str, time_origin: f64) {
        self.enabled_tab(7, generation, url, time_origin);
    }

    /// The same tab, offering a different document at the same generation.
    fn enabled_in_another_document(&mut self, generation: u64, url: &str, time_origin: f64) {
        self.state(
            json!([{
                "tab_id": 7, "url": url, "title": "A page", "origin": origin_of(url),
                "generation": generation, "time_origin": time_origin,
                "nonce": "a-nonce-from-somewhere-else", "confirmed": true, "enabled_at_ms": 1,
            }]),
            Value::Null,
        );
    }

    /// The same document, withdrawn: the tab is loading something and has no
    /// usable document until a probe identifies it.
    fn loading(&mut self, generation: u64, url: &str, time_origin: f64) {
        self.state(
            json!([{
                "tab_id": 7, "url": url, "title": "A page", "origin": origin_of(url),
                "generation": generation, "time_origin": time_origin,
                "nonce": format!("nonce-of-generation-{}", generation - 1),
                "confirmed": false, "enabled_at_ms": 1,
            }]),
            Value::Null,
        );
    }

    /// Nothing enabled any more, with the reason.
    fn disabled(&mut self, reason: &str, url: &str) {
        self.state(
            json!([]),
            json!({"tab_id": 7, "reason": reason, "url": url}),
        );
    }

    /// The next frame the host sent that is a command for the extension.
    fn command(&self) -> Value {
        loop {
            let v = self
                .inbox
                .recv_timeout(TIMEOUT)
                .expect("the host sent nothing while a command was expected");
            if v.get("type").and_then(Value::as_str) == Some("command") {
                return v;
            }
        }
    }

    fn health(&self) -> Value {
        let (status, body) = request(self.port, "GET", "/health", None, None);
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    fn call(&self, method: &str, args: Value) -> Value {
        let (status, body) = request(
            self.port,
            "POST",
            "/call",
            Some(&self.token),
            Some(&json!({"method": method, "args": args}).to_string()),
        );
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Wait until the host has recorded an enabled tab, so a test does not race
    /// the `state` frame it just wrote.
    fn wait_enabled(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.health()["enabled_tabs"] == json!(1) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host never recorded the enabled tab");
    }
}

/// The origin of a url the way the real extension reads it — scheme://authority.
/// Only ever used on the plain absolute urls these tests write.
fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    format!("{}://{}", scheme.to_lowercase(), rest[..end].to_lowercase())
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_listening(port: u16) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
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

#[test]
fn health_answers_and_the_call_endpoint_wants_the_token() {
    let host = Host::start();
    assert_eq!(host.health()["status"], json!("ok"));
    assert_eq!(host.health()["browser_connected"], json!(false));

    let (status, _) = request(host.port, "POST", "/call", None, Some("{}"));
    assert_eq!(status, 401);
    let (status, body) = request(
        host.port,
        "POST",
        "/call",
        Some("not-the-token"),
        Some("{}"),
    );
    assert_eq!(status, 401);
    assert!(body.contains("bad token"), "{body}");
}

#[test]
fn a_wrong_add_on_id_is_refused_so_the_endpoint_stays_dark() {
    let mut host = Host::start();
    host.send(json!({"v": 2, "type": "hello", "addon": "somebody-else@example.com"}));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(host.health()["browser_connected"], json!(false));

    // The status payload is a report, not a refusal: nothing enabled, and the
    // hint naming the one act that would connect a browser to this host.
    let reply = host.call("status", json!({}));
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["result"]["tabs"].as_array().unwrap().len(), 0);
    assert!(
        reply["result"]["hint"]
            .as_str()
            .unwrap()
            .contains("has not connected"),
        "{reply}"
    );
}

#[test]
fn before_any_enablement_a_read_names_the_click_and_returns_no_text() {
    let mut host = Host::start();
    host.hello();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(host.health()["browser_connected"], json!(true));
    assert_eq!(host.health()["enabled_tabs"], json!(0));

    let reply = host.call("read", json!({"tab_id": 7}));
    assert_eq!(reply["ok"], json!(false));
    let err = reply["error"].as_str().unwrap();
    assert!(
        err.contains("click the librewolf bridge extension's toolbar button"),
        "{err}"
    );
    // A request that names no tab at all says what to pass instead.
    let reply = host.call("read", json!({}));
    assert!(reply["error"].as_str().unwrap().contains("which tab?"), "{reply}");
    // Nothing was asked of the extension: an unattached call is refused here.
    assert!(
        host.inbox.try_recv().is_err(),
        "no command should have been sent"
    );
}

#[test]
fn a_read_is_answered_and_the_host_truncates_it_itself() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let status = host.call("status", json!({}));
    assert_eq!(status["ok"], json!(true));
    let tab = &status["result"]["tabs"][0];
    assert_eq!(tab["tab_id"], json!(7));
    assert_eq!(tab["url"], json!("https://example.com/a"));
    assert_eq!(tab["origin"], json!("https://example.com"));
    assert_eq!(tab["generation"], json!(1));
    assert_eq!(tab["confirmed"], json!(true));

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7, "max": 20}}).to_string()),
        )
    });
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("read"));
    assert_eq!(cmd["generation"], json!(1));
    assert_eq!(cmd["time_origin"], json!(100.0));
    assert_eq!(cmd["args"]["max"], json!(20));
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0, "nonce": "nonce-of-generation-1",
                     "result": {"url": "https://example.com/a", "title": "A page",
                                "text": "x".repeat(50), "truncated": false}}));
    let (status, body) = caller.join().unwrap();
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["result"]["text"].as_str().unwrap().chars().count(), 20);
    assert_eq!(v["result"]["truncated"], json!(true));
    assert_eq!(v["result"]["untrusted"], json!(true));
}

#[test]
fn a_same_url_reload_is_refused_even_though_no_revocation_event_arrived() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/same", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    let id = cmd["id"].as_u64().unwrap();
    // The tab reloaded at the same URL: the extension's own check caught it and
    // it answers with the *new* document's time origin, having revoked nothing
    // this host has heard about yet.
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 1, "time_origin": 999.0,
                     "result": {"url": "https://example.com/same", "title": "A page",
                                "text": "the new document's text", "truncated": false}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("different document"), "{err}");
    assert!(!body.contains("the new document's text"), "{body}");
}

#[test]
fn a_revocation_that_races_the_read_wins_and_the_text_is_dropped() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    let id = cmd["id"].as_u64().unwrap();
    // The tab navigates while the read is in flight. The extension pushes the
    // tab out of the enabled set first, then answers about the document it read.
    host.disabled("a new document began loading", "https://example.com/b");
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0, "nonce": "nonce-of-generation-1",
                     "result": {"url": "https://example.com/a", "title": "A page",
                                "text": "text from the document that is gone", "truncated": false}}));
    let (status, body) = caller.join().unwrap();
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"].as_str().unwrap().contains("not enabled"),
        "{v}"
    );
    assert!(!body.contains("that is gone"), "{body}");

    // Nothing but the extension pushing a record can enable a tab again — this
    // host has no path that invents one — and the reason it ended stays until
    // that push arrives.
    assert_eq!(host.health()["enabled_tabs"], json!(0));
    let status = host.call("status", json!({}));
    assert_eq!(status["ok"], json!(true));
    assert_eq!(status["result"]["tabs"].as_array().unwrap().len(), 0);
    assert!(
        status["result"]["revocation"]["reason"]
            .as_str()
            .unwrap()
            .contains("a new document began loading"),
        "{status}"
    );
}

#[test]
fn a_reply_about_another_generation_is_refused() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 99, "time_origin": 100.0, "nonce": "nonce-of-generation-1",
                     "result": {"url": "https://example.com/a", "title": "A page",
                                "text": "text", "truncated": false}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("answered about generation 99"),
        "{v}"
    );
}

#[test]
fn a_reply_from_another_document_is_refused_even_with_the_same_url_and_clock() {
    // The case a timestamp cannot see: same URL, same time origin, a different document.
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/same", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    assert_eq!(cmd["nonce"], json!("nonce-of-generation-1"));
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0,
                     "nonce": "a-nonce-from-the-replacement-document",
                     "result": {"url": "https://example.com/same", "title": "A page",
                                "text": "text from another document", "truncated": false}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"].as_str().unwrap().contains("different document"),
        "{v}"
    );
    assert!(!body.contains("text from another document"), "{body}");
}

#[test]
fn an_enablement_claiming_another_document_does_not_lower_the_record() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    // Same generation, same URL, same clock, a different document. The record
    // already held stands — a push cannot lower it — and the tab keeps the
    // document this host verified rather than the one it was offered.
    host.enabled_in_another_document(1, "https://example.com/a", 100.0);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(
        host.health()["enabled_tabs"],
        json!(1),
        "the held record stands"
    );

    let reply = host.call("status", json!({}));
    assert_eq!(reply["ok"], json!(true));
    let tab = &reply["result"]["tabs"][0];
    assert_eq!(tab["nonce"], json!("nonce-of-generation-1"));
    assert_eq!(tab["generation"], json!(1));
}

#[test]
fn an_extension_refusal_reaches_the_caller_with_its_reason() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "structure", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("structure"));
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": false, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0, "nonce": "nonce-of-generation-1",
                     "error": "this page cannot be read (restricted or privileged page)"}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("restricted or privileged"),
        "{v}"
    );
}

#[test]
fn detach_clears_the_record_even_when_the_extension_refuses() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "detach", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("detach"));
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": false, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0, "nonce": "nonce-of-generation-1", "error": "cannot reach the tab"}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"].as_str().unwrap().contains("dropped here anyway"),
        "{v}"
    );
    assert_eq!(host.health()["enabled_tabs"], json!(0));

    let after = host.call("read", json!({"tab_id": 7}));
    assert_eq!(after["ok"], json!(false));
    assert!(
        after["error"]
            .as_str()
            .unwrap()
            .contains("the assistant disabled the tab"),
        "{after}"
    );
}

#[test]
fn closing_the_native_port_clears_the_attachment_and_fails_the_waiting_read() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let _cmd = host.command();

    // The browser goes away with the read outstanding: drop our end of the
    // native port, which is the host's only end-of-stream signal.
    drop(host.stdin.take());

    let (status, body) = caller.join().unwrap();
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["ok"],
        json!(false),
        "a read that can no longer be answered must not be answered"
    );
    let err = v["error"].as_str().unwrap();
    assert!(
        err.contains("dropped this read before it was answered"),
        "{err}"
    );
    assert!(
        err.contains("browser closed") || err.contains("host restarted"),
        "{err}"
    );
}

#[test]
fn a_second_host_on_the_same_port_fails_instead_of_serving_the_wrong_browser() {
    let first = Host::start();
    let out = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
        .env("EIDOLON_LIBREWOLF_PORT", first.port.to_string())
        .env("EIDOLON_LIBREWOLF_TOKEN", "another-profile")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "the second host must not keep running"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("could not listen on 127.0.0.1"), "{err}");
    assert!(err.contains("will not attach to it"), "{err}");

    // The first host still answers with *its* token, and the second never did.
    assert_eq!(first.health()["status"], json!("ok"));
    let (status, _) = request(
        first.port,
        "POST",
        "/call",
        Some("another-profile"),
        Some("{}"),
    );
    assert_eq!(status, 401);
}

fn host_command(port: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"));
    c.env("EIDOLON_LIBREWOLF_PORT", port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c
}

/// `(exited, ok, stderr)`: a run that had to be killed is *not* a refusal, and a test
/// that only asked "was it unsuccessful?" would call a hang a refusal.
fn run_to_exit(mut c: Command) -> (bool, bool, String) {
    let mut child = c.spawn().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut exited = None;
    while Instant::now() < deadline {
        if let Ok(Some(st)) = child.try_wait() {
            exited = Some(st);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let exited_in_time = exited.is_some();
    let ok = exited.map(|st| st.success()).unwrap_or(false);
    let _ = child.kill();
    let _ = child.wait();
    let mut err = String::new();
    if let Some(stderr) = child.stderr.as_mut() {
        let _ = stderr.read_to_string(&mut err);
    }
    (exited_in_time, ok, err)
}

// --- the launch arguments, as positions -------------------------------------

#[test]
fn the_launch_arguments_are_read_as_positions_not_searched_for_a_shape() {
    let manifest = "/home/somebody/.librewolf/native-messaging-hosts/eidolon_librewolf.json";

    // One argument is not a shape Firefox uses, whether or not it looks like an id.
    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.arg("somebody-else@example.com");
        c
    });
    assert!(!ok, "one argument is refused: {err}");
    assert!(err.contains("argument(s)"), "{err}");

    // The right shape with a wrong id at the id's position.
    let port = free_port();
    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(port);
        c.arg(manifest).arg("somebody-else@example.com");
        c
    });
    assert!(!ok);
    assert!(err.contains("serves only"), "{err}");
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());

    // An id without an `@` is refused too: an add-on id need not contain one, so
    // looking for the character is not the check.
    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.arg(manifest).arg("not-an-at-sign-but-wrong");
        c
    });
    assert!(!ok, "{err}");
    assert!(err.contains("serves only"), "{err}");

    // And the right shape with a first argument that is not a manifest path.
    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.arg("/tmp/not-a-manifest.txt").arg(ADDON);
        c
    });
    assert!(!ok);
    assert!(err.contains("host manifest"), "{err}");
}

#[test]
fn the_right_launch_arguments_open_the_endpoint_and_a_hello_is_still_required() {
    let port = free_port();
    let token = format!("test-token-{port}");
    let manifest = "/home/somebody/.librewolf/native-messaging-hosts/eidolon_librewolf.json";
    let mut child = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
        .arg(manifest)
        .arg(ADDON)
        .env("EIDOLON_LIBREWOLF_PORT", port.to_string())
        .env("EIDOLON_LIBREWOLF_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap();
    assert!(
        wait_listening(port),
        "the endpoint should open for a well-formed launch"
    );

    // Right arguments are not the same as a right peer: the endpoint is up, and
    // nothing is served until a `hello` names this add-on.
    let body = json!({"method": "status", "args": {}}).to_string();
    let reply: Value =
        serde_json::from_str(&request(port, "POST", "/call", Some(&token), Some(&body)).1).unwrap();
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["result"]["tabs"].as_array().unwrap().len(), 0);
    assert!(
        reply["result"]["hint"]
            .as_str()
            .unwrap()
            .contains("has not connected"),
        "{reply}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

// --- the token file ---------------------------------------------------------

#[test]
fn a_token_file_others_can_read_is_refused_and_left_alone() {
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("librewolf.token");
    std::fs::write(&path, "a-token-someone-else-could-read").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.env("EIDOLON_LIBREWOLF_TOKEN_FILE", &path);
        c
    });
    assert!(!ok, "a world-readable token must not be used: {err}");
    assert!(err.contains("group or other can read"), "{err}");
    assert!(err.contains("chmod 600"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a-token-someone-else-could-read",
        "a refusal must not rewrite the file it refused"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_fifo_at_the_token_path_is_refused_within_the_time_it_took_to_start() {
    // Opening a fifo read-only blocks until a writer appears. If the open is not
    // O_NONBLOCK, this host hangs *before* it can fstat the path and refuse — and a
    // test that only checked "unsuccessful" would call that hang a refusal. So this
    // one insists the process *exited*, on its own, inside the timeout.
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("librewolf.token");
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipping: no mkfifo here");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let (exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.env("EIDOLON_LIBREWOLF_TOKEN_FILE", &path);
        c
    });
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        exited,
        "a fifo at the token path must be refused, not waited on: {err}"
    );
    assert!(!ok);
    assert!(err.contains("not a regular file"), "{err}");
}

#[test]
fn a_token_file_that_is_a_symlink_is_refused() {
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.token");
    std::fs::write(&real, "a-real-token").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("librewolf.token");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.env("EIDOLON_LIBREWOLF_TOKEN_FILE", &link);
        c
    });
    assert!(!ok, "a symlinked token must not be followed: {err}");
    assert!(
        err.contains("cannot be opened") || err.to_lowercase().contains("symbolic link"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_token_file_is_refused_rather_than_overwritten() {
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("librewolf.token");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (_exited, ok, err) = run_to_exit({
        let mut c = host_command(free_port());
        c.env("EIDOLON_LIBREWOLF_TOKEN_FILE", &path);
        c
    });
    assert!(!ok, "{err}");
    assert!(err.contains("is empty"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "",
        "it must be left as it was"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_existing_private_token_is_used_and_never_rewritten() {
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("librewolf.token");
    std::fs::write(&path, "token-made-earlier\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    let port = free_port();
    let mut child = host_command(port)
        .env("EIDOLON_LIBREWOLF_TOKEN_FILE", &path)
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    assert!(wait_listening(port));
    // The token in the file is the one the endpoint wants.
    assert_eq!(
        request(
            port,
            "POST",
            "/call",
            Some("token-made-earlier"),
            Some("{}")
        )
        .0,
        200
    );
    assert_eq!(
        request(port, "POST", "/call", Some("something-else"), Some("{}")).0,
        401
    );
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "token-made-earlier\n"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before,
        "an existing token must not be touched at all"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_makes_its_token_mode_0600_when_there_is_none() {
    let dir = std::env::temp_dir().join(format!("librewolf-token-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("librewolf.token");

    let port = free_port();
    let mut child = host_command(port)
        .env("EIDOLON_LIBREWOLF_TOKEN_FILE", &path)
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    assert!(
        wait_listening(port),
        "the host should start and make a token"
    );
    let deadline = Instant::now() + TIMEOUT;
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        mode, 0o600,
        "the token it makes must be private, not merely announced as such"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_manifest_names_the_extension_and_this_binary() {
    let out = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
        .arg("--print-host-manifest")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["name"], json!("eidolon_librewolf"));
    assert_eq!(v["type"], json!("stdio"));
    assert_eq!(v["allowed_extensions"], json!([ADDON]));
    assert!(
        v["path"].as_str().unwrap().ends_with("eidolon-librewolf"),
        "{v}"
    );
}

#[test]
fn a_slow_extension_does_not_hold_the_endpoint_shut() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        request(
            port,
            "POST",
            "/call",
            Some(&token),
            Some(&json!({"method": "read", "args": {"tab_id": 7}}).to_string()),
        )
    });
    let _cmd = host.command();
    // While the read is outstanding, status and health still answer: the record
    // is locked only around the read and the write of it, never across the wait.
    assert_eq!(host.health()["enabled_tabs"], json!(1));
    assert_eq!(host.call("status", json!({}))["ok"], json!(true));
    drop(caller);
}

#[test]
fn an_unknown_method_is_answered_by_name() {
    let host = Host::start();
    let reply = host.call("attach", json!({}));
    assert_eq!(reply["ok"], json!(false));
    assert!(
        reply["error"].as_str().unwrap().contains("unknown method"),
        "{reply}"
    );
}

#[test]
fn a_silent_extension_is_taken_for_a_dead_browser_and_the_host_exits() {
    // End of stream is not a reliable "the browser is gone" — a content process can
    // hold the pipe — so the host also treats the extension's silence as death. The
    // stdin here is held open on purpose: only the silence may end this host.
    let port = free_port();
    let token = format!("test-token-{port}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
        .env("EIDOLON_LIBREWOLF_PORT", port.to_string())
        .env("EIDOLON_LIBREWOLF_TOKEN", &token)
        .env("EIDOLON_LIBREWOLF_SILENCE_S", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = Some(child.stdin.take().unwrap());
    assert!(wait_listening(port), "the host never listened");
    write_frame(
        stdin.as_mut().unwrap(),
        &json!({"v": 2, "type": "hello", "addon": ADDON}),
    );
    assert_eq!(
        request(port, "POST", "/call", Some(&token), Some("{}")).0,
        200
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin.take());
    assert!(
        exited,
        "a host that hears nothing for its silence window must exit on its own"
    );
    let stopped = TcpStream::connect(("127.0.0.1", port)).is_err()
        || request(port, "POST", "/call", Some(&token), Some("{}")).0 != 200;
    assert!(
        stopped,
        "a host that has exited must not still be answering"
    );
}

#[test]
fn a_diagnostic_on_a_broken_stderr_does_not_stop_the_host_from_exiting() {
    // The bug this pins: stderr is a pipe to the browser, and the moment the
    // browser dies that pipe breaks. `eprintln!` panics on a broken pipe, which
    // killed the very thread that shuts the host down — so a host could outlive its
    // browser, hold the port, and answer the tools about a session that was gone.
    // Closing the read end here makes every write to stderr fail, exactly as it does
    // after the browser exits.
    let port = free_port();
    let token = format!("test-token-{port}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_eidolon-librewolf"))
        .env("EIDOLON_LIBREWOLF_PORT", port.to_string())
        .env("EIDOLON_LIBREWOLF_TOKEN", &token)
        .env("EIDOLON_LIBREWOLF_SILENCE_S", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = Some(child.stdin.take().unwrap());
    // Our end of its stderr goes away before it says anything.
    drop(child.stderr.take());
    assert!(wait_listening(port), "the host never listened");
    write_frame(
        stdin.as_mut().unwrap(),
        &json!({"v": 2, "type": "hello", "addon": ADDON}),
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin.take());
    assert!(
        exited,
        "a host whose stderr has broken must still reach its own shutdown, not wedge in a panicking log line"
    );
}

// --- the enabled set, one tab at a time -------------------------------------

#[test]
fn a_tab_that_is_loading_a_new_document_is_refused_here() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    // The extension withdrew the document the moment the browser said the tab was
    // loading: the enablement is still there, the document is not.
    host.loading(2, "https://example.com/b", 100.0);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(host.health()["enabled_tabs"], json!(1));
    let status = host.call("status", json!({}));
    assert_eq!(status["result"]["tabs"][0]["confirmed"], json!(false), "{status}");

    let read = host.call("read", json!({"tab_id": 7}));
    assert_eq!(read["ok"], json!(false));
    assert!(
        read["error"].as_str().unwrap().contains("loading a new document"),
        "{read}"
    );
    // Nothing was asked of the extension for it: a tab with no identified document
    // has nothing to issue a request against.
    assert!(host.inbox.try_recv().is_err(), "no command should have been sent");
}

#[test]
fn the_inventory_is_listed_with_nothing_enabled_and_enables_nothing() {
    let mut host = Host::start();
    host.hello();
    std::thread::sleep(Duration::from_millis(100));

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        let body = json!({"method": "tabs", "args": {}}).to_string();
        request(port, "POST", "/call", Some(&token), Some(&body))
    });
    // The inventory names no tab: the command carries none, and it is asked of a
    // host that has nothing enabled.
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("tabs"));
    assert_eq!(cmd["tab_id"], Value::Null);
    assert_eq!(cmd["generation"], Value::Null);
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": null,
                     "result": {"tabs": [
                         {"tab_id": 3, "title": "A background tab", "enabled": false},
                         {"tab_id": 7, "title": "The other one", "enabled": false}
                     ], "note": "an inventory"}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["result"]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(v["result"]["tabs"][0]["tab_id"], json!(3));
    assert_eq!(v["result"]["tabs"][0]["enabled"], json!(false));
    // Listing a tab is not enabling one: the record is exactly as empty as it
    // was, and every verb that needs a tab still refuses.
    assert_eq!(host.health()["enabled_tabs"], json!(0));
    let read = host.call("read", json!({"tab_id": 3}));
    assert_eq!(read["ok"], json!(false));
    assert!(read["error"].as_str().unwrap().contains("not enabled"), "{read}");
}

#[test]
fn two_enabled_tabs_are_two_boundaries() {
    let mut host = Host::start();
    host.hello();
    let origin_a = origin_of("https://example.com/a");
    let origin_b = origin_of("https://other.example/b");
    host.state(
        json!([
            {"tab_id": 1, "url": "https://example.com/a", "title": "A", "origin": origin_a,
             "generation": 1, "time_origin": 10.0, "nonce": "nonce-of-generation-1",
             "confirmed": true, "enabled_at_ms": 1},
            {"tab_id": 2, "url": "https://other.example/b", "title": "B", "origin": origin_b,
             "generation": 2, "time_origin": 20.0, "nonce": "nonce-of-generation-2",
             "confirmed": true, "enabled_at_ms": 2},
        ]),
        Value::Null,
    );
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(host.health()["enabled_tabs"], json!(2));

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        let body = json!({"method": "read", "args": {"tab_id": 2}}).to_string();
        request(port, "POST", "/call", Some(&token), Some(&body))
    });
    // The command names the tab it is for, and the document that tab has — not
    // the other enabled tab's.
    let cmd = host.command();
    assert_eq!(cmd["tab_id"], json!(2));
    assert_eq!(cmd["generation"], json!(2));
    assert_eq!(cmd["nonce"], json!("nonce-of-generation-2"));
    assert_eq!(cmd["time_origin"], json!(20.0));
    let id = cmd["id"].as_u64().unwrap();
    // An answer that names the *other* tab is refused, whatever it says.
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 1,
                     "generation": 2, "time_origin": 20.0, "nonce": "nonce-of-generation-2",
                     "result": {"url": "https://other.example/b", "title": "B",
                                "text": "text", "truncated": false}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(v["error"].as_str().unwrap().contains("answered about tab 1"), "{v}");

    // Disabling the other tab leaves this one exactly as it was.
    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        let body = json!({"method": "detach", "args": {"tab_id": 1}}).to_string();
        request(port, "POST", "/call", Some(&token), Some(&body))
    });
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("detach"));
    assert_eq!(cmd["tab_id"], json!(1));
    let id = cmd["id"].as_u64().unwrap();
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 1,
                     "generation": 1, "time_origin": 10.0, "nonce": "nonce-of-generation-1",
                     "result": {"detached": true}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(true));
    let status = host.call("status", json!({}));
    let tabs = status["result"]["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1, "{status}");
    assert_eq!(tabs[0]["tab_id"], json!(2));
    assert_eq!(tabs[0]["nonce"], json!("nonce-of-generation-2"));
    // And a request for the disabled tab is refused here, without asking.
    let read = host.call("read", json!({"tab_id": 1}));
    assert_eq!(read["ok"], json!(false));
    assert!(read["error"].as_str().unwrap().contains("tab 1 is not enabled"), "{read}");
}

#[test]
fn a_cross_origin_navigation_ends_the_enablement_before_anything_is_asked() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    // The tab is gone from the enabled set, with the origin it left — the
    // extension latches that crossing and pushes it.
    host.disabled(
        "the tab left https://example.com (the browser reported https://other.example/b)",
        "https://other.example/b",
    );
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(host.health()["enabled_tabs"], json!(0));

    let read = host.call("read", json!({"tab_id": 7}));
    assert_eq!(read["ok"], json!(false));
    let err = read["error"].as_str().unwrap();
    assert!(err.contains("left https://example.com"), "{err}");
    assert!(err.contains("toolbar button"), "{err}");
    // Nothing was asked of the extension for it: an enablement ends before a
    // read, not after one.
    assert!(host.inbox.try_recv().is_err(), "no command should have been sent");
}

#[test]
fn an_acting_op_carries_the_tab_and_the_document_it_is_issued_for() {
    let mut host = Host::start();
    host.hello();
    host.enabled(1, "https://example.com/a", 100.0);
    host.wait_enabled();

    let port = host.port;
    let token = host.token.clone();
    let caller = std::thread::spawn(move || {
        let body = json!({"method": "click", "args": {"tab_id": 7, "ref": "e3"}}).to_string();
        request(port, "POST", "/call", Some(&token), Some(&body))
    });
    let cmd = host.command();
    assert_eq!(cmd["op"], json!("click"));
    assert_eq!(cmd["tab_id"], json!(7));
    assert_eq!(cmd["generation"], json!(1));
    assert_eq!(cmd["nonce"], json!("nonce-of-generation-1"));
    assert_eq!(cmd["time_origin"], json!(100.0));
    assert_eq!(cmd["args"]["ref"], json!("e3"));
    let id = cmd["id"].as_u64().unwrap();
    // The tab re-identified its document while the click was out — a same-origin
    // navigation, which keeps the enablement and moves the generation. An answer
    // about the old document is refused even though the tab is still enabled.
    host.state(
        json!([{"tab_id": 7, "url": "https://example.com/a", "title": "A page",
                "origin": "https://example.com", "generation": 2, "time_origin": 100.0,
                "nonce": "nonce-of-generation-1", "confirmed": true, "enabled_at_ms": 1}]),
        Value::Null,
    );
    host.send(json!({"v": 2, "type": "result", "id": id, "ok": true, "tab_id": 7,
                     "generation": 1, "time_origin": 100.0, "nonce": "nonce-of-generation-1",
                     "result": {"clicked": true}}));
    let (_, body) = caller.join().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["error"].as_str().unwrap().contains("generation 1 to 2"),
        "{v}"
    );
}

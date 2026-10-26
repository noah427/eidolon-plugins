//! `eidolon-godot` — the godot bridge host. The README in `../` is the whole
//! picture; this file is the process.
//!
//! Two faces, one process:
//!
//! * the **loopback endpoint** the `godot_*` tools dial, on
//!   127.0.0.1:$EIDOLON_GODOT_PORT (8092) with a Bearer token, `/health` and
//!   `/call` in the same shapes as `eidolon-browser` and `eidolon-librewolf`; and
//! * the **editor's socket**, on 127.0.0.1:$EIDOLON_GODOT_BRIDGE_PORT (8094),
//!   where the Godot editor's addon dials *in* and stays.
//!
//! That direction is the whole difference between this host and the librewolf
//! one. LibreWolf starts its native host, so no `service:` block can name it;
//! Godot starts nothing, so this host is started by `eidolon plugins service
//! start godot` and the editor comes to it. Which also means nothing here ever
//! starts Godot: the engine is the operator's, the README says what they do to
//! lend it, and a verb that lands on no editor answers with that instead of
//! failing obscurely.

/// Diagnostics go to stderr — this is a daemon's log, and an operator may rotate
/// it out from under a process that has to stay up for as long as their editor is
/// open. `eprintln!` *panics* on a broken pipe, so a log line must not be able to
/// kill the bridge it is describing.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(::std::io::stderr(), $($arg)*);
    }};
}

mod bridge;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State as Extract;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::routing::{get, post};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::bridge::{Bridge, METHODS};

// The ports this host's two faces listen on. 8090 is the browser service and
// 8091 the librewolf host; these two are the godot plugin's, and the tool files
// name 8092 in their `service()` exactly as they must.
const DEFAULT_PORT: u16 = 8092;
const DEFAULT_BRIDGE_PORT: u16 = 8094;

const USAGE: &str = "\
eidolon-godot — the godot bridge host

  (no arguments)   listen, both ways: the tools dial 127.0.0.1:$EIDOLON_GODOT_PORT
                   and the Godot editor's addon dials
                   127.0.0.1:$EIDOLON_GODOT_BRIDGE_PORT
  --help           this text

Environment:
  EIDOLON_GODOT_PORT            the port the tools dial (default 8092)
  EIDOLON_GODOT_BRIDGE_PORT     the port the editor dials (default 8094)
  EIDOLON_GODOT_TOKEN_FILE      where the token lives
                                (default ~/.config/eidolon/godot.token)
  EIDOLON_GODOT_TOKEN           a token to use instead of the file (tests)
  EIDOLON_GODOT_STALE_S         how long the editor may be silent before it is
                                called gone (default 30)
  EIDOLON_GODOT_CALL_TIMEOUT_S  how long one call may wait on the editor
                                (default 30)
";

struct App {
    bridge: Arc<Bridge>,
    expected: Vec<u8>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        note!("eidolon-godot: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--help") | Some("-h") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(other) if other.starts_with("--") => {
            return Err(format!("unknown flag {other:?}\n\n{USAGE}"));
        }
        _ => {}
    }

    let tools_port = port("EIDOLON_GODOT_PORT", DEFAULT_PORT)?;
    let bridge_port = port("EIDOLON_GODOT_BRIDGE_PORT", DEFAULT_BRIDGE_PORT)?;
    let token = match std::env::var("EIDOLON_GODOT_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => load_token(&token_file())?,
    };

    let bridge = Arc::new(Bridge::new(token.clone(), stale(), call_timeout()));

    // The editor's port is bound first so that a second copy of this host fails
    // *here*, by name, rather than after it has taken the tools' port and left
    // the first one half-deaf.
    let editor_listener = TcpListener::bind(("127.0.0.1", bridge_port)).await.map_err(|e| {
        format!(
            "could not listen for the editor on 127.0.0.1:{bridge_port}: {e}. Another \
             eidolon-godot is probably already running — `eidolon plugins service status godot` \
             says which; this one will not attach to the editor on its behalf, or leave a second \
             copy of itself on the same project"
        )
    })?;
    note!("godot bridge: the editor dials 127.0.0.1:{bridge_port}");
    tokio::spawn(bridge.clone().accept(editor_listener));

    let app = Arc::new(App {
        bridge: bridge.clone(),
        expected: format!("Bearer {token}").into_bytes(),
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/call", post(call))
        .with_state(app);

    let listener = TcpListener::bind(("127.0.0.1", tools_port)).await.map_err(|e| {
        format!("could not listen on 127.0.0.1:{tools_port}: {e}. Is another eidolon-godot running?")
    })?;
    note!("godot bridge: the tools dial http://127.0.0.1:{tools_port}");

    axum::serve(listener, router).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// `/health` is answered from this host's own record and never asks the editor,
/// so `eidolon plugins service` readiness cannot be held up by one.
async fn health(Extract(app): Extract<Arc<App>>) -> Json<Value> {
    Json(app.bridge.health())
}

/// POST {method, args} -> {ok, result} | {ok: false, error}. The same shape as
/// `eidolon-browser` and `eidolon-librewolf`, so a tool file is recognizably the
/// same kind of thing.
async fn call(
    Extract(app): Extract<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let supplied = headers
        .get("authorization")
        .map_or(&[][..], |v| v.as_bytes());
    if !bool::from(supplied.ct_eq(&app.expected)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": "bad token" })),
        );
    }
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "invalid JSON body" })),
            );
        }
    };
    let method = body.get("method").cloned().unwrap_or(Value::Null);
    let Some(method) = method.as_str().filter(|m| METHODS.contains(m)) else {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "error": format!(
                    "unknown method {} — this bridge forwards {:?}, and nothing else",
                    method, METHODS
                ),
            })),
        );
    };
    let args = match body.get("args") {
        Some(a @ Value::Object(_)) => a.clone(),
        None | Some(Value::Null) => json!({}),
        Some(_) => {
            return (
                StatusCode::OK,
                Json(json!({ "ok": false, "error": "args must be an object" })),
            );
        }
    };

    // `status` is this host's, and answers with or without an editor. Everything
    // else is the addon's, and is forwarded to whoever is attached.
    let result = match method {
        "status" => Ok(app.bridge.status().await),
        other => app.bridge.request(other, args).await,
    };
    let reply = match result {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    (StatusCode::OK, Json(reply))
}

fn port(var: &str, default: u16) -> Result<u16, String> {
    match std::env::var(var) {
        Ok(p) => p.parse().map_err(|_| format!("{var} is not a port: {p:?}")),
        Err(_) => Ok(default),
    }
}

/// How long the editor may be silent before this host concludes it is gone. The
/// addon sends a tick every couple of seconds, so this is generous on purpose: it
/// has to be longer than the longest thing an editor legitimately does without
/// running a frame. Tests set it to seconds.
fn stale() -> Duration {
    seconds("EIDOLON_GODOT_STALE_S", 30)
}

fn call_timeout() -> Duration {
    seconds("EIDOLON_GODOT_CALL_TIMEOUT_S", 30)
}

fn seconds(var: &str, default: u64) -> Duration {
    match std::env::var(var) {
        Ok(v) => v.parse().map(Duration::from_secs).unwrap_or(Duration::from_secs(default)),
        Err(_) => Duration::from_secs(default),
    }
}

fn token_file() -> PathBuf {
    if let Some(p) = std::env::var_os("EIDOLON_GODOT_TOKEN_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/eidolon/godot.token")
}

/// The token in `path`, made there (mode 0600) if it isn't yet. The value is never
/// logged: it is the one thing between any local process and somebody's editor.
///
/// The addon reads this same file, which is why it lives somewhere a person can
/// see and chmod: it is the credential *both* halves hold, not a secret this host
/// keeps from the other side of its own bridge.
fn load_token(path: &Path) -> Result<String, String> {
    // What is already there decides what happens next, and nothing here ever
    // rewrites a file it did not create: a token some other process is using is
    // not this host's to replace. An existing file is usable only if it is a
    // private regular file owned by this user; anything else is a refusal that
    // says what to do about it, not a silent fix-up.
    if let Some(token) = read_private_token(path)? {
        return Ok(token);
    }

    use std::io::Write;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .map_err(|e| format!("could not read /dev/urandom: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    use std::os::unix::fs::OpenOptionsExt as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true)
        .create_new(true) // never truncate, never follow a symlink into a file that exists
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(token.as_bytes())
                .map_err(|e| format!("{}: {e}", path.display()))?;
            note!(
                "godot bridge: made a new token at {} (mode 0600); grant it to the verbs with \
                 `eidolon plugins grant godot_<verb> file:{}`",
                path.display(),
                path.display()
            );
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Something got there between the read above and this create —
            // another host starting at the same moment, or a file made by hand.
            // Theirs wins; read it under the same rules rather than overwriting it.
            read_private_token(path)?.ok_or_else(|| {
                format!(
                    "{} appeared while this host was making a token and then could not be read",
                    path.display()
                )
            })
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `Ok(Some(token))` when the path holds a private regular file of this user's,
/// `Ok(None)` when there is nothing there, `Err` when there is something else.
///
/// The checks are the point: a world-readable token, a file owned by another
/// account, a symlink pointing somewhere else, a device — each is refused with
/// the reason, because each one means this bridge is not the private thing the
/// rest of the design assumes it is.
fn read_private_token(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    // O_NONBLOCK is load-bearing, not decoration: opening a FIFO read-only blocks
    // until a writer appears, so without it a fifo sitting at the token path would
    // hang this host *before* the fstat below could deliver the refusal it
    // promises. On a regular file it does nothing.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut f = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: cannot be opened: {e}", path.display())),
    };
    // fstat, not stat: these describe the file that was actually opened.
    let meta = f.metadata().map_err(|e| format!("{}: {e}", path.display()))?;
    let mode = meta.mode() & 0o7777;
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file; the token must be one, and this host will not read a \
             symlink, a pipe or a device",
            path.display()
        ));
    }
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(format!(
            "{} is owned by uid {}, not by this user (uid {me}); refusing to use a token another \
             account controls",
            path.display(),
            meta.uid()
        ));
    }
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is mode {mode:04o}: group or other can read the token. `chmod 600 {}` and start \
             again — or delete it and let this host make a new one (the addon picks up the new \
             one when it next connects)",
            path.display(),
            path.display()
        ));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let t = s.trim();
    if t.is_empty() {
        return Err(format!(
            "{} is empty, and this host will not overwrite a token file it did not make: delete \
             it and start again to have a new one created",
            path.display()
        ));
    }
    Ok(Some(t.to_string()))
}

//! `eidolon-librewolf` — the librewolf bridge host. The README in `../` is the
//! whole picture; this file is the process.
//!
//! Two faces, one process:
//!
//! * the **native messaging host** LibreWolf spawns, because the extension's
//!   host manifest names this binary and stdio is the channel; and
//! * the **loopback endpoint** the `librewolf_*` tools dial, on
//!   127.0.0.1:$EIDOLON_LIBREWOLF_PORT (8091) with a Bearer token, `/health`
//!   and `/call` in the same shapes as `eidolon-browser`.
//!
//! There is deliberately no `service:` block in `plugin.rn`: the process that
//! has to be running is the one the browser starts, and `eidolon plugins
//! service start` cannot start a browser. The token, the `api_request`
//! endpoint and the "answers with the reason it isn't there" error are the
//! primitives that *are* reused. See the README's Prerequisites.
//!
//! Nothing here writes to stdout except native messages: a stray `println!`
//! would be read by the extension as a length prefix and desynchronize the
//! channel. Diagnostics go to stderr, which the browser collects into its
//! console.

/// Diagnostics go to stderr — the browser collects it into its console, and it
/// becomes a broken pipe the instant the browser dies. `eprintln!` *panics* on a
/// broken pipe, which would kill the very thread that is shutting this host down;
/// that is exactly how a host came to outlive its browser and keep answering the
/// tools about a session that was gone. So nothing here uses `eprintln!`.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(::std::io::stderr(), $($arg)*);
    }};
}

mod frame;
mod state;

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
use tokio::sync::{Notify, mpsc};

use crate::state::{Bridge, quote};

/// The extension this host serves. The browser enforces the same value through
/// the host manifest's `allowed_extensions`; that is *enforcement of the
/// browser's own rule*, not authentication of whatever ends up on the other end
/// of stdin — so the id is checked here too, and it is checked twice: once from
/// the argv the browser supplies, once from the extension's own `hello`.
const EXPECTED_ADDON: &str = "librewolf-bridge@eidolon.local";

/// The name the extension passes to `runtime.connectNative`, and so the stem of
/// the host manifest file in the browser's `native-messaging-hosts` directory.
const HOST_NAME: &str = "eidolon_librewolf";

const METHODS: [&str; 10] = [
    "status",
    "tabs",
    "read",
    "structure",
    "click",
    "type",
    "scroll",
    "navigate",
    "back",
    "detach",
];

const USAGE: &str = "\
eidolon-librewolf — the librewolf bridge host

  (no arguments)          run as LibreWolf's native messaging host: stdio to the
                          extension, and the loopback endpoint on
                          127.0.0.1:$EIDOLON_LIBREWOLF_PORT for the tools
  --print-host-manifest   print the host manifest to install for this binary
  --help                  this text

Environment:
  EIDOLON_LIBREWOLF_PORT        loopback port (default 8091)
  EIDOLON_LIBREWOLF_TOKEN_FILE  where the token lives
                                (default ~/.config/eidolon/librewolf.token)
  EIDOLON_LIBREWOLF_TOKEN       a token to use instead of the file (tests)
";

struct App {
    bridge: Arc<Bridge>,
    expected: Vec<u8>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        note!("eidolon-librewolf: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--print-host-manifest") => return print_host_manifest(),
        Some("--help") | Some("-h") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(other) if other.starts_with("--") => {
            return Err(format!("unknown flag {other:?}\n\n{USAGE}"));
        }
        _ => {}
    }
    check_launch_arguments(&args)?;

    let port = port()?;
    let token = match std::env::var("EIDOLON_LIBREWOLF_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => load_token(&token_file())?,
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let bridge = Arc::new(Bridge::new(EXPECTED_ADDON, tx));
    let eof = Arc::new(Notify::new());
    spawn_stdio(bridge.clone(), rx, eof.clone());
    spawn_silence_watchdog(bridge.clone(), eof.clone(), silence());

    let app = Arc::new(App {
        bridge: bridge.clone(),
        expected: format!("Bearer {token}").into_bytes(),
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/call", post(call))
        .with_state(app.clone());

    // Binding is how a second LibreWolf profile finds out it cannot have this
    // host: it fails here and exits, rather than quietly attaching to the
    // instance that holds the port and serving the wrong browser's tabs.
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        format!(
            "could not listen on 127.0.0.1:{port}: {e}. Another LibreWolf profile may already have \
             the bridge open; this host will not attach to it — close that browser, or set \
             EIDOLON_LIBREWOLF_PORT for this one"
        )
    })?;
    note!("librewolf bridge: native host for {EXPECTED_ADDON} on http://127.0.0.1:{port}");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { eof.notified().await })
        .await
        .map_err(|e| e.to_string())?;
    note!("librewolf bridge: the native messaging port is gone; exiting");
    Ok(())
}

async fn health(Extract(app): Extract<Arc<App>>) -> Json<Value> {
    Json(app.bridge.health())
}

/// POST {method, args} -> {ok, result} | {ok: false, error}. The same shape as
/// `eidolon-browser`, so a tool file is recognizably the same kind of thing.
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
            Json(
                json!({ "ok": false, "error": format!("unknown method {}", quote(&method.to_string())) }),
            ),
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

    // Each of these takes the bridge's lock only around the record it reads or
    // writes, never across the await that waits on the browser: a status call
    // stays answerable while a read is outstanding.
    let result = match method {
        "status" => app.bridge.status(),
        "tabs" => app.bridge.tabs().await,
        "read" => app.bridge.read(&args).await,
        "structure" => app.bridge.structure(&args).await,
        "click" => app.bridge.click(&args).await,
        "type" => app.bridge.type_text(&args).await,
        "scroll" => app.bridge.scroll(&args).await,
        "navigate" => app.bridge.navigate(&args).await,
        "back" => app.bridge.back(&args).await,
        "detach" => app.bridge.detach(&args).await,
        _ => unreachable!("METHODS is the list this matches on"),
    };
    let reply = match result {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    (StatusCode::OK, Json(reply))
}

/// The native messaging channel: one thread reading stdin, one writing stdout,
/// both telling the server when the port is gone.
fn spawn_stdio(bridge: Arc<Bridge>, mut out: mpsc::UnboundedReceiver<Value>, eof: Arc<Notify>) {
    let write_eof = eof.clone();
    std::thread::spawn(move || {
        while let Some(msg) = out.blocking_recv() {
            // Locked per message and dropped before the next: the guard never
            // crosses a line, and this is the only thread that writes stdout.
            let mut stdout = std::io::stdout().lock();
            if let Err(e) = frame::write_message(&mut stdout, &msg) {
                note!("native host: writing to the browser failed: {e}");
                break;
            }
        }
        write_eof.notify_one();
    });

    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        loop {
            match frame::read_message(&mut stdin) {
                Ok(Some(msg)) => bridge.deliver(msg),
                Ok(None) => {
                    note!("native host: the browser closed the native messaging port");
                    break;
                }
                Err(e) => {
                    note!("native host: reading from the browser failed: {e}");
                    break;
                }
            }
        }
        // Nothing can be served once the channel is gone: the attachment's
        // `activeTab` grant died with the browser side, and every request
        // waiting on an answer is failed rather than left to time out.
        bridge.disconnect();
        eof.notify_one();
    });
}

/// End of stream is not a reliable "the browser is gone", and neither is
/// reparenting: measured on this machine, a host whose parent had already gone
/// was left with ppid 1 at startup, so watching for the change finds nothing.
/// What *is* reliable is silence. The extension and the browser die together, so
/// an extension that has not said anything for a while is a browser that is no
/// longer there — and the host it spawned should not outlive it, holding the port
/// and answering the tools about a session that is gone. The extension sends a
/// heartbeat; anything at all from it resets the clock.
fn spawn_silence_watchdog(bridge: Arc<Bridge>, eof: Arc<Notify>, silence: Duration) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let idle = bridge.idle();
            if idle >= silence {
                note!(
                    "native host: nothing from the extension for {}s; the browser it belonged to is gone, so this host is exiting",
                    idle.as_secs()
                );
                bridge.disconnect();
                eof.notify_one();
                return;
            }
        }
    });
}

/// Firefox starts a native host with exactly two arguments: the path of the host
/// manifest it read, then the add-on id (Firefox 55+). They are read as *positions*.
/// Looking for an argument that merely resembles an id — one containing `@` — is not
/// a check: an add-on id need not contain one, so a wrong id without one would be
/// waved through the very check that is advertised as refusing wrong ids.
///
/// No arguments at all is the one other shape allowed, and only so that a person can
/// run this by hand to read `--help` or try it out. What is *served* is decided by
/// the `hello` below, whatever the command line says.
fn check_launch_arguments(args: &[String]) -> Result<(), String> {
    match &args[1..] {
        [] => Ok(()),
        [manifest, addon] => {
            if !(manifest.starts_with('/') && manifest.ends_with(".json")) {
                return Err(format!(
                    "the first argument of a native host is the path of the host manifest the \
                     browser read, and {manifest:?} is not one. Firefox passes (manifest path, \
                     add-on id); this host refuses a shape it does not recognise rather than \
                     guessing which argument is which"
                ));
            }
            if addon != EXPECTED_ADDON {
                return Err(format!(
                    "this native host serves only {EXPECTED_ADDON}; the browser passed {addon:?}"
                ));
            }
            Ok(())
        }
        other => Err(format!(
            "a native host is started with (manifest path, add-on id) and this one got {} \
             argument(s): {other:?}",
            other.len()
        )),
    }
}

/// How long the extension may be silent before this host concludes the browser is
/// gone. An operator should not have to change it; tests set it to seconds.
fn silence() -> Duration {
    match std::env::var("EIDOLON_LIBREWOLF_SILENCE_S") {
        Ok(v) => v
            .parse::<u64>()
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30)),
        Err(_) => Duration::from_secs(30),
    }
}

fn port() -> Result<u16, String> {
    match std::env::var("EIDOLON_LIBREWOLF_PORT") {
        Ok(p) => p
            .parse()
            .map_err(|_| format!("EIDOLON_LIBREWOLF_PORT is not a port: {p:?}")),
        Err(_) => Ok(8091),
    }
}

fn token_file() -> PathBuf {
    if let Some(p) = std::env::var_os("EIDOLON_LIBREWOLF_TOKEN_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config/eidolon/librewolf.token")
}

/// The token in `path`, made there (mode 0600) if it isn't yet. The value is
/// never logged: it is the only credential between a tool and this endpoint, and
/// the browser console keeps whatever lands on stderr.
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
                "librewolf bridge: made a new token at {} (mode 0600); grant it to the verbs with \
                 `eidolon plugins grant librewolf_<verb> file:{}`",
                path.display(),
                path.display()
            );
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Something got there between the read above and this create — another
            // host starting at the same moment, or a file made by hand. Theirs wins;
            // read it under the same rules rather than overwriting it.
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
/// the reason, because each one means the credential is not the private thing the
/// rest of this design assumes it is.
fn read_private_token(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    // O_NONBLOCK is load-bearing, not decoration: opening a FIFO read-only blocks
    // until a writer appears, so without it a fifo sitting at the token path would
    // hang this host *before* the fstat below could deliver the refusal it promises.
    // On a regular file it does nothing.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut f = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: cannot be opened: {e}", path.display())),
    };
    // fstat, not stat: these describe the file that was actually opened.
    let meta = f
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
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
             again — or delete it and let this host make a new one",
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
            "{} is empty, and this host will not overwrite a token file it did not make: delete it \
             and start again to have a new one created",
            path.display()
        ));
    }
    Ok(Some(t.to_string()))
}

/// The host manifest to place where the browser will read it. Printed rather
/// than installed: writing into a browser's profile or its config directory is
/// the operator's act, not a tool's.
fn print_host_manifest() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find my own path: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let manifest = json!({
        "name": HOST_NAME,
        "description": "The librewolf bridge: works with the tabs a person enables in LibreWolf",
        "path": exe.display().to_string(),
        "type": "stdio",
        "allowed_extensions": [EXPECTED_ADDON],
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
    );
    Ok(())
}

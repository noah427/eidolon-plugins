//! `eidolon-claude` — the driver host a session dials to run a turn on an
//! external CLI.
//!
//! Run by hand while the extraction is a design:
//!
//! ```text
//! eidolon-claude --socket /run/user/1000/eidolon-claude.sock
//! ```
//!
//! and, once the plugin exists, started by `eidolon plugins service start
//! claude`. It binds a private unix socket and serves one connection per
//! session; the whole access story is the socket's mode, and nothing else.

use std::process::ExitCode;
use std::sync::Arc;

use eidolon_claude::real::{self, Adapter};
use eidolon_claude::{
    DEFAULT_HEALTH_PORT, Host, PROTOCOL, Script, bind_private, default_socket, serve, serve_health,
};

const USAGE: &str = "\
eidolon-claude — the driver host

USAGE:
    eidolon-claude [--socket <path>] [--health-port <port>] [--real [--cli <path>]]
    eidolon-claude hook        (run by the CLI; not by a person)

OPTIONS:
    --socket <path>      bind the turn protocol here
                         (default $XDG_RUNTIME_DIR/eidolon-claude.sock)
    --health-port <port> bind the readiness probe here (default 8093; 0 to
                         serve none, which `plugins service status` will report
                         as no readiness declared)
    --real               spawn the CLI named by --cli and translate its stream,
                         instead of scripting a turn
    --cli <path>         the CLI to spawn with --real (default `claude`)
    --backend <name>     the backend name this host declares in its hello and the
                         session must declare for it (default claude-cli)
    --fake               the scripted backend (the default)
    --version            print the version
    -h, --help           print this

ENVIRONMENT:
    EIDOLON_FAKE_CANCEL_ACK=0   leave a cancel unanswered, so a session's
                                cancelled-unacknowledged path can be exercised
    EIDOLON_FAKE_SEQ_GAP=1      skip a sequence number, for the gap rule
";

fn main() -> ExitCode {
    // The socket is created with this process's umask, and `bind_private`'s
    // `chmod` is a second syscall after it — so narrowing the umask *here*,
    // before any runtime or thread exists, is what keeps the socket from being
    // connectable by anyone but its owner in the instant `bind` creates it.
    //
    // **0o077, not 0o177.** The obvious mask is 0o177, which strips the owner's
    // search bit along with everyone else's — and a umask does not know what it
    // is masking. A *socket* never needs `x`. A *directory* is unusable without
    // it, and this host creates one: the scratch every per-turn artifact lives
    // in. Under 0o177 `create_dir_all` produced a directory nothing could enter,
    // and the turn came back `Permission denied (os error 13)`. 0o077 leaves
    // directories at 0700 — owner-only, which is what they should be — and any
    // file at 0600. For the socket it is exactly as private as 0600: connecting
    // needs *write*, and neither mask grants that to anyone but the owner. The
    // `chmod` to 0600 afterwards is the tightening, not the whole of it.
    //
    // Restoring the owner's bit afterwards would also have worked for the leaf
    // this host names, and for nothing else: `create_dir_all` makes every missing
    // component, and an intermediate left without its search bit fails at the
    // *next* component with the same `EACCES`. The mask is the place to get this
    // right, not a repair after it.
    unsafe {
        libc::umask(0o077);
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    // The subcommand the CLI runs once per tool call. Not a host, and it must
    // never print anything but its decision.
    if argv.first().map(String::as_str) == Some("hook") {
        real::hook_client();
    }
    // The stdio MCP server the CLI spawns in registry mode.
    if argv.first().map(String::as_str) == Some("mcp") {
        real::mcp_client();
    }

    let mut socket = None;
    let mut health_port = DEFAULT_HEALTH_PORT;
    let mut real_cli: Option<std::path::PathBuf> = None;
    let mut backend = eidolon_claude::DEFAULT_BACKEND.to_string();
    let mut args = argv.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => match args.next() {
                Some(p) => socket = Some(std::path::PathBuf::from(p)),
                None => {
                    eprintln!("eidolon-claude: --socket wants a path");
                    return ExitCode::from(2);
                }
            },
            "--health-port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(port) => health_port = port,
                None => {
                    eprintln!("eidolon-claude: --health-port wants a port number");
                    return ExitCode::from(2);
                }
            },
            "--fake" => {}
            "--real" => real_cli = Some(std::path::PathBuf::from("claude")),
            // The name the session must declare for this host. It has to match on
            // both sides: the session refuses a host whose hello names a different
            // backend, which is the point of the check.
            "--backend" => match args.next() {
                Some(name) => backend = name,
                None => {
                    eprintln!("eidolon-claude: --backend wants a name");
                    return ExitCode::from(2);
                }
            },
            "--cli" => match args.next() {
                Some(p) => real_cli = Some(std::path::PathBuf::from(p)),
                None => {
                    eprintln!("eidolon-claude: --cli wants a path");
                    return ExitCode::from(2);
                }
            },
            "--version" => {
                println!("eidolon-claude {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("eidolon-claude: unknown argument `{other}`\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let path = socket.unwrap_or_else(|| default_socket("claude"));
    // What this host offers is what it can actually do. Both modes are served
    // now: the hook door for own-tools, the MCP door for registry.
    let me = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("eidolon-claude"));
    let real = real_cli.map(|cli| Adapter {
        cli,
        me,
        hook_deadline: eidolon_claude::real::HOOK_DEADLINE,
    });
    let host = Arc::new(Host {
        backend,
        script: Script::default().from_env(),
        modes: vec!["registry".to_string(), "own_tools".to_string()],
        real,
        ..Host::fake()
    });

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("eidolon-claude: no runtime: {e}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        // Binding happens inside the runtime: a tokio listener has to be
        // registered with one, and before this fix it panicked here and left a
        // socket behind at the umask's mode.
        let listener = match bind_private(&path) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!(
                    "eidolon-claude: could not bind {}: {e}\n\
                     A socket that cannot be made private is not served from.",
                    path.display()
                );
                return ExitCode::from(1);
            }
        };
        println!(
            "eidolon-claude: listening on {} (protocol {PROTOCOL}, {} backend)",
            path.display(),
            if host.real.is_some() { "real" } else { "fake" },
        );
        // The readiness probe is a convenience; the transport is the socket. A
        // port someone else already holds must not take the turn service down
        // with the probe, so say so and serve anyway.
        if health_port != 0 {
            match serve_health(health_port).await {
                Ok(port) => {
                    println!("eidolon-claude: readiness on http://127.0.0.1:{port}/health")
                }
                Err(e) => eprintln!(
                    "eidolon-claude: no readiness on 127.0.0.1:{health_port}: {e}\n\
                     Serving anyway on the socket. `plugins service start` will report this\n\
                     start as failed, because its probe never answers — free the port, or\n\
                     declare no readiness (pass --health-port 0) and say so in the README."
                ),
            }
        }
        serve(listener, host).await;
        ExitCode::SUCCESS
    })
}

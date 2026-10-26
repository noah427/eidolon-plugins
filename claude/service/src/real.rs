//! The real backend: the half that spawns the vendor CLI and reports what it
//! saw.
//!
//! The stream-json shapes here are not invented — they are the ones
//! `crates/claude/src/stream.rs` has parsed since it was the in-process driver,
//! confirmed against a captured stream (`crates/claude/tests/fixtures/salve.jsonl`).
//! Two of them are easy to get wrong and are called out where they are handled:
//! an assistant message arrives **per content block** with a shared
//! `message.id`, so consecutive `assistant` lines are one message; and thinking
//! arrives both as `stream_event` deltas and again whole inside the message,
//! with a signature that must be kept verbatim — one of the two is journaled,
//! never both.
//!
//! The door the CLI needs to reach the gate is served here: the settings file
//! names `<this binary> hook`, the CLI runs it once per tool call, and it
//! forwards to the socket this turn bound. Every failure on that path is a
//! **deny**, because a hook that cannot answer must not become an allow.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::{Conn, bind_private, read_frame, write_frame};

/// The socket the CLI's hook client is told about. Set on the CLI's
/// environment, which is what carries it to the hook command the CLI runs.
pub const HOOK_ENV: &str = "EIDOLON_CLAUDE_HOOK";

/// The socket the MCP shim is told about, for registry mode.
pub const MCP_ENV: &str = "EIDOLON_CLAUDE_MCP";

/// How a turn ended, for the one cleanup path every ending falls through.
enum Ending {
    /// The CLI's stream ended — finished, or failed with a reason.
    Streamed(std::io::Result<()>),
    /// The session cancelled the turn.
    Cancelled,
}

/// What the adapter needs that is not on the wire.
#[derive(Clone)]
pub struct Adapter {
    /// The vendor CLI to spawn.
    pub cli: PathBuf,
    /// This binary, named by the settings file for `hook`.
    pub me: PathBuf,
    /// How long a `PreToolUse` hook waits for the session's gate before this
    /// host answers it itself with a **deny**. See [`HOOK_DEADLINE`].
    pub hook_deadline: std::time::Duration,
}

/// How long the own-tools door waits for the session to answer the gate.
///
/// A gate that is asked and never answered is not a failure the hook can report
/// — it is a **stall**, and the CLI's `PreToolUse` command blocks until it ends.
/// Every other way this door fails is already a deny (a payload that names no
/// tool, a connection that dies, a session that hangs up), but a hang has no
/// failure path at all, so without this the door's own documentation ("every way
/// of failing is a deny") is not true of the one case that matters most.
///
/// **The number is coupled to the session's, and cannot be uncoupled from this
/// side.** The session has an idle bound of its own (300 s by default in
/// `eidolon-driver`), and when that expires it takes the whole turn down; this
/// must therefore be *shorter*, so that the operator gets a denied call rather
/// than a dead turn. Two hundred and forty seconds leaves a person time to
/// answer a flag — which is the real reason it is not much smaller, since in the
/// TUI a flag is a question waiting on somebody who may be reading something
/// else — and still lands before the session gives up. The clean fix is a
/// deadline on the wire, where both sides can read it; until then this is a
/// constant on this side and a documented coupling, not a guarantee.
pub const HOOK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

/// Run one turn: spawn the CLI, translate its stream into frames.
///
/// Returns when the turn has settled, or when the connection or the process
/// failed. `pid` is published so the connection can signal the process group on
/// a cancel — the ack is the *host's* word that it signalled the group it
/// spawned, which is why it is sent by whoever did the signalling.
pub(crate) async fn real_turn(
    conn: &Arc<Conn>,
    req: &Value,
    adapter: &Adapter,
    // `backend` is the name this host answers to: the prefix the session's
    // catalog key for this host carries. See `model_flag`.
    backend: &str,
    pid: Arc<Mutex<Option<u32>>>,
    cancel: Arc<tokio::sync::Notify>,
    cancel_req: Arc<Mutex<Option<Value>>>,
) -> std::io::Result<()> {
    let field = |k: &str| {
        req.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let turn_id = field("turn_id");
    let mode = field("mode");
    let prompt = field("prompt");
    let model = field("model");
    let cwd = field("cwd");
    let scratch = PathBuf::from(field("scratch"));
    let resume = req.get("resume").and_then(Value::as_str).map(str::to_string);
    let images = req.get("images").cloned().unwrap_or_else(|| json!([]));

    if mode != "own_tools" && mode != "registry" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("this host serves own_tools and registry, not `{mode}`"),
        ));
    }

    std::fs::create_dir_all(&scratch)?;
    // Every per-turn artifact — the two door sockets, the settings file — is
    // created inside this directory, so it must be one the owner can enter. The
    // umask in `main` (0o077) leaves it 0700; nothing here has to repair it, and
    // nothing here could repair an *intermediate* component if the mask were
    // widened again. The real-process test below is what holds that invariant.
    // Unique per turn, not per millisecond: one host serves many sessions, and
    // two turns starting together must not collide on the hook socket's path.
    let nonce = format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms(),
        NONCES.fetch_add(1, Ordering::Relaxed)
    );

    // Everything this turn creates, removed on the way out whatever way that is.
    let mut artifacts: Vec<PathBuf> = Vec::new();

    // The doors come up before the CLI that will knock on them.
    let hook_sock = scratch.join(format!("hook-{nonce}.sock"));
    artifacts.push(hook_sock.clone());
    let door = if mode == "own_tools" {
        Some(hook_door(
            bind_private(&hook_sock)?,
            conn.clone(),
            turn_id.clone(),
            nonce.clone(),
            adapter.hook_deadline,
        ))
    } else {
        None
    };
    let mcp_sock = scratch.join(format!("mcp-{nonce}.sock"));
    artifacts.push(mcp_sock.clone());
    let registry_door = if mode == "registry" {
        // What the session is offering this turn, pushed once with the turn:
        // the harness's registry's own "what the model is offered" rule.
        let tools = req.get("tools").and_then(Value::as_array).cloned();
        match tools {
            Some(tools) => Some(mcp_door(
                bind_private(&mcp_sock)?,
                conn.clone(),
                turn_id.clone(),
                tools,
            )),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a registry turn must carry the tools the session is offering",
                ));
            }
        }
    } else {
        None
    };

    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(model) = model_flag(backend, &model) {
        args.push("--model".into());
        args.push(model);
    }
    if let Some(resume) = &resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }

    let mut envs: Vec<(String, String)> = Vec::new();
    if mode == "own_tools" {
        // The gate is reached through the CLI's own pre-tool hook: the settings
        // file names this binary, and the socket travels on the environment.
        let settings = scratch.join(format!("settings-{nonce}.json"));
        artifacts.push(settings.clone());
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "hooks": { "PreToolUse": [ { "matcher": "", "hooks": [ {
                    "type": "command",
                    "command": format!("{} hook", adapter.me.display()),
                } ] } ] }
            }))?,
        )?;
        args.push("--settings".into());
        args.push(settings.display().to_string());
        envs.push((HOOK_ENV.to_string(), hook_sock.display().to_string()));
    } else {
        // The registry is reached through the CLI's own MCP client. `--tools ""`
        // leaves it no built-ins and `--strict-mcp-config` no server but this one,
        // which is what makes the harness's tools the only ones it can reach. The
        // config is inline JSON, as the in-process driver hands it over.
        let config = json!({
            "mcpServers": { "eidolon": {
                "command": adapter.me.display().to_string(),
                "args": ["mcp"],
                "env": { MCP_ENV: mcp_sock.display().to_string() },
            } }
        })
        .to_string();
        args.push("--mcp-config".into());
        args.push(config);
        args.push("--strict-mcp-config".into());
        args.push("--tools".into());
        args.push(String::new());
    }

    let mut cmd = Command::new(&adapter.cli);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    if !cwd.is_empty() {
        cmd.current_dir(&cwd);
    }
    // A session of its own, so nothing the CLI does can reach the terminal the
    // UI is drawing on — and so the whole group can be signalled at once.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = cmd.spawn()?;
    *pid.lock().await = child.id();

    // The prompt as the CLI reads it: one JSON user message, images first — the
    // order the wire reads a message in. Core's `image_to_wire` wraps each image
    // block's `source` and nothing else.
    if let Some(mut stdin) = child.stdin.take() {
        let mut content: Vec<Value> = images
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("source").map(|s| json!({ "type": "image", "source": s })))
                    .collect()
            })
            .unwrap_or_default();
        content.push(json!({ "type": "text", "text": prompt }));
        let line = json!({ "type": "user", "message": { "role": "user", "content": content } });
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.shutdown().await?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no CLI stdout"))?;
    // The CLI's own words about why it stopped, kept for the failure message:
    // "it ended" is not a diagnosis.
    let stderr = child.stderr.take();
    let mut complaint = Some(tokio::spawn(async move {
        let mut said = String::new();
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if said.len() < 4096 {
                    said.push_str(&line);
                    said.push('\n');
                }
            }
        }
        said
    }));

    // The turn task owns the process, so it owns the kill *and* the ack: only a
    // side that can reap can observe that a group is gone.
    //
    // **One exit.** Every way out of a turn — the stream ending, an error, a
    // cancel — falls through the same cleanup below. The alternative is what this
    // used to do: `return` from inside the cancel arm, which skipped the door
    // aborts and left the per-turn hook and MCP tasks detached and still
    // answering for a turn that was over.
    // The input of the most recent model call, for `context_size`. It lives out
    // here rather than inside `read_stream` because a *cancel* has to report it
    // too: a killed process sends no `result` line, and the number cannot ride on
    // `settle` — that names the turn's end, not what the transcript had grown to.
    // Overwritten by every call, so what survives is the last one's.
    let mut last_input: Option<u64> = None;
    // The event sequence, shared with the cancel path: a `context_size` sent
    // after the kill has to take the *next* number, and only the reader knows
    // which one that is. Kept here so both halves can see it.
    let seqs = std::sync::atomic::AtomicU64::new(0);
    let ending = tokio::select! {
        r = read_stream(conn, stdout, &turn_id, &mode, &mut last_input, &seqs) => Ending::Streamed(r),
        _ = cancel.notified() => Ending::Cancelled,
    };

    let outcome: std::io::Result<()> = match ending {
        Ending::Streamed(Ok(())) => Ok(()),
        Ending::Streamed(Err(e)) => {
            // **Bounded.** A CLI that closes its stdout and then holds stderr
            // open would otherwise hang the turn for as long as it liked, and the
            // session would wait on a settle that could not come.
            let said = match complaint.take() {
                Some(handle) => {
                    match tokio::time::timeout(std::time::Duration::from_millis(500), handle).await {
                        Ok(Ok(said)) => said,
                        _ => String::new(),
                    }
                }
                None => String::new(),
            };
            let said = said.trim();
            let last = said.lines().last().unwrap_or("");
            Err(std::io::Error::other(if last.is_empty() {
                e.to_string()
            } else {
                format!("{e} — the CLI said: {last}")
            }))
        }
        Ending::Cancelled => {
            // Bounded escalation with a reap inside the wait: a group is not gone
            // until its leader has been waited for, so polling without reaping
            // would report "not gone" for a process that died at once.
            let observed = match child.id() {
                Some(id) => {
                    let pgid = -(id as i32);
                    unsafe {
                        libc::kill(pgid, libc::SIGTERM);
                    }
                    let mut gone = false;
                    for phase in 0..2 {
                        let deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
                        loop {
                            let _ = child.try_wait();
                            gone = unsafe {
                                libc::kill(pgid, 0) != 0 && *libc::__errno_location() == libc::ESRCH
                            };
                            if gone || tokio::time::Instant::now() >= deadline {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        if gone {
                            break;
                        }
                        // It ignored being asked, or a descendant did. Everything
                        // still in the group is now stopped.
                        if phase == 0 {
                            unsafe {
                                libc::kill(pgid, libc::SIGKILL);
                            }
                        }
                    }
                    // A zombie counts as present, so the group cannot be seen to
                    // be gone until the leader has been waited for.
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        child.wait(),
                    )
                    .await;
                    gone || unsafe {
                        libc::kill(pgid, 0) != 0 && *libc::__errno_location() == libc::ESRCH
                    }
                }
                None => false,
            };
            let req_id = cancel_req.lock().await.clone().unwrap_or(Value::Null);
            // `killed` says the group was *seen* to be gone, which is the only
            // thing it is allowed to mean.
            conn.send(&json!({
                "method": "cancel_ack", "turn_id": turn_id,
                "req_id": req_id, "killed": observed,
            }))
            .await?;
            // What the transcript had grown to before the kill, if a model call
            // ever answered. A turn killed before that reported nothing, and an
            // unknown is not a zero.
            if let Some(tokens) = last_input
                && tokens > 0
            {
                // The next unused number. A reader killed mid-send may have used
                // this one already, and then the session drops it as a duplicate —
                // which loses a context size, never a turn, and can never open a
                // gap in the sequence.
                conn.send(&json!({
                    "method": "event", "turn_id": turn_id,
                    "seq": seqs.load(Ordering::Relaxed),
                    "kind": "context_size", "payload": { "tokens": tokens },
                }))
                .await?;
            }
            conn.send(&json!({
                "method": "settle", "turn_id": turn_id,
                "outcome": "cancelled", "stop_reason": "cancelled",
            }))
            .await?;
            Ok(())
        }
    };

    // ---------------------------------------------------------- one cleanup
    //
    // A **bounded** reap: a process that survived `SIGKILL`, or one that closed
    // its stdout and kept running, must not hang the turn. `kill_on_drop` signals
    // it again when the child handle goes.
    if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
    }
    // The per-turn tasks and the paths they bound go with the turn. A door that
    // outlives its turn is a door that answers for a turn that is over.
    if let Some(door) = door {
        door.abort();
    }
    if let Some(door) = registry_door {
        door.abort();
    }
    if let Some(handle) = complaint.take() {
        handle.abort();
    }
    for artifact in artifacts {
        let _ = std::fs::remove_file(artifact);
    }
    outcome
}

/// Translate the CLI's stream-json into the wire's frames.
async fn read_stream(
    conn: &Arc<Conn>,
    stdout: tokio::process::ChildStdout,
    turn_id: &str,
    mode: &str,
    last_input: &mut Option<u64>,
    seqs: &std::sync::atomic::AtomicU64,
) -> std::io::Result<()> {
    let mut lines = BufReader::new(stdout).lines();
    let mut seq: u64 = 0;
    // The message being assembled: its id, its blocks, and the usage it carried.
    // Blocks arrive **per content block** under one id, so this is a
    // concatenation, not a sequence of messages.
    let mut current: Option<(String, Vec<Value>, Option<Value>)> = None;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue; // the CLI writes non-JSON on stdout sometimes; not a frame
        };
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" => {
                if ev.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(id) = ev.get("session_id").and_then(Value::as_str)
                {
                    conn.send(&json!({
                        "method": "session", "turn_id": turn_id, "opaque": id,
                    }))
                    .await?;
                }
            }
            "stream_event" => {
                // The raw SSE event, for a live view. Journaled never: the whole
                // message below carries the same content, thinking included, and
                // two records of one thought is one too many.
                let event = &ev["event"];
                let kind = match event.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content_block_delta" => match event
                        .pointer("/delta/type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                    {
                        "text_delta" => "text_delta",
                        "thinking_delta" => "thinking_delta",
                        _ => continue,
                    },
                    _ => continue,
                };
                let text = event
                    .pointer("/delta/text")
                    .or_else(|| event.pointer("/delta/thinking"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                conn.send(&json!({
                    "method": "event", "turn_id": turn_id, "seq": seq, "kind": kind,
                    "payload": { "text": text },
                }))
                .await?;
                seq += 1;
                seqs.store(seq, Ordering::Relaxed);
            }
            "assistant" => {
                let msg = &ev["message"];
                let id = msg.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let blocks: Vec<Value> = msg
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let usage = msg.get("usage").cloned();
                // The input of this model call, which is what `context_size`
                // reports. Overwritten by every call, so the last one survives.
                if let Some(t) = input_of(usage.as_ref()) {
                    *last_input = Some(t);
                }
                match &mut current {
                    Some((cur, acc, u)) if *cur == id => {
                        acc.extend(blocks);
                        if u.is_none() {
                            *u = usage;
                        }
                    }
                    _ => {
                        if let Some((_, acc, u)) = current.take() {
                            seq = flush_message(conn, turn_id, seq, acc, u).await?;
                        }
                        current = Some((id, blocks, usage));
                    }
                }
            }
            "user" => {
                if let Some((_, acc, u)) = current.take() {
                    seq = flush_message(conn, turn_id, seq, acc, u).await?;
                }
                let items = ev
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for item in items {
                    if item.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = item
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let payload = if mode == "registry" {
                        // The session ran it and holds the result; this frame is
                        // only where in the stream it belongs, so it carries the
                        // id and nothing else.
                        json!({ "call_id": id })
                    } else {
                        json!({
                            "call_id": id,
                            "content": text_of(item.get("content")),
                            "is_error": item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        })
                    };
                    conn.send(&json!({
                        "method": "event", "turn_id": turn_id, "seq": seq,
                        "kind": "tool_result", "payload": payload,
                    }))
                    .await?;
                    seq += 1;
                    seqs.store(seq, Ordering::Relaxed);
                }
            }
            "result" => {
                if let Some((_, acc, u)) = current.take() {
                    // The last message goes out before the settle names the turn
                    // over; its own sequence number is the end of the stream.
                    seq = flush_message(conn, turn_id, seq, acc, u).await?;
                    seqs.store(seq, Ordering::Relaxed);
                }
                // What the transcript had grown to, said before the settle it
                // belongs to. A killed process sends no `result`, which is why the
                // cancel path reports the same number itself.
                if let Some(tokens) = *last_input
                    && tokens > 0
                {
                    conn.send(&json!({
                        "method": "event", "turn_id": turn_id, "seq": seq,
                        "kind": "context_size", "payload": { "tokens": tokens },
                    }))
                    .await?;
                }
                let stop = ev
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string();
                let failed = ev.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                conn.send(&json!({
                    "method": "settle",
                    "turn_id": turn_id,
                    "outcome": if failed { "failed" } else { "completed" },
                    "stop_reason": stop,
                }))
                .await?;
                return Ok(());
            }
            _ => {}
        }
    }
    // The stream ended without a `result`: the CLI died mid-turn.
    Err(std::io::Error::other(
        "the CLI ended without a result event",
    ))
}

/// What to put after `--model`, or nothing at all.
///
/// The session hands over a **catalog key**, and a driver's key is
/// `<backend>:<id>` (`crates/providers/src/catalog.rs`: `format!("{}:{id}", …)`).
/// The key for "this host, no particular model" is therefore `<backend>:` — a
/// backend's name with an empty id, which is not the name of anything a vendor's
/// CLI has ever heard of. Sending it as `--model claude-cli:` would make the CLI
/// refuse the turn for an unknown model, and the fault would look like the
/// plugin's.
///
/// So this host strips its **own** backend's prefix, and only its own: a bare
/// `sonnet` and another backend's key both pass through untouched, because the
/// only key this process can be sure it understands is the one it answers to.
/// What is left of `<backend>:` is nothing, and nothing is what `""` already
/// means — the host picks its own default.
///
/// Seen in the chain: on the turn *back* onto this host after a switch, the
/// session sent `model = "claude-cli:"`.
fn model_flag(backend: &str, model: &str) -> Option<String> {
    let bare = match model.strip_prefix(backend) {
        Some(rest) => rest.strip_prefix(':').unwrap_or(rest),
        None => model,
    };
    (!bare.is_empty()).then(|| bare.to_string())
}

/// The input of one model call: what `RecordKind::ContextSize` counts, and the
/// same sum the in-process driver takes (`usage.rs` `total_input`). A cache read
/// and the write of a new cache entry are both tokens the call had to carry, so
/// leaving them out would understate the transcript's size by most of it.
///
/// `None` when the call said nothing about usage — an unknown is not a zero.
fn input_of(usage: Option<&Value>) -> Option<u64> {
    let u = usage?.as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"))
}

/// Journal one assembled assistant message, then the one `usage` frame that
/// stands for the model call it was. One usage frame per model call is what the
/// session counts as a turn's call count, so it travels with the message rather
/// than with the CLI's turn total.
async fn flush_message(
    conn: &Arc<Conn>,
    turn_id: &str,
    seq: u64,
    blocks: Vec<Value>,
    usage: Option<Value>,
) -> std::io::Result<u64> {
    if blocks.is_empty() {
        return Ok(seq);
    }
    conn.send(&json!({
        "method": "event", "turn_id": turn_id, "seq": seq, "kind": "assistant_message",
        "payload": { "role": "assistant", "content": blocks },
    }))
    .await?;
    // One sequence number for the event. The `usage` frame below is not an
    // event and carries none, so the next event must not skip one — a gap is
    // what makes a session fail the whole turn.
    let next = seq + 1;

    // Priced when the CLI said what it cost, unpriced when it did not — which is
    // not the same as free, and the session keeps the difference.
    let priced = usage.as_ref().and_then(|u| u.as_object()).map(|u| {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        json!({
            "input_tokens": n("input_tokens"),
            "output_tokens": n("output_tokens"),
            "cache_creation_input_tokens": n("cache_creation_input_tokens"),
            "cache_read_input_tokens": n("cache_read_input_tokens"),
        })
    });
    conn.send(&json!({
        "method": "usage",
        "turn_id": turn_id,
        "priced": priced.unwrap_or(Value::Bool(false)),
        "vendor": usage.unwrap_or(Value::Null),
    }))
    .await?;
    Ok(next)
}

/// A tool result's text: a string, or a list of text parts.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ------------------------------------------------------------------ the door

/// Serve the `PreToolUse` socket for one turn.
///
/// Each connection is one hook invocation. The gate is the session's — this
/// asks and never executes — and every way of failing is a deny, because a hook
/// that cannot answer must not become an allow.
fn hook_door(
    listener: UnixListener,
    conn: Arc<Conn>,
    turn_id: String,
    nonce: String,
    deadline: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let conn = conn.clone();
            let turn_id = turn_id.clone();
            let nonce = nonce.clone();
            tokio::spawn(async move {
                let Ok(buf) = read_frame(&mut stream).await else {
                    return;
                };
                let payload: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
                let name = payload
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = payload.get("tool_input").cloned().unwrap_or_else(|| json!({}));
                let call_id = payload
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("hook-{nonce}"));
                let decision = if name.is_empty() {
                    deny("the hook payload names no tool")
                } else {
                    // Bounded, and a deny when it expires: an unanswered gate
                    // must not become a stalled CLI, and it must certainly not
                    // become an allow. See `HOOK_DEADLINE`.
                    let asked = match tokio::time::timeout(
                        deadline,
                        conn.ask(json!({
                            "method": "adjudicate", "turn_id": turn_id,
                            "call_id": call_id, "tool": name, "input": input,
                            // The tier, because the tool names are this side's to
                            // know. A session that does not read it falls back to
                            // the conservative one, which asks rather than widens.
                            "approval": approval_for(&name),
                        })),
                    )
                    .await
                    {
                        Ok(asked) => asked,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            if deadline.as_secs() >= 1 {
                                format!("no answer within {}s", deadline.as_secs())
                            } else {
                                format!("no answer within {}ms", deadline.as_millis())
                            },
                        )),
                    };
                    match asked {
                        Ok(v) if v.get("allow").and_then(Value::as_bool) == Some(true) => {
                            allow(v.get("reason").and_then(Value::as_str).unwrap_or_default())
                        }
                        Ok(v) => deny(v.get("reason").and_then(Value::as_str).unwrap_or_default()),
                        Err(e) => deny(&format!("the session could not be asked: {e}")),
                    }
                };
                let _ = write_frame(&mut stream, &decision).await;
                let _ = stream.shutdown().await;
            });
        }
    })
}

/// The decision Claude Code reads on the hook's stdout.
fn allow(reason: &str) -> Value {
    json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "permissionDecisionReason": if reason.is_empty() { "the session allowed this call" } else { reason },
    } })
}

fn deny(reason: &str) -> Value {
    let reason = if reason.is_empty() { "the session denied this call" } else { reason };
    json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": format!("eidolon driver: {reason}"),
    } })
}

/// Serve the MCP door for one turn: what the CLI's own MCP client reaches.
///
/// This host executes nothing. `list` answers from the tools the session pushed
/// with the turn, and `call` asks the session, which runs it through its own
/// registry under its own gate and hands back the result. The later
/// `tool_result` event this host emits for that call is a **marker**: the text
/// in the record is the session's, never anything written here.
fn mcp_door(
    listener: UnixListener,
    conn: Arc<Conn>,
    turn_id: String,
    tools: Vec<Value>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let conn = conn.clone();
            let turn_id = turn_id.clone();
            let tools = tools.clone();
            tokio::spawn(async move {
                let Ok(buf) = read_frame(&mut stream).await else {
                    return;
                };
                let req: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
                let reply = match req.get("op").and_then(Value::as_str).unwrap_or("") {
                    "list" => json!({ "tools": tools }),
                    "call" => {
                        let call_id = req
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let tool = req
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let input = req.get("input").cloned().unwrap_or_else(|| json!({}));
                        match conn
                            .ask(json!({
                                "method": "call", "turn_id": turn_id,
                                "call_id": call_id, "tool": tool, "input": input,
                            }))
                            .await
                        {
                            Ok(answer) => answer,
                            Err(e) => json!({
                                "content": format!("the session could not be asked: {e}"),
                                "is_error": true,
                                "images": [],
                            }),
                        }
                    }
                    other => json!({ "error": format!("unknown door op {other:?}") }),
                };
                let _ = write_frame(&mut stream, &reply).await;
                let _ = stream.shutdown().await;
            });
        }
    })
}

// ------------------------------------------------------------------ the mcp shim

/// `eidolon-claude mcp`: MCP over stdio, forwarded to the door.
///
/// A pipe, and a stalled MCP server stalls the CLI's whole turn, so this is
/// synchronous on purpose. A request without an id is a notification: act,
/// answer nothing.
pub fn mcp_client() -> ! {
    use std::io::{BufRead, Write};
    let socket = std::env::var(MCP_ENV).ok();
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or_default();
        let reply = match method {
            "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": req.pointer("/params/protocolVersion")
                    .and_then(Value::as_str).unwrap_or("2024-11-05"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "eidolon", "version": env!("CARGO_PKG_VERSION") },
            } }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => match forward_op(socket.as_deref(), &json!({ "op": "list" })) {
                Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "tools": v.get("tools").cloned().unwrap_or_else(|| json!([])),
                } }),
                Err(e) => mcp_error(id, &format!("the driver host: {e}")),
            },
            "tools/call" => {
                let tool = req.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
                let input = req.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
                // The CLI's own id for this call, so its view and the session's
                // agree on which call the result belongs to.
                let call_id = req
                    .pointer("/params/_meta/claudecode~1toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let call_id = if call_id.is_empty() { format!("mcp-{}", now_ms()) } else { call_id };
                let asked = json!({ "op": "call", "call_id": call_id, "tool": tool, "input": input });
                match forward_op(socket.as_deref(), &asked) {
                    Ok(v) => {
                        let text = v.get("content").and_then(Value::as_str).unwrap_or_default().to_string();
                        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        let mut content = vec![json!({ "type": "text", "text": text })];
                        if let Some(images) = v.get("images").and_then(Value::as_array) {
                            for image in images {
                                content.push(json!({
                                    "type": "image",
                                    "data": image.get("data").cloned().unwrap_or(Value::Null),
                                    "mimeType": image.get("media_type").cloned().unwrap_or(Value::Null),
                                }));
                            }
                        }
                        json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": content, "isError": is_error,
                        } })
                    }
                    Err(e) => mcp_error(id, &format!("the driver host: {e}")),
                }
            }
            other => mcp_error(id, &format!("no such method: {other}")),
        };
        if writeln!(out, "{reply}").is_err() || out.flush().is_err() {
            break;
        }
    }
    std::process::exit(0);
}

fn mcp_error(id: Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } })
}

/// One request and one reply over the door, synchronously.
fn forward_op(socket: Option<&str>, request: &Value) -> std::io::Result<Value> {
    let path = socket.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no driver socket configured")
    })?;
    forward(path, &serde_json::to_vec(request).unwrap_or_default())
}

// ------------------------------------------------------------ the hook client

/// `eidolon-claude hook`: read the CLI's payload on stdin, ask the door, print
/// the decision.
///
/// Never returns an error to the caller: every failure prints a deny and exits
/// 0, because a non-zero hook exit is itself interpreted by the CLI.
pub fn hook_client() -> ! {
    use std::io::{Read, Write};
    let mut payload = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut payload);
    let decision = match std::env::var(HOOK_ENV) {
        Err(_) => deny("no hook socket configured"),
        Ok(path) => forward(&path, &payload).unwrap_or_else(|e| deny(&format!("hook socket: {e}"))),
    };
    println!("{decision}");
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// One request and one reply over the door, synchronously: this runs once per
/// tool call in a fresh process and must never hang the CLI.
fn forward(path: &str, payload: &[u8]) -> std::io::Result<Value> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream as StdStream;
    let mut stream = StdStream::connect(path)?;
    // A backstop for a door that died without closing its socket — the door has
    // its own, shorter bound and answers with a deny before this fires, so this
    // should never be the thing that ends a hook. It exists so that this process,
    // which the CLI is blocked on, cannot outlive the turn no matter what
    // happened at the other end.
    stream.set_read_timeout(Some(HOOK_DEADLINE + std::time::Duration::from_secs(10)))?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > crate::MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the door answered with an implausible frame",
        ));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

// -------------------------------------------------------------------- helpers

/// What tier a vendor tool name is, for the session's gate.
///
/// This table is **the plugin's**, not the harness's: the names are Claude
/// Code's own built-ins, and a generic client has no business knowing them. It
/// is the one the in-process driver carried (`crates/claude/src/lib.rs`, its
/// `approval_for`).
///
/// **A declaration, not a permit, and it has exactly one reader.** The session
/// uses it in one place: a tool name the operator's own table has no row for is
/// *asked about*, and this string becomes the parenthesis in the question —
/// `unclassified tool (declared read_only)` (`crates/rune/policy.rn`, the
/// `classify_tool` fallthrough). Where the table has a row the field is not read
/// at all. `Bash` does not reach here either: policy.rn's input-field map sends
/// the name to the shell algebra, which classifies the command itself. So
/// nothing set here can allow a call — the most a wrong tier can do is change
/// the wording of a question somebody was going to be asked anyway.
///
/// Everything not named here is `mutating`: a tool this host has never heard of
/// is not thereby safe.
pub fn approval_for(tool: &str) -> &'static str {
    match tool {
        "Read" | "Glob" | "Grep" | "LS" | "WebFetch" | "WebSearch" | "TodoRead" | "ToolSearch"
        | "LSP" | "TaskList" | "TaskGet" => "read_only",
        _ => "mutating",
    }
}

/// Signal the whole process group the CLI was given, and **observe** it go.
///
/// `SIGTERM` first, because a process should be asked to stop before it is
/// stopped; then a bounded wait; then `SIGKILL`, because a CLI — or a descendant
/// it left behind — that ignores the first would otherwise outlive the turn and
/// keep whatever authority it was handed.
///
/// The answer is whether the group was **seen** to be gone, and that is the only
/// thing `killed` on the wire is allowed to mean. Sending a signal proves
/// nothing: the leader exiting is not the group exiting, and a descendant is not
/// the leader.
pub async fn terminate_group(pid: u32, grace: std::time::Duration) -> bool {
    let pgid = -(pid as i32);
    // Safety: the group of a child this host spawned with its own session, so
    // the group id is the child's pid.
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    if group_gone(pgid, grace).await {
        return true;
    }
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
    group_gone(pgid, grace).await
}

/// Wait, bounded, for nothing to be left in the group.
async fn group_gone(pgid: i32, grace: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        // `kill(-pgid, 0)` succeeds while any process in the group exists and
        // fails with `ESRCH` when none does. A zombie counts as existing, which
        // is why the caller reaps: a group is not gone until its leader has been
        // waited for.
        let gone = unsafe { libc::kill(pgid, 0) != 0 && *libc::__errno_location() == libc::ESRCH };
        if gone {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

static NONCES: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_is_read_only_and_anything_unknown_is_not() {
        for read in ["Read", "Grep", "Glob", "TaskGet"] {
            assert_eq!(approval_for(read), "read_only", "{read}");
        }
        for other in ["Bash", "Write", "Edit", "NotebookEdit", "SomeToolFromNextYear"] {
            assert_eq!(approval_for(other), "mutating", "{other}");
        }
    }

    #[test]
    fn a_catalog_key_for_this_host_is_not_a_model_name() {
        // The key the session sends for "this host, no model named".
        assert_eq!(model_flag("claude-cli", "claude-cli:"), None);
        assert_eq!(model_flag("claude-cli", "claude-cli"), None);
        assert_eq!(model_flag("claude-cli", ""), None);
        // A key with a model in it is a model, and the vendor wants the id.
        assert_eq!(
            model_flag("claude-cli", "claude-cli:sonnet").as_deref(),
            Some("sonnet")
        );
        // A bare id is already an id.
        assert_eq!(model_flag("claude-cli", "sonnet").as_deref(), Some("sonnet"));
        // Another backend's key is not this host's to take apart — a name that
        // happens to be a prefix of ours must not be stripped.
        assert_eq!(
            model_flag("claude-cli", "claude:opus").as_deref(),
            Some("claude:opus")
        );
        assert_eq!(
            model_flag("acme", "claude-cli:sonnet").as_deref(),
            Some("claude-cli:sonnet")
        );
    }
}

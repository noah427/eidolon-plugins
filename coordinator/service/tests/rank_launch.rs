//! The rank addition as the child actually receives it: the argv the host builds,
//! the file that argv names, and the second read that refuses bytes changed after
//! the request was bound.
//!
//! The child here is a shell script that records its own argv and then answers the
//! held receipt on the inherited channel, so `prepare` runs its real code path:
//! exact argv, real spawn, real receipt. It is not a claim about `eidolon` — the
//! `--system-append` flag is the core/CLI lane's: the installed binary at this
//! clone's ref carries it on no subcommand, and the core lane's current head carries
//! it on `run` but not on `chat`. What is proved here is this plugin's half of the
//! contract: the flag and the path to the *exact hashed bytes* are in the argv, and a
//! harness whose `chat` cannot take the flag is refused by name instead of being
//! started unranked.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serialises the fixture's **write-to-exec window** against every other spawn
/// in this binary.
///
/// `Text file busy` is not a permissions problem: `execve` refuses a file that
/// *any* process holds open for writing, and `fork` hands the child a copy of
/// every open descriptor until that child execs. `recording_child` writes the
/// script and `prepare` immediately execs it for the `chat --help` capability
/// probe, so a sibling test forking between that write and that exec is one
/// interleaving that makes the probe fail with `ETXTBSY` — even though the
/// writer closed its own handle before returning.
///
/// **A possible interleaving, not a measured one.** The failure was seen once,
/// in a `nix build`. Reverting this fix and rebuilding passed, so neither the
/// window's width nor the reason it surfaced there rather than here is
/// established; nothing in this comment is a measurement, and the serialisation
/// should not be read as a diagnosed timing behaviour.
///
/// Serialising the four tests removes that window at its cause. A retry loop
/// around the probe would hide it instead, and would make a transient `ETXTBSY`
/// indistinguishable from a genuinely busy executable **at the point of
/// failure** — a retry does eventually tell the two apart; what it cannot do is
/// attribute the cause where it happened.
static FIXTURE: Mutex<()> = Mutex::new(());

use eidolon_coordinator::{
    rank::Blocks,
    managed_start::{Host, LaunchState, Request},
    process_host::ProcessHost,
    Delivery, Row, Roster, Transport,
};

struct NoRoster;
impl Roster for NoRoster {
    fn rows(&self) -> Result<Vec<Row>, String> {
        Ok(Vec::new())
    }
}

struct NoTransport;
impl Transport for NoTransport {
    fn send(&self, _: &str, _: &str) -> Result<Delivery, String> {
        panic!("a held launch must not send a brief")
    }
}
impl eidolon_coordinator::managed_start::BriefTransport for NoTransport {
    fn send_correlated(&self, _: &str, _: &str, _: &str, _: &str) -> Result<eidolon_coordinator::managed_start::Correlated, String> {
        panic!("a held launch must not send a brief")
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("coord-launch-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A child that answers `chat --help` (claiming `--system-append` when `capable`),
/// writes its launch argv to `record`, and answers `held` on the channel the host
/// opened, naming the operation, the instance and its own pid.
fn recording_child(root: &Path, record: &Path, capable: bool) -> PathBuf {
    let script = root.join("child.sh");
    let help = if capable {
        "      --system-append <PATH>  Append this file's text to the system prompt"
    } else {
        "      --persona <NAME>        Wear a persona from the vault"
    };
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
if [ "$1" = "chat" ] && [ "$2" = "--help" ]; then
  echo "Usage: eidolon chat [OPTIONS]"
  echo "{help}"
  exit 0
fi
printf '%s\n' "$@" > '{record}'
fd=
op=
inst=
while [ $# -gt 0 ]; do
  case "$1" in
    --managed-start-fd) fd="$2"; shift 2 ;;
    --managed-operation) op="$2"; shift 2 ;;
    --managed-instance) inst="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '{{"state":"held","operation":"%s","instance":"%s","session":"fixture-held-child","pid":%s}}\n' \
  "$op" "$inst" "$$" >&"$fd"
"#,
            record = record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn block(dir: &Path, rank: &str, text: &str) -> PathBuf {
    let ranks = dir.join("ranks");
    std::fs::create_dir_all(&ranks).unwrap();
    let path = ranks.join(format!("{rank}.md"));
    std::fs::write(&path, text).unwrap();
    path
}

/// The request exactly as the monitor builds it: rank named, digest read from the
/// block's bytes. No test invents a digest.
fn bound(ranks: &Path, rank: Option<&str>) -> Request {
    let blocks = Blocks::new(ranks.to_path_buf());
    Request {
        operation: "rank-launch".into(),
        caller: "1792959651919".into(),
        workspace: std::env::temp_dir().to_string_lossy().into_owned(),
        model: "mock".into(),
        persona: None,
        brief: "bounded task".into(),
        rank: rank.map(str::to_string),
        rank_digest: blocks.resolve(rank).unwrap().map(|addition| addition.digest),
        shape: eidolon_coordinator::managed_start::LaunchShape::Headless,
        destination: None,
    }
}

fn argv_of(record: &Path) -> Vec<String> {
    std::fs::read_to_string(record)
        .expect("the child recorded its argv")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_launch_argv_carries_the_generic_flag_and_a_file_of_the_hashed_bytes() {
    // Hold the fixture window: no sibling test may fork while this script is
    // written and exec'd. See `FIXTURE`.
    let _fixture = FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let root = tmpdir("argv");
    let ranks = root.join("ranks");
    let miles = block(&root, "miles", "Hold the flank; report the count.\n");
    let record = root.join("argv.txt");
    let child = recording_child(&root, &record, true);

    let request = bound(&ranks, Some("miles"));
    let logs = root.join("logs");
    let mut host = ProcessHost::new(child, logs.clone(), Box::new(NoRoster), Box::new(NoTransport))
        .with_ranks(Blocks::new(ranks.clone()));
    host.register_fresh(&request).unwrap();
    host.prepare(&request).expect("the recording child answers a held receipt");

    let argv = argv_of(&record);
    let at = argv
        .iter()
        .position(|a| a == "--system-append")
        .unwrap_or_else(|| panic!("argv carries no --system-append: {argv:?}"));
    assert_eq!(
        argv.len(),
        at + 2,
        "the flag and its path are the last words, appended after `chat --model`: {argv:?}"
    );
    assert!(argv.iter().any(|a| a == "chat"), "the launch is still a chat: {argv:?}");
    assert!(!argv.iter().any(|a| a == "--persona"), "no persona was asked for: {argv:?}");

    let handed = PathBuf::from(&argv[at + 1]);
    assert_ne!(
        handed, miles,
        "the block's path is not handed onward: the CLI would read it a third time, and that \
         read is bound by nothing"
    );
    assert_eq!(handed.parent(), Some(logs.as_path()), "the copy belongs to this launch: {handed:?}");
    assert_eq!(
        std::fs::read(&handed).unwrap(),
        std::fs::read(&miles).unwrap(),
        "the file the child is handed holds the block's bytes"
    );
    assert_eq!(
        eidolon_coordinator::rank::digest_hex(&std::fs::read(&handed).unwrap()),
        request.rank_digest.clone().unwrap(),
        "and they hash to the digest the operation is bound to"
    );
    assert_eq!(
        std::fs::metadata(&handed).unwrap().permissions().mode() & 0o777,
        0o400,
        "the launch's copy is read-only before the child opens it: {handed:?}"
    );

    // What the child was handed is a copy, so an edit to the operator's block after
    // the spawn does not silently change the text this launch will compose.
    std::fs::write(&miles, "rewritten while the child was starting\n").unwrap();
    assert_eq!(
        std::fs::read(&handed).unwrap(),
        b"Hold the flank; report the count.\n",
        "the launch's own bytes are still the bytes that were hashed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_rank_with_no_block_adds_nothing_and_still_starts() {
    // Hold the fixture window: no sibling test may fork while this script is
    // written and exec'd. See `FIXTURE`.
    let _fixture = FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let root = tmpdir("noblock");
    let ranks = root.join("ranks");
    std::fs::create_dir_all(&ranks).unwrap();
    let record = root.join("argv.txt");
    let child = recording_child(&root, &record, true);

    let request = bound(&ranks, Some("miles"));
    assert_eq!(request.rank_digest, None, "a missing block binds no text");
    let mut host = ProcessHost::new(child, root.join("logs"), Box::new(NoRoster), Box::new(NoTransport))
        .with_ranks(Blocks::new(ranks));
    host.register_fresh(&request).unwrap();
    host.prepare(&request).expect("no block is no addition, not a failure");

    let argv = std::fs::read_to_string(&record).unwrap();
    assert!(
        !argv.contains("--system-append"),
        "an unwritten rank must not append anything: {argv}"
    );
    assert!(argv.contains("chat"), "the launch still happened: {argv}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bytes_changed_between_the_bound_request_and_the_spawn_are_refused() {
    // Hold the fixture window: no sibling test may fork while this script is
    // written and exec'd. See `FIXTURE`.
    let _fixture = FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let root = tmpdir("changed");
    let ranks = root.join("ranks");
    let miles = block(&root, "miles", "the text the operation was bound to\n");
    let record = root.join("argv.txt");
    let child = recording_child(&root, &record, true);

    let request = bound(&ranks, Some("miles"));
    // The operator edits the block after the request was prepared — the case the
    // digest exists for. The spawn must not happen at all.
    std::fs::write(&miles, "different text, never reviewed for this operation\n").unwrap();

    let mut host = ProcessHost::new(child, root.join("logs"), Box::new(NoRoster), Box::new(NoTransport))
        .with_ranks(Blocks::new(ranks));
    host.register_fresh(&request).unwrap();
    let error = host.prepare(&request).expect_err("changed bytes are a refusal");
    assert!(error.contains("`miles` rank block"), "{error}");
    assert!(error.contains("changed after this launch was prepared"), "{error}");
    assert!(error.contains("miles.md"), "the refusal names the file: {error}");
    assert!(
        error.contains(request.rank_digest.as_deref().unwrap()),
        "and the digest the operation is bound to: {error}"
    );
    assert!(!record.exists(), "nothing was spawned, so no child recorded an argv");
    assert_eq!(
        host.inspect(&request).unwrap(),
        LaunchState::NotCreated,
        "a refused block is not an attempted launch whose outcome is unknown"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_harness_whose_chat_lacks_the_flag_is_refused_by_name_not_started_unranked() {
    // Hold the fixture window: no sibling test may fork while this script is
    // written and exec'd. See `FIXTURE`.
    let _fixture = FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let root = tmpdir("noflag");
    let ranks = root.join("ranks");
    block(&root, "miles", "Hold the flank.\n");
    let record = root.join("argv.txt");
    let child = recording_child(&root, &record, false);

    let request = bound(&ranks, Some("miles"));
    let mut host = ProcessHost::new(child, root.join("logs"), Box::new(NoRoster), Box::new(NoTransport))
        .with_ranks(Blocks::new(ranks));
    host.register_fresh(&request).unwrap();
    let error = host.prepare(&request).expect_err("a chat without the flag cannot carry a rank");
    assert!(error.contains("`chat` does not accept `--system-append`"), "{error}");
    assert!(error.contains("`miles` rank block"), "{error}");
    assert!(!error.is_empty());
    assert!(!record.exists(), "nothing was launched");
    assert_eq!(host.inspect(&request).unwrap(), LaunchState::NotCreated);
    let _ = std::fs::remove_dir_all(&root);
}

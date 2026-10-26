//! **Which process is whose ancestor, and which window is whose** — the two
//! questions a *visible* managed launch has to answer before it may place
//! anything, answered from `/proc` with every unreadable link a refusal.
//!
//! ## Why this is not optional
//!
//! The headless launch asserts `held.pid == child.id()`: a direct-child fact,
//! true because the service spawns the harness itself. Through a terminal the
//! harness is `foot`'s **grandchild**, so that assertion fails by
//! construction — and the honest replacement is not a weaker one ("some pid
//! answered", "a window exists") but the real one: the claimed pid's parent
//! chain must contain the process we spawned. Anything else would let a
//! stranger's window be adopted as our child.
//!
//! The same walk answers the second question: the window to place is the one
//! whose client pid appears in that chain, because `foot` is the process the
//! compositor knows and the harness is not.
//!
//! ## What these functions deliberately do not do
//!
//! They read files and return facts. They spawn nothing, dispatch nothing and
//! move no window; the caller does that, and the caller is the one that has to
//! re-read afterwards and report what actually happened rather than what it
//! asked for.

use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

/// How far up a parent chain this will walk before refusing. A terminal hop is
/// one or two links; anything near this bound is not a launch we understand.
pub const MAX_DEPTH: usize = 8;

/// Why an ancestry question could not be answered.
///
/// Every variant is a *refusal*, never a "no": a chain that cannot be read is
/// not evidence that the processes are unrelated, and a launch must fail closed
/// on it rather than adopt something it could not check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AncestryRefusal {
    /// `/proc/<pid>/stat` was absent or unreadable — the process is gone, or
    /// this is not the machine's `/proc`.
    Unreadable {
        /// The process whose entry was wanted.
        pid: u32,
    },
    /// The entry exists but does not parse as a `stat` line.
    Malformed {
        /// The process whose entry did not parse.
        pid: u32,
    },
    /// The walk hit its bound without reaching the ancestor.
    TooDeep {
        /// The process the walk started from.
        pid: u32,
    },
}

impl std::fmt::Display for AncestryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AncestryRefusal::Unreadable { pid } => {
                write!(f, "cannot read /proc/{pid}/stat: the process is gone or unknown")
            }
            AncestryRefusal::Malformed { pid } => {
                write!(f, "/proc/{pid}/stat does not parse as a stat line")
            }
            AncestryRefusal::TooDeep { pid } => write!(
                f,
                "the parent chain from {pid} did not reach the expected ancestor within {MAX_DEPTH} links"
            ),
        }
    }
}

/// The immediate parent of `pid`, read from `/proc/<pid>/stat`.
///
/// Parsed from the **last** `)` because the command name in field 2 is
/// parenthesised and may itself contain spaces or parentheses — the classic
/// way this parse goes wrong.
pub fn parent_of(proc_root: &Path, pid: u32) -> Result<u32, AncestryRefusal> {
    let path = proc_root.join(pid.to_string()).join("stat");
    let line = std::fs::read_to_string(&path).map_err(|_| AncestryRefusal::Unreadable { pid })?;
    let after = line
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or(AncestryRefusal::Malformed { pid })?;
    // Fields after the command: state, ppid, pgrp, ...
    let mut fields = after.split_whitespace();
    let _state = fields.next().ok_or(AncestryRefusal::Malformed { pid })?;
    let ppid = fields
        .next()
        .ok_or(AncestryRefusal::Malformed { pid })?
        .parse::<u32>()
        .map_err(|_| AncestryRefusal::Malformed { pid })?;
    Ok(ppid)
}

/// Does `pid` descend from `ancestor`, within [`MAX_DEPTH`] links?
///
/// A process is not its own ancestor: `descends_from(p, p)` is `false`, which
/// is what makes this the right check for "the harness is somewhere below the
/// terminal we spawned" rather than a tautology.
pub fn descends_from(
    proc_root: &Path,
    pid: u32,
    ancestor: u32,
) -> Result<bool, AncestryRefusal> {
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        let parent = parent_of(proc_root, current)?;
        if parent == ancestor {
            return Ok(true);
        }
        // 0 is the pid no process has, and 1 is init: the chain has ended.
        if parent == 0 || parent == 1 {
            return Ok(false);
        }
        current = parent;
    }
    Err(AncestryRefusal::TooDeep { pid })
}

/// One row of `hyprctl clients -j`, reduced to what placement needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    /// The compositor's window address, which is what a dispatch selector
    /// takes.
    pub address: String,
    /// The pid the compositor says owns the window. **`0` means it names no
    /// process** (a foreign or XWayland surface): such a window can still be
    /// placed by address, and can never match an ancestry — which is the
    /// honest distinction rather than dropping the row and losing the address
    /// with it.
    pub pid: u32,
    /// The window's workspace id. Hyprland uses a negative id and a
    /// `special:` name for a scratch workspace.
    pub workspace_id: i64,
    /// The window's workspace name, which may be a name rather than a number.
    pub workspace_name: String,
    /// The window class, which is how a launch recognises its own terminal.
    pub class: String,
    /// The window title, for the report a caller writes afterwards.
    pub title: String,
}

/// Read `hyprctl clients -j` into [`Client`]s.
///
/// A row without an address, a pid or a workspace is **dropped** rather than
/// defaulted: a client this cannot describe is not one placement may use, and
/// inventing a workspace id of 0 for it would be how a window ends up
/// somewhere nobody chose.
pub fn clients_from(json: &serde_json::Value) -> Vec<Client> {
    let Some(rows) = json.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            Some(Client {
                address: row.get("address")?.as_str()?.to_string(),
                pid: row.get("pid")?.as_u64()?.try_into().ok()?,
                workspace_id: row.get("workspace")?.get("id")?.as_i64()?,
                workspace_name: row.get("workspace")?.get("name")?.as_str()?.to_string(),
                class: row.get("class").and_then(|c| c.as_str()).unwrap_or_default().to_string(),
                title: row.get("title").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The window hosting `pid`, if any — the client whose own pid is the
/// process or one of its ancestors.
///
/// Returns the **nearest** such client, because the closest ancestor that the
/// compositor knows about is the terminal the harness actually lives in.
pub fn window_hosting(
    proc_root: &Path,
    pid: u32,
    clients: &[Client],
) -> Result<Option<Client>, AncestryRefusal> {
    let mut current = pid;
    for _ in 0..=MAX_DEPTH {
        if let Some(found) = clients.iter().find(|c| c.pid == current) {
            return Ok(Some(found.clone()));
        }
        let parent = parent_of(proc_root, current)?;
        if parent == 0 || parent == 1 {
            return Ok(None);
        }
        current = parent;
    }
    Ok(None)
}

/// Why a destination could not be chosen.
///
/// Each is a refusal a launcher **reports**, never a fallback: the placement
/// convention is "the Imperator's desktop workspace + 1", and a workspace that
/// has no successor in that sense is a question for the operator, not a number
/// to guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementRefusal {
    /// No window could be found for the Imperator's process, so there is no
    /// desktop workspace to be underneath.
    NoWindow,
    /// The workspace is a scratch/special one, which has no numeric successor.
    Special {
        /// The workspace name the compositor gave it.
        name: String,
    },
    /// The workspace is named rather than numbered.
    NotNumeric {
        /// The workspace name as given.
        name: String,
    },
    /// No roster row carries the caller's address at all — a different fact
    /// from a row whose registration cannot be read, and it asks the caller to
    /// establish the address rather than to fix a directory.
    NoRow {
        /// The address the caller gave.
        caller: String,
    },
    /// The chain of starts does not reach a session marked as the Imperator, so
    /// "the Imperator's workspace + 1" has no Imperator to be relative to. Never
    /// repaired by falling back to the caller's own workspace: that is the
    /// silent wrong answer this variant exists to replace.
    ChainBroken {
        /// The session the walk had reached when it stopped.
        at: String,
        /// Which link is missing, in the walk's own words.
        why: String,
    },
    /// The caller's registration could not be turned into a pid: no presence
    /// directory, no readable `meta.json`, an `id` that disagrees with the name
    /// looked up, or no usable pid in it. Named rather than folded into
    /// `NoWindow`, because the two ask different questions of different people.
    NoRegistration {
        /// The registration directory the roster row named.
        registration: String,
        /// What was actually wrong, in the words of the read that failed.
        why: String,
    },
}

impl std::fmt::Display for PlacementRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlacementRefusal::NoWindow => write!(
                f,
                "the Imperator has no desktop window on this compositor, so there is no workspace to place a worker underneath"
            ),
            PlacementRefusal::Special { name } => write!(
                f,
                "the Imperator is on the scratch workspace `{name}`, which has no `underneath`: ask where the worker should go"
            ),
            PlacementRefusal::NotNumeric { name } => write!(
                f,
                "the Imperator is on the named workspace `{name}`, which has no numeric successor: ask where the worker should go"
            ),
            PlacementRefusal::NoRow { caller } => write!(
                f,
                "the roster carries no row for `{caller}`, so nothing here knows which process or window \
                 that address is: a null `session` is not established, never `no such session`, and an \
                 address is never matched from a name, a tag or a cwd"
            ),
            PlacementRefusal::ChainBroken { at, why } => write!(
                f,
                "the chain of starts does not reach an Imperator: it stopped at `{at}` because {why}. \
                 The destination is the Imperator's workspace plus one, so with no Imperator there is \
                 no number to place a worker on — and the caller's own workspace is not a substitute"
            ),
            PlacementRefusal::NoRegistration { registration, why } => write!(
                f,
                "the caller's registration `{registration}` cannot be turned into a process, so its window cannot be found: {why}"
            ),
        }
    }
}

/// **The settled convention, as arithmetic:** the worker goes to the desktop
/// workspace the Imperator is on, **plus one**, resolved at spawn time.
///
/// Not a constant (the pair observed on 2026-10-26 is evidence the linkage can
/// be resolved, not an instruction to code `11`), not the calling Legatus's own
/// workspace, and not a function of the worker's rank.
pub fn destination(workspace_id: i64, workspace_name: &str) -> Result<i64, PlacementRefusal> {
    if workspace_name.starts_with("special:") || workspace_id < 0 {
        return Err(PlacementRefusal::Special { name: workspace_name.to_string() });
    }
    if workspace_id <= 0 {
        return Err(PlacementRefusal::NotNumeric { name: workspace_name.to_string() });
    }
    Ok(workspace_id + 1)
}

/// **The compositor's client list, behind a seam** — the read-only half of
/// [`Placement`], split out because resolving a destination must be answerable
/// *without* placing anything and must be testable without a compositor.
///
/// One query, one answer: `hyprctl -j clients`. A failure is the compositor's
/// words, never an empty desktop — an empty list would silently mean "no window
/// hosts this pid", which is a refusal with a different reason.
pub trait Clients: Send + Sync {
    /// Every window the compositor lists, parsed; a row it cannot describe is
    /// dropped by [`parse_clients`], not defaulted.
    fn clients(&self) -> Result<Vec<Client>, String>;
}

impl Clients for Hyprland {
    fn clients(&self) -> Result<Vec<Client>, String> {
        parse_clients(&hyprctl(&["-j", "clients"])?)
    }
}

/// **Where a visible worker belongs, resolved from the caller's own registration.**
///
/// The three hops the design names, with every one of them a fact rather than an
/// inference: the roster row names the caller's **registration directory**
/// (`Row::id`), that directory's `meta.json` carries the harness's own `pid`
/// (`crates/swarm/src/presence.rs::Meta`), and the `/proc` walk plus the client
/// list turn that pid into the window hosting it — whose workspace, plus one, is
/// the destination.
///
/// Deliberately **read-only**: it reads two files and a JSON list and returns a
/// number with the window it came from. It places nothing, dispatches nothing,
/// and is safe to call before any launch exists.
///
/// Refuses rather than guesses at every hop, including the one this function
/// exists because of: the roster row's `pid` is **not** published by
/// `eidolon peers --json` (`Row` has no such field), so `meta.json` is the only
/// surface that carries it — and `meta.json` also lets a wrong-directory read be
/// caught, since the file's own `id` must equal the registration asked for.
pub fn destination_for(
    presence_root: &Path,
    proc_root: &Path,
    registration: &str,
    clients: &[Client],
) -> Result<(i64, Client), PlacementRefusal> {
    if registration.is_empty() {
        return Err(PlacementRefusal::NoRegistration {
            registration: registration.to_string(),
            why: "the roster row carries no registration id, so there is no presence directory to read".into(),
        });
    }
    let path = presence_root.join(registration).join("meta.json");
    let text = std::fs::read_to_string(&path).map_err(|e| PlacementRefusal::NoRegistration {
        registration: registration.to_string(),
        why: format!("{}: {e}", path.display()),
    })?;
    let meta: serde_json::Value = serde_json::from_str(&text).map_err(|e| PlacementRefusal::NoRegistration {
        registration: registration.to_string(),
        why: format!("{} is not JSON: {e}", path.display()),
    })?;
    // A registration directory whose own `id` disagrees with the name we looked
    // up is a different registration, and its pid is somebody else's process.
    if meta.get("id").and_then(|v| v.as_str()) != Some(registration) {
        return Err(PlacementRefusal::NoRegistration {
            registration: registration.to_string(),
            why: format!("{} names registration {:?}, not `{registration}`", path.display(), meta.get("id")),
        });
    }
    let pid = match meta.get("pid").and_then(|v| v.as_u64()) {
        Some(pid) if pid > 0 && pid <= u32::MAX as u64 => pid as u32,
        // Zero, too wide, or absent: a pid that would read as alive forever or
        // name nothing. Not a guess.
        other => {
            return Err(PlacementRefusal::NoRegistration {
                registration: registration.to_string(),
                why: format!("{} carries no usable pid ({other:?})", path.display()),
            })
        }
    };
    plan_placement(proc_root, pid, clients)
}

/// **The real resolver** — the chain of starts to its root, that session's
/// registration to its pid, and its window's workspace plus one. The number is
/// *beside the chain root*; calling the root the Imperator is the convention, not
/// this code's finding.
///
/// Two reads (one presence `meta.json`, one `hyprctl -j clients`) and a `/proc`
/// walk. It places nothing, dispatches nothing and moves no window; whether it is
/// *called at all* is the service's business, and `destination_for_operation`
/// calls it once per operation and pins the answer in the record.
pub struct ImperatorChain {
    /// Where the harness keeps `<registration>/meta.json`, or `None` when this
    /// process has no `XDG_RUNTIME_DIR`. `None` is a refusal at call time, never
    /// a reason to stop the monitor: the headless path needs no runtime dir, and
    /// a host that only ever starts headless children should still run.
    pub presence: Option<std::path::PathBuf>,
    /// `/proc`, named so a test can read a fixture.
    pub proc_root: std::path::PathBuf,
    /// The compositor's client list, read-only.
    pub compositor: std::sync::Arc<dyn Clients>,
}

impl crate::Resolver for ImperatorChain {
    fn destination(
        &self,
        caller: &str,
        store: &crate::Store,
        rows: &[crate::Row],
    ) -> Result<crate::Destination, String> {
        let root = crate::chain_root(&store.starts, rows, caller)?;
        let row = rows
            .iter()
            .find(|r| r.session.as_deref() == Some(root.as_str()))
            .ok_or_else(|| format!("the roster carries no row for the chain root `{root}`"))?;
        let presence = self.presence.as_ref().ok_or_else(|| {
            "this process has no XDG_RUNTIME_DIR, so it cannot read the presence directory that carries \
             the Imperator's pid: nothing here will guess a path"
                .to_string()
        })?;
        let clients = self.compositor.clients()?;
        let (workspace, window) = destination_for(presence, &self.proc_root, &row.id, &clients)
            .map_err(|e| e.to_string())?;
        Ok(crate::Destination { workspace, root, window: window.address })
    }
}

/// Where a visible worker spawned from `imperator_pid` belongs, with the
/// window the answer came from — or the refusal that says why it cannot be
/// answered.
pub fn plan_placement(
    proc_root: &Path,
    imperator_pid: u32,
    clients: &[Client],
) -> Result<(i64, Client), PlacementRefusal> {
    let window = window_hosting(proc_root, imperator_pid, clients)
        .map_err(|_| PlacementRefusal::NoWindow)?
        .ok_or(PlacementRefusal::NoWindow)?;
    let destination = destination(window.workspace_id, &window.workspace_name)?;
    Ok((destination, window))
}

/// **Does the claimed pid belong to this launch?** Answered by the launch's
/// own shape rather than by whichever check happens to pass.
///
/// Headless: strict equality, because the child we spawned *is* the harness —
/// a grandchild there is a fault, not a terminal. Terminal: equality still
/// holds when a terminal execs the harness directly, and otherwise the claimed
/// pid must descend from the process we spawned, bounded and refusing when
/// `/proc` cannot be read. An unrelated pid is false under both, never a
/// refusal that a caller could mistake for success.
pub fn identity_holds(
    proc_root: &Path,
    shape: crate::managed_start::LaunchShape,
    spawned: u32,
    claimed: u64,
) -> Result<bool, AncestryRefusal> {
    use crate::managed_start::LaunchShape;
    if claimed == 0 || claimed > u64::from(u32::MAX) {
        return Ok(false);
    }
    let claimed = claimed as u32;
    match shape {
        LaunchShape::Headless => Ok(claimed == spawned),
        LaunchShape::Terminal if claimed == spawned => Ok(true),
        LaunchShape::Terminal => descends_from(proc_root, claimed, spawned),
    }
}

/// The fake `/proc` these tests read: `<root>/<pid>/stat` with a real stat
/// line, command name included.
#[cfg(test)]
fn write_stat(root: &Path, pid: u32, comm: &str, ppid: u32) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stat"),
        format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560\n"),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory that removes itself, so this module's tests add
    /// no dependency to a crate the publication owner re-locks.
    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn proc_tree() -> (TempRoot, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ancestry-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // foot 400 -> a shell 410 -> the harness 420
        write_stat(&root, 400, "foot", 1);
        write_stat(&root, 410, "nu", 400);
        write_stat(&root, 420, "eidolon", 410);
        // A live process that is nobody's ancestor here: real, present, unrelated.
        write_stat(&root, 800, "stranger", 1);
        (TempRoot(root.clone()), root)
    }

    #[test]
    fn the_parent_parse_survives_a_command_name_with_parens() {
        let (_d, root) = proc_tree();
        write_stat(&root, 430, "weird (name) here", 420);
        assert_eq!(parent_of(&root, 430).unwrap(), 420);
        assert_eq!(parent_of(&root, 420).unwrap(), 410);
    }

    #[test]
    fn a_grandchild_descends_and_a_sibling_does_not() {
        let (_d, root) = proc_tree();
        assert!(descends_from(&root, 420, 400).unwrap(), "two links up");
        assert!(descends_from(&root, 420, 410).unwrap(), "one link up");
        assert!(!descends_from(&root, 400, 420).unwrap(), "never downwards");
        assert!(!descends_from(&root, 420, 420).unwrap(), "not its own ancestor");
        assert!(
            !descends_from(&root, 420, 999).unwrap(),
            "a stranger is not in the chain"
        );
    }

    /// A dead process, or one this cannot read, is a refusal and not a "no".
    #[test]
    fn an_unreadable_link_is_a_refusal() {
        let (_d, root) = proc_tree();
        assert_eq!(
            descends_from(&root, 999, 400),
            Err(AncestryRefusal::Unreadable { pid: 999 })
        );
    }

    fn clients() -> Vec<Client> {
        let json = serde_json::json!([
            {"address": "0xaa", "pid": 400, "class": "foot", "title": "~> ed",
             "workspace": {"id": 10, "name": "10"}},
            {"address": "0xbb", "pid": 900, "class": "vesktop", "title": "chat",
             "workspace": {"id": 12, "name": "12"}},
            {"address": "0xcc", "pid": 901, "class": "foot", "title": "worker",
             "workspace": {"id": 11, "name": "11"}},
        ]);
        clients_from(&json)
    }

    /// The window that hosts the harness is its terminal, found by walking up —
    /// and the *nearest* one wins, not the first row in the list.
    #[test]
    fn the_window_is_found_by_walking_the_chain_not_by_class() {
        let (_d, root) = proc_tree();
        let found = window_hosting(&root, 420, &clients()).unwrap().expect("foot hosts it");
        assert_eq!(found.address, "0xaa");
        assert_eq!(found.pid, 400);
        // A pid with no client anywhere in its chain has no window.
        write_stat(&root, 500, "orphan", 1);
        assert!(window_hosting(&root, 500, &clients()).unwrap().is_none());
    }

    /// **The settled convention, and its refusals.** Workspace + 1, with the
    /// three cases that have no arithmetic answer refusing rather than
    /// guessing.
    #[test]
    fn the_destination_is_the_imperators_workspace_plus_one() {
        assert_eq!(destination(10, "10").unwrap(), 11);
        assert_eq!(destination(12, "12").unwrap(), 13);
        assert_eq!(
            destination(-99, "special:void"),
            Err(PlacementRefusal::Special { name: "special:void".into() })
        );
        assert_eq!(
            destination(0, "scratch"),
            Err(PlacementRefusal::NotNumeric { name: "scratch".into() })
        );
    }

    /// **Both shapes, and the strictness that separates them.** The headless
    /// check must reject a grandchild — that is what makes it worth having —
    /// while the terminal check must accept one, because that is what a
    /// visible launch is.
    #[test]
    fn identity_follows_the_launch_shape() {
        use crate::managed_start::LaunchShape;
        let (_d, root) = proc_tree();
        // 400 spawned 420 through 410.
        assert!(identity_holds(&root, LaunchShape::Headless, 400, 400).unwrap());
        assert!(
            !identity_holds(&root, LaunchShape::Headless, 400, 420).unwrap(),
            "a headless grandchild is a fault, not a terminal hop"
        );
        assert!(identity_holds(&root, LaunchShape::Terminal, 400, 420).unwrap());
        assert!(identity_holds(&root, LaunchShape::Terminal, 400, 400).unwrap());
        assert!(
            !identity_holds(&root, LaunchShape::Terminal, 400, 800).unwrap(),
            "a live but unrelated pid belongs to nobody here"
        );
        assert!(!identity_holds(&root, LaunchShape::Terminal, 400, 0).unwrap());
        // Unreadable ancestry is a refusal under the terminal shape, not a no.
        assert!(matches!(
            identity_holds(&root, LaunchShape::Terminal, 400, 999),
            Err(AncestryRefusal::Unreadable { .. })
        ));
    }

    #[test]
    fn placement_is_planned_from_the_imperators_window_or_refused() {
        let (_d, root) = proc_tree();
        let (dest, window) = plan_placement(&root, 420, &clients()).unwrap();
        assert_eq!(dest, 11, "the worker belongs on 10 + 1");
        assert_eq!(window.address, "0xaa");
        // No window in the chain: refused, never defaulted to "some workspace".
        write_stat(&root, 600, "headless", 1);
        assert_eq!(plan_placement(&root, 600, &clients()), Err(PlacementRefusal::NoWindow));
    }
}

// ── placing the window, and confirming it ──────────────────────────────────

/// Where the window actually ended up, and what that answer rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The workspace the compositor reports the window on **after** the
    /// dispatch — read back, never assumed.
    pub workspace_id: i64,
    /// The window as it reads afterwards, so a caller can log what it moved
    /// rather than what it asked to move.
    pub client: Client,
}

/// Why a placement could not be confirmed.
///
/// Neither variant is "it worked". A dispatch the compositor accepted is not a
/// window on the destination, and a window that cannot be found afterwards is
/// not a window that never moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementError {
    /// The compositor refused the request, in its own words.
    Dispatch { why: String },
    /// The window is not in `clients -j` after the dispatch: it closed, or the
    /// read failed.
    Missing { address: String },
    /// The window is there and is **not** where the dispatch asked for.
    Unconfirmed {
        /// The workspace asked for.
        wanted: i64,
        /// The workspace the compositor reports.
        found: i64,
    },
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlacementError::Dispatch { why } => write!(f, "the compositor refused the placement: {why}"),
            PlacementError::Missing { address } => write!(
                f,
                "window {address} is not listed after the placement: it closed, or the read failed"
            ),
            PlacementError::Unconfirmed { wanted, found } => write!(
                f,
                "the window is on workspace {found}, not {wanted}; reporting the actual workspace rather than the intended one"
            ),
        }
    }
}

/// **Move a window, then find out whether it moved.**
///
/// The order is the contract: one dispatch, one bounded re-read, and a verdict
/// that can be `Unconfirmed` — because the alternative, reporting the
/// destination that was *asked for*, is the class of claim this whole feature
/// exists to avoid. `clients` is a closure so the caller owns the wait and the
/// bound; `dispatch` does exactly one thing and returns the compositor's own
/// refusal if it has one.
pub fn place_window(
    address: &str,
    destination: i64,
    dispatch: impl FnOnce(&str, i64) -> Result<(), String>,
    clients: impl FnOnce() -> Result<Vec<Client>, String>,
) -> Result<Placed, PlacementError> {
    dispatch(address, destination).map_err(|why| PlacementError::Dispatch { why })?;
    let found = clients()
        .map_err(|why| PlacementError::Dispatch { why })?
        .into_iter()
        .find(|c| c.address == address)
        .ok_or_else(|| PlacementError::Missing { address: address.to_string() })?;
    if found.workspace_id != destination {
        return Err(PlacementError::Unconfirmed { wanted: destination, found: found.workspace_id });
    }
    Ok(Placed { workspace_id: found.workspace_id, client: found })
}

/// **The precondition a visible launch must satisfy before it may spawn.**
///
/// A visible child must not take the operator's keyboard. The mechanism is the
/// operator's own config — a persistent window rule matching the launcher's
/// class namespace — and nothing a launch can *do*; there is therefore exactly
/// one **production** implementation, [`AttestedNoFocus`] (`DeclaredNoFocus`
/// beside the guard's own tests is a stand-in that establishes nothing), and
/// what it establishes is an
/// **attestation**: that a person watched the rule hold in the compositor
/// instance this launch is about to spawn into. A host that cannot offer one
/// cannot start a visible child at all, and [`observed_no_focus`] is the only
/// thing that supplies it.
///
/// That is the difference this seam exists to enforce: a launch may not spawn a
/// window and then disclaim the guarantee in a comment. The check runs **before
/// the process exists**, and its absence is a refusal, not a caveat.
pub trait NoFocus: Send + Sync {
    /// Establish a no-focus map for a window with this class, before it maps.
    /// The returned value releases it when dropped, on every path.
    fn establish(&self, app_id: &str) -> Result<Box<dyn NoFocusGuard>, String>;
}

/// A no-focus map that has been established. Dropping it releases the map, so
/// a launch that fails after establishing one does not leave it behind.
pub trait NoFocusGuard {}

/// **The one no-focus capability this tree can honestly offer: an attestation,
/// not a mechanism.**
///
/// The operator's compositor config carries a persistent rule
/// (`coordinator-managed-worker-no-focus`) matching `^(eidolon-worker-[0-9a-f]+)$`
/// with `no_initial_focus = true` and no monitor or workspace property — the
/// namespace [`crate::launch::APP_ID_PREFIX`] builds, and it is deliberately the
/// only class the rule can match. Nothing is added, removed or dispatched at
/// launch: the rule is in the config the compositor loads, when such a window
/// closes the rule matches nothing, and there is no cleanup call that could
/// overreach. That is the shape the compositor-mutation incident receipt
/// requires, and it is why this type performs no compositor call at all.
///
/// **What it cannot do is prove the rule is in force.** No IPC lists window
/// rules (`hyprctl --help`'s command list has none), and the design forbids
/// inferring effectiveness from a file: reading `hyprland.lua` shows what would
/// be loaded, not what this instance loaded, nor that Hyprland applied it. So
/// the only honest thing left is a person's observation, carried here as an
/// attestation: **`<instance>@<unix seconds>`** — the compositor instance someone
/// watched a managed worker map in without taking the keyboard, **and when**.
///
/// **What the bound is, exactly.** Both environment values are read **once, at
/// process start**, and a process's environment does not change when the
/// compositor is replaced. So a compositor restarted while this service runs is
/// invisible to it: the comparison is a construction-time check, not a live
/// watch, and restarting the service is what re-reads it. Two checks give the
/// value an expiry rather than a mere equality: the stamp must not predate this
/// process's own start, and it must not predate the compositor instance it names.
///
/// The guarantee is procedural, and saying so is the point: **an attestation is
/// an operator grant, not a proof.** Nothing here establishes that a window will
/// not take focus, and the code cannot tell an observation recorded after a run
/// from a decision made before one — it records whose claim that is, and when.
#[derive(Debug)]
pub struct AttestedNoFocus {
    /// `HYPRLAND_INSTANCE_SIGNATURE` at the moment the observation was recorded.
    attested_instance: String,
    /// `HYPRLAND_INSTANCE_SIGNATURE` in the environment this process started with.
    live_instance: String,
    /// When the observation was recorded, unix seconds — the part that makes this
    /// a record of an *observation* rather than of an instance name.
    observed_at: u64,
    /// When **this process** started, unix seconds: an attestation stamped before
    /// it is refused, so what a launch inherits cannot be older than the process
    /// serving it.
    service_started: u64,
}

impl AttestedNoFocus {
    /// The value this attestation came from, normalised to
    /// `<instance>@<observed_at>` — what a launch's answer carries, rather than the
    /// raw environment bytes it was spelled with.
    pub fn attested_value(&self) -> String {
        format!("{}@{}", self.attested_instance, self.observed_at)
    }
}

impl NoFocus for AttestedNoFocus {
    fn establish(&self, app_id: &str) -> Result<Box<dyn NoFocusGuard>, String> {
        if self.live_instance != self.attested_instance {
            return Err(format!(
                "the no-focus rule was observed in compositor instance `{}`, but this process was \
                 started against `{}`: an observation does not carry over a compositor restart",
                self.attested_instance, self.live_instance
            ));
        }
        if self.observed_at.saturating_add(ATTESTATION_GRACE_S) < self.service_started {
            return Err(format!(
                "the attestation is stamped {} but this process started at {}: an attestation older \
                 than the process serving it is a standing claim, and a standing claim is what this \
                 value exists to replace",
                self.observed_at, self.service_started
            ));
        }
        match instance_start(&self.attested_instance) {
            Some(began) if self.observed_at >= began => {}
            Some(began) => {
                return Err(format!(
                    "the attestation is stamped {} but compositor instance `{}` began at {began}: an \
                     observation cannot be older than the compositor it observed",
                    self.observed_at, self.attested_instance
                ))
            }
            None => {
                return Err(format!(
                    "compositor instance `{}` carries no parseable start time, so the attestation's \
                     timestamp cannot be placed against it",
                    self.attested_instance
                ))
            }
        }
        if !crate::launch::is_managed_worker_class(app_id) {
            return Err(format!(
                "`{app_id}` is outside the `{}<launch id>` class namespace the operator's \
                 no-focus rule matches, so the rule would not apply to this window",
                crate::launch::APP_ID_PREFIX
            ));
        }
        Ok(Box::new(PersistentRuleGuard))
    }
}

/// The rule lives in the compositor's configuration and a launch neither adds
/// nor removes it, so releasing this guard is deliberately nothing. It exists so
/// that "established" and "released" stay one type on every path out of a
/// launch, and so a future launch-scoped primitive can be dropped in without
/// changing a call site.
pub struct PersistentRuleGuard;

impl NoFocusGuard for PersistentRuleGuard {}

/// **The attestation's own start time**, out of the instance signature.
///
/// Hyprland's signature is `<commit>_<unix-seconds>_<monotonic>`; the middle field
/// is when that instance began. Measured on this host, 2026-10-26: instance
/// `…_1793036127_572132252` against `ps -o lstart` for its pid, 13:35:26, one
/// second apart. A signature without a parseable middle field is `None`, which
/// every caller turns into a refusal: a stamp means nothing if it cannot be placed
/// against the instance it is about.
/// **How late a stamp may legitimately be, in seconds.**
///
/// The value is read once at start and must not predate that start, so a stamp
/// composed by the launching shell (`@$(date +%s)`) would otherwise fail by exec
/// latency — punishing the honest procedure and rewarding a stamp written into the
/// future. A few seconds of tolerance admits the honest case; a leftover stamp from
/// a previous run is minutes or hours old and is still refused.
pub const ATTESTATION_GRACE_S: u64 = 3;

pub fn instance_start(signature: &str) -> Option<u64> {
    signature.split('_').nth(1)?.parse().ok()
}

/// **The attestation, only when it is current and dated** — `None` when nothing was
/// recorded, and a refusal when what was recorded cannot be read as an observation.
///
/// The value is `<instance>@<unix-seconds>`. Two equal strings were never an
/// observation — any invented instance satisfies that — so the stamp is parsed and
/// placed against two things: **this process's own start** (an older stamp is a
/// standing claim, which is exactly what the value replaces) and the compositor
/// instance's start (an observation cannot predate the compositor it observed). A
/// value that is missing, malformed, about another instance, or too old is refused
/// with a reason rather than quietly becoming `None`.
///
/// **Operational consequence, stated because it is easy to miss.** The value is
/// read once at start, and it must not be stamped before that start. So the only
/// stamp that passes is one produced **as this process is launched** — the same
/// command that starts the service has to compose it, and a value written after the
/// service is up is refused by the very check that stops it being standing.
pub fn observed_no_focus(
    attested: Option<&str>,
    live: Option<&str>,
    service_started: u64,
    now: u64,
) -> Result<Option<AttestedNoFocus>, String> {
    let value = match attested {
        None => return Ok(None),
        Some("") => return Ok(None),
        Some(value) => value,
    };
    let (instance, seconds) = value.split_once('@').ok_or_else(|| {
        format!(
            "the attestation `{value}` carries no `@`: it must be `<compositor instance>@<unix \
             seconds>`, because a record of which instance is not a record of an observation"
        )
    })?;
    let observed_at: u64 = seconds.parse().map_err(|_| {
        format!("the attestation `{value}` does not carry a unix-seconds timestamp after `@`")
    })?;
    let live = live.ok_or_else(|| {
        "this process has no HYPRLAND_INSTANCE_SIGNATURE, so there is no instance an attestation \
         could be about"
            .to_string()
    })?;
    if instance != live {
        return Err(format!(
            "the attestation names compositor instance `{instance}`, but this process was started \
             against `{live}`: an observation is about one instance and does not carry over a restart"
        ));
    }
    let began = instance_start(live).ok_or_else(|| {
        format!(
            "compositor instance `{live}` carries no parseable start time, so the attestation cannot \
             be placed against it"
        )
    })?;
    if observed_at < began {
        return Err(format!(
            "the attestation is stamped {observed_at} but instance `{live}` began at {began}: an \
             observation cannot be older than the compositor it observed"
        ));
    }
    if observed_at.saturating_add(ATTESTATION_GRACE_S) < service_started {
        return Err(format!(
            "the attestation is stamped {observed_at} but this process started at {service_started}: an \
             attestation more than {ATTESTATION_GRACE_S}s older than the process serving it is a \
             standing claim (a few seconds of slack is allowed for a stamp composed as the service \
             is launched), and a standing claim is \
             what this value exists to replace. It has to be composed as the service is launched"
        ));
    }
    if observed_at > now.saturating_add(60) {
        return Err(format!(
            "the attestation is stamped {observed_at}, later than this process's clock ({now}): a stamp \
             from the future is not a record of an observation"
        ));
    }
    Ok(Some(AttestedNoFocus {
        attested_instance: instance.to_string(),
        live_instance: live.to_string(),
        observed_at,
        service_started,
    }))
}

/// **The compositor, behind a seam**, so a launch can be tested without one
/// and so the one implementation that talks to a live compositor is the only
/// part that has to be validated live.
pub trait Placement: Send + Sync {
    /// Find the window hosting `pid` and place it on `destination`, returning
    /// what the compositor reports afterwards — never what was asked for.
    fn place(&self, pid: u32, destination: i64) -> Result<Placed, String>;
}

/// The real compositor. **Unexercised**: live validation of the compositor
/// path is blocked, so this type is written to be small and to fail loudly
/// rather than to be trusted.
pub struct Hyprland;

impl Placement for Hyprland {
    fn place(&self, pid: u32, destination: i64) -> Result<Placed, String> {
        let clients = parse_clients(&hyprctl(&["-j", "clients"])?)?;
        let window = window_hosting(std::path::Path::new("/proc"), pid, &clients)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no window hosts pid {pid}: the terminal may not have mapped yet"))?;
        place_with_compositor(&window.address, destination, 40, std::time::Duration::from_millis(50))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    fn client(address: &str, ws: i64) -> Client {
        Client {
            address: address.into(),
            pid: 400,
            workspace_id: ws,
            workspace_name: ws.to_string(),
            class: "foot".into(),
            title: "worker".into(),
        }
    }

    /// The happy path, and that the answer is the re-read rather than the
    /// request.
    #[test]
    fn a_confirmed_move_reports_the_workspace_it_read_back() {
        let placed = place_window(
            "0xaa",
            11,
            |address, dest| {
                assert_eq!((address, dest), ("0xaa", 11));
                Ok(())
            },
            || Ok(vec![client("0xaa", 11)]),
        )
        .unwrap();
        assert_eq!(placed.workspace_id, 11);
        assert_eq!(placed.client.address, "0xaa");
    }

    /// **A dispatch the compositor accepted is not a window on the
    /// destination.** The refusal carries both numbers, so a caller reports
    /// the truth instead of the intention.
    #[test]
    fn a_move_that_did_not_take_is_unconfirmed_not_success() {
        let refused = place_window("0xaa", 11, |_, _| Ok(()), || Ok(vec![client("0xaa", 12)]))
            .unwrap_err();
        assert_eq!(refused, PlacementError::Unconfirmed { wanted: 11, found: 12 });
        assert!(refused.to_string().contains("not 11"), "{refused}");
    }

    #[test]
    fn a_refused_dispatch_or_a_missing_window_is_an_error() {
        assert_eq!(
            place_window("0xaa", 11, |_, _| Err("no such workspace".into()), || Ok(vec![])),
            Err(PlacementError::Dispatch { why: "no such workspace".into() })
        );
        assert_eq!(
            place_window("0xaa", 11, |_, _| Ok(()), || Ok(vec![client("0xbb", 11)])),
            Err(PlacementError::Missing { address: "0xaa".into() })
        );
    }
}

// ── the compositor itself: argv and parsing, separated from the running ────

/// The dispatch a placement performs, as an argument vector.
///
/// Kept pure and tested because the *spelling* is the part that goes wrong
/// quietly: `movetoworkspacesilent` takes `<workspace>,address:<addr>` as one
/// argument, and it does not switch the operator's **workspace**.
///
/// **It says nothing about focus.** A silently moved window can still take the
/// keyboard when it maps, and this host's focus behaviour is not established —
/// see the compositor-mutation incident note. Placement answers "which
/// workspace", never "did the operator keep the keyboard".
pub fn move_args(address: &str, workspace: i64) -> Vec<String> {
    vec![
        "dispatch".into(),
        "movetoworkspacesilent".into(),
        format!("{workspace},address:{address}"),
    ]
}

/// Read `hyprctl clients -j` output into clients, refusing output that is not
/// a JSON list rather than treating it as "no windows" — an unreadable
/// compositor is not an empty desktop.
pub fn parse_clients(stdout: &str) -> Result<Vec<Client>, String> {
    let json: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("hyprctl clients did not answer JSON: {e}"))?;
    if !json.is_array() {
        return Err("hyprctl clients did not answer a list".into());
    }
    Ok(clients_from(&json))
}

/// Run `hyprctl` once with these arguments and return stdout. Thin on purpose:
/// everything that can be decided without a compositor is decided in
/// [`place_window`], and this is the part that cannot be.
fn hyprctl(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| format!("running hyprctl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// **Place a visible worker's window, with a compositor underneath.**
///
/// The wait exists because the window does not exist when the terminal is
/// spawned: it is polled for, bounded, and then placed and re-read. A window
/// that never appears is [`PlacementError::Missing`]; a window that appears and
/// does not move is [`PlacementError::Unconfirmed`] with both workspaces named.
///
/// **Unexercised against a live compositor.** The argv, the parsing and the
/// verdict are tested; the two `hyprctl` calls are not, and this function must
/// not be described as verified until a bounded live run says so.
pub fn place_with_compositor(
    address: &str,
    destination: i64,
    tries: u32,
    pause: std::time::Duration,
) -> Result<Placed, PlacementError> {
    let read = || hyprctl(&["-j", "clients"]).and_then(|out| parse_clients(&out));
    for _ in 0..tries {
        let listed = read().map_err(|why| PlacementError::Dispatch { why })?;
        if listed.iter().any(|c| c.address == address) {
            return place_window(
                address,
                destination,
                |address, workspace| {
                    let args = move_args(address, workspace);
                    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
                    hyprctl(&borrowed).map(|_| ())
                },
                read,
            );
        }
        std::thread::sleep(pause);
    }
    Err(PlacementError::Missing { address: address.to_string() })
}

#[cfg(test)]
mod compositor_tests {
    use super::*;

    /// The dispatch spelling, which is the part that fails quietly.
    #[test]
    fn the_move_is_one_argument_naming_workspace_and_address() {
        assert_eq!(
            move_args("0x57ea", 11),
            vec!["dispatch", "movetoworkspacesilent", "11,address:0x57ea"]
        );
    }

    /// Real `hyprctl clients -j` shape, reduced to what placement needs — and
    /// a row it cannot describe is dropped rather than defaulted.
    #[test]
    fn clients_parse_and_a_partial_row_is_dropped() {
        let out = r#"[
            {"address":"0xaa","pid":400,"class":"foot","title":"worker",
             "workspace":{"id":11,"name":"11"}},
            {"address":"0xbb","pid":0,"class":"foot","title":"no pid",
             "workspace":{"id":12,"name":"12"}},
            {"pid":401,"class":"foot","title":"no address",
             "workspace":{"id":12,"name":"12"}}
        ]"#;
        let clients = parse_clients(out).unwrap();
        assert_eq!(
            clients.len(),
            2,
            "a window with no pid is still placeable; the row with no address is not"
        );
        assert_eq!(clients[0].address, "0xaa");
        assert_eq!(clients[0].workspace_id, 11);
        assert_eq!(clients[1].pid, 0, "the compositor names no process for it");
        assert!(
            clients.iter().all(|c| !c.address.is_empty()),
            "an address-less row cannot be a placement target"
        );
        // Not JSON, or not a list, is a refusal — not an empty desktop.
        assert!(parse_clients("not json").is_err());
        assert!(parse_clients(r#"{"windows":[]}"#).is_err());
    }
}

#[cfg(test)]
mod no_focus_tests {
    use super::*;

    const INSTANCE: &str = "663b29b846e4bad17cfa29941f6d2fbb6b61f139_1793036127_572132252";
    const CLASS: &str = "eidolon-worker-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    /// The instance began at 1793036127; the stamp is after it.
    const STAMP: u64 = 1793036300;
    const NOW: u64 = 1793036400;

    fn fresh() -> String {
        format!("{INSTANCE}@{STAMP}")
    }

    /// `Box<dyn NoFocusGuard>` is not `Debug`, so `unwrap_err` is unavailable and
    /// the refusal is taken by matching.
    fn refusal(result: Result<Box<dyn NoFocusGuard>, String>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(why) => why,
        }
    }

    /// **The one case that establishes anything**: a dated observation of this
    /// instance, stamped after the process started, for a class the rule matches —
    /// and what it establishes is a guard that releases nothing.
    #[test]
    fn a_dated_attestation_for_this_instance_covers_our_own_class_only() {
        let capability = observed_no_focus(Some(&fresh()), Some(INSTANCE), 0, NOW)
            .expect("readable")
            .expect("attested");
        assert!(capability.establish(CLASS).is_ok());
        for class in [
            "Alacritty",
            "foot",
            "eidolon-worker-",
            "eidolon-worker-XYZ",
            "eidolon-worker-0f1e2d3c4b5a69788796a5b4c3d2e1f0 ",
            "eidolon-worker-0f1e2d3c4b5a69788796a5b4c3d2e1f_",
        ] {
            let refused = refusal(capability.establish(class));
            assert!(refused.contains("namespace"), "{class}: {refused}");
        }
    }

    /// **A record of which instance is not a record of an observation.** The value
    /// must carry a stamp, and the stamp is placed against the instance it names.
    #[test]
    fn the_value_must_carry_a_stamp_and_the_stamp_must_fit_the_instance() {
        // A bare instance — the form an earlier ruling asked the operator to set.
        let refused = observed_no_focus(Some(INSTANCE), Some(INSTANCE), 0, NOW).unwrap_err();
        assert!(refused.contains("no `@`"), "{refused}");
        // A stamp before the instance began cannot be an observation of it.
        let refused = observed_no_focus(
            Some(&format!("{INSTANCE}@1793036000")),
            Some(INSTANCE),
            0,
            NOW,
        )
        .unwrap_err();
        assert!(refused.contains("older than the compositor"), "{refused}");
        // A stamp from the future is not a record of anything.
        let refused = observed_no_focus(Some(&format!("{INSTANCE}@1793099999")), Some(INSTANCE), 0, NOW)
            .unwrap_err();
        assert!(refused.contains("from the future"), "{refused}");
        // No stamp to parse at all.
        let refused = observed_no_focus(Some(&format!("{INSTANCE}@later")), Some(INSTANCE), 0, NOW)
            .unwrap_err();
        assert!(refused.contains("unix-seconds timestamp"), "{refused}");
    }

    /// **The service-start bound**: an attestation older than the process serving it
    /// is a standing claim, and is refused both where the value is read and again by
    /// `establish`, which cannot be talked out of it.
    #[test]
    fn a_stamp_older_than_this_process_is_refused() {
        // More than the grace window late: refused.
        let refused = observed_no_focus(Some(&fresh()), Some(INSTANCE), STAMP + 10, NOW).unwrap_err();
        assert!(refused.contains("standing claim"), "{refused}");
        // And the same refusal from the guard itself, on a hand-built value.
        let hand_built = AttestedNoFocus {
            attested_instance: INSTANCE.into(),
            live_instance: INSTANCE.into(),
            observed_at: STAMP,
            service_started: STAMP + 10,
        };
        let refused = refusal(hand_built.establish(CLASS));
        assert!(refused.contains("standing claim"), "{refused}");
    }

    /// **The honest procedure passes.** A stamp composed by the launching shell is a
    /// second or two older than the process it was handed to, by exec latency; that is
    /// timing, not a standing claim.
    #[test]
    fn a_stamp_a_couple_of_seconds_older_than_this_process_is_accepted() {
        let capability = observed_no_focus(Some(&fresh()), Some(INSTANCE), STAMP + 2, NOW)
            .expect("readable");
        assert!(capability.is_some(), "exec latency must not read as a standing claim");
    }

    /// An observation is about one instance: another instance's stamp does not
    /// carry over a compositor restart.
    #[test]
    fn an_observation_does_not_carry_over_a_compositor_restart() {
        let other = "663b29b8aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_1793000000_1";
        let refused = observed_no_focus(Some(&format!("{other}@1793036300")), Some(INSTANCE), 0, NOW)
            .unwrap_err();
        assert!(refused.contains("names compositor instance"), "{refused}");
        let hand_built = AttestedNoFocus {
            attested_instance: other.into(),
            live_instance: INSTANCE.into(),
            observed_at: STAMP,
            service_started: 0,
        };
        let refused = refusal(hand_built.establish(CLASS));
        assert!(refused.contains("does not carry over"), "{refused}");
    }

    /// Nothing set is nothing claimed; anything set is a refusal with a reason,
    /// never a quiet `None`.
    #[test]
    fn without_a_value_nothing_is_supplied() {
        assert!(observed_no_focus(None, Some(INSTANCE), 0, NOW).unwrap().is_none());
        assert!(observed_no_focus(Some(""), Some(INSTANCE), 0, NOW).unwrap().is_none());
        assert!(observed_no_focus(Some(&fresh()), None, 0, NOW)
            .unwrap_err()
            .contains("no HYPRLAND_INSTANCE_SIGNATURE"));
    }

    /// Releasing the guard is inert by design — the rule is the operator's, not the
    /// launch's, so there is no compositor call to make and none that could overreach.
    #[test]
    fn releasing_the_guard_dispatches_nothing() {
        let capability = observed_no_focus(Some(&fresh()), Some(INSTANCE), 0, NOW)
            .expect("readable")
            .expect("attested");
        let guard = capability.establish(CLASS).expect("in namespace");
        drop(guard);
    }

    /// The signature's middle field is the instance's start, measured against this
    /// host's own compositor on 2026-10-26 (`_1793036127_` and `ps -o lstart` giving
    /// 13:35:26).
    #[test]
    fn the_instance_signature_carries_its_start_time() {
        assert_eq!(instance_start(INSTANCE), Some(1793036127));
        assert_eq!(instance_start("no-fields-here"), None);
        assert_eq!(instance_start("a_notanumber_c"), None);
    }
}

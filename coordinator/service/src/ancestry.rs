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
/// A visible child must not take the operator's keyboard. Establishing that is
/// a compositor capability this tree does **not** have: no mechanism has been
/// validated (see the compositor-mutation incident note and the design note's
/// unresolved no-focus section), so there is deliberately **no implementation
/// of this trait anywhere**, and a host that cannot offer one cannot start a
/// visible child at all.
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

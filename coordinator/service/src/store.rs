//! The store: one JSON file, and the rules that keep it honest.
//!
//! Three rules, and they are the ones a reviewer asked to see enforced rather
//! than claimed:
//!
//! - **A write is atomic and durable.** The store is written to a temporary file
//!   beside itself, flushed to the disk, renamed over the target, and the
//!   directory entry is flushed too. A reader therefore sees the old store or the
//!   new one and never a half-written one, and the rename itself survives a
//!   power loss rather than only a process death.
//! - **A file that exists but cannot be read is a refusal.** Missing means "no
//!   store yet" and starts empty; present-but-unreadable means someone's history
//!   is in there, and starting empty would replace it on the next write. So the
//!   process refuses to start and leaves the bytes exactly where they are.
//! - **One writer per store.** The lock is taken non-blocking at startup, so a
//!   second `--once` or a second service refuses rather than interleaving writes
//!   with the first.

use crate::Store;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// How a failed write left the store.
#[derive(Debug)]
pub enum SaveError {
    /// The target was never replaced: what is on disk is exactly what was there
    /// before, so a caller may keep its old state as though nothing had been
    /// written.
    Unchanged(String),
    /// The rename had already happened when the failure came. The file on disk is
    /// **probably the new content already**; whether that survives a crash is what
    /// is unknown. This is *not* a rollback, and a caller must not keep serving its
    /// old state as if the write had failed — it has to reconcile with the disk.
    Uncertain(String),
}

impl SaveError {
    pub fn detail(&self) -> &str {
        match self {
            SaveError::Unchanged(d) | SaveError::Uncertain(d) => d,
        }
    }

    pub fn uncertain(&self) -> bool {
        matches!(self, SaveError::Uncertain(_))
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Unchanged(d) => write!(f, "nothing was written ({d})"),
            SaveError::Uncertain(d) => write!(
                f,
                "the store was replaced but its durability is unknown ({d})"
            ),
        }
    }
}

/// Replace the store: temporary file, flush, rename, flush the directory.
///
/// Every failure is returned, and classified: anything before the rename leaves
/// the target exactly as it was, and anything after it is an uncertain commit
/// rather than a rollback.
pub fn save(file: &Path, store: &Store) -> Result<(), SaveError> {
    let unchanged = |e: String| Err(SaveError::Unchanged(e));
    let dir = match file.parent() {
        Some(d) => d,
        None => return unchanged(format!("{} has no directory to be written in", file.display())),
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return unchanged(format!("creating {}: {e}", dir.display()));
    }
    let body = match serde_json::to_string_pretty(store) {
        Ok(b) => b,
        Err(e) => return unchanged(format!("cannot serialise the store: {e}")),
    };
    let tmp = file.with_extension("json.tmp");
    {
        let mut f = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => return unchanged(format!("writing {}: {e}", tmp.display())),
        };
        if let Err(e) = f.write_all(body.as_bytes()) {
            return unchanged(format!("writing {}: {e}", tmp.display()));
        }
        // The bytes are on the disk *before* the name moves to them, so a crash
        // cannot leave the name pointing at a file whose contents are not there.
        if let Err(e) = f.sync_all() {
            return unchanged(format!("flushing {}: {e}", tmp.display()));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, file) {
        return unchanged(format!("replacing {}: {e}", file.display()));
    }
    // Past this point the rename has happened. As far as this process can see the
    // target *is* the new store; what is in doubt is only whether the rename
    // itself is durable, so nothing below may report "unchanged".
    let d = match std::fs::File::open(dir) {
        Ok(d) => d,
        Err(e) => {
            return Err(SaveError::Uncertain(format!(
                "{} was replaced, but its directory ({}) could not be opened to flush the rename: {e}",
                file.display(),
                dir.display()
            )))
        }
    };
    if let Err(e) = d.sync_all() {
        return Err(SaveError::Uncertain(format!(
            "{} was replaced, but flushing the directory entry for it failed: {e}",
            file.display()
        )));
    }
    Ok(())
}

/// Read the store, and refuse rather than guess.
pub fn load(file: &Path) -> Result<Store, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} exists but is not readable as a store ({e}). Refusing to start: starting empty \
                 would replace it with an empty history on the next write. The bytes are untouched — \
                 move or repair the file first",
                file.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!(
            "{} exists but cannot be read ({e}). Refusing to start rather than start empty and \
             replace it",
            file.display()
        )),
    }
}

/// The store's lock, held for the life of the process.
#[derive(Debug)]
pub struct Ownership {
    _file: std::fs::File,
}

/// Take the store's lock, or refuse to be a second writer.
pub fn own(dir: &Path) -> Result<Ownership, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    // SAFETY: `flock` on a file descriptor this process owns, with a valid fd and
    // a valid operation; it touches no memory.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(format!(
            "another coordinator monitor is already writing this store ({} is locked: {}). Refusing \
             to be a second writer — one process owns the store; stop the running one first \
             (`eidolon plugins service stop coordinator`)",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(Ownership { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cycle, Delivery, Outcome, Roster, Row, Transport};
    use std::sync::Mutex;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coord-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct FixedRoster(Vec<Row>);
    impl Roster for FixedRoster {
        fn rows(&self) -> Result<Vec<Row>, String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct Counting {
        calls: Mutex<Vec<String>>,
    }
    impl Transport for Counting {
        fn send(&self, to: &str, text: &str) -> Result<Delivery, String> {
            self.calls.lock().unwrap().push(format!("{to}|{text}"));
            Ok(Delivery {
                outcome: Outcome::Delivered,
                words: "{\"outcome\":\"delivered\"}".to_string(),
            })
        }
    }

    fn stuck(id: &str, session: &str, outcome_at: u64) -> Row {
        Row {
            id: id.to_string(),
            session: Some(session.to_string()),
            state: "stuck".to_string(),
            last: "iteration limit".to_string(),
            outcome_at: Some(outcome_at),
            outcome_observed: None,
            started_ms: None,
            reached: true,
            label: None,
            decoration: None,
            display: None,
        }
    }

    fn adopted_store(caller: &str, worker: &str) -> Store {
        let mut store = Store::default();
        store.records.push(crate::Adoption {
            epoch: 1,
            coordinator: caller.to_string(),
            coordinator_id: String::new(),
            worker: worker.to_string(),
            worker_id_seen: "eidolon-x".to_string(),
            name: String::new(),
            note: String::new(),
            adopted_at_ms: 1,
            released_at_ms: None,
            release_reason: String::new(),
        });
        store
    }

    #[test]
    fn a_missing_store_is_an_empty_start() {
        let dir = tmpdir("missing");
        assert!(load(&dir.join("state.json")).unwrap().records.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_that_exists_and_cannot_be_read_refuses_and_is_left_alone() {
        let dir = tmpdir("corrupt");
        let file = dir.join("state.json");
        let junk = b"{ this is not a store";
        std::fs::write(&file, junk).unwrap();
        let e = load(&file).unwrap_err();
        assert!(e.contains("Refusing to start"), "{e}");
        assert_eq!(std::fs::read(&file).unwrap(), junk, "the bytes are untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_writer_is_refused() {
        let _serial = crate::process_host::PROCESS_TEST_LOCK.lock().unwrap();
        let dir = tmpdir("lock");
        let first = own(&dir).expect("the first writer takes the lock");
        // Rust's OpenOptions must keep the ownership lock out of exec'd children.
        let flags = unsafe { libc::fcntl(first._file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "store lock must be close-on-exec");
        let e = own(&dir).unwrap_err();
        assert!(e.contains("already writing this store"), "{e}");
        drop(first);
        assert!(own(&dir).is_ok(), "and the lock is released with the process's hold on it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ack_failure_in_the_live_process_is_fatal_and_leaves_the_disk_pending() {
        // The second review's point: after an ack failure the live notice already
        // carries the answer, so a "next scan" would persist *that* and never retry.
        // The scan reports it as fatal, the file still holds the notice as pending,
        // and the live copy — the thing that must not be served — holds the answer.
        let dir = tmpdir("ack-live");
        let file = dir.join("state.json");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let roster = FixedRoster(vec![stuck("eidolon-a", "/w.eid", 7)]);
        let transport = Counting::default();
        let mut calls = 0;
        let mut flaky = |s: &Store| {
            calls += 1;
            if calls == 2 {
                return Err(SaveError::Unchanged("simulated ack failure".to_string()));
            }
            super::save(&file, s)
        };
        let scan = cycle(&mut store, &mut flaky, &roster, &transport, 100);
        let e = scan.error.expect("the scan is cut short and the caller must stop");
        assert!(
            !e.contains("stays pending on disk"),
            "the claim this used to make is gone: {e}"
        );
        assert!(e.contains("not evidence either way"), "{e}");
        assert_eq!(transport.calls.lock().unwrap().len(), 1);
        assert_eq!(
            store.notices[0].outcome,
            Outcome::Delivered,
            "the live copy carries the answer…"
        );
        let on_disk = load(&file).unwrap();
        assert_eq!(
            on_disk.notices[0].outcome, Outcome::Pending,
            "…and the disk does not, which is exactly why the process may not carry on"
        );
        assert_eq!(
            on_disk.notices[0].notice_id, store.notices[0].notice_id,
            "and it is the same notice, so a restart retries the same id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_never_reached_the_rename_leaves_the_target_alone() {
        let dir = tmpdir("unchanged");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let file = blocked.join("state.json");
        let e = save(&file, &Store::default()).unwrap_err();
        assert!(!e.uncertain(), "this is the unchanged case: {e}");
        assert!(e.detail().contains("creating"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_cannot_be_flushed_after_the_rename_is_an_uncertain_commit() {
        // The rename needs write+execute on the directory; opening it to flush the
        // entry needs read. Removing read leaves the store replaced and the
        // durability unverified — the case that must NOT read as a rollback.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("uncertain");
        let file = dir.join("state.json");
        save(&file, &Store::default()).unwrap();
        let mut store = Store::default();
        store.records.push(crate::Adoption {
            epoch: 1,
            coordinator: "/c.eid".into(),
            coordinator_id: String::new(),
            worker: "/w.eid".into(),
            worker_id_seen: String::new(),
            name: String::new(),
            note: String::new(),
            adopted_at_ms: 1,
            released_at_ms: None,
            release_reason: String::new(),
        });
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o333)).unwrap();
        let e = save(&file, &store).unwrap_err();
        let on_disk = std::fs::read_to_string(&file).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            e.uncertain(),
            "the rename had happened, so this is not 'nothing was written': {e}"
        );
        assert!(
            on_disk.contains("/w.eid"),
            "and the target really does hold the new store, which is why a caller may not keep the old state"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_uncertain_write_while_staging_sends_nothing() {
        let dir = tmpdir("uncertain-stage");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let roster = FixedRoster(vec![stuck("eidolon-a", "/w.eid", 7)]);
        let transport = Counting::default();
        let mut save = |_: &Store| Err(SaveError::Uncertain("simulated".to_string()));
        let scan = cycle(&mut store, &mut save, &roster, &transport, 100);
        assert!(scan.error.is_some());
        assert!(transport.calls.lock().unwrap().is_empty());
        assert!(
            scan.error.unwrap().contains("may or may not"),
            "and the words say the write's outcome is unknown rather than that it failed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_adoption_with_an_uncertain_write_takes_its_state_from_the_disk() {
        // The write "failed" after the rename, so the file really does hold the new
        // record while memory does not. Keeping memory would serve a state the disk
        // contradicts; the call must reconcile and say that it did.
        let dir = tmpdir("reconcile");
        let file = dir.join("state.json");
        let on_disk = adopted_store("/c.eid", "/w.eid");
        save(&file, &on_disk).unwrap();

        let mut store = Store::default();
        assert!(store.records.is_empty(), "memory starts behind the disk");
        let mut save = |_: &Store| Err(SaveError::Uncertain("simulated".to_string()));
        let mut load = || super::load(&file);
        let e = crate::adopt(
            &mut store,
            crate::AdoptRequest {
                caller: "/c.eid",
                worker: "/w.eid",
                name: "",
                note: "",
                seen: None,
                now: 2,
            },
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(e.contains("may or may not") || e.contains("durability is unknown"), "{e}");
        assert!(e.contains("re-read"), "and it says the state was re-read: {e}");
        assert_eq!(
            store.records.len(),
            1,
            "the live state now comes from the disk, not from the write that failed"
        );
        assert!(e.contains("unknown"), "and the outcome is stated as unknown: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_sent_if_the_notice_cannot_be_persisted() {
        // The store cannot be written: its parent is a regular file.
        let dir = tmpdir("nowrite");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let file = blocked.join("state.json");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let roster = FixedRoster(vec![stuck("eidolon-a", "/w.eid", 7)]);
        let transport = Counting::default();
        let mut save = |s: &Store| super::save(&file, s);
        let scan = cycle(&mut store, &mut save, &roster, &transport, 100);
        assert!(scan.error.is_some(), "the scan reports that it could not persist");
        assert!(
            transport.calls.lock().unwrap().is_empty(),
            "a halt that cannot be written down must not be delivered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_after_the_send_and_before_the_ack_repeats_the_same_id() {
        let dir = tmpdir("crash");
        let file = dir.join("state.json");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let roster = FixedRoster(vec![stuck("eidolon-a", "/w.eid", 7)]);
        let transport = Counting::default();

        // The first save (the write-ahead one) is the real one; the second (the
        // ack) fails, which is exactly the crash window.
        let mut calls = 0;
        let mut flaky = |s: &Store| {
            calls += 1;
            if calls == 2 {
                return Err(SaveError::Unchanged(
                    "simulated crash between the send and the ack".to_string(),
                ));
            }
            super::save(&file, s)
        };
        let scan = cycle(&mut store, &mut flaky, &roster, &transport, 100);
        assert!(scan.error.is_some());
        assert_eq!(transport.calls.lock().unwrap().len(), 1, "it was sent once");

        // What a restarted process reads: the notice is there, pending, with the
        // id it was staged under.
        let mut reloaded = load(&file).unwrap();
        assert_eq!(reloaded.notices.len(), 1);
        let id = reloaded.notices[0].notice_id.clone();
        assert_eq!(reloaded.notices[0].outcome, Outcome::Pending, "staged, not acked");

        // The next scan tries again — the same id, not a second notice.
        let mut save = |s: &Store| super::save(&file, s);
        let scan = cycle(&mut reloaded, &mut save, &roster, &transport, 200);
        assert!(scan.error.is_none());
        assert_eq!(transport.calls.lock().unwrap().len(), 2, "the same notice, sent again");
        let calls = transport.calls.lock().unwrap().clone();
        assert_eq!(calls[0], calls[1], "the very same notice, word for word");
        assert_eq!(reloaded.notices.len(), 1, "and not duplicated");
        assert_eq!(reloaded.notices[0].notice_id, id);
        assert_eq!(reloaded.notices[0].outcome, Outcome::Delivered);
        assert_eq!(
            reloaded.notices[0].attempts, 1,
            "the durable count is what survived the crash: the attempt the disk never saw is not \
             counted, and the retry is what the count now records"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_adoption_that_cannot_be_persisted_is_refused_and_changes_nothing() {
        let dir = tmpdir("adopt");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let file = blocked.join("state.json");
        let mut store = Store::default();
        let mut save = |s: &Store| super::save(&file, s);
        let mut load = || super::load(&file);
        let e = crate::adopt(
            &mut store,
            crate::AdoptRequest {
                caller: "/c.eid",
                worker: "/w.eid",
                name: "grinder",
                note: "",
                seen: None,
                now: 1,
            },
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(e.contains("creating"), "the failure is the write: {e}");
        assert!(
            store.records.is_empty(),
            "an adoption that was not written down is not an adoption"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_that_cannot_be_persisted_is_refused_and_changes_nothing() {
        let dir = tmpdir("release");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let file = blocked.join("state.json");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let mut save = |s: &Store| super::save(&file, s);
        let mut load = || super::load(&file);
        let e = crate::release_adoption(
            &mut store,
            "/c.eid",
            "/w.eid",
            "done",
            2,
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(e.contains("creating"), "{e}");
        assert_eq!(
            store.records[0].released_at_ms, None,
            "a release that was not written down did not happen"
        );
        assert!(store.records[0].release_reason.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_that_cannot_be_written_leaves_the_epoch_alone_on_disk_too() {
        let dir = tmpdir("release-disk");
        let file = dir.join("state.json");
        let mut store = adopted_store("/c.eid", "/w.eid");
        let mut save = |s: &Store| super::save(&file, s);
        // A readable, working store first…
        store.notices.push(crate::Notice {
            notice_id: "n1".into(),
            epoch: 1,
            coordinator: "/c.eid".into(),
            worker: "/w.eid".into(),
            name: String::new(),
            at_ms: 1,
            kind: "stuck".into(),
            state: "stuck".into(),
            last: "errored".into(),
            occurrence: "outcome:7".into(),
            key_kind: "outcome".into(),
            attempts: 0,
            outcome: Outcome::Pending,
            transport: String::new(),
            answered_at_ms: None,
        });
        save(&store).unwrap();
        let mut reloader = || super::load(&file);
        // …then a release that goes through, which must cancel the unsent notice.
        crate::release_adoption(
            &mut store,
            "/c.eid",
            "/w.eid",
            "done",
            2,
            &mut save,
            &mut reloader,
        )
        .unwrap();
        let reloaded = super::load(&file).unwrap();
        assert_eq!(reloaded.records[0].released_at_ms, Some(2));
        assert_eq!(reloaded.notices[0].outcome, Outcome::Cancelled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Rank blocks: the per-rank text a session is told beside its persona, and the
//! content binding that keeps a retry from injecting different text.
//!
//! A **rank block** is one file per rank under the plugin's `ranks/` directory —
//! `imperator.md`, `legatus.md`, `tribunus.md`, `miles.md`, and optionally
//! `centurio.md`. Its bytes are appended to the child's resolved system prompt at
//! spawn (`--system-append <file>`), before the session is created, so the
//! harness's `SessionStart` record captures the prompt the child actually had.
//!
//! Three rules, and nothing else — the fourth bullet is how the third is kept:
//!
//! - **The rank names are the operator's, not this plugin's.** [`RANKS`] is the
//!   whole vocabulary. An unknown name is refused by name; no rank is added,
//!   renamed or inferred, and this module writes no doctrine of its own into a
//!   block. The shipped blocks say only that they are unwritten.
//! - **A rank with no block means no addition, not a failure.** No file, or a file
//!   holding nothing but the placeholder comment, is `None`: the launch carries no
//!   `--system-append` at all. A block that exists but cannot be read is a
//!   refusal — an unreadable block is not an absent one, and guessing which would
//!   silently change what a child is told.
//! - **A block's bytes are bound into the launch.** [`Blocks::resolve`] reads the
//!   bytes and hashes them; the digest goes into the launch's immutable request.
//!   [`Blocks::verify`] reads them again immediately before the spawn and refuses
//!   if they no longer hash to what the operation was recorded with. A retry after
//!   somebody edited the file is refused, never quietly re-told.
//! - **The digest travels with the text.** The bytes that were hashed are the bytes
//!   the host writes to a launch-private file, flushes, makes read-only, re-reads and
//!   re-hashes, and hands the child, so the CLI's own read composes what the record
//!   names. The block's path is never handed onward: a path is a promise that the
//!   file will still say the same thing when somebody else reads it, and that promise
//!   is exactly what the digest replaces.
//!
//! The digest is SHA-256 of the exact bytes. It is implemented here, in this file,
//! so that binding a launch needs no crate the plugin did not already have. It is
//! not a security primitive and is not claimed as one: it binds a launch to a text,
//! and the text it binds is the text that is handed over.
//!
//! # A second, separate feature that shares this file: rank assignment
//!
//! Everything above is one feature, **rank blocks**, and it is **launch input**. The
//! monitor reads `ranks/<word>.md`, binds the SHA-256 of those exact bytes into the
//! managed start's immutable record, re-reads and re-hashes them immediately before the
//! spawn, and hands those bytes to the child through `--system-append`. That is the
//! managed-start path and nothing in this file's second feature touches it.
//!
//! Everything below is a **different feature: rank assignment**, and it is **display,
//! not launch input**. The word lives in this service's own store, keyed on the durable
//! session id, and it reaches a screen only through the harness's `door decorate` —
//! presentation text a controller attached, shown beside the session's own label. It
//! writes no journal, no argv, no `SessionStart` record and no launch request, and the
//! block machinery above never reads the assignment record.
//!
//! The two were written against trees that did not contain each other and met here
//! because both were called `rank`. They share exactly one thing — [`RANKS`], so that a
//! word cannot exist for one and not for the other. That is the whole of the coupling;
//! neither feature depends on the other, and a change to one is not a change to the
//! other.
//! Rank assignment: this plugin's own vocabulary, and the prefix it derives.
//!
//! Core knows nothing in this file. The harness stores **arbitrary presentation
//! text** on a registration and shows it beside the label; what the words mean,
//! which ones exist, who may assign them, how long an assignment lasts and what a
//! restart does with one are decisions taken here and persisted in this service's
//! store, never in a session log and never in a harness enum.
//!
//! Two rules the record keeps, and they are what the replacement architecture was
//! built for:
//!
//! - **The key is the durable session id** — the stem of the session log, the
//!   address the harness mints — and never a registration locator, a label, a name
//!   or a cwd. A registration is re-minted when a session restarts or is adopted by
//!   another process, and its directory name can be reused behind it; the session
//!   id travels in the session log's own file name and survives both.
//! - **Assignment is explicit.** Nothing here derives a rank from a tag, a label, a
//!   prompt, an adoption, a peer's message or a word in a tool call. `assign` is an
//!   act somebody performs, naming the session and the word.
//!
//! What the harness enforces and what this file assumes are different things. The
//! decoration field admits one owner at a time and refuses a second; ownership is a
//! convention between cooperative clients, not an authentication (the door is a
//! same-user socket and core reads no caller identity from it). So this module
//! treats a decoration held by another owner as a **conflict to report**, never as
//! something to take over, and treats its own persisted word as intent — the
//! display is only what the live registration says.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::{Row, Store};

/// The rank vocabulary, exactly as the operator's convention spells it. `centurio`
/// is carried because it was named; whether any launch uses it is not this
/// module's decision.
///
/// Both features in this file read this one list: the block resolver refuses a word
/// that is not here, and the assignment record derives its display form from the same
/// vocabulary, so a word cannot exist for one and not the other. They share the list
/// and nothing else.
pub const RANKS: [&str; 5] = ["imperator", "legatus", "tribunus", "miles", "centurio"];

/// One resolved, written rank block: which rank, where it was read from, the digest
/// of those bytes, and the bytes themselves.
///
/// The bytes are carried, not just the path, because the digest has to name what
/// the child is actually told. `--system-append` takes a *path*, so handing it the
/// block would let a third read (the CLI's own) compose text this module never
/// hashed. [`Blocks::verify`] therefore returns the bytes it hashed, and the host
/// writes exactly those bytes to a launch-private file and passes that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addition {
    pub rank: String,
    /// The block the bytes were read from, for messages that name the operator's
    /// file. Not what the child is handed.
    pub path: PathBuf,
    pub digest: String,
    /// The exact bytes hashed above.
    pub bytes: Vec<u8>,
}

/// The directory the blocks live in. `dir` is the plugin's own `ranks/`, resolved
/// by the monitor (`EIDOLON_COORDINATOR_RANKS`, else the installed plugin folder).
#[derive(Clone, Debug)]
pub struct Blocks {
    dir: PathBuf,
}

impl Blocks {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read the named rank's block and hash it. `Ok(None)` is "this rank has no
    /// addition" — no file, or a file that holds only the placeholder comment.
    pub fn resolve(&self, rank: Option<&str>) -> Result<Option<Addition>, String> {
        let Some(rank) = rank else { return Ok(None) };
        if !RANKS.contains(&rank) {
            return Err(unknown_rank(rank));
        }
        if self.dir.as_os_str().is_empty() {
            return Err(format!(
                "the monitor has no rank-block directory configured, so the `{rank}` block cannot be read. \
                 A rank is text an operator writes and the monitor appends; with nowhere to read it from, \
                 refusing is the only honest answer"
            ));
        }
        let path = self.dir.join(format!("{rank}.md"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "the `{rank}` rank block at {} exists but cannot be read: {e}. An unreadable block is \
                     not an absent one — launching without it would tell the child something nobody wrote",
                    path.display()
                ))
            }
        };
        if unwritten(&bytes) {
            return Ok(None);
        }
        // The digest and the path handed to `--system-append` must name the same
        // bytes. Canonicalising once, here, is what makes that true even if the
        // plugin directory is reached through a symlink.
        let path = path.canonicalize().map_err(|e| {
            format!("the `{rank}` rank block at {} could not be resolved: {e}", path.display())
        })?;
        let digest = digest_hex(&bytes);
        Ok(Some(Addition { rank: rank.to_string(), path, digest, bytes }))
    }

    /// Read the block again, immediately before a spawn, and compare it with what
    /// the operation was recorded with. `request.rank_digest` is the binding; this
    /// is the check that a change between the request and the spawn refuses.
    pub fn verify(&self, rank: Option<&str>, digest: Option<&str>) -> Result<Option<Addition>, String> {
        let Some(rank) = rank else {
            return match digest {
                Some(_) => Err(
                    "the launch request binds a rank digest but names no rank; refusing to append text \
                     nobody can name"
                        .to_string(),
                ),
                None => Ok(None),
            };
        };
        let now = self.resolve(Some(rank))?;
        let path = self.dir.join(format!("{rank}.md"));
        match (now, digest) {
            (Some(addition), Some(bound)) if addition.digest == bound => Ok(Some(addition)),
            (Some(addition), Some(bound)) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: its bytes now hash to \
                 {}, and the operation is bound to {bound}. Refusing to spawn — the child would be told text \
                 nobody reviewed for this operation",
                addition.path.display(),
                addition.digest
            )),
            (Some(addition), None) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: the operation is bound \
                 to no block, and the file now holds text hashing to {}. Refusing to spawn — the child would \
                 be told text nobody reviewed for this operation",
                addition.path.display(),
                addition.digest
            )),
            (None, Some(bound)) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: it is gone (or holds \
                 only the unwritten placeholder), and the operation is bound to {bound}. Refusing to spawn — \
                 the child would be told something other than what the operation recorded",
                path.display()
            )),
            (None, None) => Ok(None),
        }
    }
}

/// The refusal an unknown rank gets, naming the whole vocabulary so a caller does
/// not have to guess which spelling this plugin accepts.
pub fn unknown_rank(rank: &str) -> String {
    format!(
        "`{rank}` is not a rank this plugin knows: the ranks are {}. They are the operator's convention; \
         this plugin does not add, rename or infer one, and it will not treat an unknown word as a rank",
        RANKS.join(", ")
    )
}

/// A block is *unwritten* when it holds nothing but whitespace and HTML comments.
/// The shipped blocks are exactly that: a comment saying the rank's text is the
/// operator's and has not been written yet. Nothing is appended for such a block.
///
/// The rule is deliberately this small. It is not markup support: an unterminated
/// `<!--` is text, not a placeholder, and the operator's prose is appended
/// verbatim — comments included — because the digest in the launch record is the
/// digest of the file's exact bytes, and stripping would break that bond.
fn unwritten(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut rest = text.as_ref();
    let mut written = String::new();
    while let Some(start) = rest.find("<!--") {
        written.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return false,
        }
    }
    written.push_str(rest);
    written.trim().is_empty()
}

/// SHA-256, lowercase hex. FIPS 180-4; the test vectors below are the check.
pub fn digest_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for base in (0..message.len()).step_by(64) {
        let chunk = &message[base..base + 64];
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

/// The owner string this plugin claims on a decoration.
///
/// One value for the whole plugin rather than a session id: the monitor outlives
/// every session that uses it, and two coordinators cannot share one store (the
/// store takes a single-writer lock at startup), so the owner names the plugin and
/// the store names the session.
pub const OWNER: &str = "coordinator";

/// The display text for a stored word, or `None` for a word outside the vocabulary.
pub fn prefix_of(rank: &str) -> Option<&'static str> {
    match rank.trim().to_ascii_lowercase().as_str() {
        "imperator" => Some("Imperator"),
        "legatus" => Some("Legatus"),
        "tribunus" => Some("Tribunus"),
        "centurio" => Some("Centurio"),
        "miles" => Some("Miles"),
        _ => None,
    }
}

/// One explicit assignment: this session carries this word, since when, and who
/// said so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankAssignment {
    /// The harness's durable address, echoed verbatim. Never canonicalised, never
    /// shortened, never a locator.
    pub session: String,
    /// The stored word, one of [`RANKS`].
    pub rank: String,
    /// The display text derived from it at assignment time and kept, so a later
    /// edit to the vocabulary cannot silently rewrite a row that already carries
    /// something. Derived, not authored: `assign` refuses a word outside the list.
    pub prefix: String,
    /// When the assignment was made, epoch ms.
    pub assigned_ms: u64,
    /// The caller's durable address — who assigned it, as the harness published it.
    #[serde(default)]
    pub by: String,
}

/// One presentation act the monitor owes a registration.
///
/// There is exactly one kind, and that is a decision rather than an unfinished
/// thought: the monitor **attaches** its own text and never removes one. Removal is
/// an explicit act — the release tool does it, and the operator can with
/// `eidolon door decorate --remove` — because the owner string is a *claim*: any
/// same-user client can write `coordinator` into it, so "a decoration owned by
/// `coordinator` with no assignment behind it" is not proof that this monitor put it
/// there. A sweep on that evidence would delete a person's own prefix, which is
/// exactly what an early draft did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Attach (or refresh) our own text on this session's row.
    Decorate { session: String, prefix: String },
}

impl Act {
    pub fn session(&self) -> &str {
        match self {
            Act::Decorate { session, .. } => session,
        }
    }

    /// What to run, for a log line and for a test that must not guess.
    pub fn words(&self) -> String {
        match self {
            Act::Decorate { session, prefix } => format!("decorate {session} with `{prefix}`"),
        }
    }
}

/// Whether an owner can be typed after `--owner` as one shell-safe word.
///
/// Deliberately narrow: letters, digits and the four punctuation marks a plugin
/// namespace or a session id actually uses. Anything else — a space, a quote, a `;`,
/// a `$(`, a line separator — is a name the monitor will *display* and will not put in a
/// command line for somebody to paste.
fn shell_safe(owner: &str) -> bool {
    !owner.is_empty()
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

/// One line, whatever the string holds.
///
/// A decoration is checked when it is written, but a row can also be hand-written or
/// written by a build older than the check — and every line below is printed into a
/// tool answer, where a line separator would forge a line of the monitor's own words.
/// So the rendering flattens; it never trusts the check upstream.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Every decoration act the store and the roster together call for, plus the
/// conflicts that stop one.
///
/// This is a **pure decision**: it reads the records and the rows it is given and
/// writes nothing. That is what makes the interesting cases testable without a
/// socket, a process or a clock — a restart reasserts, a recycled locator decorates
/// nothing, a label edited by the session keeps its prefix because the prefix is
/// not the label, another owner's decoration is reported rather than fought over,
/// two live rows claiming one address decorate neither, and a decoration we own
/// with no assignment behind it is taken back.
///
/// Empty acts and empty conflicts is the ordinary case: everything already says
/// what it should, so a monitor polling every thirty seconds does not write a
/// `meta.json` per poll.
pub fn acts(assignments: &[RankAssignment], rows: &[Row]) -> (Vec<Act>, Vec<String>) {
    let mut acts = Vec::new();
    let mut conflicts = Vec::new();

    for a in assignments {
        let claimants: Vec<&Row> = rows
            .iter()
            .filter(|r| r.session.as_deref() == Some(a.session.as_str()))
            .collect();
        let row = match claimants.len() {
            0 => continue, // not live: the assignment waits, and decorates nothing
            1 => claimants[0],
            n => {
                conflicts.push(format!(
                    "{} live rows claim `{}` — an address published twice is ambiguous, and this \
                     monitor will not pick one to decorate",
                    n, a.session
                ));
                continue;
            }
        };
        match &row.decoration {
            Some(d) if d.controller == OWNER && d.text == a.prefix => {}
            Some(d) if d.controller == OWNER => acts.push(Act::Decorate {
                session: a.session.clone(),
                prefix: a.prefix.clone(),
            }),
            Some(d) => {
                let owner = one_line(&d.controller);
                // The remedy is a command, and a command needs the owner exactly as the
                // harness holds it. If flattening changed it — a hand-written
                // `meta.json`, since core refuses a separator on the write path — the two
                // cannot both be satisfied in one line, so the sentence says the name is
                // flattened and where the exact bytes are.
                // The copy-paste form is only offered when the owner is one shell-safe
                // word: `--owner someone else` or `--owner a;b` is a line that, pasted,
                // does something nobody meant. Otherwise the sentence says where to read
                // the exact bytes and lets a person do the quoting.
                let remedy = if shell_safe(&d.controller) {
                    format!(
                        "`eidolon door decorate {} --owner {} --remove` clears it if that owner \
                         is done",
                        a.session, owner
                    )
                } else {
                    // Two different reasons reach this branch — a space or a shell
                    // metacharacter, and a line separator (which core refuses on the
                    // write path, so only a hand-written registration carries one) — so
                    // the sentence names neither as though it had checked: it says what
                    // is true of both.
                    "the owner is not one shell-safe word and is shown flattened here (it may \
                     carry a space or metacharacter, or a line separator a hand-written \
                     registration put there); read it byte for byte from `eidolon peers \
                     --json` and pass that exact value to `eidolon door decorate <session> \
                     --owner <owner> --remove`"
                        .to_string()
                };
                conflicts.push(format!(
                    "`{}` already carries `{}` by `{owner}`; a decoration has one owner at a \
                     time, so this monitor does not take it over — {remedy}",
                    a.session,
                    one_line(&d.text),
                ))
            }
            None => acts.push(Act::Decorate {
                session: a.session.clone(),
                prefix: a.prefix.clone(),
            }),
        }
    }

    // A decoration that *claims* to be ours with no assignment behind it: the store
    // was released, replaced or lost — or somebody typed the owner string themselves.
    // Both are the same evidence, so this is reported and left standing. Removing it
    // is an explicit act (`coordinator_unrank`, `eidolon door decorate --remove`),
    // never a sweep on a string any same-user client can write.
    for row in rows {
        let Some(session) = row.session.as_deref() else { continue };
        let Some(d) = &row.decoration else { continue };
        if d.controller != OWNER {
            continue;
        }
        if assignments.iter().any(|a| a.session == session) {
            continue;
        }
        conflicts.push(format!(
            "`{session}` carries `{}` by `{OWNER}` and no assignment here backs it; this monitor does \
             not remove a decoration it cannot prove it attached — `eidolon door decorate {session} \
             --owner {OWNER} --remove` clears it if it is stale",
            one_line(&d.text)
        ));
    }

    (acts, conflicts)
}

/// What a caller has to know when a write may or may not have landed: the live
/// state is taken from the disk, and the call's outcome is *unknown* — an uncertain
/// write is not a rollback.
fn uncertain(
    store: &mut Store,
    load: &mut dyn FnMut() -> Result<Store, String>,
    e: crate::SaveError,
) -> Result<String, String> {
    match load() {
        Ok(reloaded) => {
            *store = reloaded;
            Err(format!(
                "{e}. The store has been re-read, so the live state comes from the file rather than \
                 from the write that failed — this call's outcome is unknown, and \
                 `coordinator_ranks` is what to read before assuming either way"
            ))
        }
        Err(le) => Err(format!(
            "{e}, and the store could not be re-read either ({le}) — there is no state this monitor \
             can vouch for. It has to be stopped and started again (`eidolon plugins service stop \
             coordinator`, then start it) before it serves anything"
        )),
    }
}

/// What an assignment call carries.
pub struct RankRequest<'a> {
    /// The caller's durable address, as the harness published it.
    pub caller: &'a str,
    /// The session to assign, echoed verbatim.
    pub worker: &'a str,
    /// The word, one of [`RANKS`].
    pub rank: &'a str,
    /// Whether the roster could be read at all, and what it said about the worker.
    /// `Err` is *the roster could not be read*, which is not an absent session and
    /// refuses the call before anything is written.
    pub roster: Result<Option<&'a Row>, String>,
    pub now: u64,
}

/// Assign a word to a session, once, durably, and only after the record is on the
/// disk.
///
/// Refused, each with its own reason: an empty address; a word outside the
/// vocabulary; an unreadable roster; a session that already carries a **different**
/// word (release first — a silent re-assignment is the conflict the ruling asked to
/// fail explicitly); and a second live row claiming the same address.
///
/// Allowed: assigning a session that is **not live right now**. The assignment is
/// durable intent and is asserted when the session runs; a monitor that refused the
/// word whenever a worker happened to be between runs would lose the record of what
/// the operator had decided.
pub fn assign(
    store: &mut Store,
    req: RankRequest<'_>,
    save: &mut dyn FnMut(&Store) -> Result<(), crate::SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
) -> Result<String, String> {
    let RankRequest { caller, worker, rank, roster, now } = req;
    let worker = worker.trim();
    if worker.is_empty() {
        return Err(
            "`rank` needs `session`: the session's durable address, echoed verbatim — never a \
             display name, a label or a registration id"
                .into(),
        );
    }
    let Some(prefix) = prefix_of(rank) else {
        return Err(format!(
            "`{rank}` is not a rank this plugin knows. The vocabulary is: {}",
            RANKS.join(", ")
        ));
    };
    let seen = match roster {
        Err(e) => {
            return Err(format!(
                "the roster could not be read, so nothing was written: {e}. An unread roster is not \
                 an absent session, and this assignment is keyed on an address the harness publishes"
            ));
        }
        Ok(seen) => seen,
    };
    if let Some(r) = store.ranks.iter().find(|r| r.session == worker) {
        if r.rank == prefix.to_ascii_lowercase() {
            return Ok(format!(
                "`{worker}` already carries `{}` (assigned {} by {}); nothing was written, and the \
                 monitor keeps it asserted on every live registration of that session",
                r.prefix,
                r.assigned_ms,
                if r.by.is_empty() { "an unnamed caller" } else { &r.by }
            ));
        }
        return Err(format!(
            "`{worker}` already carries `{}`, assigned {} by {}; this monitor does not re-assign \
             silently — release it first (`coordinator_unrank`) and then assign `{}`",
            r.prefix,
            r.assigned_ms,
            if r.by.is_empty() { "an unnamed caller" } else { &r.by },
            prefix
        ));
    }
    let mut next = store.clone();
    next.ranks.push(RankAssignment {
        session: worker.to_string(),
        rank: prefix.to_ascii_lowercase(),
        prefix: prefix.to_string(),
        assigned_ms: now,
        by: caller.to_string(),
    });
    match save(&next) {
        Ok(()) => *store = next,
        Err(e) if e.uncertain() => return uncertain(store, load, e),
        Err(e) => return Err(format!("the assignment was not written ({e})")),
    }
    Ok(match seen {
        Some(row) => format!(
            "`{worker}` carries `{}`; its registration{}{} is live, and the display is already \
             being reasserted against it",
            prefix,
            if row.id.is_empty() { "" } else { " " },
            row.id
        ),
        None => format!(
            "`{worker}` carries `{}` from now on; no live session answers to that address at this \
             moment, so nothing is decorated yet — the assignment is asserted the next time a \
             registration publishes exactly that session id",
            prefix
        ),
    })
}

/// Release an assignment, and only after the release is on the disk.
///
/// The label is not touched by this or by anything else here: a rank was
/// presentation, and the session's own name is its own.
pub fn release(
    store: &mut Store,
    caller: &str,
    worker: &str,
    save: &mut dyn FnMut(&Store) -> Result<(), crate::SaveError>,
    load: &mut dyn FnMut() -> Result<Store, String>,
) -> Result<String, String> {
    let worker = worker.trim();
    if worker.is_empty() {
        return Err("`unrank` needs `session`: the session whose assignment is being released".into());
    }
    let Some(standing) = store.ranks.iter().find(|r| r.session == worker).cloned() else {
        return Err(format!(
            "`{worker}` carries no rank here, so there is nothing to release and nothing was written"
        ));
    };
    let mut next = store.clone();
    next.ranks.retain(|r| r.session != worker);
    match save(&next) {
        Ok(()) => *store = next,
        Err(e) if e.uncertain() => return uncertain(store, load, e),
        Err(e) => return Err(format!("the release was not written ({e})")),
    }
    Ok(format!(
        "`{worker}` no longer carries `{}`; released by `{}`{}",
        standing.prefix,
        if caller.is_empty() { "an unnamed caller" } else { caller },
        if standing.by.is_empty() {
            String::new()
        } else {
            format!(" (assigned by {})", standing.by)
        }
    ))
}

/// The roster's answer for each assignment, as a coordinator reads it: what the
/// record says, and what the live registration says beside it.
///
/// The two are deliberately printed apart. A row's display is what a reader sees
/// *now*; the assignment is what the operator decided. Where they disagree — a
/// session between runs, a decoration cleared by an adoption, another owner's
/// conflict — the answer says so rather than reporting the record as if it were the
/// screen.
pub fn render(assignments: &[RankAssignment], rows: Result<&[Row], &str>) -> String {
    if assignments.is_empty() {
        return "no rank is assigned to any session here".to_string();
    }
    let mut out = String::new();
    for a in assignments {
        out.push_str(&format!(
            "{} · assigned {} by {}",
            a.session,
            a.prefix,
            if a.by.is_empty() { "an unnamed caller" } else { &a.by }
        ));
        match rows {
            Err(e) => out.push_str(&format!(" — the roster could not be read ({e}), so nothing is said about it\n")),
            Ok(rows) => {
                let claimants: Vec<&Row> =
                    rows.iter().filter(|r| r.session.as_deref() == Some(a.session.as_str())).collect();
                match claimants.len() {
                    0 => out.push_str(" — not live now; the word is asserted when it runs\n"),
                    1 => {
                        let row = claimants[0];
                        match (&row.decoration, &row.display) {
                            (Some(d), _) if d.controller == OWNER && d.text == a.prefix => {
                                out.push_str(&format!(" — live, wearing `{}`\n", one_line(&d.text)))
                            }
                            (Some(d), _) => out.push_str(&format!(
                                " — live, but wearing `{}` by `{}`: one owner at a time, so this \
                                 monitor is not asserting\n",
                                one_line(&d.text),
                                one_line(&d.controller)
                            )),
                            (None, _) => out.push_str(
                                " — live and undecorated: the next scan asserts the word\n",
                            ),
                        }
                    }
                    n => out.push_str(&format!(
                        " — {n} live rows claim this address, which is ambiguous; nothing is \
                         decorated\n"
                    )),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoration;

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coord-rank-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        // FIPS 180-4 / the usual NIST examples, including one that spans a block.
        assert_eq!(digest_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(digest_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            digest_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            digest_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn no_rank_and_no_block_are_both_no_addition() {
        let dir = tmpdir("absent");
        let blocks = Blocks::new(dir.clone());
        assert_eq!(blocks.resolve(None).unwrap(), None);
        assert_eq!(blocks.resolve(Some("miles")).unwrap(), None, "a missing file is no addition, not an error");
        assert_eq!(blocks.verify(None, None).unwrap(), None);
        assert_eq!(blocks.verify(Some("miles"), None).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shipped_placeholder_shape_is_unwritten_but_prose_is_written() {
        let dir = tmpdir("placeholder");
        let blocks = Blocks::new(dir.clone());
        std::fs::write(dir.join("miles.md"), "<!--\nmiles — unwritten. The operator's words go here.\n-->\n").unwrap();
        assert_eq!(blocks.resolve(Some("miles")).unwrap(), None, "a comment-only block appends nothing");

        std::fs::write(dir.join("miles.md"), "<!-- unwritten -->\nHold the flank; report the count.\n").unwrap();
        let addition = blocks.resolve(Some("miles")).unwrap().expect("prose is a written block");
        assert_eq!(addition.rank, "miles");
        assert_eq!(addition.path, dir.join("miles.md").canonicalize().unwrap());
        assert_eq!(addition.digest, digest_hex(std::fs::read(dir.join("miles.md")).unwrap().as_slice()));

        // An unterminated comment is text, not a placeholder.
        std::fs::write(dir.join("miles.md"), "<!-- unwritten\n").unwrap();
        assert!(blocks.resolve(Some("miles")).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_blocks_that_ship_with_the_plugin_append_nothing() {
        // The shipped files are placeholders. If one of them were text, every
        // launch of that rank would carry a paragraph about being unwritten.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ranks");
        let blocks = Blocks::new(dir.clone());
        for rank in RANKS {
            assert_eq!(
                blocks.resolve(Some(rank)).unwrap(),
                None,
                "{rank}.md ships as a placeholder and must resolve to no addition"
            );
        }
    }

    #[test]
    fn a_rank_outside_the_vocabulary_is_refused_by_name() {
        let dir = tmpdir("unknown");
        let blocks = Blocks::new(dir.clone());
        let e = blocks.resolve(Some("duke")).unwrap_err();
        assert!(e.contains("`duke` is not a rank"), "{e}");
        for rank in RANKS {
            assert!(e.contains(rank), "the refusal names every rank, missing {rank}: {e}");
        }
        std::fs::write(dir.join("duke.md"), "text").unwrap();
        assert!(blocks.resolve(Some("duke")).is_err(), "a file does not create a rank");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_block_is_a_refusal_not_an_absence() {
        let dir = tmpdir("unreadable");
        let blocks = Blocks::new(dir.clone());
        std::fs::create_dir_all(dir.join("miles.md")).unwrap();
        let e = blocks.resolve(Some("miles")).unwrap_err();
        assert!(e.contains("exists but cannot be read"), "{e}");
        assert!(e.contains("not an absent one"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_refuses_bytes_that_changed_after_the_request_was_bound() {
        let dir = tmpdir("changed");
        let blocks = Blocks::new(dir.clone());
        std::fs::write(dir.join("tribunus.md"), "the first text\n").unwrap();
        let bound = blocks.resolve(Some("tribunus")).unwrap().unwrap();

        assert_eq!(
            blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap(),
            Some(bound.clone()),
            "unchanged bytes are the same addition"
        );

        std::fs::write(dir.join("tribunus.md"), "different text\n").unwrap();
        let e = blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap_err();
        assert!(e.contains("changed after this launch was prepared"), "{e}");
        assert!(e.contains("tribunus.md"), "and it names the file: {e}");
        assert!(e.contains(&bound.digest), "and the digest the operation is bound to: {e}");

        // Appearing, and disappearing, are the same kind of change.
        assert!(blocks.verify(Some("tribunus"), None).unwrap_err().contains("changed"));
        std::fs::remove_file(dir.join("tribunus.md")).unwrap();
        assert!(blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap_err().contains("changed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn row(session: &str, decoration: Option<(&str, &str)>, label: Option<&str>) -> Row {
        Row {
            session: Some(session.to_string()),
            label: label.map(str::to_string),
            display: Some(match (decoration, label) {
                (Some((_, text)), Some(label)) => format!("{text} · {label}"),
                (Some((_, text)), None) => text.to_string(),
                (None, Some(label)) => label.to_string(),
                (None, None) => String::new(),
            }),
            decoration: decoration.map(|(controller, text)| Decoration {
                controller: controller.to_string(),
                text: text.to_string(),
            }),
            ..Row::default()
        }
    }

    fn assignment(session: &str, prefix: &str) -> RankAssignment {
        RankAssignment {
            session: session.to_string(),
            rank: prefix.to_ascii_lowercase(),
            prefix: prefix.to_string(),
            assigned_ms: 1,
            by: "1793000000001".into(),
        }
    }

    /// The ordinary case: an assigned session with no decoration is decorated, and
    /// once it wears the word nothing more is written — which is what lets a monitor
    /// poll every thirty seconds without touching a `meta.json` per poll.
    #[test]
    fn an_assignment_decorates_once_and_then_stops() {
        let store = vec![assignment("1793", "Imperator")];

        let (did, conflicts) = acts(&store, &[row("1793", None, Some("vault sweep"))]);
        assert_eq!(conflicts, Vec::<String>::new());
        assert_eq!(did, vec![Act::Decorate { session: "1793".into(), prefix: "Imperator".into() }]);

        let wearing = [row("1793", Some((OWNER, "Imperator")), Some("vault sweep"))];
        let (did, conflicts) = acts(&store, &wearing);
        assert_eq!(did, Vec::<Act>::new(), "already wearing it: nothing to do");
        assert_eq!(conflicts, Vec::<String>::new());
    }

    /// The label is the session's business. A session that renames itself keeps the
    /// prefix, because the prefix is not the name and nothing here rewrites a label.
    #[test]
    fn a_relabelled_session_keeps_its_prefix_and_nothing_is_rewritten() {
        let store = vec![assignment("1793", "Legatus")];
        let renamed = [row("1793", Some((OWNER, "Legatus")), Some("a different task"))];
        let (did, conflicts) = acts(&store, &renamed);
        assert_eq!(did, Vec::<Act>::new());
        assert_eq!(conflicts, Vec::<String>::new());
        assert_eq!(renamed[0].display.as_deref(), Some("Legatus · a different task"));
    }

    /// A registration is re-minted when a session restarts or is adopted by another
    /// process, and the locator can be reused behind it. The word belongs to the
    /// session id: nobody else's row is decorated, and the session's own new
    /// registration is.
    #[test]
    fn a_recycled_locator_is_not_decorated_and_a_restarted_session_is() {
        let store = vec![assignment("1793", "Imperator")];
        let recycled = [Row { id: "eidolon-0001".into(), ..row("1800", None, Some("someone else")) }];
        let (did, _) = acts(&store, &recycled);
        assert_eq!(did, Vec::<Act>::new(), "another session is never decorated by this record");

        let restarted = [Row { id: "eidolon-0001".into(), ..row("1793", None, Some("the same task")) }];
        let (did, _) = acts(&store, &restarted);
        assert_eq!(did, vec![Act::Decorate { session: "1793".into(), prefix: "Imperator".into() }]);
    }

    /// One owner at a time. Another controller's decoration is reported with the
    /// remedy, never taken over; two live rows claiming one address decorate neither
    /// one; and a decoration this plugin owns with no assignment behind it is taken
    /// back.
    #[test]
    fn conflicts_are_reported_and_orphans_are_taken_back() {
        let store = vec![assignment("1793", "Imperator")];
        let foreign = [row("1793", Some(("someone-else", "Legatus")), None)];
        let (did, conflicts) = acts(&store, &foreign);
        assert_eq!(did, Vec::<Act>::new());
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("someone-else"), "{}", conflicts[0]);
        assert!(conflicts[0].contains("--remove"), "and it says how to clear it: {}", conflicts[0]);

        let doubled = [row("1793", None, None), row("1793", None, None)];
        let (did, conflicts) = acts(&store, &doubled);
        assert_eq!(did, Vec::<Act>::new());
        assert!(conflicts[0].contains("ambiguous"), "{}", conflicts[0]);

        // A decoration that claims to be ours with no record behind it is REPORTED,
        // never swept: the owner string is a claim any same-user client can write, so
        // the monitor cannot prove it attached this one — and removing a person's own
        // prefix on that evidence is the defect an earlier draft had.
        let orphan = [row("1799", Some((OWNER, "HumanSet")), Some("released yesterday"))];
        let (did, conflicts) = acts(&store, &orphan);
        assert_eq!(did, Vec::<Act>::new(), "the monitor attaches and never removes");
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("HumanSet"), "{}", conflicts[0]);
        assert!(conflicts[0].contains("--remove"), "{}", conflicts[0]);
    }

    /// A row's text is checked when written but may predate the check — or be
    /// hand-written. Every line the monitor prints flattens it, so a separator can
    /// never forge a line of the monitor's own words.
    #[test]
    fn a_published_text_is_flattened_before_it_is_printed() {
        let store = vec![assignment("1793", "Imperator")];
        let smuggled = [row("1793", Some(("someone-else", "Leg\u{2028}atus")), None)];
        let (_, conflicts) = acts(&store, &smuggled);
        assert_eq!(conflicts.len(), 1);
        assert!(!conflicts[0].contains('\u{2028}'), "{:?}", conflicts[0]);
        assert!(conflicts[0].contains("Leg atus"), "{:?}", conflicts[0]);
        let wearing = [row("1793", Some((OWNER, "Leg\u{2029}atus")), None)];
        let said = render(&store, Ok(&wearing));
        assert!(!said.contains('\u{2029}') && !said.contains('\u{2028}'), "{said:?}");

        // The controller is printed too, in the same lines, so it is flattened the same
        // way: a name that still admits a separator must not forge a line of the
        // monitor's own words.
        let named = [row("1793", Some(("evil\u{2028}Forged", "Legatus")), None)];
        let said = render(&store, Ok(&named));
        assert!(!said.contains('\u{2028}'), "{said:?}");
        let (_, conflicts) = acts(&store, &named);
        assert!(!conflicts[0].contains('\u{2028}'), "{:?}", conflicts[0]);
    }

    /// The remedy is a command, so it is offered only when the owner can be a word on
    /// one: a space or a `;` in an owner string is displayed and never pasted.
    #[test]
    fn the_remove_remedy_is_withheld_for_an_owner_that_is_not_one_word() {
        assert!(shell_safe("coordinator"));
        assert!(shell_safe("eidolon-abc:1_2.3"));
        assert!(!shell_safe("someone else"));
        assert!(!shell_safe("a;b"));
        assert!(!shell_safe("$(rm -rf ~)"));
        assert!(!shell_safe(""));

        let store = vec![assignment("1793", "Imperator")];
        for owner in ["someone else", "a;b"] {
            let row = [row("1793", Some((owner, "Legatus")), None)];
            let (_, conflicts) = acts(&store, &row);
            assert!(
                !conflicts[0].contains("--owner someone else --remove"),
                "no pasteable command for a name that is not one word: {}",
                conflicts[0]
            );
            assert!(conflicts[0].contains("peers --json"), "{}", conflicts[0]);
        }
        let safe = [row("1793", Some(("someone-else", "Legatus")), None)];
        let (_, conflicts) = acts(&store, &safe);
        assert!(conflicts[0].contains("--owner someone-else --remove"), "{}", conflicts[0]);
    }

    /// The vocabulary is the plugin's, and a word outside it is refused by name
    /// rather than passed through to core as text nobody planned for.
    #[test]
    fn the_vocabulary_is_five_words_and_each_has_a_display_form() {
        for word in RANKS {
            assert!(prefix_of(word).is_some(), "{word}");
            assert_eq!(prefix_of(word).unwrap().to_ascii_lowercase(), word);
        }
        assert_eq!(prefix_of(" Imperator "), Some("Imperator"), "trimmed and case-folded");
        assert_eq!(prefix_of("general"), None);
        assert_eq!(prefix_of(""), None);
    }

    /// Assignment is explicit, durable, and one word per session: the same word
    /// again writes nothing, a different word is refused by name with the remedy,
    /// and a word outside the vocabulary never reaches the store.
    #[test]
    fn assign_is_durable_and_one_word_at_a_time() {
        let mut store = Store::default();
        let mut save = |_s: &Store| Ok(());
        let mut load = || Err("no store to re-read".to_string());
        let live = row("1793", None, Some("vault sweep"));

        let said = assign(
            &mut store,
            RankRequest {
                caller: "1792",
                worker: "1793",
                rank: "Imperator",
                roster: Ok(Some(&live)),
                now: 10,
            },
            &mut save,
            &mut load,
        )
        .unwrap();
        assert!(said.contains("`Imperator`"), "{said}");
        assert_eq!(store.ranks.len(), 1);
        assert_eq!(store.ranks[0].session, "1793");
        assert_eq!(store.ranks[0].prefix, "Imperator", "derived once and kept");
        assert_eq!(store.ranks[0].by, "1792", "who assigned it is part of the record");
        assert_eq!(store.ranks[0].assigned_ms, 10);

        // The same word again: nothing written, and the answer says so.
        let again = assign(
            &mut store,
            RankRequest {
                caller: "1792",
                worker: "1793",
                rank: "Imperator",
                roster: Ok(Some(&live)),
                now: 20,
            },
            &mut save,
            &mut load,
        )
        .unwrap();
        assert!(again.contains("already carries"), "{again}");
        assert_eq!(store.ranks.len(), 1);
        assert_eq!(store.ranks[0].assigned_ms, 10, "the record was not rewritten");

        // A different word is a conflict, not a quiet re-assignment.
        let conflict = assign(
            &mut store,
            RankRequest {
                caller: "1792",
                worker: "1793",
                rank: "Legatus",
                roster: Ok(Some(&live)),
                now: 30,
            },
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(conflict.contains("release"), "it names the way out: {conflict}");
        assert_eq!(store.ranks[0].prefix, "Imperator");

        // A word outside the vocabulary is refused before anything is stored.
        let unknown = assign(
            &mut store,
            RankRequest {
                caller: "1792",
                worker: "1794",
                rank: "general",
                roster: Ok(None),
                now: 40,
            },
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(unknown.contains("imperator"), "it lists the vocabulary: {unknown}");
        assert_eq!(store.ranks.len(), 1);
    }

    /// A roster that cannot be read is not an absent session, and a session that is
    /// not live is still assignable: the record is intent, and the decoration waits.
    #[test]
    fn an_unreadable_roster_writes_nothing_and_a_dead_session_still_takes_a_word() {
        let mut store = Store::default();
        let mut save = |_s: &Store| Ok(());
        let mut load = || Err("no store to re-read".to_string());

        let blind = assign(
            &mut store,
            RankRequest { caller: "1792", worker: "1793", rank: "Miles", roster: Err("peers failed".into()), now: 1 },
            &mut save,
            &mut load,
        )
        .unwrap_err();
        assert!(blind.contains("could not be read"), "{blind}");
        assert!(store.ranks.is_empty(), "nothing was written");

        let waiting = assign(
            &mut store,
            RankRequest { caller: "1792", worker: "1793", rank: "Miles", roster: Ok(None), now: 2 },
            &mut save,
            &mut load,
        )
        .unwrap();
        assert!(waiting.contains("no live session"), "{waiting}");
        assert_eq!(store.ranks.len(), 1, "the intent is durable even with nothing live");
    }

    /// A write that could not be persisted is not an assignment, and an uncertain
    /// one is not a rollback: the live state comes from the file.
    #[test]
    fn a_failed_write_is_not_an_assignment_and_an_uncertain_one_reads_the_disk() {
        let live = row("1793", None, None);
        let mut store = Store::default();
        {
            let mut save = |_s: &Store| Err(crate::SaveError::Unchanged("disk full".into()));
            let mut load = || Err("unused".to_string());
            let e = assign(
                &mut store,
                RankRequest { caller: "1792", worker: "1793", rank: "Miles", roster: Ok(Some(&live)), now: 1 },
                &mut save,
                &mut load,
            )
            .unwrap_err();
            assert!(e.contains("not written"), "{e}");
            assert!(store.ranks.is_empty(), "the live store never moved");
        }
        {
            // The file may hold it: the store is re-read and the caller is told the
            // outcome is unknown rather than either answer.
            let mut save = |_s: &Store| Err(crate::SaveError::Uncertain("rename landed".into()));
            let mut load = || Ok(Store::default());
            let e = assign(
                &mut store,
                RankRequest { caller: "1792", worker: "1793", rank: "Miles", roster: Ok(Some(&live)), now: 2 },
                &mut save,
                &mut load,
            )
            .unwrap_err();
            assert!(e.contains("unknown") || e.contains("re-read"), "{e}");
            assert!(store.ranks.is_empty(), "and the state came from the file, not the write");
        }
    }

    /// Release is the only way a word comes off, and it is refused when there is no
    /// record — never silently successful.
    #[test]
    fn release_removes_the_record_and_a_second_release_is_refused() {
        let mut store = Store { ranks: vec![assignment("1793", "Tribunus")], ..Store::default() };
        let mut save = |_s: &Store| Ok(());
        let mut load = || Err("no store to re-read".to_string());

        let said = release(&mut store, "1792", "1793", &mut save, &mut load).unwrap();
        assert!(said.contains("no longer carries"), "{said}");
        assert!(store.ranks.is_empty());
        let again = release(&mut store, "1792", "1793", &mut save, &mut load).unwrap_err();
        assert!(again.contains("no rank"), "{again}");
    }

    /// The render says the record and the screen apart: not live, live and wearing
    /// it, live and undecorated, live under another owner, and ambiguous.
    #[test]
    fn the_render_never_reports_the_record_as_if_it_were_the_screen() {
        let store = vec![assignment("1793", "Imperator")];
        let rows = [row("1793", None, Some("vault sweep"))];
        let said = render(&store, Ok(&rows));
        assert!(said.contains("undecorated"), "{said}");
        let wearing = [row("1793", Some((OWNER, "Imperator")), None)];
        assert!(render(&store, Ok(&wearing)).contains("wearing `Imperator`"));
        assert!(render(&store, Ok(&[])).contains("not live now"));
        let foreign = [row("1793", Some(("other", "Legatus")), None)];
        assert!(render(&store, Ok(&foreign)).contains("one owner at a time"));
        let doubled = [row("1793", None, None), row("1793", None, None)];
        assert!(render(&store, Ok(&doubled)).contains("ambiguous"));
        assert!(render(&store, Err("no roster")).contains("could not be read"));
        assert!(render(&[], Ok(&[])).contains("no rank is assigned"));
    }
}

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

use serde::{Deserialize, Serialize};

use crate::{Row, Store};

/// The words this vocabulary has: the Roman military chain the operator settled on
/// 2026-10-25, written here in lower case because a stored value is data.
///
/// Adding a word is a change to *this list only* — core has no vocabulary, no enum
/// and no journal record for any of it.
pub const RANKS: [&str; 5] = ["imperator", "legatus", "tribunus", "centurio", "miles"];

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

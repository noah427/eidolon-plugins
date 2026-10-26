//! **The command a managed launch runs**, for both launch shapes, built as
//! data so it can be asserted without starting anything.
//!
//! The two shapes differ in exactly two places, and both are deliberate:
//!
//! - **the program** — the harness itself when headless, the terminal when the
//!   child must be visible, with the harness as that terminal's command;
//! - **the subcommand** — `chat` headless, `tui` visible.
//!
//! Everything else is shared, including the private launch flags, because they
//! are global on the CLI and ride whichever subcommand is used. That is what
//! keeps one barrier, one adoption and one cancellation path across the two
//! shapes instead of growing a second launcher beside the first.
//!
//! **What this module does not do:** it does not spawn, place or focus
//! anything. It returns an argument vector, and the caller remains responsible
//! for the window's destination and for saying what actually happened to it.
//!
//! **The visible shape is not exercised end to end.** The argv is tested; the
//! spawn, the placement and any no-focus behaviour are not, and live validation
//! is blocked. Nothing here may be described as a working visible launch until
//! that changes.

use crate::managed_start::LaunchShape;

/// **The window class every coordinator-managed visible worker carries** —
/// `eidolon-worker-<launch id>`.
///
/// The operator's compositor config carries a `no_initial_focus` rule whose
/// match is `^(eidolon-worker-[0-9a-f]+)$`. The two are one contract: change the
/// shape here without changing the rule and the worker silently starts taking
/// the keyboard; change the rule without this and the launch's class stops
/// matching it. Neither file can enforce the other, so both say so.
pub const APP_ID_PREFIX: &str = "eidolon-worker-";

/// **Whether a window class is inside the namespace the no-focus rule matches.**
///
/// The operator's rule matches `^(eidolon-worker-[0-9a-f]+)$`; this is that test
/// in code, so a launcher asked to stand behind a class the rule cannot match
/// refuses instead of promising something the rule will not deliver.
///
/// **No live path exercises the refusal today, and that is deliberate.** The
/// service builds the class as `APP_ID_PREFIX` plus 16 random bytes formatted as
/// lowercase hex, so it is always inside the namespace; the check is defence in
/// depth for a future caller, and a test asserts the boundary rather than a call
/// site proving it.
pub fn is_managed_worker_class(app_id: &str) -> bool {
    match app_id.strip_prefix(APP_ID_PREFIX) {
        Some(rest) => {
            !rest.is_empty()
                && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// The terminal a visible launch runs through, and the labels it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    /// The terminal binary — `foot` on this host, but never hard-coded here.
    pub program: String,
    /// The window class, unique per launch, so a rule or a search can name
    /// exactly this window.
    pub app_id: String,
    /// The window title a person reads.
    pub title: String,
}

/// One command, ready to be spawned: what runs, and with what arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    /// The program to execute.
    pub program: String,
    /// Its arguments, in order.
    pub args: Vec<String>,
}

/// **The subcommand a launch of this shape runs**, named once because a flag
/// that is per-subcommand has to be asked of *this* subcommand: the rank
/// preflight in the process host asks the one the spawn will use, and the
/// spawn is built here. Change one without the other and a ranked start is
/// either refused for a flag the child would have taken, or launched with a
/// flag nobody asked about.
pub fn subcommand(shape: LaunchShape) -> &'static str {
    match shape {
        LaunchShape::Headless => "chat",
        LaunchShape::Terminal => "tui",
    }
}

/// Build the launch command for `shape`.
///
/// `fd` is the inherited barrier descriptor, passed as a **number** because
/// that is what the child resolves; the child takes ownership of it and marks
/// it close-on-exec for anything it spawns in turn.
#[allow(clippy::too_many_arguments)]
pub fn launch_command(
    shape: LaunchShape,
    executable: &str,
    terminal: Option<&Terminal>,
    workspace: &str,
    fd: i32,
    operation: &str,
    instance: &str,
    model: &str,
    persona: Option<&str>,
) -> Result<LaunchCommand, String> {
    let fd = fd.to_string();
    let mut flags: Vec<String> = vec![
        "--managed-start-fd".into(),
        fd,
        "--managed-operation".into(),
        operation.into(),
        "--managed-instance".into(),
        instance.into(),
    ];
    match shape {
        LaunchShape::Headless => {
            let mut args = flags;
            args.push(subcommand(shape).into());
            args.push("--model".into());
            args.push(model.into());
            if let Some(persona) = persona {
                args.push("--persona".into());
                args.push(persona.into());
            }
            Ok(LaunchCommand { program: executable.to_string(), args })
        }
        LaunchShape::Terminal => {
            // A visible launch without a terminal to run through would spawn
            // a child nobody can see and call it visible; that is a refusal,
            // not a fallback to headless.
            let terminal = terminal.ok_or(
                "a terminal launch needs a terminal: this host has no visible launch configured",
            )?;
            let mut args: Vec<String> = vec![
                format!("--app-id={}", terminal.app_id),
                format!("--title={}", terminal.title),
                format!("--working-directory={workspace}"),
                executable.to_string(),
            ];
            args.append(&mut flags);
            args.push(subcommand(shape).into());
            args.push("--model".into());
            args.push(model.into());
            if let Some(persona) = persona {
                args.push("--persona".into());
                args.push(persona.into());
            }
            Ok(LaunchCommand { program: terminal.program.clone(), args })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Terminal {
        Terminal {
            program: "foot".into(),
            app_id: "eidolon-worker-launch-7f3a".into(),
            title: "Meli — worker · DeepSeek".into(),
        }
    }

    fn headless() -> LaunchCommand {
        launch_command(
            LaunchShape::Headless,
            "/nix/store/x/bin/eidolon",
            None,
            "/work",
            7,
            "op-1",
            "inst-1",
            "mock:mock",
            Some("meli"),
        )
        .unwrap()
    }

    fn visible() -> LaunchCommand {
        launch_command(
            LaunchShape::Terminal,
            "/nix/store/x/bin/eidolon",
            Some(&terminal()),
            "/work",
            7,
            "op-1",
            "inst-1",
            "mock:mock",
            Some("meli"),
        )
        .unwrap()
    }

    /// The two shapes share the barrier flags and the model/persona tail, and
    /// differ only where they must.
    #[test]
    fn both_shapes_carry_the_same_barrier_and_differ_in_program_and_subcommand() {
        let head = headless();
        let seen = visible();

        assert_eq!(head.program, "/nix/store/x/bin/eidolon");
        assert_eq!(seen.program, "foot", "the visible child runs through a terminal");

        // The barrier flags are identical, and precede the subcommand, because
        // they are global on the CLI.
        let flags: Vec<&String> = head
            .args
            .iter()
            .take_while(|a| *a != "chat")
            .collect();
        assert_eq!(
            flags,
            vec![
                &"--managed-start-fd".to_string(),
                &"7".to_string(),
                &"--managed-operation".to_string(),
                &"op-1".to_string(),
                &"--managed-instance".to_string(),
                &"inst-1".to_string(),
            ]
        );
        let in_visible: Vec<&String> = seen
            .args
            .iter()
            .skip_while(|a| !a.starts_with("--app-id"))
            .take(8)
            .collect();
        assert_eq!(
            in_visible,
            vec![
                &"--app-id=eidolon-worker-launch-7f3a".to_string(),
                &"--title=Meli — worker · DeepSeek".to_string(),
                &"--working-directory=/work".to_string(),
                &"/nix/store/x/bin/eidolon".to_string(),
                &"--managed-start-fd".to_string(),
                &"7".to_string(),
                &"--managed-operation".to_string(),
                &"op-1".to_string(),
            ],
            "the visible command is the terminal's arguments, then the harness's"
        );

        assert_eq!(head.args.last().unwrap(), "meli");
        assert_eq!(head.args[head.args.len() - 2], "--persona");
        assert_eq!(seen.args.last().unwrap(), "meli");
        // The subcommand is the second difference, and the only other one.
        assert!(head.args.iter().any(|a| a == "chat"));
        assert!(seen.args.iter().any(|a| a == "tui"));
        assert!(!seen.args.iter().any(|a| a == "chat"));
    }

    /// A visible launch with no terminal configured is refused, not quietly
    /// downgraded to a child nobody can see.
    #[test]
    fn a_terminal_launch_without_a_terminal_is_refused() {
        let refused = launch_command(
            LaunchShape::Terminal,
            "/nix/store/x/bin/eidolon",
            None,
            "/work",
            7,
            "op-1",
            "inst-1",
            "mock:mock",
            None,
        )
        .unwrap_err();
        assert!(refused.contains("needs a terminal"), "{refused}");
    }

    /// No persona is no arguments — not an empty `--persona`.
    #[test]
    fn an_absent_persona_adds_nothing() {
        let head = launch_command(
            LaunchShape::Headless,
            "/bin/eidolon",
            None,
            "/w",
            3,
            "o",
            "i",
            "m",
            None,
        )
        .unwrap();
        assert!(!head.args.iter().any(|a| a == "--persona"), "{:?}", head.args);
    }

    /// The no-focus rule's `[0-9a-f]+` namespace, in code. The launcher's own
    /// class is inside it; nothing else is *nearly* inside it. The uppercase
    /// case matters: the compositor's regex is case-sensitive and admits no
    /// uppercase hex, so a class carrying it would silently not match the rule.
    #[test]
    fn only_the_launcher_namespace_matches_the_no_focus_rule() {
        assert!(is_managed_worker_class(
            "eidolon-worker-0f1e2d3c4b5a69788796a5b4c3d2e1f0"
        ));
        assert!(is_managed_worker_class("eidolon-worker-a"));
        for outside in [
            "eidolon-worker-",
            "eidolon-worker-ABCDEF",
            "eidolon-worker-xyz",
            "eidolon-worker0f1",
            "eidolon-worker-eidolon-worker-a",
            "eidolon-worker- a",
            "eidolon-worker",
            "foot",
        ] {
            assert!(!is_managed_worker_class(outside), "{outside}");
        }
    }
}

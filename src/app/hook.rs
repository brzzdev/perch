//! Telling the outside world that a worktree appeared or went away — the shell
//! commands configured as `perch.hook.created` and `perch.hook.removed`, per
//! [ADR 0003](../../docs/adr/0003-hooks-are-told-never-asked.md).
//!
//! A hook is told what happened and is never consulted: it cannot veto a
//! removal, cannot license a forcing, and its failure is warned about and
//! otherwise ignored. Nothing here returns a decision, which is the rule made
//! structural.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

#[cfg(not(test))]
use std::env;

use crate::git;

/// The moment being reported. The variant's name is both the `PERCH_EVENT`
/// value and the config key's last component, so the two can't drift.
#[derive(Clone, Copy)]
pub(crate) enum Event {
    Created,
    Removed,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Event::Created => "created",
            Event::Removed => "removed",
        }
    }
}

/// Runs the hook configured for `event`, if there is one.
///
/// `branch` is `None` for a detached worktree, which reaches the hook as an
/// empty `PERCH_BRANCH` rather than an absent one: the variable is part of
/// the payload either way, and a hook testing `-z` shouldn't have to know that
/// detached worktrees exist. `main` is both the `PERCH_MAIN` value and the
/// working directory, so a hook can run plain `git` commands and reach the repo
/// rather than a directory that may no longer exist.
///
/// The hook's stdout is captured and re-emitted on stderr: the handoff writes
/// the destination path to stdout for the shell wrapper to `cd` into, and an
/// inherited stdout would let a chatty hook send the user somewhere absurd. Its
/// stderr passes through untouched, and its stdin is closed — a hook is not a
/// prompt, so it has no business reading the user's keystrokes.
pub(crate) fn fire(event: Event, worktree: &Path, branch: Option<&str>, main: &Path) {
    if suppressed() {
        return;
    }
    let Some(command) = git::hook_command(event.name()) else {
        return;
    };

    let output = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(main)
        .env("PERCH_BRANCH", branch.unwrap_or_default())
        .env("PERCH_EVENT", event.name())
        .env("PERCH_MAIN", main)
        .env("PERCH_WORKTREE", worktree)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output();

    match output {
        Ok(output) => {
            let _ = io::stderr().write_all(&output.stdout);
            if !output.status.success() {
                warn(event, &describe(output.status));
            }
        }
        Err(e) => warn(event, &format!("could not be run: {e}")),
    }
}

/// Unit-test builds suppress hooks so they cannot run commands from the
/// developer's global git config. Otherwise, `PERCH_NO_HOOKS` set to anything
/// non-empty turns hooks off for scripts that drive `perch` themselves.
#[cfg(test)]
fn suppressed() -> bool {
    true
}

#[cfg(not(test))]
fn suppressed() -> bool {
    env::var_os("PERCH_NO_HOOKS").is_some_and(|v| !v.is_empty())
}

/// How a hook ended, for the warning line. A signal leaves no exit code.
fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited {code}"),
        None => "was killed by a signal".to_string(),
    }
}

/// Says a hook went wrong and that nothing follows from it — the whole of what
/// `perch` does about a hook it isn't happy with.
fn warn(event: Event, what: &str) {
    eprintln!(
        "{} the {} hook {what}; continuing.",
        super::warn(),
        event.name(),
    );
}

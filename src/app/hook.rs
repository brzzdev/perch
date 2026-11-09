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
use std::sync::OnceLock;

#[cfg(not(test))]
use std::env;

use super::warn;
use crate::git;
use crate::grammar::HookVariable;

/// The `--hook-env` pairs given to every hook this run fires.
static ENVIRONMENT: OnceLock<Vec<HookVariable>> = OnceLock::new();

pub(crate) fn set_environment(variables: Vec<HookVariable>) {
    let _ = ENVIRONMENT.set(variables);
}

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
/// The `--hook-env` pairs are set first, so the payload wins a clashing key.
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

    let extra = ENVIRONMENT.get().map_or(&[][..], Vec::as_slice);
    let output = prepare(&command, event, worktree, branch, main, extra)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output();

    match output {
        Ok(output) => {
            let _ = io::stderr().write_all(&output.stdout);
            if !output.status.success() {
                warn_failure(event, &describe(output.status));
            }
        }
        Err(e) => warn_failure(event, &format!("could not be run: {e}")),
    }
}

fn prepare(
    command: &str,
    event: Event,
    worktree: &Path,
    branch: Option<&str>,
    main: &Path,
    extra: &[HookVariable],
) -> Command {
    let mut hook = Command::new("sh");
    hook.arg("-c").arg(command).current_dir(main);
    for variable in extra {
        hook.env(variable.key(), variable.value());
    }
    hook.env("PERCH_BRANCH", branch.unwrap_or_default())
        .env("PERCH_EVENT", event.name())
        .env("PERCH_MAIN", main)
        .env("PERCH_WORKTREE", worktree);
    hook
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
fn warn_failure(event: Event, what: &str) {
    eprintln!("{} the {} hook {what}; continuing.", warn(), event.name());
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn the_payload_wins_every_clashing_hook_env_key() {
        let payload = [
            ("PERCH_BRANCH", "topic"),
            ("PERCH_EVENT", "removed"),
            ("PERCH_MAIN", "/main"),
            ("PERCH_WORKTREE", "/worktrees/topic"),
        ];
        let extra = payload
            .map(|(key, _)| HookVariable::parse(&format!("{key}=spoofed")).expect("valid pair"));

        let hook = prepare(
            "true",
            Event::Removed,
            Path::new("/worktrees/topic"),
            Some("topic"),
            Path::new("/main"),
            &extra,
        );

        let environment: HashMap<&OsStr, Option<&OsStr>> = hook.get_envs().collect();
        for (key, value) in payload {
            assert_eq!(
                environment[OsStr::new(key)],
                Some(OsStr::new(value)),
                "{key}"
            );
        }
    }
}

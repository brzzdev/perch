//! Every outcome line the destruction flows print, and the warnings they print
//! on the way there.
//!
//! [`removal`](super) decides what happens; this decides how it reads.
//! All local destruction flows — the stale-branch prompt that follows a switch,
//! `perch br rm`, and `perch wt rm` — hand their [`removal::Report`] to [`removal_outcome`]
//! and print what comes back, so the answer never depends on which command you
//! arrived through. The *Marker* glyphs a picker row draws instead belong to
//! [`marker`](super::marker).
//!
//! Nothing here runs a git process or writes to a stream — every function takes
//! values and returns the lines its caller prints — so the wording is asserted
//! against values rather than by running the binary.

use std::path::Path;

use console::{StyledObject, style};

use crate::app::removal::Risk;
use crate::app::{display_path, removal, shell_quote, warn};
use crate::git;

/// The glyph fronting a line that reports something done.
fn done() -> StyledObject<&'static str> {
    style("✓").green().bold()
}

/// The risks as they are *shown*, one warning line each — what stands in for the
/// markers when a target was named on the command line and no row was ever
/// drawn. Per [ADR 0001](../../../docs/adr/0001-warned-means-forceable.md) this is
/// the warning that licenses the forcing the confirmation then asks about.
pub(crate) fn warnings(risk: Risk, subject: &str, path: &Path) -> Vec<String> {
    describe(risk, subject, path)
        .into_iter()
        .map(|line| format!("{} {line}", warn()))
        .collect()
}

/// One line per risk, in prose. [`warnings`] renders these for the terminal;
/// the non-interactive refusal joins them into its error message instead, which
/// is why the bare wording is worth having on its own. `subject` names what the
/// risk is attached to.
pub(crate) fn describe(risk: Risk, subject: &str, path: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    if risk.dirty {
        lines.push(format!("{} has uncommitted changes", display_path(path)));
    }
    match risk.unmerged {
        Some(git::Unmerged::Ahead(n)) => {
            lines.push(format!("{subject} has {n} unmerged commit(s)"));
        }
        Some(git::Unmerged::NoUpstream) => {
            lines.push(format!("{subject} has unmerged commits and no upstream"));
        }
        None => {}
    }
    lines
}

/// The warning that licenses deleting a shared upstream ref. It is deliberately
/// separate from [`warnings`]: local merged-ness assumes the upstream remains,
/// so no local `Risk` or marker can speak for deleting it too.
pub(crate) fn upstream_warning(upstream: &git::RemoteBranch) -> String {
    format!(
        "{} deleting {}/{} removes a shared upstream ref and may remove the last name for its commits",
        warn(),
        upstream.remote,
        upstream.branch,
    )
}

pub(crate) fn upstream_outcome(
    upstream: &git::RemoteBranch,
    outcome: &git::RemoteBranchDeleteOutcome,
) -> String {
    let name = format!("{}/{}", upstream.remote, upstream.branch);
    match outcome {
        git::RemoteBranchDeleteOutcome::Deleted => {
            format!("{} deleted upstream {name}", done())
        }
        git::RemoteBranchDeleteOutcome::AlreadyAbsent => {
            format!("{} upstream {name} was already absent", done())
        }
        git::RemoteBranchDeleteOutcome::Moved { expected, now } => format!(
            "{} kept upstream {name}: it moved from {expected} to {now} after it was shown",
            warn(),
        ),
        git::RemoteBranchDeleteOutcome::Failed(detail) => {
            format!("{} could not delete upstream {name}: {detail}", warn())
        }
    }
}

pub(crate) fn upstream_kept_local(upstream: &git::RemoteBranch) -> String {
    format!(
        "{} kept upstream {}/{} because the local branch still exists",
        warn(),
        upstream.remote,
        upstream.branch,
    )
}

/// What a removal did, as the lines to print — the target says which steps could
/// have run, and the report says what each one did.
///
/// A branch kept for unmerged commits reads differently from one whose delete
/// failed, a failure carries git's own reason, and a kept branch carries the
/// `git branch -D` hint for forcing it by hand. The match is exhaustive in both
/// directions: a new outcome has to be worded rather than falling through to a
/// line that claims something else happened.
pub(crate) fn removal_outcome(report: &removal::Report<'_>) -> Vec<String> {
    match report.target {
        removal::Target::Branch { name } => report
            .branch
            .iter()
            .map(|outcome| branch_line("", name, outcome))
            .collect(),

        removal::Target::Held { name, path } => match &report.worktree {
            // A worktree that refused took the branch step with it, so say why
            // and stop — the branch is still there, holding whatever it held.
            Some(git::WorktreeRemoveOutcome::Failed(detail)) => {
                worktree_failure(path, Some(name), detail)
            }
            Some(git::WorktreeRemoveOutcome::Removed) => {
                // The worktree goes first, so one that went is reported
                // alongside whatever the branch did afterwards.
                let prefix = format!("removed worktree at {}, ", display_path(path));
                report
                    .branch
                    .iter()
                    .map(|outcome| branch_line(&prefix, name, outcome))
                    .collect()
            }
            // Unreachable: [`removal::remove`] always runs the worktree step for
            // a target that names one. Saying nothing is the safe reading if
            // that ever changes — better silence than a line claiming a removal
            // that never happened.
            None => Vec::new(),
        },

        removal::Target::Worktree { path } => match &report.worktree {
            Some(git::WorktreeRemoveOutcome::Failed(detail)) => {
                worktree_failure(path, None, detail)
            }
            // A detached worktree goes alone: there was no branch step to report.
            Some(git::WorktreeRemoveOutcome::Removed) => vec![format!(
                "{} removed worktree at {}",
                done(),
                display_path(path),
            )],
            // Unreachable, for the same reason as the `Held` arm above.
            None => Vec::new(),
        },
    }
}

/// A worktree that git refused to remove, with its own reason indented beneath.
/// `branch` names the branch left alone, where the worktree held one.
fn worktree_failure(path: &Path, branch: Option<&str>, detail: &str) -> Vec<String> {
    let at = display_path(path);
    let headline = match branch {
        Some(branch) => format!(
            "{} failed to remove the worktree at {at}; leaving {branch} alone:",
            warn(),
        ),
        None => format!("{} failed to remove the worktree at {at}:", warn()),
    };
    std::iter::once(headline)
        .chain(detail.lines().map(|line| format!("  {line}")))
        .collect()
}

/// Who a branch was being put back for, where that was actually established.
/// A worktree merely never ruled out is not one seen, and a line saying
/// otherwise reports a holder nobody found.
fn for_whom(holder: git::Holder) -> &'static str {
    match holder {
        git::Holder::Seen => " for the worktree now holding it",
        git::Holder::Unknown => "",
    }
}

/// What became of the branch, as one line. `prefix` carries the worktree that
/// already went, so success and failure both report it.
fn branch_line(prefix: &str, branch: &str, outcome: &git::BranchDeleteOutcome) -> String {
    match outcome {
        git::BranchDeleteOutcome::Deleted => {
            format!("{} {prefix}deleted {branch}", done())
        }
        // The branch went; its config wouldn't. Naming the keys is the whole
        // value of the line — the user is the only one who can clear them now.
        git::BranchDeleteOutcome::DeletedLeavingConfig(keys) => format!(
            "{} {prefix}deleted {branch}, but left config behind: {keys}",
            warn(),
        ),
        // Nothing is known to have survived here, only that nobody could look —
        // so the line says that, and not that something was left behind.
        git::BranchDeleteOutcome::DeletedConfigUnverified(detail) => format!(
            "{} {prefix}deleted {branch}, but couldn't check whether its config \
             went with it: {detail}",
            warn(),
        ),
        // The one outcome describing a repository that needs repair, so it leads
        // with what happened to the ref and hands over the command to undo it.
        // Who it was put back for is only said where a holder was actually seen.
        git::BranchDeleteOutcome::DeletedNotRestored {
            tip,
            detail,
            holder,
        } => format!(
            "{} {prefix}deleted {branch}, then couldn't put it back{}: {detail} \
             (restore it with `git branch -- {} {tip}`)",
            warn(),
            for_whom(*holder),
            shell_quote(branch),
        ),
        // The ref is there, so nothing needs repairing and nothing is offered to
        // repair it with — but the branch standing there is not the one proven,
        // and the user is the only one who can say whether that matters.
        git::BranchDeleteOutcome::DeletedThenRecreated { tip, now } => format!(
            "{} {prefix}deleted {branch} at {tip}, and something recreated it at {now}",
            warn(),
        ),
        // Nobody could look, so nothing is recommended: a `git branch` here
        // would be advice to recreate something that may already be standing.
        // A holder seen before any of that failed is still a holder seen.
        git::BranchDeleteOutcome::DeletedStateUnknown {
            tip,
            detail,
            holder,
        } => format!(
            "{} {prefix}deleted {branch} at {tip}, then couldn't put it back{} or read it \
             back: {detail} (check whether it exists before recreating it)",
            warn(),
            for_whom(*holder),
        ),
        git::BranchDeleteOutcome::NotMerged => format!(
            "{} {prefix}kept {branch} with unmerged commits \
             (run `git branch -D -- {}` to force-delete)",
            warn(),
            shell_quote(branch),
        ),
        git::BranchDeleteOutcome::Failed(detail) => {
            format!("{} {prefix}could not delete {branch}: {detail}", warn())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strips ANSI styling so assertions read as the user sees the line.
    fn plain(s: &str) -> String {
        console::strip_ansi_codes(s).into_owned()
    }

    fn plain_all(lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| plain(l)).collect()
    }

    fn held() -> removal::Target<'static> {
        removal::Target::Held {
            name: "fix/typo",
            path: Path::new("/tmp/wt"),
        }
    }

    fn report(
        target: removal::Target<'_>,
        worktree: Option<git::WorktreeRemoveOutcome>,
        branch: Option<git::BranchDeleteOutcome>,
    ) -> removal::Report<'_> {
        removal::Report {
            target,
            worktree,
            branch,
        }
    }

    /// A branch no worktree held has no worktree step, so nothing is claimed
    /// about one.
    #[test]
    fn a_branch_that_went_alone_names_only_itself() {
        let report = report(
            removal::Target::Branch { name: "fix/typo" },
            None,
            Some(git::BranchDeleteOutcome::Deleted),
        );
        assert_eq!(plain_all(&removal_outcome(&report)), ["✓ deleted fix/typo"]);
    }

    /// The worktree goes first, so a removal that happened is reported alongside
    /// whatever the branch did afterwards rather than on a line of its own.
    #[test]
    fn a_worktree_that_went_with_its_branch_is_named_too() {
        let report = report(
            held(),
            Some(git::WorktreeRemoveOutcome::Removed),
            Some(git::BranchDeleteOutcome::Deleted),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            ["✓ removed worktree at /tmp/wt, deleted fix/typo"]
        );
    }

    /// A detached worktree has no branch step, so nothing is claimed about one.
    #[test]
    fn a_detached_worktree_goes_alone() {
        let report = report(
            removal::Target::Worktree {
                path: Path::new("/tmp/wt"),
            },
            Some(git::WorktreeRemoveOutcome::Removed),
            None,
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            ["✓ removed worktree at /tmp/wt"]
        );
    }

    /// Kept and failed are different answers: this one says the branch is still
    /// there on purpose, and hands over the command that would force it.
    #[test]
    fn a_kept_branch_carries_the_hint_for_forcing_it() {
        let report = report(
            removal::Target::Branch {
                name: "spike/abandoned",
            },
            None,
            Some(git::BranchDeleteOutcome::NotMerged),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            ["! kept spike/abandoned with unmerged commits \
                 (run `git branch -D -- spike/abandoned` to force-delete)"]
        );
    }

    /// The hint is meant to be pasted into a shell, and git allows `$` and
    /// backticks in a ref name — so the name has to survive the paste as a
    /// literal rather than running.
    #[test]
    fn a_branch_name_that_could_run_as_a_command_is_quoted() {
        let report = report(
            removal::Target::Branch {
                name: "topic$(touch${IFS}/tmp/pwned)",
            },
            None,
            Some(git::BranchDeleteOutcome::NotMerged),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            [
                "! kept topic$(touch${IFS}/tmp/pwned) with unmerged commits \
                 (run `git branch -D -- 'topic$(touch${IFS}/tmp/pwned)'` to force-delete)"
            ]
        );
    }

    /// A branch that went while its config stayed is neither a clean deletion
    /// nor a failure: the line has to say both, and name the keys, because
    /// clearing them is now the user's job and nobody else knows they are there.
    #[test]
    fn config_outliving_its_branch_is_named_key_by_key() {
        let report = report(
            removal::Target::Branch { name: "feature" },
            None,
            Some(git::BranchDeleteOutcome::DeletedLeavingConfig(
                "branch.feature.remote, branch.feature.merge".into(),
            )),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            ["! deleted feature, but left config behind: \
              branch.feature.remote, branch.feature.merge"]
        );
    }

    /// Deleted-and-something-else is not the same answer as could-not-delete,
    /// and a headline claiming the branch is still there would send the user
    /// looking for a ref that has gone. Each of these leads with the deletion.
    #[test]
    fn every_deleted_outcome_leads_with_the_deletion() {
        let line = |outcome| {
            let report = report(
                removal::Target::Branch { name: "feature" },
                None,
                Some(outcome),
            );
            plain_all(&removal_outcome(&report)).join("")
        };

        assert_eq!(
            line(git::BranchDeleteOutcome::DeletedConfigUnverified(
                "fatal: bad config line 3".into()
            )),
            "! deleted feature, but couldn't check whether its config went with it: \
             fatal: bad config line 3"
        );
        assert_eq!(
            line(git::BranchDeleteOutcome::DeletedNotRestored {
                tip: "abc123".into(),
                detail: "cannot lock ref".into(),
                holder: git::Holder::Seen,
            }),
            "! deleted feature, then couldn't put it back for the worktree now holding it: \
             cannot lock ref (restore it with `git branch -- feature abc123`)"
        );
        assert_eq!(
            line(git::BranchDeleteOutcome::DeletedThenRecreated {
                tip: "abc123".into(),
                now: "def456".into(),
            }),
            "! deleted feature at abc123, and something recreated it at def456"
        );
    }

    /// A ref nobody could read is not a ref known to be missing, and the line
    /// that reports one must recommend nothing: telling the user to recreate a
    /// branch that may be standing there is worse than telling them to look.
    #[test]
    fn an_unreadable_ref_earns_no_repair_command() {
        let report = report(
            removal::Target::Branch { name: "feature" },
            None,
            Some(git::BranchDeleteOutcome::DeletedStateUnknown {
                tip: "abc123".into(),
                detail: "cannot lock ref; and reading it back failed too: not a git repository"
                    .into(),
                holder: git::Holder::Unknown,
            }),
        );
        let line = plain_all(&removal_outcome(&report)).join("");
        assert_eq!(
            line,
            "! deleted feature at abc123, then couldn't put it back or read it back: \
             cannot lock ref; and reading it back failed too: not a git repository \
             (check whether it exists before recreating it)"
        );
        assert!(
            !line.contains("git branch"),
            "nothing may be recommended over a ref nobody could look at: {line}"
        );
    }

    /// A holder seen before the restore failed is still a holder seen, whichever
    /// outcome the reading afterwards produces — the two lines say the same
    /// thing about the worktree and differ only in what they know of the ref.
    #[test]
    fn a_seen_holder_survives_a_lookup_that_failed_after_it() {
        let report = report(
            removal::Target::Branch { name: "feature" },
            None,
            Some(git::BranchDeleteOutcome::DeletedStateUnknown {
                tip: "abc123".into(),
                detail: "cannot lock ref".into(),
                holder: git::Holder::Seen,
            }),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            [
                "! deleted feature at abc123, then couldn't put it back for the worktree now \
              holding it or read it back: cannot lock ref \
              (check whether it exists before recreating it)"
            ]
        );
    }

    /// A worktree that was only ever *unruled-out* is not a worktree seen, and
    /// the line must not claim one — the restore is attempted either way.
    #[test]
    fn an_unobserved_holder_is_not_reported_as_one() {
        let report = report(
            removal::Target::Branch { name: "feature" },
            None,
            Some(git::BranchDeleteOutcome::DeletedNotRestored {
                tip: "abc123".into(),
                detail: "cannot lock ref".into(),
                holder: git::Holder::Unknown,
            }),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            [
                "! deleted feature, then couldn't put it back: cannot lock ref \
              (restore it with `git branch -- feature abc123`)"
            ]
        );
    }

    /// git knows why it refused and the user doesn't, so its reason is passed
    /// through rather than summarised away.
    #[test]
    fn a_failed_delete_surfaces_gits_reason() {
        let report = report(
            removal::Target::Branch { name: "old/thing" },
            None,
            Some(git::BranchDeleteOutcome::Failed(
                "error: cannot delete branch".into(),
            )),
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            ["! could not delete old/thing: error: cannot delete branch"]
        );
    }

    /// Git's reason is indented beneath the headline, one line per line of it.
    #[test]
    fn a_refusing_worktree_says_which_branch_it_kept_alive() {
        let report = report(
            held(),
            Some(git::WorktreeRemoveOutcome::Failed(
                "fatal: is locked\nreason: in use".into(),
            )),
            None,
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            [
                "! failed to remove the worktree at /tmp/wt; leaving fix/typo alone:",
                "  fatal: is locked",
                "  reason: in use",
            ]
        );
    }

    /// With no branch to keep alive, the headline says only what refused.
    #[test]
    fn a_refusing_detached_worktree_names_no_branch() {
        let report = report(
            removal::Target::Worktree {
                path: Path::new("/tmp/wt"),
            },
            Some(git::WorktreeRemoveOutcome::Failed(
                "fatal: is locked".into(),
            )),
            None,
        );
        assert_eq!(
            plain_all(&removal_outcome(&report)),
            [
                "! failed to remove the worktree at /tmp/wt:",
                "  fatal: is locked",
            ]
        );
    }

    /// The risk is attached to two different things, so each line names the one
    /// it belongs to: the worktree by path, the branch by name.
    #[test]
    fn describe_names_the_worktree_by_path_and_the_branch_by_name() {
        let risk = Risk {
            dirty: true,
            unmerged: Some(git::Unmerged::Ahead(2)),
        };
        assert_eq!(
            describe(risk, "feature", Path::new("/tmp/wt")),
            [
                "/tmp/wt has uncommitted changes",
                "feature has 2 unmerged commit(s)",
            ]
        );
    }

    #[test]
    fn describe_says_when_there_is_no_upstream_to_count_against() {
        let risk = Risk {
            dirty: false,
            unmerged: Some(git::Unmerged::NoUpstream),
        };
        assert_eq!(
            describe(risk, "feature", Path::new("/tmp/wt")),
            ["feature has unmerged commits and no upstream"]
        );
    }

    /// The warning the user is shown is the whole line, glyph included — the
    /// caller prints it rather than finishing it off.
    #[test]
    fn warnings_front_each_risk_with_the_warning_glyph() {
        let risk = Risk {
            dirty: true,
            unmerged: Some(git::Unmerged::Ahead(2)),
        };
        assert_eq!(
            plain_all(&warnings(risk, "feature", Path::new("/tmp/wt"))),
            [
                "! /tmp/wt has uncommitted changes",
                "! feature has 2 unmerged commit(s)",
            ]
        );
    }
}

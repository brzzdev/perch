//! Repository facts for shell completion.
//!
//! Grammar decides which facts a completion needs, then filters and renders the
//! result. This module only performs the Git reads requested by that query.

use crate::grammar::{Completion, CompletionSource};
use crate::{AppResult, git};

pub(crate) fn run(query: &Completion) -> AppResult<()> {
    let output = match query.source() {
        CompletionSource::LocalBranches => {
            let local = git::local_branches()?;
            query.render(local.iter().map(String::as_str))
        }
        CompletionSource::ReachableBranches => {
            let remote = git::current_remote(None, git::current_branch(None)?.as_deref());
            let (local, remote_only) = super::reachable_branches(&remote)?;
            query.render(local.iter().chain(&remote_only).map(String::as_str))
        }
        CompletionSource::Worktrees => {
            let worktrees = super::wt::removal_candidates()?;
            query.render(worktrees.iter().map(String::as_str))
        }
    };
    print!("{output}");
    Ok(())
}

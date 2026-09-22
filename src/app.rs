use std::path::{Path, PathBuf};

use console::{Key, Term, style};
use indicatif::ProgressBar;

use crate::grammar::{Invocation, Navigation, Verb, WorktreeDirectoryName};
use crate::{AppResult, Error, git};

pub mod br;
pub mod complete;
pub(crate) mod hook;
pub(crate) mod marker;
pub(crate) mod picker;
pub(crate) mod prefetch;
mod reclamation;
pub(crate) mod removal;
pub mod wt;

use picker::{
    Catalogue, MultiItem, PickerOptions, Selection, interactive_keys, multi_select, pick,
};

pub(crate) struct CursorGuard(Option<Term>);

impl CursorGuard {
    pub(crate) fn hide() -> Self {
        let term = Term::stderr();
        if term.is_term() {
            let _ = term.hide_cursor();
            Self(Some(term))
        } else {
            Self(None)
        }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        if let Some(term) = &self.0 {
            let _ = term.show_cursor();
        }
    }
}

/// The stderr terminal, but only when it's interactive. Returns `None` in
/// piped/CI runs where there's no TTY to drive a prompt — callers fall back to
/// doing nothing rather than blocking on key input.
fn interactive_term() -> Option<Term> {
    let term = Term::stderr();
    term.is_term().then_some(term)
}

impl Verb {
    /// The picker's prompt. All three verbs draw the same list, so the prompt is
    /// the only thing on screen saying what selecting a row will do.
    pub(crate) fn prompt(self) -> &'static str {
        match self {
            Verb::Go => "Switch to",
            Verb::Here => "Check out here",
            Verb::Worktree => "Worktree",
        }
    }
}

pub(crate) fn run_invocation(invocation: Invocation) -> AppResult<()> {
    if matches!(
        &invocation,
        Invocation::Navigate(Navigation::Worktree { .. })
            | Invocation::ListWorktrees
            | Invocation::RemoveWorktrees(_)
    ) {
        reclamation::retry();
    }

    match invocation {
        Invocation::Navigate(Navigation::Go(target)) => run(target.as_deref()),
        Invocation::Navigate(Navigation::Here(target)) => run_br(target.as_deref()),
        Invocation::Navigate(Navigation::Worktree {
            worktree_name,
            target,
            shell_handoff,
        }) => wt::run(
            worktree_name.as_ref().map(WorktreeDirectoryName::as_str),
            target.as_deref(),
            shell_handoff,
        ),
        Invocation::RemoveBranches(removal) => br::run_rm(&removal),
        Invocation::ListWorktrees => wt::run_ls(),
        Invocation::RemoveWorktrees(removal) => wt::run_rm(&removal),
        Invocation::Help(page) => {
            print!("{}", page.text());
            Ok(())
        }
        Invocation::Version => {
            println!("perch {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Invocation::Complete(query) => complete::run(&query),
    }
}

/// Run the private detached reclamation mode when requested by a child process.
/// `None` means this is an ordinary user invocation.
#[must_use]
pub fn run_internal_reclamation() -> Option<AppResult<()>> {
    reclamation::run_worker()
}

/// `perch [<branch>]` — take me to the branch, wherever it lives.
pub fn run(target: Option<&str>) -> AppResult<()> {
    run_verb(Verb::Go, target)
}

/// `perch br [<branch>]` — check the branch out here, in this worktree.
pub fn run_br(target: Option<&str>) -> AppResult<()> {
    run_verb(Verb::Here, target)
}

fn run_verb(verb: Verb, target: Option<&str>) -> AppResult<()> {
    let old_branch = git::current_branch()?;
    let remote = git::current_remote(old_branch.as_deref());

    // `perch .` refreshes the branch we're already on against its remote,
    // rather than switching anywhere.
    if target == Some(".") {
        let Some(current) = old_branch.as_deref() else {
            return Err(Error::Detached);
        };
        return refresh_current(&remote, current);
    }

    let target = if let Some(name) = target {
        name.to_string()
    } else {
        let listed = live_worktrees()?;
        let Some(picked) = select_branch(old_branch.as_deref(), &remote, &listed, verb)? else {
            return Ok(());
        };
        picked
    };

    // Read the worktrees again rather than reusing what the picker was drawn
    // from: that snapshot was taken before it opened, and the picker then sat
    // waiting on a keystroke. A worktree taken on the target in the meantime
    // would be missed here, and git refuses the checkout this would otherwise
    // attempt; one removed in the meantime would send the shell somewhere that
    // no longer exists.
    let worktrees = live_worktrees()?;

    // `git checkout` refuses for a branch already checked out in another
    // worktree; hand off to the shell wrapper instead.
    if old_branch.as_deref() != Some(target.as_str())
        && let Some(held_by) = git::worktree_for_branch(&worktrees, &target)
    {
        // `br` promises a checkout *here*, so it can't quietly `cd` elsewhere.
        if verb == Verb::Here {
            return Err(Error::HeldByWorktree {
                branch: target,
                path: display_path(&held_by.path),
            });
        }
        // The target may track a different remote than the current branch.
        let target_remote = git::current_remote(Some(target.as_str()));
        if let Err(e) = wt::update_in(&held_by.path, &target, &target_remote, None) {
            eprintln!(
                "{} update of {} failed: {e}",
                style("!").yellow().bold(),
                target,
            );
        }
        eprintln!(
            "{} {} is checked out at {}",
            style("→").cyan().bold(),
            target,
            display_path(&held_by.path)
        );
        // `target` is where we're about to hand off, so it must not be on offer.
        if let Err(e) = prompt_delete_stale_branches(None, Some(&target), &remote) {
            if e.is_interrupt() {
                return Err(e);
            }
            eprintln!(
                "{} stale-branch check failed: {e}",
                style("!").yellow().bold()
            );
        }
        handoff_cd(&held_by.path);
        return Ok(());
    }

    let stashed = if git::has_tracked_changes()? {
        git::stash_push()?;
        true
    } else {
        false
    };

    let result = switch_and_update(&target, old_branch.as_deref(), &remote);

    if stashed {
        if result.is_err()
            && let Some(old) = old_branch.as_deref()
        {
            eprintln!("Switching back to {old} and restoring stashed changes.");
            let _ = git::checkout(old);
        }
        report_stash_pop();
    }

    result
}

/// Pop the auto-stash and report the outcome — a clean restore, a conflict to
/// resolve, or an outright failure. Shared by the switch and refresh flows.
fn report_stash_pop() {
    match git::stash_pop() {
        Ok(git::StashPopOutcome::Clean) => {}
        Ok(git::StashPopOutcome::Conflict) => eprintln!(
            "Conflicts detected restoring stashed changes. Resolve them, then run `git stash drop` to clean up the stash entry."
        ),
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!(
                "Stash pop failed. Inspect `git status` and `git stash list` to recover manually."
            );
        }
    }
}

/// Outcome of the keep/discard prompt shown when `perch .` finds local
/// work that a refresh would otherwise overwrite.
enum RefreshChoice {
    /// Rebase local commits onto the remote, restoring any stashed edits.
    Keep,
    /// Hard-reset to the remote, discarding local commits and tracked edits.
    Discard,
}

/// Refresh the branch we're on against its remote (`perch .`).
///
/// With nothing to pull it just reports status. When the remote has new
/// commits, a clean tree integrates them with no prompt — fast-forwarding, or
/// (when the branch has diverged, e.g. after rebasing through a web UI)
/// rebasing local commits on top, which drops any already upstream and replays
/// genuine new work. A dirty tree would be disturbed by that, so there it
/// prompts to keep the uncommitted work (stash, rebase, restore) or discard it
/// (hard reset to the remote).
fn refresh_current(remote: &str, current: &str) -> AppResult<()> {
    let remote_ref = format!("{remote}/{current}");

    let has_remote = {
        let spinner = ProgressBar::new_spinner().with_message(format!("Fetching {remote}…"));
        let _cursor_guard = CursorGuard::hide();
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        let fetch_outcome = git::fetch(None, remote);
        let has_remote = git::remote_branch_exists(remote, current);

        spinner.finish_and_clear();

        report_fetch_failure(&fetch_outcome);
        has_remote
    };

    if !has_remote {
        report_update(&git::MergeReport::NoRemote);
        return Ok(());
    }

    let (ahead, behind) = git::ahead_behind_remote(remote, current)?;

    // Nothing incoming: report status and leave the working tree (and any
    // uncommitted edits) untouched.
    if behind == 0 {
        if ahead == 0 {
            report_update(&git::MergeReport::UpToDate);
        } else {
            eprintln!(
                "{current} is {ahead} {} ahead of {remote_ref}; nothing to pull.",
                plural(ahead, "commit")
            );
        }
        return Ok(());
    }

    // A dirty tree would be disturbed by integrating, so let the user decide.
    if git::has_tracked_changes()? {
        match prompt_keep_discard(current, &remote_ref, ahead, behind)? {
            Some(RefreshChoice::Keep) => keep_local_work(&remote_ref)?,
            Some(RefreshChoice::Discard) => {
                git::reset_hard(remote, current)?;
                eprintln!("Reset {current} to {remote_ref}.");
            }
            None => eprintln!("Left {current} unchanged."),
        }
        return Ok(());
    }

    // Clean tree: integrate seamlessly. A fast-forward when the branch hasn't
    // diverged; otherwise rebase local commits onto the remote.
    if ahead == 0 {
        match git::fast_forward_merge(None, current, remote)? {
            git::FastForwardResult::Merged(report) => report_update(&report),
            // History always fast-forwards here, so a refusal means an untracked
            // file is in the way (`has_tracked_changes` ignores those).
            git::FastForwardResult::Diverged => eprintln!(
                "{} couldn't fast-forward {current} to {remote_ref}; an untracked file is likely blocking it",
                style("!").yellow().bold()
            ),
        }
    } else {
        rebase_onto(&remote_ref)?;
    }
    Ok(())
}

/// Keep uncommitted work while integrating remote commits: stash the edits,
/// rebase onto `remote_ref` (a fast-forward when the branch hasn't diverged),
/// then restore the edits. Mirrors the auto-stash dance in [`run`] so conflicts
/// and pop failures are surfaced the same way.
fn keep_local_work(remote_ref: &str) -> AppResult<()> {
    git::stash_push()?;
    // A clean rebase replays the stash cleanly; an abort leaves the tree at the
    // original HEAD, so the stash still applies. Restore either way.
    let result = rebase_onto(remote_ref);
    report_stash_pop();
    result
}

/// `"{word}"` for one, `"{word}s"` otherwise.
fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Describe the incoming remote work alongside the dirty tree, then ask whether
/// to keep the uncommitted work or discard it. Reached only with a dirty tree
/// and commits to integrate. Returns `None` on cancel or in non-interactive
/// runs, where acting destructively without confirmation would be unsafe.
fn prompt_keep_discard(
    branch: &str,
    remote_ref: &str,
    ahead: u32,
    behind: u32,
) -> AppResult<Option<RefreshChoice>> {
    if ahead > 0 {
        eprintln!(
            "{branch} has diverged from {remote_ref}: {ahead} local {} not on the remote, {behind} on the remote not yet local.",
            plural(ahead, "commit")
        );
    } else {
        eprintln!(
            "{remote_ref} has {behind} new {}.",
            plural(behind, "commit")
        );
    }
    eprintln!("Working tree has uncommitted changes.");

    let Some(term) = interactive_term() else {
        return Ok(None);
    };

    eprint!(
        "{} {} {} ",
        style("?").green().bold(),
        style(format!("Refresh to {remote_ref}?")).bold(),
        style("[k]eep (stash & rebase) / [d]iscard (hard reset) / esc").dim(),
    );
    let _cursor_guard = CursorGuard::hide();
    loop {
        let choice = match term.read_key()? {
            Key::Char('k' | 'K') => Some(RefreshChoice::Keep),
            Key::Char('d' | 'D') => Some(RefreshChoice::Discard),
            Key::Char('n' | 'N') | Key::Escape => None,
            _ => continue,
        };
        eprintln!(
            "{}",
            match choice {
                Some(RefreshChoice::Keep) => "keep",
                Some(RefreshChoice::Discard) => "discard",
                None => "cancel",
            }
        );
        return Ok(choice);
    }
}

fn switch_and_update(target: &str, old_branch: Option<&str>, remote: &str) -> AppResult<()> {
    let already_on_target = old_branch.is_some_and(|b| b == target);

    if !already_on_target {
        git::checkout(target)?;
    }

    match fetch_and_ff(None, target, remote, None)? {
        git::FastForwardResult::Diverged => reconcile_diverged(target, remote)?,
        git::FastForwardResult::Merged(report) => report_update(&report),
    }

    // An in-place switch stays put, so there's no worktree to protect.
    prompt_delete_stale_branches(
        if already_on_target { None } else { old_branch },
        None,
        remote,
    )?;

    Ok(())
}

/// Fetch `remote` then fast-forward `branch` onto it, optionally inside the
/// worktree at `dir` (via `git -C`). Shows a spinner and surfaces fetch
/// failures; the caller decides how to handle the [`git::FastForwardResult`]
/// (the in-place switch offers a rebase on diverge; worktree updates don't).
/// Without a `fetched` covering `remote` this fetches unconditionally, which is
/// what every caller but `wt` needs.
pub(crate) fn fetch_and_ff(
    dir: Option<&std::path::Path>,
    branch: &str,
    remote: &str,
    fetched: Option<&prefetch::FetchedRemote>,
) -> AppResult<git::FastForwardResult> {
    let (fetch_outcome, merge_result) = {
        let spinner = ProgressBar::new_spinner().with_message(format!("Updating {branch}…"));
        let _cursor_guard = CursorGuard::hide();
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        let fetch_outcome = prefetch::fetch_unless_covered(dir, remote, fetched);
        let result = git::fast_forward_merge(dir, branch, remote);

        spinner.finish_and_clear();
        (fetch_outcome, result)
    };

    report_fetch_failure(&fetch_outcome);

    merge_result
}

/// Warn that a fetch failed (so callers know results may be stale), printing
/// git's detail lines. A no-op on success. Shared by the switch and refresh
/// flows, both of which fetch behind a spinner.
pub(crate) fn report_fetch_failure(outcome: &git::FetchOutcome) {
    let git::FetchOutcome::Failed(detail) = outcome else {
        return;
    };
    eprintln!(
        "{} fetch failed; results may be stale",
        style("!").yellow().bold()
    );
    for line in detail.lines() {
        eprintln!("  {line}");
    }
}

pub(crate) fn report_update(result: &git::MergeReport) {
    match result {
        git::MergeReport::UpToDate => eprintln!("Already up to date."),
        git::MergeReport::Pulled(1) => eprintln!("Pulled 1 commit."),
        git::MergeReport::Pulled(n) => eprintln!("Pulled {n} commits."),
        git::MergeReport::NoRemote => eprintln!("No remote tracking branch."),
    }
}

fn reconcile_diverged(branch: &str, remote: &str) -> AppResult<()> {
    let remote_ref = format!("{remote}/{branch}");
    eprintln!("Local branch has diverged from {remote_ref}.");

    if confirm(&format!("Rebase onto {remote_ref}?"), false)? != Confirmation::Accepted {
        eprintln!("{}", reconcile_hint(&remote_ref));
        return Err(Error::Diverged);
    }

    rebase_onto(&remote_ref)
}

/// The hint left behind when the user declines the rebase and has to reconcile
/// by hand. Kept apart from the printing so the wording can be tested without a
/// repo on disk.
fn reconcile_hint(remote_ref: &str) -> String {
    let quoted = shell_quote(remote_ref);
    format!("Run `git rebase -- {quoted}` or `git merge -- {quoted}` to reconcile.")
}

/// Rebase the current branch onto `remote_ref`, reporting an aborted rebase the
/// same way for every caller (refresh's keep path and the diverged-switch
/// reconcile).
fn rebase_onto(remote_ref: &str) -> AppResult<()> {
    match git::rebase(remote_ref)? {
        git::RebaseOutcome::Clean => Ok(()),
        git::RebaseOutcome::Aborted => {
            eprintln!("{}", aborted_rebase_hint(remote_ref));
            Err(Error::Diverged)
        }
    }
}

/// The hint left behind when a rebase conflicts and git puts the branch back
/// where it was. Kept apart from the printing for the same reason as
/// [`reconcile_hint`].
fn aborted_rebase_hint(remote_ref: &str) -> String {
    format!(
        "Rebase aborted due to conflicts. Run `git rebase -- {}` manually to reconcile.",
        shell_quote(remote_ref)
    )
}

/// The distinct ways a yes/no prompt can end, so a destructive caller can
/// preserve Escape as cancellation instead of treating it as an explicit no.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Confirmation {
    Accepted,
    Cancelled,
    Declined,
}

/// Reads a yes/no confirmation. An explicit answer returns `Accepted` or
/// `Declined`, Escape returns `Cancelled`, and a non-interactive call resolves
/// to the answer selected by `default_yes`.
pub(crate) fn confirm(prompt: &str, default_yes: bool) -> AppResult<Confirmation> {
    let Some(term) = interactive_term() else {
        return Ok(if default_yes {
            Confirmation::Accepted
        } else {
            Confirmation::Declined
        });
    };
    let hint = if default_yes {
        "[Y/n] / esc"
    } else {
        "[y/N] / esc"
    };
    eprint!(
        "{} {} {} ",
        style("?").green().bold(),
        style(prompt).bold(),
        style(hint).dim(),
    );
    let _cursor_guard = CursorGuard::hide();
    loop {
        let answer = match term.read_key()? {
            Key::Char('y' | 'Y') => Confirmation::Accepted,
            Key::Escape => Confirmation::Cancelled,
            Key::Enter if default_yes => Confirmation::Accepted,
            Key::Char('n' | 'N') | Key::Enter => Confirmation::Declined,
            _ => continue,
        };
        eprintln!(
            "{}",
            match answer {
                Confirmation::Accepted => "y",
                Confirmation::Cancelled => "cancel",
                Confirmation::Declined => "n",
            }
        );
        return Ok(answer);
    }
}

/// The worktrees a verb can actually reach. A *Missing* one is still registered
/// but gone from disk, so it can't be entered — and both the checkout and the
/// worktree path recreate it. For the list it therefore holds nothing.
pub(crate) fn live_worktrees() -> AppResult<Vec<git::Worktree>> {
    Ok(git::worktree_list()?
        .into_iter()
        .filter(|w| !w.prunable)
        .collect())
}

fn select_branch(
    current: Option<&str>,
    remote: &str,
    worktrees: &[git::Worktree],
    verb: Verb,
) -> AppResult<Option<String>> {
    let sections = picker::sections(&build_catalogue(current, remote, worktrees)?, verb);
    // Non-interactive (piped/CI): we can't prompt, so report nothing to switch
    // to rather than blocking on key input.
    let Some(keys) = interactive_keys() else {
        return Ok(None);
    };
    let selection = pick(
        current,
        &sections,
        PickerOptions {
            prompt: verb.prompt(),
            allow_create_from_filter: false,
        },
        keys,
    )?;
    Ok(selection.map(|s| match s {
        Selection::Existing(name) => name,
        Selection::Create(_) => unreachable!("create-from-filter disabled"),
    }))
}

/// Every branch a *Verb* can reach: the locals, and the ones that exist only on
/// `remote`. Returned as the two halves rather than one list, since the picker
/// draws them as separate sections. A remote it cannot read degrades to the
/// locals alone — a completion or a picker missing the remote half is worth
/// more than neither.
///
/// The single read behind the *Catalogue* and the completions both, so what the
/// picker lists is what TAB offers and what a named target resolves against.
pub(crate) fn reachable_branches(remote: &str) -> AppResult<(Vec<String>, Vec<String>)> {
    let local = git::local_branches()?;
    let remote_only = git::remote_only_branches(&local, remote).unwrap_or_default();
    Ok((local, remote_only))
}

/// Reads every branch the repo offers and pairs each with the worktree holding
/// it, if any. This is the whole of the git side of the list — grouping it into
/// sections and deciding what each row says belongs to [`picker::sections`],
/// which is pure and testable without a repo on disk.
pub(crate) fn build_catalogue(
    current: Option<&str>,
    remote: &str,
    worktrees: &[git::Worktree],
) -> AppResult<Catalogue> {
    let (local, remote_only) = reachable_branches(remote)?;

    if local.is_empty() && remote_only.is_empty() {
        return Err(Error::NoBranches);
    }

    Ok(Catalogue {
        current: current.map(str::to_string),
        // Once per worktree rather than once per row: `display_path` reads the
        // environment, and there are far fewer worktrees than branches.
        held: worktrees
            .iter()
            .filter_map(|w| w.branch.clone().map(|b| (b, display_path(&w.path))))
            .collect(),
        local,
        remote_only,
    })
}

/// Offers to delete stale branches, and the worktrees holding them.
///
/// `old_branch` is pre-ticked, being the branch just switched away from.
/// `destination` is the branch we're about to hand the shell off into: it is
/// never offered, since deleting it would remove the very worktree the caller
/// is about to `cd` to.
pub(crate) fn prompt_delete_stale_branches(
    old_branch: Option<&str>,
    destination: Option<&str>,
    remote: &str,
) -> AppResult<()> {
    // Non-interactive (piped/CI): nothing here can be asked, so ask git nothing
    // either. The equivalence probe below writes an object per candidate, and a
    // run that will never prompt has no use for the answer.
    if !is_interactive() {
        return Ok(());
    }

    let assessment = removal::assess(removal::Request::Stale(removal::StaleRequest::new(
        git::stale_branches(remote)?,
        git::worktree_list().unwrap_or_default(),
        remote,
        old_branch,
        destination,
    )))?;
    let Some(selection) = select_removal_locals(
        &assessment,
        None,
        false,
        "Delete stale branches (space to toggle, →/← all/none)",
    )?
    else {
        return Ok(());
    };
    let choice = selection.into_choice();
    let pending = assessment.choose(choice)?;
    pending
        .finish(removal::UpstreamChoice::keep(), removal::StderrReporter)
        .map_err(removal::FinishFailure::into_error)?;

    Ok(())
}

pub(crate) struct RemovalSelection<'a> {
    choice: removal::LocalChoice,
    offers: Vec<&'a removal::Offer>,
}

impl<'a> RemovalSelection<'a> {
    pub(crate) fn offers(&self) -> &[&'a removal::Offer] {
        &self.offers
    }

    pub(crate) fn into_choice(self) -> removal::LocalChoice {
        self.choice
    }
}

/// Turns a named target or picker interaction into the selected offers and the
/// opaque choice that Removal accepts. Callers supply only the verb-specific
/// picker prompt.
pub(crate) fn select_removal_locals<'a>(
    assessment: &'a removal::Assessment,
    target: Option<&str>,
    force: bool,
    prompt: &str,
) -> AppResult<Option<RemovalSelection<'a>>> {
    if let Some(name) = target {
        let named = assessment.named(name)?;
        let Some(choice) = named_removal_choice(&named, force)? else {
            return Ok(None);
        };
        return Ok(Some(RemovalSelection {
            choice,
            offers: vec![assessment.offer(named.id())],
        }));
    }

    if assessment.offers().is_empty() {
        return Ok(None);
    }

    let Some(keys) = interactive_keys() else {
        return Ok(None);
    };
    let items: Vec<MultiItem> = assessment.offers().iter().map(MultiItem::from).collect();
    let Some(selected) = multi_select(prompt, assessment.legend(), &items, keys)? else {
        return Ok(None);
    };
    if selected.is_empty() {
        return Ok(None);
    }
    let offers: Vec<&removal::Offer> = selected
        .into_iter()
        .map(|index| &assessment.offers()[index])
        .collect();
    let ids = offers.iter().map(|offer| offer.id()).collect();
    let choice = if force {
        removal::LocalChoice::forced_picked(ids)
    } else {
        removal::LocalChoice::picked(ids)
    };
    Ok(Some(RemovalSelection { choice, offers }))
}

fn named_removal_choice(
    named: &removal::NamedOffer,
    force: bool,
) -> AppResult<Option<removal::LocalChoice>> {
    if force {
        return Ok(Some(removal::LocalChoice::forced(named.id())));
    }
    if named.warnings().is_empty() {
        return Ok(Some(removal::LocalChoice::named(named.id())));
    }
    if !is_interactive() {
        return Err(Error::Unconfirmed(named.refusal().to_string()));
    }
    for warning in named.warnings() {
        eprintln!("{warning}");
    }
    if confirm(named.question(), false)? != Confirmation::Accepted {
        return Ok(None);
    }
    Ok(Some(removal::LocalChoice::named(named.id())))
}

/// Renders `word` so a shell reads it as the single literal it is. Git allows
/// `$`, backticks, `;` and `&` in a ref name, so a branch called
/// ``topic$(rm -rf ~)`` would otherwise run its own payload the moment someone
/// pasted a command we printed. Names needing nothing are returned bare, which
/// keeps the overwhelmingly common case readable; anything else is single-quoted,
/// where the only character with meaning is `'` itself.
///
/// Quoting alone doesn't cover a name that looks like an option, so the commands
/// built from this pass `--` before the ref as well.
/// How to spell `branch` as the argument to a bare `perch`, so that telling
/// someone to run it actually reaches the branch.
///
/// A branch named after a verb is read as that verb, and `perch wt` opens the
/// worktree picker rather than going anywhere — so those names need the `--`
/// escape hatch. Grammar uses the same command facts for this check, invocation
/// parsing, and completion filtering.
pub(crate) fn go_there_argument(branch: &str) -> String {
    let quoted = shell_quote(branch);
    if crate::grammar::needs_top_level_escape(branch) {
        format!("-- {quoted}")
    } else {
        quoted
    }
}

pub(crate) fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "._/@+-".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Contracts a leading home directory to `~` so paths stay readable in prompts.
pub(crate) fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// True when there's a terminal to show a warning on and read an answer from.
pub(crate) fn is_interactive() -> bool {
    interactive_term().is_some()
}

/// Hand a target directory to the shell wrapper, which reads it from stdout and
/// runs `cd`. When stdout is a terminal the wrapper isn't capturing it, so a
/// bare path would just be dumped to the screen with no `cd` — print an
/// actionable hint to stderr instead.
pub(crate) fn handoff_cd(path: &std::path::Path) {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        eprintln!(
            "{} shell integration not active — can't cd for you. Run:",
            style("!").yellow().bold(),
        );
        eprintln!("  cd {}", path.display());
        eprintln!("  (enable auto-cd: see README \"Shell integration\")");
    } else {
        println!("{}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_hint_names_both_ways_out() {
        assert_eq!(
            reconcile_hint("origin/main"),
            "Run `git rebase -- origin/main` or `git merge -- origin/main` to reconcile."
        );
    }

    #[test]
    fn aborted_rebase_hint_points_back_at_the_upstream() {
        assert_eq!(
            aborted_rebase_hint("origin/main"),
            "Rebase aborted due to conflicts. Run `git rebase -- origin/main` manually to reconcile."
        );
    }

    /// Both hints are meant to be pasted into a shell, and git allows `$` and
    /// backticks in a ref name — so the ref has to survive the paste as a
    /// literal rather than running.
    #[test]
    fn the_reconcile_hints_quote_a_ref_that_could_run_as_a_command() {
        let remote_ref = "origin/topic$(touch${IFS}/tmp/pwned)";
        assert_eq!(
            reconcile_hint(remote_ref),
            "Run `git rebase -- 'origin/topic$(touch${IFS}/tmp/pwned)'` \
             or `git merge -- 'origin/topic$(touch${IFS}/tmp/pwned)'` to reconcile."
        );
        assert_eq!(
            aborted_rebase_hint(remote_ref),
            "Rebase aborted due to conflicts. \
             Run `git rebase -- 'origin/topic$(touch${IFS}/tmp/pwned)'` manually to reconcile."
        );
    }

    #[test]
    fn shell_quote_leaves_an_ordinary_branch_name_bare() {
        assert_eq!(shell_quote("feature/login-2"), "feature/login-2");
    }

    #[test]
    fn shell_quote_wraps_anything_a_shell_would_interpret() {
        assert_eq!(shell_quote("topic;ls"), "'topic;ls'");
        assert_eq!(shell_quote("topic&&ls"), "'topic&&ls'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    /// A single quote can't be escaped inside single quotes, so it has to close
    /// the quoting, contribute an escaped `'`, and reopen.
    #[test]
    fn shell_quote_handles_a_quote_in_the_name() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn an_ordinary_branch_is_named_to_perch_bare() {
        assert_eq!(go_there_argument("feature"), "feature");
    }

    #[test]
    fn a_branch_named_after_a_verb_is_escaped_past_the_dispatcher() {
        assert_eq!(go_there_argument("br"), "-- br");
        assert_eq!(go_there_argument("wt"), "-- wt");
    }

    #[test]
    fn an_escaped_branch_is_still_shell_quoted() {
        assert_eq!(go_there_argument("a b"), "'a b'");
    }
}

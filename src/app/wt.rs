use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use console::{measure_text_width, style};
use indicatif::ProgressBar;

use super::picker::{PickerOptions, Selection, interactive_keys, pick};
use super::prefetch::{FetchedRemote, Prefetch, fetch_unless_covered};
use super::{
    CursorGuard, build_catalogue, display_path, fetch_and_ff, handoff_cd, hook, marker, picker,
    prompt_delete_stale_branches, removal, report_fetch_failure, report_update,
    select_removal_locals,
};
use crate::grammar::{ShellHandoff, Verb, WorktreeRemoval};
use crate::{AppResult, Error, git};

enum Action {
    UseExisting(git::Worktree),
    CreateForBranch(String),
    CreateNewBranch(String),
}

/// Whether the branch behind a name has to exist, which the fresh state alone
/// can't say — it reports what is there, not what was asked for. A name typed
/// at the shell, or into the filter behind "Create new", claims nothing, and
/// creating it is the point. A picked row claims the branch was there when the
/// *Catalogue* was drawn, so a branch that has gone since leaves nothing to
/// fall back to that the user chose.
#[derive(Clone, Copy, PartialEq)]
enum Existence {
    MayCreate,
    MustExist,
}

pub(crate) fn run(
    worktree_name: Option<&str>,
    target: Option<&str>,
    shell_handoff: ShellHandoff,
) -> AppResult<()> {
    let current_branch = git::current_branch()?;
    let remote = git::current_remote(current_branch.as_deref());

    // A worktree whose directory was deleted by hand can't be entered, so its
    // branch is one to (re)create. `worktree_add`/`checkout` prune the stale
    // registration when it gets in the way.
    let listed = super::live_worktrees()?;

    // Started before the *Catalogue* is read, so the round trip overlaps the
    // local reads and the wait for a pick. With no name and no terminal,
    // `select` gives up at once, and there would be nothing to join. The
    // catalogue may therefore be read while the fetch is updating refs, so its
    // local and remote-only halves can reflect slightly different moments;
    // `resolve_target` runs after the join and decides the final action.
    //
    // Not where any worktree has submodules. Each worktree keeps its own
    // submodule repositories, and a fetch recurses on demand only into those
    // where it runs, for gitlinks moved by the commits it fetched. Once the
    // prefetch has moved the shared remote refs, a worktree's own fetch finds
    // nothing new and never reaches its submodules. Without the prefetch each
    // fetch runs from the worktree it updates.
    let has_submodules = listed
        .iter()
        .any(|worktree| git::declares_submodules(&worktree.path))
        || git::holds_submodule_repositories();
    let prefetch = ((target.is_some() || super::is_interactive()) && !has_submodules)
        .then(|| Prefetch::start(&remote));

    let (branch, existence) = if let Some(name) = target {
        (name.to_string(), Existence::MayCreate)
    } else {
        let Some(picked) = select(&listed, current_branch.as_deref(), &remote)? else {
            return Ok(());
        };
        picked
    };

    // Joined before the worktrees are read again below, so a slow join cannot
    // make that snapshot stale in its turn.
    let fetched = prefetch.map(Prefetch::join).transpose()?;

    // Read the worktrees again before deciding what the branch needs: the list
    // above was drawn before the picker opened, and it then sat waiting on a
    // keystroke. A worktree taken on the branch in the meantime would send this
    // into `worktree add`, which git refuses; one removed would hand the shell a
    // path that is no longer there.
    let worktrees = super::live_worktrees()?;
    let main = main_of(&worktrees)?;
    let action = resolve_target(&branch, existence, &worktrees, &remote)?;

    // The branch comes back alongside the path so the stale prompt can leave the
    // targeted worktree alone.
    let (target_path, target_branch) = match action {
        Action::UseExisting(wt) => {
            let branch = wt.branch.clone().unwrap_or_default();
            // The worktree's branch may track a different remote than ours.
            let branch_remote = git::current_remote(Some(branch.as_str()));
            if let Err(e) = update_in(&wt.path, &branch, &branch_remote, fetched.as_ref()) {
                eprintln!(
                    "{} update of {} failed: {e}",
                    style("!").yellow().bold(),
                    branch,
                );
            }
            if shell_handoff == ShellHandoff::Emit {
                eprintln!(
                    "{} switched to worktree at {}",
                    style("→").cyan().bold(),
                    display_path(&wt.path)
                );
            } else {
                eprintln!(
                    "{} worktree for {branch} is at {}",
                    style("→").cyan().bold(),
                    display_path(&wt.path)
                );
            }
            (wt.path, branch)
        }
        Action::CreateForBranch(branch) => {
            let branch_remote = git::current_remote(Some(branch.as_str()));
            let path = create_worktree(
                &main.path,
                worktree_name.unwrap_or(&branch),
                &branch,
                None,
                &branch_remote,
                fetched.as_ref(),
            )?;
            (path, branch)
        }
        Action::CreateNewBranch(branch) => {
            let default = git::default_branch(&remote).ok_or_else(|| Error::Git {
                command: "worktree add".into(),
                message: format!("no default branch on {remote}; cannot pick a base"),
            })?;
            let base = format!("{remote}/{default}");
            let path = create_worktree(
                &main.path,
                worktree_name.unwrap_or(&branch),
                &branch,
                Some(&base),
                &remote,
                fetched.as_ref(),
            )?;
            (path, branch)
        }
    };

    // `create_worktree` has already fired the created hook, before the prompt
    // below: that prompt is interactive and an interrupt propagates as an
    // error, so a hook after it would never run on Ctrl-C — stranding a
    // worktree nothing was told about.
    if let Err(e) = prompt_delete_stale_branches(None, Some(&target_branch), &remote) {
        if e.is_interrupt() {
            return Err(e);
        }
        eprintln!(
            "{} stale-branch check failed: {e}",
            style("!").yellow().bold()
        );
    }
    if shell_handoff == ShellHandoff::Emit {
        handoff_cd(&target_path);
    }
    Ok(())
}

pub(crate) fn run_ls() -> AppResult<()> {
    let worktrees = git::worktree_list()?;
    let track = git::ahead_behind_map();

    let max_branch = worktrees
        .iter()
        .map(|w| w.branch.as_deref().unwrap_or("(detached)").len())
        .max()
        .unwrap_or(0);

    // Build the styled status segment per row up front so the path column can be
    // aligned to the widest one (ANSI codes inflate byte length, so pad by
    // visible width rather than relying on `{:width$}`).
    let statuses: Vec<String> = worktrees
        .iter()
        .map(|w| {
            let dirty = !w.prunable && git::worktree_dirty(&w.path);
            let (ahead, behind) = w
                .branch
                .as_deref()
                .and_then(|b| track.get(b).copied())
                .unwrap_or((0, 0));
            marker::worktree_status(dirty, ahead, behind)
        })
        .collect();
    let status_width = statuses
        .iter()
        .map(|s| measure_text_width(s))
        .max()
        .unwrap_or(0);

    for (w, status) in worktrees.iter().zip(&statuses) {
        let label = w.branch.as_deref().unwrap_or("(detached)");
        let main_mark = if w.is_main { "*" } else { " " };
        if status_width == 0 {
            println!("{main_mark} {label:max_branch$}  {}", w.path.display());
        } else {
            let pad = " ".repeat(status_width - measure_text_width(status));
            println!(
                "{main_mark} {label:max_branch$}  {status}{pad}  {}",
                w.path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn run_rm(options: &WorktreeRemoval) -> AppResult<()> {
    let worktrees = git::worktree_list()?;
    let cwd = env::current_dir()
        .ok()
        .and_then(|dir| dir.canonicalize().ok());
    let assessment = removal::assess(removal::Request::Worktrees(removal::WorktreeRequest::new(
        worktrees,
        cwd,
        options.target(),
        removal::Forcing::from(options.force()),
    )))?;
    if assessment.offers().is_empty() {
        eprintln!("No worktrees to remove.");
        return Ok(());
    }
    let Some(selection) = select_removal_locals(
        &assessment,
        options.target(),
        options.force(),
        "Remove worktrees (space to toggle, →/← all/none)",
    )?
    else {
        return Ok(());
    };
    let progress_message = removal_progress_message(selection.offers());
    let choice = selection.into_choice();
    let pending = assessment.choose(choice)?;
    let result = {
        let spinner = ProgressBar::new_spinner().with_message(progress_message);
        let _cursor_guard = CursorGuard::hide();
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));
        let result = pending.finish(
            removal::UpstreamChoice::keep(),
            RemovalProgress { spinner: &spinner },
        );
        spinner.finish_and_clear();
        result
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(failure) => {
            if let Some(path) = failure.handoff() {
                handoff_cd(path);
            }
            return Err(failure.into_error());
        }
    };
    if let Some(path) = outcome.handoff() {
        handoff_cd(path);
    }
    Ok(())
}

struct RemovalProgress<'a> {
    spinner: &'a ProgressBar,
}

impl removal::Reporter for RemovalProgress<'_> {
    fn emit(&mut self, line: String) {
        self.spinner.suspend(|| eprintln!("{line}"));
    }

    fn removed(&mut self, hook: removal::RemovedHook<'_>) {
        self.spinner.suspend(|| hook.fire());
    }
}

fn removal_progress_message(offers: &[&removal::Offer]) -> String {
    match offers {
        [offer] => format!("Removing {}…", offer.name()),
        offers => format!("Removing {} worktrees…", offers.len()),
    }
}

/// `wt rm --complete` — the names `wt rm` will accept, one per line, for the
/// shell completions to offer. It is [`removable`] and
/// [`removal::worktree_names`] and nothing else, the same worktree facts
/// Removal assesses for `run_rm`.
///
/// `.` is the one accepted target left off. It is a single character, every
/// shell already completes it as a path, and it names the worktree the cwd sits
/// in rather than any worktree in particular. Removal resolves it from the cwd,
/// and [`removal::worktree_names`] never sees it.
///
/// This returns unique raw names in worktree order. Grammar renders the
/// newline-separated answer after Git facts return to it.
pub(crate) fn removal_candidates() -> AppResult<Vec<String>> {
    let worktrees = git::worktree_list()?;
    let mut seen = HashSet::new();
    Ok(removable(&worktrees)
        .flat_map(|worktree| removal::worktree_names(worktree.branch.as_deref(), &worktree.path))
        .filter(|name| seen.insert(*name))
        .map(str::to_string)
        .collect())
}

/// Fetch + fast-forward `branch` in the worktree at `path`. Unlike the in-place
/// switch, a diverged branch is only reported (we don't drive an interactive
/// rebase in a worktree the user isn't sitting in).
pub(crate) fn update_in(
    path: &Path,
    branch: &str,
    remote: &str,
    fetched: Option<&FetchedRemote>,
) -> AppResult<()> {
    match fetch_and_ff(Some(path), branch, remote, fetched)? {
        git::FastForwardResult::Diverged => eprintln!(
            "{} {} has diverged from {}/{}; not updating.",
            style("!").yellow().bold(),
            branch,
            remote,
            branch,
        ),
        // A worktree branch with no upstream is unremarkable here — unlike the
        // in-place switch, stay quiet rather than printing "No remote…".
        git::FastForwardResult::Merged(git::MergeReport::NoRemote) => {}
        git::FastForwardResult::Merged(report) => report_update(&report),
    }
    Ok(())
}

/// Creates the worktree for `branch` and reports it — to the user, and to the
/// created hook. Every worktree `perch` creates comes through here, which
/// is what makes the hook a consequence of creation and of nothing else rather
/// than a line each creation arm has to remember.
///
/// Fetches `remote` first unless `fetched` covers it. A failed fetch is
/// reported and creation goes ahead regardless: a new branch made offline is
/// legitimate.
fn create_worktree(
    main_path: &Path,
    worktree_name: &str,
    branch: &str,
    base: Option<&str>,
    remote: &str,
    fetched: Option<&FetchedRemote>,
) -> AppResult<PathBuf> {
    let path = worktree_path_for(main_path, worktree_name)?;
    ensure_path_clear(&path)?;
    ensure_parent(&path);

    let (fetch_outcome, result) = {
        let spinner = ProgressBar::new_spinner();
        let _g = CursorGuard::hide();
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));
        spinner.set_message(format!("Fetching {remote}…"));
        let fetch_outcome = fetch_unless_covered(None, remote, fetched);
        spinner.set_message(format!("Creating worktree for {branch}…"));
        let outcome = git::worktree_add(&path, branch, base);
        spinner.finish_and_clear();
        (fetch_outcome, outcome)
    };
    report_fetch_failure(&fetch_outcome);
    result?;

    if let Some(base) = base {
        eprintln!(
            "{} created {} from {} at {}",
            style("✓").green().bold(),
            branch,
            base,
            path.display()
        );
    } else {
        eprintln!(
            "{} created worktree at {}",
            style("✓").green().bold(),
            path.display()
        );
    }
    hook::fire(hook::Event::Created, &path, Some(branch), main_path);
    Ok(path)
}

/// Which branch the user picked, by name, and what picking it claimed. Deciding
/// what that branch *needs* is deliberately not done here: the answer depends on
/// which worktrees exist, and this returns while the snapshot behind the list is
/// going stale. The name and the claim are the two parts of the answer that
/// can't go stale, because they are the user's, not git's — [`resolve_target`]
/// re-reads everything else.
fn select(
    worktrees: &[git::Worktree],
    current_branch: Option<&str>,
    remote: &str,
) -> AppResult<Option<(String, Existence)>> {
    let catalogue = build_catalogue(current_branch, remote, worktrees)?;
    let sections = picker::sections(&catalogue, Verb::Worktree);
    // Non-interactive (piped/CI): we can't prompt, so report nothing to open
    // rather than blocking on key input.
    let Some(keys) = interactive_keys() else {
        return Ok(None);
    };
    let selection = pick(
        current_branch,
        &sections,
        PickerOptions {
            prompt: Verb::Worktree.prompt(),
            allow_create_from_filter: true,
        },
        keys,
    )?;
    Ok(selection.map(|s| match s {
        Selection::Create(name) => (name, Existence::MayCreate),
        Selection::Existing(name) => (name, Existence::MustExist),
    }))
}

/// What the branch needs, decided against state read after the pick. Falling
/// through every lookup means no branch by that name exists anywhere, which is
/// a new branch when the name was typed and an error when it was picked: the
/// row said the branch was there, so its absence is news, not an instruction.
fn resolve_target(
    name: &str,
    existence: Existence,
    worktrees: &[git::Worktree],
    remote: &str,
) -> AppResult<Action> {
    if let Some(wt) = git::worktree_for_branch(worktrees, name) {
        return Ok(Action::UseExisting(wt));
    }
    let locals = git::local_branches()?;
    if locals.iter().any(|b| b == name) {
        return Ok(Action::CreateForBranch(name.to_string()));
    }
    let remote_only = git::remote_only_branches(&locals, remote).unwrap_or_default();
    if remote_only.iter().any(|b| b == name) {
        return Ok(Action::CreateForBranch(name.to_string()));
    }
    if existence == Existence::MustExist {
        return Err(Error::Vanished {
            branch: name.to_string(),
        });
    }
    Ok(Action::CreateNewBranch(name.to_string()))
}

/// The worktrees `wt rm` can act on: every one but the main worktree, which git
/// will not remove. Branchless (detached) and missing ones stay in — a worktree
/// whose directory was deleted by hand often shows up detached, and clearing its
/// dead registration is exactly what `wt rm` is for.
fn removable(worktrees: &[git::Worktree]) -> impl Iterator<Item = &git::Worktree> {
    worktrees.iter().filter(|w| !w.is_main)
}

fn main_of(worktrees: &[git::Worktree]) -> AppResult<&git::Worktree> {
    worktrees
        .iter()
        .find(|w| w.is_main)
        .ok_or_else(|| Error::Git {
            command: "worktree list".into(),
            message: "no main worktree found".into(),
        })
}

fn worktree_path_for(main_path: &Path, worktree_name: &str) -> AppResult<PathBuf> {
    let parent = main_path.parent().ok_or_else(|| Error::Git {
        command: "worktree".into(),
        message: format!("main worktree has no parent: {}", main_path.display()),
    })?;
    let repo_name = main_path.file_name().ok_or_else(|| Error::Git {
        command: "worktree".into(),
        message: format!("cannot determine repo name from {}", main_path.display()),
    })?;
    Ok(parent.join("worktrees").join(repo_name).join(worktree_name))
}

fn ensure_path_clear(path: &Path) -> AppResult<()> {
    if path.exists() {
        return Err(Error::Git {
            command: "worktree add".into(),
            message: format!(
                "{} exists but is not a registered worktree; remove it manually and retry.",
                path.display()
            ),
        });
    }
    Ok(())
}

fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(branch: Option<&str>, path: &str) -> git::Worktree {
        git::Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            is_main: false,
            prunable: false,
        }
    }

    /// A nested branch name gives a directory named after its last segment
    /// alone, so the two names differ and both have to answer — this is the gap
    /// the awk in the completions had, offering only one of them.
    #[test]
    fn a_worktree_answers_to_its_branch_and_to_its_directory() {
        let w = worktree(Some("feat/login"), "/tmp/worktrees/repo/feat/login");
        assert_eq!(
            removal::worktree_names(w.branch.as_deref(), &w.path).collect::<Vec<_>>(),
            ["feat/login", "login"]
        );
    }

    /// A detached or missing worktree has no branch, which is exactly when the
    /// directory name is the only handle on it.
    #[test]
    fn a_branchless_worktree_answers_only_to_its_directory() {
        let w = worktree(None, "/tmp/worktrees/repo/detached");
        assert_eq!(
            removal::worktree_names(w.branch.as_deref(), &w.path).collect::<Vec<_>>(),
            ["detached"]
        );
    }
}

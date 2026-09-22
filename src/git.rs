use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::ffi::OsStr;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[cfg(unix)]
use crate::session;
use crate::{AppResult, Error};

pub enum MergeReport {
    UpToDate,
    Pulled(u32),
    NoRemote,
}

pub enum FastForwardResult {
    Merged(MergeReport),
    Diverged,
}

pub enum StashPopOutcome {
    Clean,
    Conflict,
}

pub enum FetchOutcome {
    Ok,
    Failed(String),
}

pub enum RebaseOutcome {
    Clean,
    Aborted,
}

pub fn current_branch() -> AppResult<Option<String>> {
    let output = run(&["branch", "--show-current"])?;
    let name = output.trim().to_string();
    Ok(if name.is_empty() { None } else { Some(name) })
}

/// Resolves the remote name to use for fetch, merge, and default-branch
/// detection. Prefers `branch.<current>.remote`, then the sole configured
/// remote, falling back to `origin`.
#[must_use]
pub fn current_remote(current: Option<&str>) -> String {
    if let Some(branch) = current
        && let Ok(output) = run(&["config", "--get", &format!("branch.{branch}.remote")])
        && let Some(name) = output.lines().next().map(str::trim)
        && !name.is_empty()
        // `.` means push to the local repo — useless for fetch/merge.
        && name != "."
    {
        return name.to_string();
    }

    if let Ok(output) = run(&["remote"]) {
        let mut remotes = output.lines().filter(|l| !l.is_empty());
        if let Some(only) = remotes.next()
            && remotes.next().is_none()
        {
            return only.to_string();
        }
    }

    "origin".to_string()
}

pub fn local_branches() -> AppResult<Vec<String>> {
    let output = run(&["branch", "--format=%(refname:short)"])?;
    let branches = output.lines().map(String::from).collect();
    Ok(branches)
}

/// The explicitly configured upstream of a local branch, where it is published
/// under the same name. A branch tracking `origin/main` as its base is not the
/// upstream removal candidate for a local branch named `feature`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamRef {
    pub remote: String,
    pub branch: String,
}

/// A live upstream ref, including the server tip a leased deletion must still
/// find when it runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBranch {
    pub remote: String,
    pub branch: String,
    pub tip: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpstreamInspection {
    Absent(UpstreamRef),
    Default(UpstreamRef),
    DefaultUnknown(UpstreamRef),
    Removable(RemoteBranch),
}

/// The same-named, explicitly configured upstream of `branch`. No fallback
/// remote is guessed: a fetch default is not authority to delete a shared ref.
pub fn same_named_upstream(branch: &str) -> AppResult<Option<UpstreamRef>> {
    let local_ref = format!("refs/heads/{branch}");
    let output = run(&[
        "for-each-ref",
        "--format=%(refname:short)%09%(upstream:remotename)%09%(upstream:remoteref)",
        &local_ref,
    ])?;
    let expected_remote_ref = format!("refs/heads/{branch}");

    Ok(output.lines().find_map(|line| {
        let mut parts = line.split('\t');
        let name = parts.next()?;
        let remote = parts.next()?;
        let upstream_ref = parts.next()?;
        (name == branch
            && !remote.is_empty()
            && remote != "."
            && upstream_ref == expected_remote_ref)
            .then(|| UpstreamRef {
                remote: remote.to_string(),
                branch: branch.to_string(),
            })
    }))
}

/// Reads an upstream from the server rather than trusting a possibly stale
/// remote-tracking ref. The same call obtains the server's default branch so it
/// can never be offered for deletion.
pub fn inspect_upstream(upstream: &UpstreamRef) -> AppResult<UpstreamInspection> {
    let refname = format!("refs/heads/{}", upstream.branch);
    let output = run(&[
        "ls-remote",
        "--symref",
        "--",
        &upstream.remote,
        "HEAD",
        &refname,
    ])?;
    let mut default = None;
    let mut tip = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("ref: ")
            && let Some((target, "HEAD")) = rest.split_once('\t')
        {
            default = Some(target);
        } else if let Some((oid, found)) = line.split_once('\t')
            && found == refname
        {
            tip = Some(oid);
        }
    }

    let Some(tip) = tip else {
        return Ok(UpstreamInspection::Absent(upstream.clone()));
    };
    match default {
        Some(default) if default == refname => Ok(UpstreamInspection::Default(upstream.clone())),
        Some(_) => Ok(UpstreamInspection::Removable(RemoteBranch {
            remote: upstream.remote.clone(),
            branch: upstream.branch.clone(),
            tip: tip.to_string(),
        })),
        None => Ok(UpstreamInspection::DefaultUnknown(upstream.clone())),
    }
}

pub fn remote_only_branches(local: &[String], remote: &str) -> AppResult<Vec<String>> {
    let prefix = format!("refs/remotes/{remote}/");
    let strip = format!("{remote}/");
    let output = run(&["for-each-ref", "--format=%(refname:short)", &prefix])?;
    let locals: HashSet<&str> = local.iter().map(String::as_str).collect();
    let branches = output
        .lines()
        .filter(|r| !r.ends_with("/HEAD"))
        .filter_map(|r| r.strip_prefix(&strip))
        .filter(|name| !locals.contains(name))
        .map(String::from)
        .collect();
    Ok(branches)
}

pub fn has_tracked_changes() -> AppResult<bool> {
    let output = run(&["status", "--porcelain", "--untracked-files=no"])?;
    Ok(!output.is_empty())
}

pub fn stash_push() -> AppResult<()> {
    run(&["stash", "push", "--quiet", "-m", "perch: auto-stash"])?;
    Ok(())
}

pub fn stash_pop() -> AppResult<StashPopOutcome> {
    let output = Command::new("git").args(["stash", "pop"]).output()?;
    if output.status.success() {
        return Ok(StashPopOutcome::Clean);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stdout.contains("CONFLICT") || stderr.contains("CONFLICT") {
        return Ok(StashPopOutcome::Conflict);
    }

    let stdout_trimmed = stdout.trim();
    let detail = if stdout_trimmed.is_empty() {
        stderr.trim()
    } else {
        stdout_trimmed
    };
    Err(Error::Git {
        command: "stash pop".to_string(),
        message: detail.to_string(),
    })
}

pub fn checkout(branch: &str) -> AppResult<()> {
    let output = Command::new("git")
        .args(["checkout", branch, "--quiet"])
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The branch is held by a worktree whose directory is gone. Clear the dead
    // registration and retry; a live worktree survives prune, so this is a
    // no-op there and the original error still surfaces.
    if is_stale_worktree_error(&stderr) {
        let _ = worktree_prune();
        run(&["checkout", branch, "--quiet"])?;
        return Ok(());
    }

    if !stderr.contains("Submodule") || !stderr.contains("could not be updated") {
        return Err(Error::Git {
            command: "checkout".to_string(),
            message: stderr.trim().to_string(),
        });
    }

    // Submodule checkout failed — likely missing objects. Fetch inside
    // each submodule and retry once.
    let _ = Command::new("git")
        .args(["submodule", "foreach", "--recursive", "git", "fetch"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    run(&["checkout", branch, "--quiet"])?;
    Ok(())
}

#[must_use]
pub fn fetch(dir: Option<&Path>, remote: &str) -> FetchOutcome {
    let output = match git_cmd(dir).args(fetch_args(remote)).output() {
        Ok(output) => output,
        Err(e) => return FetchOutcome::Failed(e.to_string()),
    };
    if output.status.success() {
        return FetchOutcome::Ok;
    }
    FetchOutcome::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

/// Starts `git fetch` without waiting for it, for a caller with something to
/// do meanwhile. The child leads its own session, so it has no terminal to
/// prompt on, and `GIT_TERMINAL_PROMPT=0` has git fail at once rather than try
/// stdin, which is `/dev/null`. Askpass programs are switched off too, since
/// they prompt without a terminal: an empty `GIT_ASKPASS` stands in for both
/// `core.askPass` and `SSH_ASKPASS` on git's side. On ssh's, an empty
/// `SSH_ASKPASS` names no program to run, which holds for an OpenSSH too old
/// to know `SSH_ASKPASS_REQUIRE` and for a `core.sshCommand` of the user's
/// own; left unset instead, OpenSSH would fall back to its default askpass.
/// `SSH_ASKPASS_REQUIRE=never` says the same to an OpenSSH that does know it.
/// A fetch that needs a person fails fast
/// into the foreground retry, which has the user's askpass back. Auth that
/// needs no person, an agent or a keychain helper, still works. Its output
/// goes nowhere: only whether it succeeded is ever read.
pub fn fetch_in_background(remote: &str) -> std::io::Result<Child> {
    let mut command = git_cmd(None);
    command
        .args(fetch_args(remote))
        .env("GIT_ASKPASS", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    session::detach(&mut command);
    command.spawn()
}

fn fetch_args(remote: &str) -> [&str; 4] {
    ["fetch", "--quiet", "--prune", remote]
}

/// The URL `remote` fetches from, as resolved in `dir`. The same remote name
/// can point elsewhere from another worktree, through `extensions.worktreeConfig`
/// or an `includeIf`, so where a fetch runs is part of what it fetches. `None`
/// where the remote has no URL there.
#[must_use]
pub fn remote_url(dir: Option<&Path>, remote: &str) -> Option<String> {
    run_in(dir, &["remote", "get-url", remote])
        .ok()
        .map(|url| url.trim().to_string())
}

/// Every config entry in force in `dir`, in the order git reads them. The
/// order is kept because it carries precedence: a scalar setting takes its
/// last value, so the same entries listed in another order can mean something
/// different.
///
/// Entries come back NUL-separated, which keeps a value containing a newline
/// whole — line-separated output would split it into two entries that no
/// longer say what the config does.
#[must_use]
pub fn config_entries(dir: Option<&Path>) -> Vec<String> {
    let Ok(output) = run_in(dir, &["config", "--null", "--list"]) else {
        return Vec::new();
    };
    output.split('\0').map(str::to_string).collect()
}

/// Rebase the current branch onto `onto` (e.g. `origin/main`). Git's stdout
/// and stderr stream directly to the terminal so users see progress and
/// conflict markers in real time. On failure the rebase is aborted, leaving
/// the working tree clean.
pub fn rebase(onto: &str) -> AppResult<RebaseOutcome> {
    let status = Command::new("git")
        .args(["-c", "advice.skippedCherryPicks=false", "rebase", onto])
        .status()?;
    if status.success() {
        return Ok(RebaseOutcome::Clean);
    }
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(RebaseOutcome::Aborted)
}

pub fn fast_forward_merge(
    dir: Option<&Path>,
    branch: &str,
    remote: &str,
) -> AppResult<FastForwardResult> {
    let remote_ref = format!("{remote}/{branch}");

    let has_remote = git_cmd(dir)
        .args(["rev-parse", "--verify", &remote_ref])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success();

    if !has_remote {
        return Ok(FastForwardResult::Merged(MergeReport::NoRemote));
    }

    let before = rev_parse(dir, "HEAD")?;

    // Capture stderr so git's diverging hint and "fatal: Not possible to
    // fast-forward" don't leak — we surface a tailored message instead.
    let output = git_cmd(dir)
        .args([
            "-c",
            "advice.diverging=false",
            "merge",
            "--ff-only",
            &remote_ref,
            "--quiet",
        ])
        .output()?;

    if !output.status.success() {
        return Ok(FastForwardResult::Diverged);
    }

    let after = rev_parse(dir, "HEAD")?;

    if before == after {
        return Ok(FastForwardResult::Merged(MergeReport::UpToDate));
    }

    let output = run_in(dir, &["rev-list", "--count", &format!("{before}..{after}")])?;
    let count: u32 = output.trim().parse()?;
    Ok(FastForwardResult::Merged(MergeReport::Pulled(count)))
}

/// True if `<remote>/<branch>` exists as a remote-tracking ref.
#[must_use]
pub fn remote_branch_exists(remote: &str, branch: &str) -> bool {
    let remote_ref = format!("{remote}/{branch}");
    git_cmd(None)
        .args(["rev-parse", "--verify", &remote_ref])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `(ahead, behind)` for `HEAD` relative to `<remote>/<branch>`: how many
/// commits are local-only (a hard reset would discard them) and how many the
/// remote has that aren't local. A rebase or amend gives commits new SHAs, so
/// rewritten-but-equivalent work still counts as ahead.
pub fn ahead_behind_remote(remote: &str, branch: &str) -> AppResult<(u32, u32)> {
    let range = format!("{remote}/{branch}...HEAD");
    let output = run(&["rev-list", "--left-right", "--count", &range])?;
    // `--left-right --count` prints "<left>\t<right>": left is reachable from
    // the remote only (behind), right from HEAD only (ahead).
    let mut counts = output.split_whitespace();
    let behind = counts.next().unwrap_or("0").parse()?;
    let ahead = counts.next().unwrap_or("0").parse()?;
    Ok((ahead, behind))
}

/// Hard-reset the current branch and working tree to `<remote>/<branch>`,
/// discarding local commits and tracked changes. Untracked files are left
/// alone, matching plain `git reset --hard`.
pub fn reset_hard(remote: &str, branch: &str) -> AppResult<()> {
    let remote_ref = format!("{remote}/{branch}");
    run(&["reset", "--hard", &remote_ref])?;
    Ok(())
}

/// What the staleness clauses measure against: the default branch's short name
/// and the commit it points at.
struct Anchor<'a> {
    branch: &'a str,
    tip: &'a str,
}

/// One `refs/heads/` entry: where the branch points and what it tracks.
struct BranchRef<'a> {
    tip: &'a str,
    /// The branch this one tracks, as named *on the remote*
    /// (`refs/heads/main`), empty when untracked. Git resolves this itself, so
    /// it holds for remote names that contain a `/`.
    upstream_branch: &'a str,
    /// Git's `upstream:track` summary: `ahead 2, behind 1`, `gone`, or empty.
    track: &'a str,
}

impl BranchRef<'_> {
    /// The upstream was configured but no longer exists on the remote.
    fn gone(&self) -> bool {
        self.track == "gone"
    }

    /// True when this tracks the remote branch called `name` — on any remote,
    /// since a branch published to a second remote is no less published.
    fn tracks(&self, name: &str) -> bool {
        self.upstream_branch.strip_prefix("refs/heads/") == Some(name)
    }

    /// For a branch already known to be reachable from the anchor, the ref
    /// evidence that qualifies it as stale.
    fn ground_on(&self, anchor: &Anchor<'_>) -> Option<Ground> {
        // Cut from the anchor, as `wt` creates them. Commits it holds that the
        // published anchor doesn't are taken for work of its own, and being
        // merged into the local anchor for where that work went. A branch cut
        // from an anchor that was itself ahead inherits the same count without
        // having earned it; nothing in the refs separates the two, and
        // `stale_branches` documents that as a known cost.
        if self.tracks(anchor.branch) {
            return (parse_track(self.track).0 > 0).then_some(Ground::TracksAnchorAhead);
        }
        // Published under a name of its own: whether the anchor holds its work
        // is a question about two remote branches, which no local ref answers.
        if !self.upstream_branch.is_empty() {
            return None;
        }
        // Tracks nothing, and the anchor has moved past it.
        (self.tip != anchor.tip).then_some(Ground::UntrackedTipInAnchor)
    }
}

/// Every local branch's ref state. Refs are shared across worktrees, so this is
/// independent of which one the caller is standing in.
fn read_branch_refs() -> AppResult<String> {
    run(&[
        "for-each-ref",
        "--format=%(refname:short)%09%(objectname)%09%(upstream:remoteref)%09\
         %(upstream:track,nobracket)",
        "refs/heads/",
    ])
}

fn branch_refs(refs_output: &str) -> HashMap<&str, BranchRef<'_>> {
    refs_output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?;
            let tip = parts.next()?;
            let upstream_branch = parts.next()?;
            let track = parts.next().unwrap_or("");
            Some((
                name,
                BranchRef {
                    tip,
                    upstream_branch,
                    track,
                },
            ))
        })
        .collect()
}

/// The ref that staleness is judged against — the default branch, not whatever
/// branch the current worktree happens to be on.
///
/// The local copy comes first: it is what you merge into, so work merged
/// locally but not yet pushed still counts. Its remote counterpart stands in
/// where there is no local copy. When neither resolves there is nothing to
/// judge "merged" against, and callers stand the merged rule down rather than
/// falling back to `HEAD`.
fn merged_anchor(remote: &str) -> Option<(String, String)> {
    let default = default_branch(remote)?;
    let local = format!("refs/heads/{default}");
    if rev_parse(None, &local).is_ok() {
        return Some((local, default));
    }
    let remote_ref = format!("refs/remotes/{remote}/{default}");
    rev_parse(None, &remote_ref)
        .ok()
        .map(|_| (remote_ref, default))
}

/// Which ref fact put a branch on the cleanup prompt. The facts cannot overlap:
/// a deleted upstream is read first, and a branch whose upstream is gone is
/// never tested against the anchor.
///
/// A ground says why a branch is offered, never what deleting it would destroy,
/// so it is rendered as a word and never as a `Marker` — see [ADR
/// 0004](../docs/adr/0004-a-ground-is-not-a-marker.md).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ground {
    /// The upstream it tracked was deleted from the remote.
    Gone,
    /// It tracks the anchor's counterpart and is ahead of it.
    TracksAnchorAhead,
    /// It tracks nothing and its tip is reachable from, but not equal to, the
    /// anchor's tip.
    UntrackedTipInAnchor,
}

/// A stale branch and the ground it is stale on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleBranch {
    pub ground: Ground,
    pub name: String,
}

/// The rule half of [`stale_branches`], split out so it can be tested against
/// fixed git output. `merged` is the anchor's `--merged` list; `None` for the
/// anchor stands the merged rule down, leaving only deleted upstreams.
fn stale_from(
    refs: &HashMap<&str, BranchRef<'_>>,
    merged: &str,
    anchor: Option<&Anchor<'_>>,
) -> Vec<StaleBranch> {
    let mut stale: Vec<StaleBranch> = refs
        .iter()
        .filter(|(_, r)| r.gone())
        .map(|(name, _)| StaleBranch {
            ground: Ground::Gone,
            name: (*name).to_string(),
        })
        .collect();

    if let Some(anchor) = anchor {
        stale.extend(merged.lines().filter_map(|name| {
            let ground = refs
                .get(name)
                .filter(|branch| !branch.gone())?
                .ground_on(anchor)?;
            Some(StaleBranch {
                ground,
                name: name.to_string(),
            })
        }));
    }

    stale
}

/// Branches whose refs qualify them for the cleanup prompt.
///
/// Merged-ness alone doesn't settle it. Every branch reachable from the anchor
/// has its tip as its own merge-base with it, so a branch freshly cut from the
/// anchor is topologically indistinguishable from one whose commits were
/// fast-forwarded onto it — and, if it was cut from a merged topic, from one
/// whose commits arrived by merge commit too. Topology is therefore no evidence
/// at all here; only what a branch *tracks* is. Three ref facts qualify a
/// branch:
///
/// - its configured upstream was deleted;
/// - it tracks the anchor's counterpart and is *ahead* of it;
/// - it tracks nothing and its tip is reachable from, but not equal to, the
///   anchor's tip.
///
/// A branch cut from the anchor and never committed to satisfies neither anchor
/// ground, as long as the anchor it was cut from was level with what it tracks.
///
/// Where a branch is published under a name of its own, neither anchor ground
/// applies.
/// Whether the anchor holds its work is a question about two remote branches,
/// and no local ref answers it. Such a branch waits for its upstream to be
/// deleted, as a rebased or squashed one does — neither is an ancestor of the
/// anchor in any way this rule can read.
///
/// Both anchor grounds read a proxy, and both are wrong at the edges. They go
/// quiet where the branch comes to look untouched — an untracked one once the
/// anchor moves past it, a `wt` one once the anchor reaches its own upstream —
/// and they misfire where an untouched branch comes to look worked on: cut
/// from an anchor that was already behind, or already ahead. In each pair the
/// two branches carry identical refs, so the proxy is the whole of the
/// evidence.
/// [ADR 0002](../docs/adr/0002-staleness-is-anchored-to-the-default-branch.md)
/// records why that is accepted rather than guessed at.
pub fn stale_branches(remote: &str) -> AppResult<Vec<StaleBranch>> {
    let current = current_branch()?;
    let keep = kept_branches(remote);

    let refs_output = read_branch_refs()?;
    let refs = branch_refs(&refs_output);

    let resolved = merged_anchor(remote);
    let (merged, anchor_tip) = match &resolved {
        Some((anchor_ref, _)) => (
            run(&[
                "branch",
                "--format=%(refname:short)",
                "--merged",
                anchor_ref,
            ])?,
            rev_parse(None, anchor_ref)?,
        ),
        None => (String::new(), String::new()),
    };
    let anchor = resolved.as_ref().map(|(_, branch)| Anchor {
        branch,
        tip: &anchor_tip,
    });

    let mut branches = stale_from(&refs, &merged, anchor.as_ref());

    branches.retain(|b| current.as_deref() != Some(&b.name) && !keep.contains(&b.name));
    branches.sort_by(|a, b| a.name.cmp(&b.name));
    branches.dedup_by(|a, b| a.name == b.name);
    Ok(branches)
}

/// The shell command configured as `perch.hook.<event>`, or `None` when it's
/// unset or blank. Read straight from git config like `perch.keep`, so a global
/// hook and a per-repo override layer the way git says they do — `--get` yields
/// the last value, and last wins.
#[must_use]
pub fn hook_command(event: &str) -> Option<String> {
    let key = format!("perch.hook.{event}");
    let output = run(&["config", "--get", &key]).ok()?;
    let command = output.trim();
    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

#[must_use]
pub(crate) fn default_branch(remote: &str) -> Option<String> {
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    let prefix = format!("refs/remotes/{remote}/");
    if let Ok(output) = run(&["symbolic-ref", &head_ref]) {
        let trimmed = output.trim();
        if let Some(name) = trimmed.strip_prefix(prefix.as_str()) {
            return Some(name.to_string());
        }
    }
    None
}

/// Branches held back from the cleanup sweep: `perch.keep` entries, plus the
/// remote's default branch. Private, and read by [`stale_branches`] alone —
/// keeping is about the sweep and nothing else, so nothing that draws a picker
/// or removes a named branch may consult it. See ADR 0011.
fn kept_branches(remote: &str) -> HashSet<String> {
    let mut kept: HashSet<String> = run(&["config", "--get-all", "perch.keep"])
        .map(|o| o.lines().map(String::from).collect())
        .unwrap_or_default();
    if let Some(default) = default_branch(remote) {
        kept.insert(default);
    }
    kept
}

/// Detached worktrees have `branch: None`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_main: bool,
    /// Registered but its directory is gone (`git worktree prune` clears it).
    /// Such an entry still blocks checkout/add of its branch, but cannot be
    /// entered.
    pub prunable: bool,
}

/// Lists all worktrees for the current repo. Per `git worktree list
/// --porcelain`, the first record is the main worktree.
pub fn worktree_list() -> AppResult<Vec<Worktree>> {
    worktree_list_in(None)
}

/// [`worktree_list`], asked from `dir` rather than the process cwd — which
/// matters where the cwd may have just been removed, or may belong to another
/// repository entirely.
pub fn worktree_list_in(dir: Option<&Path>) -> AppResult<Vec<Worktree>> {
    let output = run_in(dir, &["worktree", "list", "--porcelain"])?;
    let mut worktrees: Vec<Worktree> = Vec::new();
    // Each porcelain record starts with a `worktree <path>` line; the attributes
    // that follow apply to it until the next such line. Build the record in place
    // and push it when the next one begins (or the output ends).
    let mut current: Option<Worktree> = None;

    for line in output.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            worktrees.extend(current.take());
            current = Some(Worktree {
                path: PathBuf::from(p),
                branch: None,
                is_main: worktrees.is_empty(),
                prunable: false,
            });
        } else if let Some(wt) = current.as_mut() {
            if let Some(b) = line.strip_prefix("branch refs/heads/") {
                wt.branch = Some(b.to_string());
            } else if line == "prunable" || line.starts_with("prunable ") {
                wt.prunable = true;
            }
        }
    }
    worktrees.extend(current);
    Ok(worktrees)
}

/// True if the worktree at `path` has uncommitted changes (tracked edits or
/// untracked, non-ignored files). A missing/unreadable path reports clean.
#[must_use]
pub fn worktree_dirty(path: &Path) -> bool {
    worktree_dirtiness(path).unwrap_or(false)
}

/// A fresh dirtiness reading, or `None` when Git could not inspect the
/// worktree. Callers taking a destructive fast path must distinguish that from
/// clean so Git's own guard remains in charge of unreadable state.
#[must_use]
pub fn worktree_dirtiness(path: &Path) -> Option<bool> {
    git_cmd(Some(path))
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
}

/// Whether the worktree contains an initialized submodule. Git refuses to
/// remove such a worktree even when its status is clean, so a destructive fast
/// path must keep that guard in charge. This reads gitlinks from the index
/// rather than `.gitmodules`, which may legitimately omit one Git can remove.
/// `None` means Git or the filesystem could not inspect it.
#[must_use]
pub fn worktree_has_initialized_submodules(path: &Path) -> Option<bool> {
    let output = git_cmd(Some(path))
        .args(["ls-files", "--stage", "-z"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;

    for entry in output.stdout.split(|byte| *byte == b'\0') {
        let Some(rest) = entry.strip_prefix(b"160000 ") else {
            continue;
        };
        let tab = rest.iter().position(|byte| *byte == b'\t')?;
        #[cfg(unix)]
        let relative = PathBuf::from(OsStr::from_bytes(&rest[tab + 1..]));
        #[cfg(not(unix))]
        let relative = PathBuf::from(std::str::from_utf8(&rest[tab + 1..]).ok()?);
        match std::fs::symlink_metadata(path.join(relative).join(".git")) {
            Ok(_) => return Some(true),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }

    Some(false)
}

/// Maps each local branch to its (ahead, behind) commit counts versus its
/// upstream. Built from a single `for-each-ref` over the shared `refs/heads`,
/// so it costs one git call regardless of how many worktrees exist. Branches
/// with no upstream (or a `[gone]` one) are absent from the map.
#[must_use]
pub fn ahead_behind_map() -> HashMap<String, (u32, u32)> {
    let Ok(output) = run(&[
        "for-each-ref",
        "--format=%(refname:short)%09%(upstream:track,nobracket)",
        "refs/heads/",
    ]) else {
        return HashMap::new();
    };

    output
        .lines()
        .filter_map(|line| {
            let (name, track) = line.split_once('\t')?;
            let (ahead, behind) = parse_track(track);
            (ahead != 0 || behind != 0).then(|| (name.to_string(), (ahead, behind)))
        })
        .collect()
}

/// Parses git's `upstream:track,nobracket` field, e.g. `ahead 2, behind 1`.
fn parse_track(track: &str) -> (u32, u32) {
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// Branches currently checked out in any worktree (including the main one).
/// These cannot be deleted with `git branch -D`.
pub fn worktree_branches() -> AppResult<HashSet<String>> {
    Ok(worktree_list()?
        .into_iter()
        .filter_map(|w| w.branch)
        .collect())
}

/// A branch `git branch -d` would refuse to delete, and why there is or isn't a
/// commit count to show for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmerged {
    /// Ahead of a live upstream by this many commits.
    Ahead(u32),
    /// No upstream (or a `[gone]` one), so there is nothing to count against.
    NoUpstream,
}

/// Branches that `git branch -d` would refuse. Callers use this both to mark
/// rows and to decide when a force-delete is licensed, so it must mirror `-d`'s
/// rule exactly: *"the branch must be fully merged in its upstream branch, or
/// in HEAD if no upstream was set"*. The two are alternatives, not a pair —
/// where an upstream exists it alone decides, and a branch merged into HEAD but
/// ahead of its upstream is still refused.
///
/// `dir` supplies the HEAD half, and must be the worktree whose HEAD will be
/// current when the delete runs. `--merged` is relative to HEAD, and every
/// branch is merged into itself, so asking from inside the worktree being
/// removed would report its own branch as merged and skip the warning it exists
/// to give.
pub fn unmerged_branches(dir: Option<&Path>) -> AppResult<HashMap<String, Unmerged>> {
    let merged_output = run_in(dir, &["branch", "--format=%(refname:short)", "--merged"])?;
    let refs_output = read_branch_refs()?;
    Ok(unmerged_from(&merged_output, &branch_refs(&refs_output)))
}

/// The rule half of [`unmerged_branches`], split out so it can be tested
/// against fixed git output.
fn unmerged_from(
    merged_output: &str,
    refs: &HashMap<&str, BranchRef<'_>>,
) -> HashMap<String, Unmerged> {
    let merged: HashSet<&str> = merged_output.lines().collect();

    refs.iter()
        .filter_map(|(&name, r)| {
            // A `[gone]` upstream reports no ahead count, so it must be treated
            // as absent rather than as "zero ahead, therefore merged".
            if r.upstream_branch.is_empty() || r.gone() {
                return (!merged.contains(name)).then(|| (name.to_string(), Unmerged::NoUpstream));
            }
            let (ahead, _) = parse_track(r.track);
            (ahead > 0).then(|| (name.to_string(), Unmerged::Ahead(ahead)))
        })
        .collect()
}

/// What was established when a branch was proven *Equivalent*: where the branch
/// stood, and the anchor it was proven against. A *License* covers what was
/// proven and nothing more, so both are re-checked before the force-delete — a
/// proof survives neither the branch moving nor the anchor being rewound out
/// from under it. See [ADR
/// 0005](../docs/adr/0005-proof-of-equivalence-is-a-license.md).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proof {
    /// The ref the proof was made against, kept so it can be resolved again.
    pub anchor_ref: String,
    pub anchor_tip: String,
    pub tip: String,
}

/// Branches whose whole diff against the anchor is already in the anchor under
/// some other commit — squash-merged, rebase-merged or cherry-picked — each with
/// the [`Proof`] established for it.
///
/// Equivalence is positive evidence and only ever subtracts, so ask it only of
/// branches a warning would otherwise be drawn over — ones both stale and
/// unmerged. A branch cut from the anchor and never committed to has no diff to
/// find already there, and is never proven by this; anything that cannot be
/// established at all leaves the branch treated as holding unique work.
///
/// `dir` must be the worktree the delete will run from, so what is proven here
/// and what is re-checked there are read through the same root. Refs are shared,
/// so the two agree anyway; asking from one place is what keeps that a fact
/// rather than a coincidence.
#[must_use]
pub fn equivalent_branches(
    dir: Option<&Path>,
    remote: &str,
    candidates: &[&str],
) -> HashMap<String, Proof> {
    let Some((anchor_ref, _)) = merged_anchor(remote) else {
        return HashMap::new();
    };
    let Some(anchor_tip) = resolve(dir, &anchor_ref) else {
        return HashMap::new();
    };
    candidates
        .iter()
        .filter_map(|name| {
            let tip = resolve(dir, &format!("refs/heads/{name}"))?;
            // Every question below is asked of the *resolved* anchor, never the
            // ref name: a ref that moved mid-probe and moved back would
            // otherwise be proven against one commit and re-checked against
            // another. The name is kept only for that later re-check.
            equivalent_to(dir, &anchor_tip, &tip).then(|| {
                let proof = Proof {
                    anchor_ref: anchor_ref.clone(),
                    anchor_tip: anchor_tip.clone(),
                    tip,
                };
                ((*name).to_string(), proof)
            })
        })
        .collect()
}

/// Whether everything `tip` adds over its merge-base with the anchor is already
/// in the anchor. Two routes, either of which proves it, because a branch's work
/// can land in two shapes and neither test sees both:
///
/// - [`patch_landed`] asks whether the anchor already carries the branch's patch.
///   It survives the anchor moving on over the same files, and is the only route
///   that answers a squash merge — but it reads the branch's diff as *one* patch,
///   so a rebase-merge that replayed several commits individually defeats it.
/// - [`content_present`] asks whether the files the branch touched now read
///   identically in the anchor. That is blind to how the work got there, so a
///   rebase-merge or a scattered cherry-pick answers it, but any later edit to
///   the same files does too and it falls back to the first route.
///
/// `anchor` is a resolved commit, not a ref name, so every question is asked of
/// the same anchor the [`Proof`] will record. Every step is a question, so a git
/// that refuses one answers "not equivalent" and says nothing about it.
fn equivalent_to(dir: Option<&Path>, anchor: &str, tip: &str) -> bool {
    let Some(base) = run_in(dir, &["merge-base", anchor, tip])
        .ok()
        .map(|b| b.trim().to_string())
    else {
        return false;
    };
    patch_landed(dir, anchor, tip, &base) || content_present(dir, anchor, tip, &base)
}

/// Whether the anchor already carries the branch's patch.
///
/// Git compares content by patch id, but only between commits — so the branch's
/// whole diff is synthesised as one commit with `commit-tree`, parented on the
/// merge-base, and handed to `git cherry`, which prints `-` for a commit whose
/// patch is already upstream and `+` for one that isn't. The synthesised commit
/// is dangling and unreachable; gc reaps it. An empty diff has no patch id to
/// match, so a branch holding no work of its own comes back `+` — exactly the
/// answer equivalence owes it.
///
/// `cherry` is not the whole answer, because the patch ids it compares are
/// normalised: they ignore whitespace, so a branch differing from what landed by
/// whitespace alone would pass. That is fine for `git rebase`, which drops such a
/// commit but leaves the branch to be recovered from; it is not fine for a
/// force-delete. So `cherry` is used to *find* the commit that carries the patch
/// — asking it the other way round names it — and the two are then compared
/// verbatim, whitespace and all.
fn patch_landed(dir: Option<&Path>, anchor: &str, tip: &str, base: &str) -> bool {
    let landed = || {
        let probe = run_in(
            dir,
            &[
                "commit-tree",
                &format!("{tip}^{{tree}}"),
                "-p",
                base,
                "-m",
                "perch: equivalence probe",
            ],
        )
        .ok()?;
        let probe = probe.trim();
        if !run_in(dir, &["cherry", anchor, probe])
            .ok()?
            .lines()
            .next()?
            .starts_with('-')
        {
            return None;
        }
        // Asked this way round, `-` marks the anchor-side commits whose patch the
        // probe already carries: the landing commits themselves.
        let matches = run_in(dir, &["cherry", probe, anchor]).ok()?;
        let wanted = verbatim_patch_id(dir, "diff", &[base, tip])?;
        let exact = matches
            .lines()
            .filter_map(|l| l.strip_prefix("- "))
            .any(|sha| verbatim_patch_id(dir, "show", &[sha]).is_some_and(|id| id == wanted));
        exact.then_some(true)
    };
    landed().unwrap_or(false)
}

/// The patch id of the diff `command` produces over `rest`, computed *verbatim*
/// — the whitespace-sensitive reading, unlike the normalised ids `git cherry`
/// compares. `git patch-id` reads a diff on stdin and answers `<patch-id>
/// <commit-id>`; only the first field is a fact about the content.
///
/// The diff is asked for in [`UNSHRINKABLE`] terms like every other the proof
/// reads: a textconv driver that renders two files alike would otherwise make an
/// exact comparison agree about content that differs, which is the whole thing
/// this comparison exists to catch.
fn verbatim_patch_id(dir: Option<&Path>, command: &str, rest: &[&str]) -> Option<String> {
    let mut args = vec![command];
    args.extend(UNSHRINKABLE);
    args.extend(rest);
    let diff = run_in(dir, &args).ok()?;
    let output = run_with_stdin(dir, &["patch-id", "--verbatim"], diff.as_bytes()).ok()?;
    let id = output.split_whitespace().next()?.to_string();
    (!id.is_empty()).then_some(id)
}

/// Diff options that stop repository configuration from shrinking a comparison
/// the proof depends on. Each one has a config that would otherwise hide a real
/// difference — `diff.ignoreSubmodules` a changed gitlink, a `textconv` driver
/// or `diff.external` two files that render alike — and a proof read from a
/// shrunken diff is a branch deleted for work the anchor never took.
const UNSHRINKABLE: [&str; 3] = ["--ignore-submodules=none", "--no-textconv", "--no-ext-diff"];

/// Whether every path the branch touched since the merge-base now reads
/// byte-identically in the anchor. A branch that touched nothing proves nothing
/// — the whole-tree comparison a missing pathspec would run is exactly the
/// "trivially equivalent" answer ADR 0005 refuses, so an empty list is a `false`
/// and never a `git diff` without paths.
fn content_present(dir: Option<&Path>, anchor: &str, tip: &str, base: &str) -> bool {
    // `-z` because git otherwise quotes paths that need it, and a quoted path is
    // not the path. `--no-renames` because a rename is reported as its
    // destination alone, which would drop the source — a deletion the branch
    // performed — out of the comparison, and prove a branch whose deletion never
    // landed. The rest is [`UNSHRINKABLE`]'s business.
    let mut listing_args = vec!["diff"];
    listing_args.extend(UNSHRINKABLE);
    listing_args.extend(["--name-only", "--no-renames", "-z", base, tip]);
    let Ok(listing) = run_in(dir, &listing_args) else {
        return false;
    };
    // A path perch cannot read is a path it cannot compare: `run_in` decodes
    // lossily, and a mangled path matches nothing as a pathspec — which `git
    // diff --quiet` reports as no difference, proving the branch on an empty
    // comparison. Refuse the whole answer rather than a silent subset.
    if listing.contains('\u{fffd}') {
        return false;
    }
    let touched: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    if touched.is_empty() {
        return false;
    }
    // `--literal-pathspecs` because these are filenames, not patterns, and `--`
    // does not disable pathspec magic: a file literally named `:(exclude)x`
    // would otherwise cancel the comparison of `x` and prove the branch on what
    // was left.
    let mut args = vec!["--literal-pathspecs", "diff"];
    args.extend(UNSHRINKABLE);
    args.extend(["--quiet", tip, anchor, "--"]);
    args.extend(touched);
    // A difference exits 1, which `run_in` reports as an error — and a branch
    // whose files read differently is exactly the branch this cannot prove.
    run_in(dir, &args).is_ok()
}

/// What `refname` points at, or `None` where it doesn't resolve. A ref that is
/// absent and a ref that could not be read answer alike here; where the
/// difference matters, ask [`ref_state`] instead.
#[must_use]
pub(crate) fn resolve(dir: Option<&Path>, refname: &str) -> Option<String> {
    rev_parse(dir, refname).ok()
}

/// What a ref lookup established: where the ref points, that it is not there, or
/// that the question went unanswered. The last is not the middle — reporting a
/// branch gone on the strength of a failed read is how a user is told to
/// recreate something that already exists.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RefState {
    At(String),
    Missing,
    Unreadable(String),
}

/// `rev-parse --verify --quiet` separates the three, but not by exit code alone:
/// it exits 0 with the object id, and 1 both for a ref that isn't there *and*
/// for one that is there but broken — the second warning about it on stderr, the
/// first saying nothing at all. So silence is what distinguishes them. Only the
/// presence of output is read and never its wording, which would be a fact about
/// the user's locale rather than about the ref.
fn ref_state(dir: Option<&Path>, refname: &str) -> RefState {
    let Ok(output) = git_cmd(dir)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
    else {
        return RefState::Unreadable("git could not be run".to_string());
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let unreadable = || {
        RefState::Unreadable(if stderr.is_empty() {
            format!("git rev-parse exited {:?}", output.status.code())
        } else {
            stderr.clone()
        })
    };
    match output.status.code() {
        Some(0) => {
            let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if oid.is_empty() {
                unreadable()
            } else {
                RefState::At(oid)
            }
        }
        Some(1) if stderr.is_empty() => RefState::Missing,
        _ => unreadable(),
    }
}

#[must_use]
pub fn worktree_for_branch(worktrees: &[Worktree], branch: &str) -> Option<Worktree> {
    worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .cloned()
}

/// Add a worktree at `path` for `branch`. If `base` is `Some`, create the
/// branch from `base` (e.g. `origin/main`); if `None`, check out an existing
/// local or remote-tracking branch.
pub fn worktree_add(path: &Path, branch: &str, base: Option<&str>) -> AppResult<()> {
    let path_str = path_to_str(path)?;
    // `--track` rather than relying on git's default: `branch.autoSetupMerge`
    // can be off, and the upstream a new branch carries is what tells
    // `stale_branches` it was branched off the anchor rather than merged
    // into it.
    let args: Vec<&str> = match base {
        Some(base) => vec!["worktree", "add", "--track", "-b", branch, path_str, base],
        None => vec!["worktree", "add", path_str, branch],
    };

    let output = Command::new("git").args(&args).output()?;
    if output.status.success() {
        return Ok(());
    }

    // A worktree whose directory was deleted by hand stays registered and
    // blocks re-adding ("missing but already registered"). Clear the dead
    // registrations and try once more.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_stale_worktree_error(&stderr) {
        let _ = worktree_prune();
        let retry = Command::new("git").args(&args).output()?;
        if retry.status.success() {
            return Ok(());
        }
        return Err(Error::Git {
            command: "worktree add".to_string(),
            message: String::from_utf8_lossy(&retry.stderr).trim().to_string(),
        });
    }

    Err(Error::Git {
        command: "worktree add".to_string(),
        message: stderr.trim().to_string(),
    })
}

/// Remove worktree registrations whose working directories no longer exist.
pub fn worktree_prune() -> AppResult<()> {
    run(&["worktree", "prune"])?;
    Ok(())
}

/// True when git refused because a branch/path is held by a worktree whose
/// directory is gone — a `git worktree prune` clears the stale registration.
fn is_stale_worktree_error(stderr: &str) -> bool {
    stderr.contains("already registered") || stderr.contains("used by worktree")
}

#[derive(Debug, Clone)]
pub enum WorktreeRemoveOutcome {
    Removed,
    Failed(String),
}

/// Remove the worktree at `path`. With `force`, uncommitted and untracked
/// changes in it are discarded; without it, git refuses a dirty tree. A *locked*
/// worktree survives either way (git wants `--force --force`) and is reported as
/// a failure rather than escalated.
pub fn worktree_remove(path: &Path, force: bool) -> AppResult<WorktreeRemoveOutcome> {
    let path_str = path_to_str(path)?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    let output = Command::new("git").args(&args).output()?;
    if output.status.success() {
        return Ok(WorktreeRemoveOutcome::Removed);
    }

    // The directory is already gone (deleted by hand): `git worktree remove` may
    // balk, but `prune` clears the dead registration. Only claim success if the
    // entry is actually gone afterwards — a locked worktree survives prune, and
    // reporting a phantom removal would then delete a branch still held by it.
    if !path.exists() {
        let _ = worktree_prune();
        let still_registered = worktree_list()?.iter().any(|w| w.path == path);
        if !still_registered {
            return Ok(WorktreeRemoveOutcome::Removed);
        }
    }

    Ok(WorktreeRemoveOutcome::Failed(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn path_to_str(path: &Path) -> AppResult<&str> {
    path.to_str().ok_or_else(|| Error::Git {
        command: "worktree".to_string(),
        message: format!("non-utf8 path: {}", path.display()),
    })
}

/// Delete `branch` only while it still points at `expected`, discarding the
/// commits it holds. `None` means it had moved, and nothing was deleted.
///
/// This is the delete a *Proof* licenses, and it is one command because the
/// proof covers one commit: checking the tip and then running `branch -D` would
/// leave a window in which the branch could grow a commit nobody proved and lose
/// it unwarned. `update-ref` compares and deletes in a single operation, closing
/// it. It leaves the branch's config behind where `branch -D` wouldn't, so that
/// is cleared afterwards — a branch that is gone must not leave an upstream
/// setting for a later branch of the same name to inherit.
pub fn delete_branch_at(
    dir: Option<&Path>,
    branch: &str,
    expected: &str,
) -> AppResult<Option<BranchDeleteOutcome>> {
    let refname = format!("refs/heads/{branch}");
    // `update-ref` is plumbing and will happily delete a branch some worktree
    // still has checked out, which `branch -D` refuses — leaving that worktree
    // pointing at a ref that no longer exists. A held branch normally reaches
    // this only after its worktree has gone, but one that appeared since the row
    // was drawn is exactly the "became risky after the warning" case ADR 0001
    // hands to git's own guard, so the guard is kept here by hand. Asked before
    // the delete it is only a check-then-act, so it is asked again after and the
    // ref put back where it was if a worktree won the race: `update-ref` is one
    // operation but two of them are not, and the branch a worktree checked out
    // is the branch it must still find there.
    if held_by_worktree(dir, branch)? {
        return Ok(Some(refused_as_held(branch)));
    }
    let output = git_cmd(dir)
        .args(["update-ref", "-d", &refname, expected])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Git words the mismatch several ways across versions, so the tip is
        // re-read rather than the message parsed: still there and unmoved means
        // a real failure, anything else means the proof no longer covers it.
        return Ok(match resolve(dir, &refname) {
            Some(tip) if tip == expected => {
                Some(BranchDeleteOutcome::Failed(stderr.trim().to_string()))
            }
            _ => None,
        });
    }
    // Past the delete, nothing may leave by `?`: the branch is already gone, and
    // an error that travels takes with it the fact that it went. A holder found
    // *or* a holder that couldn't be looked for both mean the same thing here —
    // put it back — and only the restore itself failing is worth an outcome of
    // its own, since that is the one state nobody can repair from the message
    // alone.
    let held = held_by_worktree(dir, branch);
    if matches!(held, Ok(false)) {
        return Ok(Some(clear_branch_config(dir, branch)));
    }
    // The empty old-value pins "and only if it does not exist yet", so a ref
    // recreated in the gap is not clobbered by putting this one back.
    let holder = match held {
        // A holder was seen; that a holder exists is a fact worth reporting.
        Ok(_) => Holder::Seen,
        // Only the question failed, so nothing may be said about an answer.
        Err(_) => Holder::Unknown,
    };
    Ok(Some(
        match run_in(dir, &["update-ref", &refname, expected, ""]) {
            Ok(_) => match held {
                Ok(_) => refused_as_held(branch),
                Err(e) => BranchDeleteOutcome::Failed(format!(
                    "couldn't tell whether a worktree holds it: {e}"
                )),
            },
            // A refused restore is not the same as a missing branch: the empty
            // old-value also refuses where the ref exists again, which is a
            // repository needing nothing rather than one needing repair. So the
            // ref is read before anything is claimed about it — and read in a
            // way that can say "I don't know", since telling someone to recreate
            // a branch that may be standing there is its own kind of wrong.
            Err(e) => match ref_state(dir, &refname) {
                RefState::At(now) => BranchDeleteOutcome::DeletedThenRecreated {
                    tip: expected.to_string(),
                    now,
                },
                RefState::Missing => BranchDeleteOutcome::DeletedNotRestored {
                    tip: expected.to_string(),
                    detail: e.to_string(),
                    holder,
                },
                RefState::Unreadable(why) => BranchDeleteOutcome::DeletedStateUnknown {
                    tip: expected.to_string(),
                    detail: format!("{e}; and reading it back failed too: {why}"),
                    holder,
                },
            },
        },
    ))
}

/// Whether any worktree has `branch` checked out, asked from the same `dir` the
/// delete runs in — the process cwd may be a worktree this very run has removed,
/// and a question asked from a directory that is gone answers nothing. An error
/// is not an answer either, so it travels rather than reading as "nobody holds
/// it": the whole point of the question is to refuse where it cannot be settled.
fn held_by_worktree(dir: Option<&Path>, branch: &str) -> AppResult<bool> {
    Ok(worktree_list_in(dir)?
        .into_iter()
        .any(|w| w.branch.as_deref() == Some(branch)))
}

fn refused_as_held(branch: &str) -> BranchDeleteOutcome {
    BranchDeleteOutcome::Failed(format!(
        "cannot delete branch '{branch}': it is checked out in a worktree"
    ))
}

/// Clears the config a deleted branch leaves behind, which `branch -D` would
/// have taken with it. A branch that is gone must not leave an upstream setting
/// for a later branch of the same name to inherit.
///
/// Each key is unset by its exact name rather than the section dropped whole:
/// `--remove-section branch.<name>` cannot parse every name git allows — it
/// fails outright on `topic]x` — and would leave the keys it choked on behind.
/// Nothing to remove is the ordinary case and not a failure.
///
/// What survives is read back rather than inferred from exit codes: `--list`
/// repeats a multi-valued key, and the `--unset-all` that clears the first
/// mention then fails on the second having nothing left to do — an accurate
/// report can only come from looking again. Whatever is still there is named,
/// since the branch has gone and the leftovers are now the user's to clear.
fn clear_branch_config(dir: Option<&Path>, branch: &str) -> BranchDeleteOutcome {
    let keys = |listing: String| -> Vec<String> {
        listing
            .split('\0')
            .filter(|key| config_branch(key) == Some(branch))
            .map(String::from)
            .collect()
    };
    let mut to_clear = match local_config_keys(dir) {
        Ok(listing) => keys(listing),
        // Config that can't even be read can't be cleared, and saying the branch
        // went cleanly would be a claim about something never looked at — but
        // nor is any key known to have survived, so this is neither.
        Err(e) => return BranchDeleteOutcome::DeletedConfigUnverified(e.to_string()),
    };
    to_clear.dedup();
    for key in &to_clear {
        let _ = run_in(dir, &["config", "--local", "--unset-all", key]);
    }
    let left = match local_config_keys(dir) {
        Ok(listing) => keys(listing),
        Err(e) => return BranchDeleteOutcome::DeletedConfigUnverified(e.to_string()),
    };
    if left.is_empty() {
        return BranchDeleteOutcome::Deleted;
    }
    BranchDeleteOutcome::DeletedLeavingConfig(left.join(", "))
}

fn local_config_keys(dir: Option<&Path>) -> AppResult<String> {
    run_in(dir, &["config", "--local", "--list", "--name-only", "-z"])
}

/// Which branch a `branch.<name>.<key>` config entry belongs to, or `None` for
/// any other key. Git splits a config name at its *first* and *last* dot, so the
/// branch in the middle keeps whatever dots it has: `branch.release/1.2.remote`
/// belongs to `release/1.2` and not to `release/1`, and matching on a prefix
/// would have one branch's deletion clear the other's upstream.
fn config_branch(key: &str) -> Option<&str> {
    let (branch, _) = key.strip_prefix("branch.")?.rsplit_once('.')?;
    (!branch.is_empty()).then_some(branch)
}

/// Delete a branch with `git branch -D`, discarding unmerged commits. Only for
/// branches whose risk was shown to the user first — see [`unmerged_branches`] —
/// or for an explicit `--force`. A branch licensed by *proof* instead goes
/// through [`delete_branch_at`], which is pinned to what was proven.
///
/// `dir` must be the worktree whose HEAD the risk was judged from, so that the
/// markers shown and the deletion performed agree about what is merged.
pub fn force_delete_branch(dir: Option<&Path>, branch: &str) -> AppResult<BranchDeleteOutcome> {
    let output = git_cmd(dir)
        .args(["branch", "-D", "--quiet", branch])
        .output()?;
    if output.status.success() {
        return Ok(BranchDeleteOutcome::Deleted);
    }
    Ok(BranchDeleteOutcome::Failed(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// What became of a branch. The three `Deleted*` cases all mean the branch went
/// — they differ in what happened *after* that, and are kept apart because a
/// line reading "could not delete" over a branch that was in fact deleted tells
/// the user the opposite of the repository's state.
#[derive(Debug, Clone)]
pub enum BranchDeleteOutcome {
    Deleted,
    /// Deleted, but whether its config went with it could not be established —
    /// git's reason, since no key was ever read to name.
    DeletedConfigUnverified(String),
    /// Deleted, but config of its own outlived it — the keys, so the user can
    /// clear what `perch` couldn't.
    DeletedLeavingConfig(String),
    /// Deleted at `tip`, and confirmed gone after putting it back failed. The
    /// one outcome describing a repository that needs repair — `holder` says
    /// whether a worktree was actually seen holding the branch, since the
    /// restore is attempted where that question is what failed too.
    DeletedNotRestored {
        tip: String,
        detail: String,
        holder: Holder,
    },
    /// Deleted at `tip`, and whether it is there now could not be established:
    /// putting it back failed, and so did reading it afterwards. Distinct from
    /// [`Self::DeletedNotRestored`] because nothing may be recommended over a
    /// ref nobody could look at — `holder` survives all the same, being a fact
    /// established before any of that went wrong.
    DeletedStateUnknown {
        tip: String,
        detail: String,
        holder: Holder,
    },
    /// Deleted at `tip`, and standing again at `now` by another hand: the
    /// restore was refused because there was nothing left to restore into.
    /// Nothing to repair — the branch of that name is simply no longer the one
    /// that was proven.
    DeletedThenRecreated {
        tip: String,
        now: String,
    },
    /// Kept because it has commits not merged into its upstream or HEAD.
    NotMerged,
    Failed(String),
}

/// What became of a selected upstream deletion. The lease-specific outcome is
/// distinct from a server refusal because retrying the latter unchanged may make
/// sense, while a moved ref must be inspected before anything tries again.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RemoteBranchDeleteOutcome {
    Deleted,
    AlreadyAbsent,
    Moved { expected: String, now: String },
    Failed(String),
}

/// Deletes `upstream` only while the server still has the tip shown to the user.
/// A failed push is followed by one live read so a lease refusal and an already
/// completed deletion are not reported as a generic transport failure.
pub fn delete_remote_branch(upstream: &RemoteBranch) -> AppResult<RemoteBranchDeleteOutcome> {
    let refname = format!("refs/heads/{}", upstream.branch);
    let lease = format!("--force-with-lease={refname}:{}", upstream.tip);
    let delete = format!(":{refname}");
    let output = git_cmd(None)
        .args(["push", "--quiet", &lease, "--", &upstream.remote, &delete])
        .output()?;
    if output.status.success() {
        return Ok(RemoteBranchDeleteOutcome::Deleted);
    }

    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let read = run(&["ls-remote", "--heads", "--", &upstream.remote, &refname]);
    let Ok(read) = read else {
        return Ok(RemoteBranchDeleteOutcome::Failed(detail));
    };
    let now = read
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(oid, found)| (found == refname).then_some(oid));
    Ok(match now {
        None => RemoteBranchDeleteOutcome::AlreadyAbsent,
        Some(now) if now != upstream.tip => RemoteBranchDeleteOutcome::Moved {
            expected: upstream.tip.clone(),
            now: now.to_string(),
        },
        Some(_) => RemoteBranchDeleteOutcome::Failed(detail),
    })
}

/// Whether a worktree was seen holding a branch, or only failed to be ruled out.
/// The two read alike in what they cause — the branch goes back either way — and
/// differently in what may be said about them.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Holder {
    Seen,
    Unknown,
}

/// Delete `branch` only if git considers it fully merged (`git branch -d`).
/// Unlike [`delete_branches`] this never force-deletes, so unmerged work is
/// preserved rather than silently discarded.
///
/// `-d` judges merged-ness against the HEAD it runs under, so `dir` must be the
/// worktree the caller measured risk from — see [`unmerged_branches`].
pub fn delete_branch_if_merged(dir: Option<&Path>, branch: &str) -> AppResult<BranchDeleteOutcome> {
    let output = git_cmd(dir)
        .args(["branch", "-d", "--quiet", branch])
        .output()?;
    if output.status.success() {
        return Ok(BranchDeleteOutcome::Deleted);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not fully merged") {
        return Ok(BranchDeleteOutcome::NotMerged);
    }
    Ok(BranchDeleteOutcome::Failed(stderr.trim().to_string()))
}

fn rev_parse(dir: Option<&Path>, refname: &str) -> AppResult<String> {
    let output = run_in(dir, &["rev-parse", refname])?;
    Ok(output.trim().to_string())
}

/// A `git` command, optionally rooted in `dir` via `-C` so it operates on
/// another worktree without mutating the process's working directory.
fn git_cmd(dir: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd
}

fn run(args: &[&str]) -> AppResult<String> {
    run_in(None, args)
}

/// Runs a git command that reads its input on stdin, which is how the plumbing
/// that takes a diff — `patch-id` — is driven. The input is written from memory
/// rather than piped from another process, so a git that exits early can't wedge
/// this on a full pipe.
fn run_with_stdin(dir: Option<&Path>, args: &[&str], input: &[u8]) -> AppResult<String> {
    use std::io::Write;

    let mut child = git_cmd(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    // Dropping the handle closes stdin, which is what tells git the diff ended.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::Git {
            command: args.first().copied().unwrap_or("<unknown>").to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_in(dir: Option<&Path>, args: &[&str]) -> AppResult<String> {
    let output = git_cmd(dir).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git {
            command: args.first().copied().unwrap_or("<unknown>").to_string(),
            message: stderr.trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        Anchor, Ground, StaleBranch, Unmerged, branch_refs, parse_track, stale_from, unmerged_from,
    };
    use std::collections::HashMap;

    /// One `for-each-ref` line per row: name, tip, upstream branch, track.
    fn head_refs(rows: &[(&str, &str, &str, &str)]) -> String {
        rows.iter()
            .map(|(name, tip, upstream, track)| format!("{name}\t{tip}\t{upstream}\t{track}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Run the `-d` mirror. Tips play no part in it, so rows name only the
    /// branch, its upstream, and its track summary.
    fn unmerged(merged: &str, rows: &[(&str, &str, &str)]) -> HashMap<String, Unmerged> {
        let rows: Vec<_> = rows
            .iter()
            .map(|(name, upstream, track)| (*name, "tip", *upstream, *track))
            .collect();
        let refs_output = head_refs(&rows);
        unmerged_from(merged, &branch_refs(&refs_output))
    }

    /// Run the rule against an anchor named `main`, keeping only the names —
    /// which clause fired is [`grounds`]' concern. Its own branch is left out of
    /// `merged` throughout: [`stale_branches`] filters it as kept, so it isn't
    /// this half's concern.
    fn stale(refs_output: &str, merged: &str, anchor_tip: &str) -> Vec<String> {
        let mut got: Vec<String> = grounds(refs_output, merged, anchor_tip)
            .into_iter()
            .map(|b| b.name)
            .collect();
        got.sort();
        got
    }

    /// As [`stale`], but keeping the ground each branch is stale on.
    fn grounds(refs_output: &str, merged: &str, anchor_tip: &str) -> Vec<StaleBranch> {
        let refs = branch_refs(refs_output);
        let anchor = Anchor {
            branch: "main",
            tip: anchor_tip,
        };
        let mut got = stale_from(&refs, merged, Some(&anchor));
        got.sort_by(|a, b| a.name.cmp(&b.name));
        got
    }

    /// Each fact that qualifies a branch is carried to the row independently.
    #[test]
    fn each_staleness_fact_reports_its_own_ground() {
        let refs_output = head_refs(&[
            ("main", "aaa", "refs/heads/main", ""),
            ("abandoned", "bbb", "refs/heads/abandoned", "gone"),
            ("ahead", "ccc", "refs/heads/main", "ahead 1"),
            ("untracked", "ddd", "", ""),
        ]);
        let got = grounds(&refs_output, "ahead\nuntracked", "aaa");
        assert_eq!(
            got,
            vec![
                StaleBranch {
                    ground: Ground::Gone,
                    name: "abandoned".to_string(),
                },
                StaleBranch {
                    ground: Ground::TracksAnchorAhead,
                    name: "ahead".to_string(),
                },
                StaleBranch {
                    ground: Ground::UntrackedTipInAnchor,
                    name: "untracked".to_string(),
                },
            ]
        );
    }

    /// A branch whose upstream is gone is never tested against the anchor, so
    /// it cannot appear twice or under the wrong ground — even when the
    /// anchor's `--merged` list names it.
    #[test]
    fn a_gone_branch_listed_as_merged_is_reported_once_as_gone() {
        let refs_output = head_refs(&[
            ("main", "aaa", "refs/heads/main", ""),
            ("abandoned", "bbb", "refs/heads/abandoned", "gone"),
        ]);
        let got = grounds(&refs_output, "abandoned", "aaa");
        assert_eq!(
            got,
            vec![StaleBranch {
                ground: Ground::Gone,
                name: "abandoned".to_string(),
            }]
        );
    }

    /// The reported bug: a branch cut from the anchor and never committed to
    /// tracks the *anchor's* counterpart, not its own, so it is not stale —
    /// whether or not the anchor has since moved past it.
    #[test]
    fn branch_cut_from_the_anchor_is_not_stale() {
        let refs_output = head_refs(&[
            ("main", "aaa", "refs/heads/main", ""),
            ("feature", "bbb", "refs/heads/main", "behind 1"),
        ]);
        let got = stale(&refs_output, "feature", "aaa");
        assert!(
            got.is_empty(),
            "a freshly cut branch is not stale, got: {got:?}"
        );
    }

    /// A `wt` branch tracks the anchor's counterpart, so being ahead of it is
    /// what marks the commits as its own — and being in `--merged` is where
    /// they went. These refs carry both readings at once, which is the point:
    /// a branch the anchor fast-forwarded over, and — accepted misfire 2 of
    /// ADR 0002 — an empty branch cut from an anchor that was already ahead,
    /// inheriting a count it never earned. Nothing distinguishes them, so the
    /// rule offers both.
    #[test]
    fn branch_ahead_of_the_anchors_counterpart_is_stale_once_merged() {
        let refs_output = head_refs(&[
            ("main", "bbb", "refs/heads/main", "ahead 1"),
            ("feature", "bbb", "refs/heads/main", "ahead 1"),
        ]);
        let got = stale(&refs_output, "feature", "bbb");
        assert_eq!(got, vec!["feature".to_string()]);
    }

    /// A branch that published its own counterpart is never judged by its tip:
    /// one whose commits the anchor fast-forwarded over is identical to one
    /// pushed without any. Offering it would delete branches nobody finished.
    #[test]
    fn published_branch_sharing_the_anchors_tip_is_not_stale() {
        let refs_output = head_refs(&[
            ("main", "bbb", "refs/heads/main", ""),
            ("feature", "bbb", "refs/heads/feature", ""),
        ]);
        let got = stale(&refs_output, "feature", "bbb");
        assert!(
            got.is_empty(),
            "an empty published branch must not be offered, got: {got:?}"
        );
    }

    /// Tracks nothing and the anchor has moved past it: merged locally, then
    /// left behind.
    #[test]
    fn untracked_branch_behind_the_anchor_is_stale() {
        let refs_output = head_refs(&[
            ("main", "ccc", "refs/heads/main", ""),
            ("scratch", "bbb", "", ""),
        ]);
        let got = stale(&refs_output, "scratch", "ccc");
        assert_eq!(got, vec!["scratch".to_string()]);
    }

    /// A branch published under its own name is left alone however its commits
    /// sit relative to the anchor: whatever the shape, an empty branch pushed at
    /// a merged topic's tip presents the same one.
    #[test]
    fn published_branch_is_not_stale() {
        let refs_output = head_refs(&[
            ("main", "ccc", "refs/heads/main", ""),
            ("feature", "bbb", "refs/heads/feature", ""),
        ]);
        let got = stale(&refs_output, "feature", "ccc");
        assert!(
            got.is_empty(),
            "a live upstream of its own settles nothing, got: {got:?}"
        );
    }

    /// The anchor having moved past it says nothing about a `wt` branch — an
    /// empty one cut from a merged topic sits behind the anchor exactly as one
    /// whose work the anchor absorbed does. Only the ahead count separates them.
    #[test]
    fn anchor_tracking_branch_behind_the_anchor_is_not_stale() {
        let refs_output = head_refs(&[
            ("main", "ccc", "refs/heads/main", ""),
            ("feature", "bbb", "refs/heads/main", ""),
        ]);
        let got = stale(&refs_output, "feature", "ccc");
        assert!(
            got.is_empty(),
            "a zero ahead count settles nothing, got: {got:?}"
        );
    }

    /// A deleted upstream speaks for itself, without appearing in `--merged`.
    #[test]
    fn gone_upstream_is_stale_without_being_merged() {
        let refs_output = head_refs(&[
            ("main", "aaa", "refs/heads/main", ""),
            ("merged-work", "bbb", "", ""),
            ("abandoned", "ccc", "refs/heads/abandoned", "gone"),
        ]);
        let got = stale(&refs_output, "", "aaa");
        assert_eq!(
            got,
            vec!["abandoned".to_string()],
            "a deleted upstream needs no merged listing"
        );
    }

    /// With no anchor the merged list is never consulted, so a branch listed
    /// there is still not stale.
    #[test]
    fn without_an_anchor_the_merged_list_is_ignored() {
        let refs_output = head_refs(&[("scratch", "bbb", "", "")]);
        let refs = branch_refs(&refs_output);
        let got = stale_from(&refs, "scratch", None);
        assert!(got.is_empty(), "no anchor, no merged rule, got: {got:?}");
    }

    /// `-d` accepts merged-into-HEAD *or* merged-into-upstream, but they are
    /// alternatives: where an upstream exists it alone decides, and git refuses
    /// with "not yet merged to <upstream>, even though it is merged to HEAD".
    #[test]
    fn ahead_of_upstream_is_unmerged_even_when_merged_into_head() {
        let got = unmerged(
            "main\nfeature",
            &[("feature", "refs/heads/feature", "ahead 2")],
        );
        assert_eq!(got.get("feature"), Some(&Unmerged::Ahead(2)));
    }

    /// HEAD decides only where no upstream was set.
    #[test]
    fn untracked_branch_merged_into_head_is_not_unmerged() {
        let got = unmerged("main\nscratch", &[("scratch", "", "")]);
        assert!(got.is_empty(), "merged into HEAD, got: {got:?}");
    }

    #[test]
    fn in_sync_with_upstream_is_not_unmerged() {
        let got = unmerged("main", &[("feature", "refs/heads/feature", "")]);
        assert!(got.is_empty(), "merged into upstream, got: {got:?}");
    }

    #[test]
    fn ahead_of_upstream_is_unmerged_with_a_count() {
        let got = unmerged(
            "main",
            &[("feature", "refs/heads/feature", "ahead 3, behind 1")],
        );
        assert_eq!(got.get("feature"), Some(&Unmerged::Ahead(3)));
    }

    /// The case a plain ahead-of-upstream check misses: no upstream means
    /// nothing to be ahead of, yet `git branch -d` still refuses.
    #[test]
    fn local_only_branch_is_unmerged_without_a_count() {
        let got = unmerged("main", &[("scratch", "", "")]);
        assert_eq!(got.get("scratch"), Some(&Unmerged::NoUpstream));
    }

    /// A `[gone]` upstream reports no ahead count, which must not be read as
    /// "zero ahead, therefore merged".
    #[test]
    fn gone_upstream_is_unmerged_without_a_count() {
        let got = unmerged("main", &[("orphan", "refs/heads/orphan", "gone")]);
        assert_eq!(got.get("orphan"), Some(&Unmerged::NoUpstream));
    }

    /// Git splits a config name at its first and last dot, so a branch whose
    /// name contains one owns the whole middle — and a branch whose name is a
    /// prefix of another's owns none of it.
    #[test]
    fn a_config_key_belongs_to_the_branch_between_the_outer_dots() {
        use super::config_branch;
        assert_eq!(config_branch("branch.feature.remote"), Some("feature"));
        assert_eq!(
            config_branch("branch.release/1.2.remote"),
            Some("release/1.2")
        );
        assert_eq!(config_branch("branch.topic]x.merge"), Some("topic]x"));
        assert_eq!(config_branch("remote.origin.url"), None);
        assert_eq!(config_branch("branch.autosetupmerge"), None);
    }

    #[test]
    fn parse_track_reads_ahead_behind() {
        assert_eq!(parse_track("ahead 2"), (2, 0));
        assert_eq!(parse_track("behind 1"), (0, 1));
        assert_eq!(parse_track("ahead 2, behind 1"), (2, 1));
    }

    #[test]
    fn parse_track_in_sync_or_gone_is_zero() {
        assert_eq!(parse_track(""), (0, 0));
        assert_eq!(parse_track("gone"), (0, 0));
    }
}
